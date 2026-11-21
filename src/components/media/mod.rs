mod animated_image_paintable;
mod audio_player;
mod content_viewer;
#[cfg(not(windows))]
mod location_viewer;
// Windows has no libshumate, so locations are shown without a map.
#[cfg(windows)]
#[path = "location_viewer_simple.rs"]
mod location_viewer;
mod video_player;
mod video_player_renderer;

pub(crate) use self::{
    animated_image_paintable::AnimatedImagePaintable,
    audio_player::*,
    content_viewer::{ContentType, MediaContentViewer},
    location_viewer::LocationViewer,
    video_player::VideoPlayer,
};
