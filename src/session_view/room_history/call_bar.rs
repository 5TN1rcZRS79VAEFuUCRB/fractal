use adw::{prelude::*, subclass::prelude::*};
use gettextrs::gettext;
use gtk::{gio, glib, glib::clone};
use tracing::warn;

use super::call_video;
use crate::{
    gettext_f, ngettext_f,
    session::{AudioApp, Call, CallState, Room, Session, ShareAudio, playing_apps},
    spawn, toast,
};

mod imp {
    use std::{
        cell::{OnceCell, RefCell},
        collections::HashMap,
    };

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
        camera_button: OnceCell<gtk::Button>,
        share_button: OnceCell<gtk::Button>,
        action_button: OnceCell<gtk::Button>,
        videos_revealer: OnceCell<gtk::Revealer>,
        videos_box: OnceCell<gtk::Box>,
        /// The video tiles we show, by video ID.
        tiles: RefCell<HashMap<String, gtk::Widget>>,
        /// The windows showing a video big, by video ID.
        windows: RefCell<HashMap<String, gtk::Window>>,
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
            let camera_button = gtk::Button::builder()
                .icon_name("camera-web-symbolic")
                .valign(gtk::Align::Center)
                .build();
            camera_button.connect_clicked(clone!(
                #[weak(rename_to = imp)]
                self,
                move |_| imp.toggle_camera()
            ));
            let share_button = gtk::Button::builder()
                .icon_name("screen-shared-symbolic")
                .valign(gtk::Align::Center)
                .build();
            share_button.connect_clicked(clone!(
                #[weak(rename_to = imp)]
                self,
                move |_| imp.toggle_screen_share()
            ));
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
            content.append(&camera_button);
            content.append(&share_button);
            content.append(&action_button);

            let videos_box = gtk::Box::builder()
                .spacing(6)
                .margin_start(12)
                .margin_end(12)
                .margin_bottom(6)
                .build();
            let videos_scroll = gtk::ScrolledWindow::builder()
                .vscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .child(&videos_box)
                .build();
            let videos_revealer = gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .child(&videos_scroll)
                .build();

            let bar = gtk::Box::new(gtk::Orientation::Vertical, 0);
            bar.append(&content);
            bar.append(&videos_revealer);

            let revealer = gtk::Revealer::builder()
                .transition_type(gtk::RevealerTransitionType::SlideDown)
                .child(&bar)
                .build();
            obj.set_child(Some(&revealer));
            obj.add_css_class("toolbar");

