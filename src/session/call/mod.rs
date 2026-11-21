//! Voice calls compatible with Element Call (`MatrixRTC` over `LiveKit`).

use std::{
    cell::{Cell, OnceCell, RefCell},
    sync::LazyLock,
};

use gettextrs::gettext;
use gtk::{
    glib,
    glib::{clone, subclass::Signal},
    prelude::*,
    subclass::prelude::*,
};
use tokio::sync::mpsc;
use tracing::{debug, error};

mod app_audio;
mod engine;
mod rtc;
mod screen;

pub(crate) use self::app_audio::{AudioApp, ShareAudio, playing_apps};
use self::engine::{Command, Update};
use crate::{prelude::*, session::Room, spawn, spawn_tokio};

/// The state of a call.
#[derive(Debug, Default, Hash, Eq, PartialEq, Clone, Copy, glib::Enum)]
#[enum_type(name = "CallState")]
pub enum CallState {
    /// We are joining the call.
    #[default]
    Connecting,
    /// We are in the call.
    Connected,
    /// The call is over.
    Ended,
}

mod imp {
    use super::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::Call)]
    pub struct Call {
        /// The room of this call.
        #[property(get, construct_only)]
        room: OnceCell<Room>,
        /// The state of this call.
        #[property(get, builder(CallState::default()))]
        state: Cell<CallState>,
        /// Whether our microphone is muted.
        #[property(get, set = Self::set_muted, explicit_notify)]
        muted: Cell<bool>,
        /// The number of people in the call, including us.
        #[property(get)]
        participants_count: Cell<u32>,
        /// The display names of the people speaking right now.
        #[property(get)]
        speaking: RefCell<String>,
        /// Whether we are sharing our screen.
        #[property(get)]
        sharing_screen: Cell<bool>,
        /// Whether we are waiting for the user to choose what to share.
        #[property(get)]
        starting_screen_share: Cell<bool>,
        /// Why the call ended, if it failed.
        #[property(get, nullable)]
        error: RefCell<Option<String>>,
        /// The channel to send commands to the call task.
        pub(super) commands: RefCell<Option<mpsc::UnboundedSender<Command>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Call {
        const NAME: &'static str = "Call";
        type Type = super::Call;
    }

    #[glib::derived_properties]
    impl ObjectImpl for Call {
        fn signals() -> &'static [Signal] {
            static SIGNALS: LazyLock<Vec<Signal>> = LazyLock::new(|| {
                vec![
                    // Sharing the screen, or its sound, failed.
                    Signal::builder("screen-share-failed")
                        .param_types([String::static_type()])
                        .build(),
                ]
            });
            SIGNALS.as_ref()
        }
    }

    impl Call {
        /// Set whether our microphone is muted.
        fn set_muted(&self, muted: bool) {
            if self.muted.get() == muted {
                return;
            }
            self.muted.set(muted);
            let _ = self.send(Command::SetMuted(muted));
            self.obj().notify_muted();
        }

        /// Send a command to the call task.
        ///
        /// Returns `false` if the call task is gone.
        pub(super) fn send(&self, command: Command) -> bool {
            self.commands
                .borrow()
                .as_ref()
                .is_some_and(|commands| commands.send(command).is_ok())
        }

        /// Handle an update from the call task.
        pub(super) fn handle_update(&self, update: Update) {
            let obj = self.obj();
            match update {
                Update::Connected => self.set_state(CallState::Connected),
                Update::Participants(participants) => {
                    let count = u32::try_from(participants.len()).unwrap_or(u32::MAX);
                    if self.participants_count.replace(count) != count {
                        obj.notify_participants_count();
                    }

                    let room = obj.room();
                    let speaking = participants
                        .iter()
                        .filter(|participant| participant.speaking)
                        .map(|participant| {
                            room.get_or_create_members()
                                .get_or_create(participant.user_id.clone())
                                .display_name()
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    if *self.speaking.borrow() != speaking {
                        self.speaking.replace(speaking);
                        obj.notify_speaking();
                    }
                }
                Update::ScreenShare(active) => {
                    self.set_starting_screen_share(false);
                    if self.sharing_screen.replace(active) != active {
                        obj.notify_sharing_screen();
                    }
                }
                Update::ScreenShareFailed(error) => {
                    self.set_starting_screen_share(false);
                    obj.emit_by_name::<()>("screen-share-failed", &[&error]);
                }
                Update::Ended(error) => {
                    self.commands.take();
                    if error.is_some() {
                        self.error.replace(error);
                        obj.notify_error();
                    }
                    self.set_state(CallState::Ended);
                }
            }
        }

        /// Set whether we are waiting for the user to choose what to share.
        pub(super) fn set_starting_screen_share(&self, starting: bool) {
            if self.starting_screen_share.replace(starting) != starting {
                self.obj().notify_starting_screen_share();
            }
        }

        /// Set the state of this call.
        fn set_state(&self, state: CallState) {
            if self.state.replace(state) != state {
                debug!("Call state: {state:?}");
                self.obj().notify_state();
            }
        }
    }
}

glib::wrapper! {
    /// A voice call in a room.
    pub struct Call(ObjectSubclass<imp::Call>);
}

impl Call {
    /// Join the call in the given room.
    pub(crate) fn join(room: &Room) -> Self {
        let obj: Self = glib::Object::builder().property("room", room).build();

        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
        obj.imp().commands.replace(Some(commands_tx));

        let matrix_room = room.matrix_room().clone();
        let task = spawn_tokio!(engine::run(
            matrix_room.clone(),
            commands_rx,
            updates_tx.clone()
        ));
        // If the call task crashes, still remove our membership and end the call.
        spawn_tokio!(async move {
            if let Err(error) = task.await
                && error.is_panic()
            {
                error!("Call task crashed");
                engine::clean_up_membership(&matrix_room).await;
                let _ = updates_tx.send(Update::Ended(Some(gettext("internal error"))));
            }
        });

        spawn!(clone!(
            #[weak]
            obj,
            async move {
                while let Some(update) = updates_rx.recv().await {
                    obj.imp().handle_update(update);
                }
                if obj.is_active() {
                    obj.imp().handle_update(Update::Ended(Some(gettext(
                        "the call stopped unexpectedly",
                    ))));
                }
            }
        ));

        obj
    }

    /// Leave this call.
    pub(crate) fn leave(&self) {
        if !self.imp().send(Command::Leave) && self.is_active() {
            // The call task is gone, so nobody will tell us the call ended.
            self.imp().handle_update(Update::Ended(None));
        }
    }

    /// Ask the user what to share and share their screen, with the given
    /// sound.
    pub(crate) fn start_screen_share(&self, audio: ShareAudio) {
        if self.sharing_screen() || self.starting_screen_share() {
            return;
        }
        if self.imp().send(Command::StartScreenShare(audio)) {
            self.imp().set_starting_screen_share(true);
        }
    }

    /// Stop sharing the screen.
    pub(crate) fn stop_screen_share(&self) {
        self.imp().send(Command::StopScreenShare);
    }

    /// Connect to the signal emitted when sharing the screen fails.
    pub(crate) fn connect_screen_share_failed<F: Fn(&Self, String) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            "screen-share-failed",
            true,
            glib::closure_local!(move |obj: Self, error: String| f(&obj, error)),
        )
    }

    /// Whether this call is still running.
    pub(crate) fn is_active(&self) -> bool {
        self.state() != CallState::Ended
    }
}
