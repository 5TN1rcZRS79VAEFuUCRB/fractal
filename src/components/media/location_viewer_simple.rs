//! A location viewer without a map, for systems without libshumate (Windows).

use adw::{prelude::*, subclass::prelude::*};
use geo_uri::GeoUri;
use gettextrs::gettext;
use gtk::glib;

use crate::i18n::gettext_f;

mod imp {
    use std::cell::{Cell, OnceCell};

    use super::*;

    #[derive(Debug, Default, glib::Properties)]
    #[properties(wrapper_type = super::LocationViewer)]
    pub struct LocationViewer {
        coordinates: OnceCell<gtk::Label>,
        link: OnceCell<gtk::LinkButton>,
        /// Whether to display this location in a compact format.
        #[property(get, set = Self::set_compact, explicit_notify)]
        compact: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LocationViewer {
        const NAME: &'static str = "LocationViewer";
        type Type = super::LocationViewer;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("location-viewer");
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for LocationViewer {
        fn constructed(&self) {
            self.parent_constructed();

            let icon = gtk::Image::builder()
                .icon_name("map-marker-symbolic")
                .pixel_size(48)
                .css_classes(["dim-label"])
                .build();
            let coordinates = gtk::Label::builder()
                .selectable(true)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .build();
            let link = gtk::LinkButton::with_label("", &gettext("Open in OpenStreetMap"));
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .valign(gtk::Align::Center)
                .margin_top(12)
                .margin_bottom(12)
                .build();
            content.append(&icon);
            content.append(&coordinates);
            content.append(&link);
            self.obj().set_child(Some(&content));

            self.coordinates.set(coordinates).unwrap();
            self.link.set(link).unwrap();
        }
    }

    impl WidgetImpl for LocationViewer {}
    impl BinImpl for LocationViewer {}

    impl LocationViewer {
        /// Set the compact format of this location.
        fn set_compact(&self, compact: bool) {
            if self.compact.replace(compact) != compact {
                self.obj().notify_compact();
            }
        }

        /// Show the given location.
        pub(super) fn set_location(&self, geo_uri: &GeoUri) {
            let latitude = geo_uri.latitude();
            let longitude = geo_uri.longitude();
            let description = gettext_f(
                "Location at latitude {latitude} and longitude {longitude}",
                &[
                    ("latitude", &latitude.to_string()),
                    ("longitude", &longitude.to_string()),
                ],
            );
            if let Some(coordinates) = self.coordinates.get() {
                coordinates.set_label(&format!("{latitude:.5}, {longitude:.5}"));
            }
            if let Some(link) = self.link.get() {
                link.set_uri(&format!(
                    "https://www.openstreetmap.org/?mlat={latitude}&mlon={longitude}#map=15/{latitude}/{longitude}"
                ));
            }
            self.obj()
                .update_property(&[gtk::accessible::Property::Description(&description)]);
        }
    }
}

glib::wrapper! {
    /// A widget displaying a location.
    pub struct LocationViewer(ObjectSubclass<imp::LocationViewer>)
        @extends gtk::Widget, adw::Bin,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl LocationViewer {
    /// Create a new location viewer.
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Show the given location.
    pub(crate) fn set_location(&self, geo_uri: &GeoUri) {
        self.imp().set_location(geo_uri);
    }
}

impl Default for LocationViewer {
    fn default() -> Self {
        Self::new()
    }
}
