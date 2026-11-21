//! The parts of the glycin API Fractal+ uses, for systems without glycin
//! (Windows and macOS).
//!
//! Images are decoded with GTK's own loaders, and WebP with libwebp. Only the
//! first frame of animated images is shown.

use std::cell::Cell;

use gdk::prelude::*;

/// Errors while loading an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, glib::ErrorDomain)]
#[error_domain(name = "glycin-shim-loader-error")]
pub enum LoaderError {
    /// Any other failure.
    Failed,
    /// The image format is not supported.
    UnknownImageFormat,
}

/// How to sandbox loaders. There is no sandbox here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxSelector {
    /// Pick automatically.
    Auto,
    /// Don't use a sandbox.
    NotSandboxed,
}

/// Where to load an image from.
#[derive(Debug, Clone)]
enum Source {
    File(gio::File),
    Bytes(glib::Bytes),
}

/// Loads images.
#[derive(Debug, Clone)]
pub struct Loader {
    source: Source,
}

impl Loader {
    /// A loader for the image in the given file.
    pub fn new(file: &impl IsA<gio::File>) -> Self {
        Self {
            source: Source::File(file.clone().upcast()),
        }
    }

    /// A loader for the image in the given bytes.
    pub fn for_bytes(bytes: &glib::Bytes) -> Self {
        Self {
            source: Source::Bytes(bytes.clone()),
        }
    }

    /// Has no effect: there is no sandbox.
    pub fn set_sandbox_selector(&self, _sandbox_selector: SandboxSelector) {}

    /// Load the image.
    pub async fn load_future(&self) -> Result<Image, glib::Error> {
        let bytes = match &self.source {
            Source::File(file) => file.load_bytes_future().await?.0,
            Source::Bytes(bytes) => bytes.clone(),
        };
        let texture = gio::spawn_blocking(move || decode(&bytes))
            .await
            .map_err(|_| glib::Error::new(LoaderError::Failed, "the image decoder crashed"))??;
        Ok(Image {
            frame: Frame { texture },
        })
    }
}

/// Decode an image to a texture.
fn decode(bytes: &glib::Bytes) -> Result<gdk::Texture, glib::Error> {
    let data: &[u8] = bytes;
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        let image = webp::Decoder::new(data)
            .decode()
            .ok_or_else(|| glib::Error::new(LoaderError::Failed, "invalid WebP image"))?;
        let (format, channels) = if image.is_alpha() {
            (gdk::MemoryFormat::R8g8b8a8, 4)
        } else {
            (gdk::MemoryFormat::R8g8b8, 3)
        };
        let width = image.width();
        return Ok(gdk::MemoryTexture::new(
            width.cast_signed(),
            image.height().cast_signed(),
            format,
            &glib::Bytes::from(&*image),
            (width * channels) as usize,
        )
        .upcast());
    }
    gdk::Texture::from_bytes(bytes).map_err(|error| {
        // GTK falls back to gdk-pixbuf, which has its own error for this.
        const PIXBUF_UNKNOWN_TYPE: i32 = 3;
        let pixbuf_unknown = error.domain() == glib::Quark::from_str("gdk-pixbuf-error-quark")
            && error.code() == PIXBUF_UNKNOWN_TYPE;
        if pixbuf_unknown || error.matches(gdk::TextureError::UnsupportedFormat) {
            glib::Error::new(LoaderError::UnknownImageFormat, error.message())
        } else {
            error
        }
    })
}

/// A loaded image.
#[derive(Debug, Clone)]
pub struct Image {
    frame: Frame,
}

impl Image {
    /// The width of the image.
    pub fn width(&self) -> u32 {
        self.frame.width()
    }

    /// The height of the image.
    pub fn height(&self) -> u32 {
        self.frame.height()
    }

    /// The first frame. Images are never animated here.
    pub async fn next_frame_future(&self) -> Result<Frame, glib::Error> {
        Ok(self.frame.clone())
    }

    /// The first frame. It is scaled when drawn instead of when loaded.
    pub async fn specific_frame_future(
        &self,
        _frame_request: &FrameRequest,
    ) -> Result<Frame, glib::Error> {
        Ok(self.frame.clone())
    }
}

/// A frame of an image.
#[derive(Debug, Clone)]
pub struct Frame {
    texture: gdk::Texture,
}

impl Frame {
    /// The width of the frame.
    pub fn width(&self) -> u32 {
        self.texture.width().cast_unsigned()
    }

    /// The height of the frame.
    pub fn height(&self) -> u32 {
        self.texture.height().cast_unsigned()
    }

    /// How long to show the frame, in microseconds. Always 0: not animated.
    pub fn delay(&self) -> i64 {
        0
    }

    /// The texture of the frame.
    pub fn texture(&self) -> gdk::Texture {
        self.texture.clone()
    }
}

/// The size to load a frame at.
#[derive(Debug, Default)]
pub struct FrameRequest {
    scale: Cell<Option<(u32, u32)>>,
}

impl FrameRequest {
    /// A request for a frame at its natural size.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for the frame at the given size.
    pub fn set_scale(&self, width: u32, height: u32) {
        self.scale.set(Some((width, height)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels() -> Vec<u8> {
        (0..4 * 3).flat_map(|i| [i as u8 * 20, 100, 200, 255]).collect()
    }

    #[test]
    fn decodes_png() {
        let texture = gdk::MemoryTexture::new(
            4,
            3,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(pixels()),
            16,
        );
        let png = texture.save_to_png_bytes();
        let decoded = decode(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 3));
    }

    #[test]
    fn decodes_webp() {
        let data = pixels();
        let webp = webp::Encoder::from_rgba(&data, 4, 3).encode_lossless();
        let decoded = decode(&glib::Bytes::from(&*webp)).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 3));
    }

    #[test]
    fn rejects_garbage() {
        let error = decode(&glib::Bytes::from_static(b"not an image")).unwrap_err();
        assert!(error.matches(LoaderError::UnknownImageFormat), "{error}");
    }
}
