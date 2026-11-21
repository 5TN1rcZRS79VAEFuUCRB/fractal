//! The parts of the glycin-gtk4 API Fractal+ uses, for systems without
//! glycin.

/// Get the texture of a frame.
pub fn frame_get_texture(frame: &glycin_shim::Frame) -> gdk::Texture {
    frame.texture()
}