            self.revealer.set(revealer).unwrap();
            self.status.set(status).unwrap();
            self.mute_button.set(mute_button).unwrap();
            self.camera_button.set(camera_button).unwrap();
            self.share_button.set(share_button).unwrap();
            self.action_button.set(action_button).unwrap();
            self.videos_revealer.set(videos_revealer).unwrap();
            self.videos_box.set(videos_box).unwrap();
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
                let mut handlers = [
                    "state",
                    "participants-count",
                    "speaking",
                    "sharing-screen",
                    "starting-screen-share",
                    "camera-on",
                    "starting-camera",
                ]
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
                .collect::<Vec<_>>();
                handlers.push(call.connect_videos_changed(clone!(
                    #[weak(rename_to = imp)]
                    self,
                    move |_| imp.update_videos()
                )));
                handlers.push(call.connect_camera_failed(clone!(
                    #[weak(rename_to = imp)]
                    self,
                    move |_, error| {
                        let obj = imp.obj();
                        toast!(obj, gettext_f("Camera: {error}", &[("error", &error)]));
                    }
                )));
                handlers.push(call.connect_screen_share_failed(clone!(
                    #[weak(rename_to = imp)]
                    self,
                    move |_, error| {
                        let obj = imp.obj();
                        toast!(
                            obj,
                            gettext_f("Screen sharing: {error}", &[("error", &error)])
                        );
                    }
                )));
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
            self.update_videos();
        }

        /// Show the current videos of the call.
        fn update_videos(&self) {
            let (Some(videos_box), Some(videos_revealer)) =
                (self.videos_box.get(), self.videos_revealer.get())
            else {
                return;
            };
            let call = self.call.borrow().as_ref().map(|(call, _)| call.clone());
            let room = self.room.borrow().clone();
            let videos = match (&call, &room) {
                (Some(call), Some(_)) => call.videos(),
                _ => Vec::new(),
            };

            // Remove the tiles and windows of videos that are gone.
            let ids = videos
                .iter()
                .map(|video| video.id.clone())
                .collect::<std::collections::HashSet<_>>();
            self.tiles.borrow_mut().retain(|id, tile| {
                let keep = ids.contains(id);
                if !keep {
                    videos_box.remove(tile);
                }
                keep
            });
            // Closing a window removes it from the map, so don't hold the map.
            let closed = self
                .windows
                .borrow()
                .iter()
                .filter(|(id, _)| !ids.contains(*id))
                .map(|(_, window)| window.clone())
                .collect::<Vec<_>>();
            for window in closed {
                window.close();
            }

            // Add tiles for new videos, keeping the order of the call.
            let mut previous: Option<gtk::Widget> = None;
            for video in &videos {
                let Some(room) = &room else {
                    break;
                };
                let existing = self.tiles.borrow().get(&video.id).cloned();
                let tile = existing.unwrap_or_else(|| {
                    let title = call_video::video_title(room, video);
                    let new_tile = call_video::tile(&title, video, {
                        let video = video.clone();
                        let title = title.clone();
                        clone!(
                            #[weak(rename_to = imp)]
                            self,
                            move || imp.open_video(&title, &video)
                        )
                    });
                    videos_box.append(&new_tile);
                    self.tiles
                        .borrow_mut()
                        .insert(video.id.clone(), new_tile.clone());
                    new_tile
                });
                videos_box.reorder_child_after(&tile, previous.as_ref());
                previous = Some(tile);
            }
            videos_revealer.set_reveal_child(!videos.is_empty());
        }

        /// Show a video big in its own window, or raise its window.
        fn open_video(&self, title: &str, video: &crate::session::Video) {
            if let Some(window) = self.windows.borrow().get(&video.id) {
                window.present();
                return;
            }
            let obj = self.obj();
            let window = call_video::open_window(obj.upcast_ref(), title, video);
            let id = video.id.clone();
            window.connect_close_request(clone!(
                #[weak(rename_to = imp)]
                self,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_| {
                    imp.windows.borrow_mut().remove(&id);
                    glib::Propagation::Proceed
                }
            ));
            self.windows.borrow_mut().insert(video.id.clone(), window);
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

        /// Turn the camera on or off.
        fn toggle_camera(&self) {
            if let Some(call) = self.call.borrow().as_ref().map(|(call, _)| call.clone()) {
                call.set_camera(!call.camera_on());
            }
        }

        /// Start or stop sharing the screen.
        fn toggle_screen_share(&self) {
            let Some(call) = self.call.borrow().as_ref().map(|(call, _)| call.clone()) else {
                return;
            };
            if call.sharing_screen() {
                call.stop_screen_share();
                return;
            }
            if call.starting_screen_share() {
                return;
            }

            spawn!(clone!(
                #[weak(rename_to = imp)]
                self,
                async move {
                    if let Some(audio) = imp.choose_audio().await
                        && call.is_active()
                    {
                        call.start_screen_share(audio);
                    }
                }
            ));
        }

        /// Ask which sound to share with the screen.
        async fn choose_audio(&self) -> Option<ShareAudio> {
            let apps = match gio::spawn_blocking(playing_apps).await {
                Ok(Ok(apps)) => apps,
                Ok(Err(error)) => {
                    warn!("Could not list apps playing sound: {error}");
                    Vec::new()
                }
                Err(_) => Vec::new(),
            };

            let mut options = vec![
                (gettext("No Sound"), None, ShareAudio::None),
                (
                    gettext("All Sound"),
                    Some(gettext("From every app except this one")),
                    ShareAudio::System,
                ),
            ];
            options.extend(apps.into_iter().map(|AudioApp { name }| {
                (
                    name.clone(),
                    Some(gettext("Only this app")),
                    ShareAudio::App(name),
                )
            }));

            let list = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .css_classes(["boxed-list"])
                .build();
            let mut first: Option<gtk::CheckButton> = None;
            let mut choices = Vec::new();
            for (title, subtitle, audio) in options {
                let check = gtk::CheckButton::new();
                match &first {
                    Some(first) => check.set_group(Some(first)),
                    None => first = Some(check.clone()),
                }
                check.set_active(audio == ShareAudio::System);
                let row = adw::ActionRow::builder()
                    .title(title)
                    .activatable_widget(&check)
                    .build();
                if let Some(subtitle) = subtitle {
                    row.set_subtitle(&subtitle);
                }
                row.add_prefix(&check);
                list.append(&row);
                choices.push((check, audio));
            }

            let dialog = adw::AlertDialog::builder()
                .heading(gettext("Share Screen"))
                .body(gettext(
                    "Choose which sound to share. The sound of this call is never shared.",
                ))
                .extra_child(&list)
                .default_response("share")
                .close_response("cancel")
                .build();
            dialog.add_responses(&[
                ("cancel", &gettext("Cancel")),
                ("share", &gettext("Choose Screen…")),
            ]);
            dialog.set_response_appearance("share", adw::ResponseAppearance::Suggested);

            if dialog.choose_future(Some(&*self.obj())).await != "share" {
                return None;
            }
            choices
                .into_iter()
                .find(|(check, _)| check.is_active())
                .map(|(_, audio)| audio)
        }

        /// Update the widgets for the current state.
        fn update(&self) {
            let (
                Some(revealer),
                Some(status),
                Some(mute_button),
                Some(camera_button),
                Some(share_button),
                Some(action_button),
            ) = (
                self.revealer.get(),
                self.status.get(),
                self.mute_button.get(),
                self.camera_button.get(),
                self.share_button.get(),
                self.action_button.get(),
            )
            else {
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
                        let people = if speaking.is_empty() {
                            people
                        } else {
                            gettext_f(
                                "{people} · {speaking} speaking",
                                &[("people", &people), ("speaking", &speaking)],
                            )
                        };
                        if call.sharing_screen() {
                            gettext_f("{people} · Sharing screen", &[("people", &people)])
                        } else {
                            people
                        }
                    }
                };
                status.set_label(&text);
                mute_button.set_visible(true);
                camera_button.set_visible(call.state() == CallState::Connected);
                camera_button.set_sensitive(!call.starting_camera());
                if call.camera_on() {
                    camera_button.set_tooltip_text(Some(&gettext("Turn Off Camera")));
                    camera_button.add_css_class("accent");
                } else {
                    camera_button.set_tooltip_text(Some(&gettext("Turn On Camera")));
                    camera_button.remove_css_class("accent");
                }
                share_button.set_visible(call.state() == CallState::Connected);
                share_button.set_sensitive(!call.starting_screen_share());
                if call.sharing_screen() {
                    share_button.set_tooltip_text(Some(&gettext("Stop Sharing Screen")));
                    share_button.add_css_class("accent");
                } else {
                    share_button.set_tooltip_text(Some(&gettext("Share Screen")));
                    share_button.remove_css_class("accent");
                }
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
            camera_button.set_visible(false);
            share_button.set_visible(false);
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
