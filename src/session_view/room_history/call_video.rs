//! Widgets showing the videos of a call.

use adw::{prelude::*, subclass::prelude::*};
use gettextrs::gettext;
use gtk::{gdk, glib, glib::clone, graphene, gsk};

use crate::{
    gettext_f,
    prelude::*,
    session::{Room, Video, VideoKind, VideoSink},
};

mod view_imp {
    use std::cell::{Cell, RefCell};

    use super::*;

    #[derive(Debug, Default)]
    pub struct VideoView {
        /// The video to show.
        pub(super) sink: RefCell<Option<VideoSink>>,
        /// The serial of the frame we show.
        serial: Cell<u64>,
        /// The frame we show, with its clockwise rotation in degrees.
        frame: RefCell<Option<(gdk::Texture, u32)>>,
        /// Whether to show the video flipped, like a mirror.
        pub(super) mirror: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for VideoView {
        const NAME: &'static str = "CallVideoView";
        type Type = super::VideoView;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("call-video");
        }
    }

    impl ObjectImpl for VideoView {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_overflow(gtk::Overflow::Hidden);
            // Check for a new frame each time the screen refreshes. The tick
            // callback only runs while the widget is shown.
            obj.add_tick_callback(|obj, _| {
                if obj.imp().poll() {
                    obj.queue_draw();
                }
                glib::ControlFlow::Continue
            });
        }
    }

    impl WidgetImpl for VideoView {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // A 16:9 natural size; the parent decides the real size.
            match orientation {
                gtk::Orientation::Horizontal => (0, 320, -1, -1),
                _ => (0, 180, -1, -1),
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            #[allow(clippy::cast_precision_loss)]
            let (width, height) = (obj.width() as f32, obj.height() as f32);
            snapshot.append_color(
                &gdk::RGBA::BLACK,
                &graphene::Rect::new(0.0, 0.0, width, height),
            );

            let frame = self.frame.borrow();
            let Some((texture, rotation)) = &*frame else {
                return;
            };
            #[allow(clippy::cast_precision_loss)]
            let (mut video_width, mut video_height) =
                (texture.width() as f32, texture.height() as f32);
            let sideways = matches!(rotation, 90 | 270);
            if sideways {
                std::mem::swap(&mut video_width, &mut video_height);
            }
            if video_width <= 0.0 || video_height <= 0.0 {
                return;
            }
            // Fit the video in the widget, keeping its aspect ratio.
            let scale = (width / video_width).min(height / video_height);
            let (drawn_width, drawn_height) = if sideways {
                (video_height * scale, video_width * scale)
            } else {
                (video_width * scale, video_height * scale)
            };

            snapshot.save();
            snapshot.translate(&graphene::Point::new(width / 2.0, height / 2.0));
            if self.mirror.get() {
                snapshot.scale(-1.0, 1.0);
            }
            #[allow(clippy::cast_precision_loss)]
            snapshot.rotate(*rotation as f32);
            snapshot.append_scaled_texture(
                texture,
                gsk::ScalingFilter::Linear,
                &graphene::Rect::new(
                    -drawn_width / 2.0,
                    -drawn_height / 2.0,
                    drawn_width,
                    drawn_height,
                ),
            );
            snapshot.restore();
        }
    }

    impl VideoView {
        /// Take the newest frame, if there is one.
        ///
        /// Returns whether the frame changed.
        fn poll(&self) -> bool {
            let Some(sink) = self.sink.borrow().clone() else {
                return false;
            };
            let mut serial = self.serial.get();
            let Some(frame) = sink.texture_since(&mut serial) else {
                return false;
            };
            self.serial.set(serial);
            self.frame.replace(Some(frame));
            true
        }
    }
}

