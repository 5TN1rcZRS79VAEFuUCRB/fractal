use adw::{prelude::*, subclass::prelude::*};
use gettextrs::gettext;
use gtk::{glib, glib::clone};

use crate::{
    gettext_f, ngettext_f,
    session::{Call, CallState, Room, Session},
    toast,
};

mod imp {
    use std::cell::{OnceCell, RefCell};

    use super::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::CallBar)]
    pub struct CallBar {
        /// The room to show the call of.
        #[property(get, set = Self::set_room, explicit_notify, nullable)]
        room: RefCell<Option<Room>>,
        revealer: OnceCell<gtk::Revealer>,
        status: OnceCell<gtk::Label>,
        mute_button: OnceCell<gtk::ToggleButton>,
        action_button: OnceCell<gtk::Button>,
        /// The session we watch, with its handler.
        session: RefCell<Option<(Session, glib::SignalHandlerId)>>,
        /// The room handler.
        room_handler: RefCell<Option<glib::SignalHandlerId>>,
        /// The call we watch, with its handlers.
        call: RefCell<Option<(Call, Vec<glib::SignalHandlerId>)>>,
        /// The binding of the mute button.
        mute_binding: RefCell<Option<glib::Binding>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CallBar {
        const NAME: &'static str = "CallBar";
        type Type = super::CallBar;
        type ParentType = adw::Bin;
    }

    #[glib::derived_properties]
    impl ObjectImpl for CallBar {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            let icon = gtk::Image::from_icon_name("call-start-symbolic");
            let status = gtk::Label::builder()
                .hexpand(true)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let mute_button = gtk::ToggleButton::builder()
                .icon_name("microphone-sensitivity-high-symbolic")
                .tooltip_text(gettext("Mute"))
                .valign(gtk::Align::Center)
                .build();
            mute_button.connect_active_notify(|button| {
                if button.is_active() {
                    button.set_icon_name("microphone-disabled-symbolic");
                    button.set_tooltip_text(Some(&gettext("Unmute")));
                } else {
                    button.set_icon_name("microphone-sensitivity-high-symbolic");
                    button.set_tooltip_text(Some(&gettext("Mute")));
                }
            });
            let action_button = gtk::Button::builder().valign(gtk::Align::Center).build();
            action_button.connect_clicked(clone!(
                #[weak(rename_to = imp)]
                self,
                move |_| imp.activate()
            ));

            let content = gtk::Box::builder()
                .spacing(12)
                .margin_start(12)
                .margin_end(12)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            content.append(&icon);
            content.append(&status);
            content.append(&mute_button);
            content.append(&action_button);

            let revealer = gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .child(&content)
                .build();
            obj.set_child(Some(&revealer));
            obj.add_css_class("toolbar");

            self.revealer.set(revealer).unwrap();
            self.status.set(status).unwrap();
            self.mute_button.set(mute_button).unwrap();
            self.action_button.set(action_button).unwrap();
            self.update();
        }

        fn dispose(&self) {
            self.disconnect_room();
            self.set_call(None);
        }
    }

    impl WidgetImpl for CallBar {}
    impl BinImpl for CallBar {}

    impl CallBar {
        /// Set the room to show the call of.
        fn set_room(&self, room: Option<Room>) {
            if *self.room.borrow() == room {
                return;
            }
            self.disconnect_room();

            if let Some(room) = &room {
                let handler = room.connect_call_participants_count_notify(clone!(
                    #[weak(rename_to = imp)]
                    self,
                    move |_| imp.update()
                ));
                self.room_handler.replace(Some(handler));

                if let Some(session) = room.session() {
                    let handler = session.connect_call_notify(clone!(
                        #[weak(rename_to = imp)]
                        self,
                        move |session| imp.watch_call(session.call())
                    ));
                    self.watch_call(session.call());
                    self.session.replace(Some((session, handler)));
                }
            }

            self.room.replace(room);
            self.update();
            self.obj().notify_room();
        }

        /// Stop watching the room and session.
        fn disconnect_room(&self) {
            if let (Some(room), Some(handler)) = (&*self.room.borrow(), self.room_handler.take()) {
                room.disconnect(handler);
            }
            if let Some((session, handler)) = self.session.take() {
                session.disconnect(handler);
            }
            self.set_call(None);
        }

        /// Watch the session's call, if it is in our room.
        fn watch_call(&self, call: Option<Call>) {
            let room = self.room.borrow().clone();
            let call = call.filter(|call| Some(call.room()) == room);
            self.set_call(call);
            self.update();
        }

        /// Set the call we watch.
        fn set_call(&self, call: Option<Call>) {
            if self.call.borrow().as_ref().map(|(call, _)| call) == call.as_ref() {
                return;
            }

            if let Some((old, handlers)) = self.call.take() {
                for handler in handlers {
                    old.disconnect(handler);
                }
                if let Some(error) = old.error() {
                    let obj = self.obj();
                    toast!(obj, gettext_f("Call ended: {error}", &[("error", &error)]));
                }
            }
            if let Some(binding) = self.mute_binding.take() {
                binding.unbind();
            }

            if let Some(call) = call {
                let handlers = ["state", "participants-count", "speaking"]
                    .into_iter()
                    .map(|property| {
                        call.connect_notify_local(
                            Some(property),
                            clone!(
                                #[weak(rename_to = imp)]
                                self,
                                move |_, _| imp.update()
                            ),
                        )
                    })
                    .collect();
                if let Some(mute_button) = self.mute_button.get() {
                    let binding = call
                        .bind_property("muted", mute_button, "active")
                        .bidirectional()
                        .sync_create()
                        .build();
                    self.mute_binding.replace(Some(binding));
                }
                self.call.replace(Some((call, handlers)));
            }
        }

        /// Join or leave the call.
        fn activate(&self) {
            let Some(room) = self.room.borrow().clone() else {
                return;
            };
            let Some(session) = room.session() else {
                return;
            };
            if self.call.borrow().is_some() {
                session.leave_call();
            } else {
                session.join_call(&room);
            }
        }

        /// Update the widgets for the current state.
        fn update(&self) {
            let (Some(revealer), Some(status), Some(mute_button), Some(action_button)) = (
                self.revealer.get(),
                self.status.get(),
                self.mute_button.get(),
                self.action_button.get(),
            ) else {
                return;
            };

            let call = self.call.borrow().as_ref().map(|(call, _)| call.clone());
            if let Some(call) = call {
                let text = match call.state() {
                    CallState::Connecting => gettext("Joining call…"),
                    CallState::Connected | CallState::Ended => {
                        let count = call.participants_count();
                        let people = ngettext_f(
                            "In call · {n} person",
                            "In call · {n} people",
                            count,
                            &[("n", &count.to_string())],
                        );
                        let speaking = call.speaking();
                        if speaking.is_empty() {
                            people
                        } else {
                            gettext_f(
                                "{people} · {speaking} speaking",
                                &[("people", &people), ("speaking", &speaking)],
                            )
                        }
                    }
                };
                status.set_label(&text);
                mute_button.set_visible(true);
                action_button.set_icon_name("call-stop-symbolic");
                action_button.set_tooltip_text(Some(&gettext("Leave Call")));
                action_button.remove_css_class("suggested-action");
                action_button.add_css_class("destructive-action");
                revealer.set_reveal_child(true);
                return;
            }

            let count = self
                .room
                .borrow()
                .as_ref()
                .map(Room::call_participants_count)
                .unwrap_or_default();
            if count == 0 {
                revealer.set_reveal_child(false);
                return;
            }

            status.set_label(&ngettext_f(
                "Call in progress · {n} person",
                "Call in progress · {n} people",
                count,
                &[("n", &count.to_string())],
            ));
            mute_button.set_visible(false);
            action_button.set_icon_name("call-start-symbolic");
            action_button.set_tooltip_text(Some(&gettext("Join Call")));
            action_button.remove_css_class("destructive-action");
            action_button.add_css_class("suggested-action");
            revealer.set_reveal_child(true);
        }
    }
}

glib::wrapper! {
    /// A bar showing the call in a room, to join it or control it.
    pub struct CallBar(ObjectSubclass<imp::CallBar>)
        @extends gtk::Widget, adw::Bin,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}