glib::wrapper! {
    /// A widget drawing a call video, fitted to its size.
    pub struct VideoView(ObjectSubclass<view_imp::VideoView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl VideoView {
    /// Create a view of the given video.
    pub(crate) fn new(sink: &VideoSink, mirror: bool) -> Self {
        let obj: Self = glib::Object::new();
        obj.imp().sink.replace(Some(sink.clone()));
        obj.imp().mirror.set(mirror);
        obj
    }
}

/// The name to show for a video.
pub(super) fn video_title(room: &Room, video: &Video) -> String {
    if video.local {
        return match video.kind {
            VideoKind::Camera => gettext("You"),
            VideoKind::Screen => gettext("Your screen"),
        };
    }
    let name = room
        .get_or_create_members()
        .get_or_create(video.user_id.clone())
        .display_name();
    match video.kind {
        VideoKind::Camera => name,
        VideoKind::Screen => gettext_f("{name}’s screen", &[("name", &name)]),
    }
}

/// A small tile with a video and its name, for the call bar.
pub(super) fn tile(title: &str, video: &Video, on_open: impl Fn() + 'static) -> gtk::Widget {
    let view = VideoView::new(&video.sink, video.local && video.kind == VideoKind::Camera);
    let label = gtk::Label::builder()
        .label(title)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::End)
        .margin_start(6)
        .margin_bottom(6)
        .css_classes(["osd", "caption"])
        .build();
    let overlay = gtk::Overlay::builder()
        .child(&view)
        .width_request(256)
        .height_request(144)
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["card"])
        .tooltip_text(gettext("Open in a Window"))
        .cursor(&gdk::Cursor::from_name("pointer", None).expect("pointer cursor exists"))
        .build();
    overlay.add_overlay(&label);

    let click = gtk::GestureClick::new();
    click.connect_released(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        on_open();
    });
    overlay.add_controller(click);
    overlay.upcast()
}

/// Open a window showing a video big, which can be made full screen.
pub(super) fn open_window(parent: &gtk::Widget, title: &str, video: &Video) -> gtk::Window {
    let view = VideoView::new(&video.sink, video.local && video.kind == VideoKind::Camera);
    view.set_hexpand(true);
    view.set_vexpand(true);

    let fullscreen_button = gtk::Button::builder()
        .icon_name("view-fullscreen-symbolic")
        .tooltip_text(gettext("Full Screen"))
        .build();
    let header = adw::HeaderBar::new();
    header.pack_end(&fullscreen_button);
    let toolbar = adw::ToolbarView::builder()
        .content(&view)
        .top_bar_style(adw::ToolbarStyle::Raised)
        .build();
    toolbar.add_top_bar(&header);

    let application = parent
        .root()
        .and_downcast::<gtk::Window>()
        .and_then(|window| window.application());
    let window = adw::Window::builder()
        .title(title)
        .default_width(1024)
        .default_height(616)
        .content(&toolbar)
        .build();
    window.set_application(application.as_ref());

    // Hide the header bar in full screen, so only the video shows.
    window.connect_fullscreened_notify(clone!(
        #[weak]
        toolbar,
        move |window| toolbar.set_reveal_top_bars(!window.is_fullscreen())
    ));
    fullscreen_button.connect_clicked(clone!(
        #[weak]
        window,
        move |_| window.fullscreen()
    ));
    let double_click = gtk::GestureClick::new();
    double_click.connect_pressed(clone!(
        #[weak]
        window,
        move |_, presses, _, _| {
            if presses == 2 {
                if window.is_fullscreen() {
                    window.unfullscreen();
                } else {
                    window.fullscreen();
                }
            }
        }
    ));
    view.add_controller(double_click);
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(clone!(
        #[weak]
        window,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| match key {
            gdk::Key::Escape if window.is_fullscreen() => {
                window.unfullscreen();
                glib::Propagation::Stop
            }
            gdk::Key::F11 | gdk::Key::f => {
                if window.is_fullscreen() {
                    window.unfullscreen();
                } else {
                    window.fullscreen();
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    ));
    window.add_controller(keys);

    window.present();
    window.upcast()
}
