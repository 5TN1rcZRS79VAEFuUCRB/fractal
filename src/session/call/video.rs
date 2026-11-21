//! Video in calls: frames shared between the call task and the UI.

use std::sync::{Arc, Mutex};

use gtk::{gdk, glib, prelude::*};
use livekit::webrtc::{
    video_frame::{BoxVideoFrame, I420Buffer, VideoBuffer, VideoFormatType, VideoRotation},
    video_stream::native::NativeVideoStream,
    video_track::RtcVideoTrack,
};
use ruma::OwnedUserId;
use tokio_stream::StreamExt;

/// What a video shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum VideoKind {
    /// A camera.
    Camera,
    /// A shared screen.
    Screen,
}

/// A video in a call.
#[derive(Debug, Clone)]
pub(crate) struct Video {
    /// The `LiveKit` track ID, unique in the call.
    pub id: String,
    /// The person sending the video.
    pub user_id: OwnedUserId,
    /// Whether this is our own video.
    pub local: bool,
    /// What the video shows.
    pub kind: VideoKind,
    /// The latest frame of the video.
    pub sink: VideoSink,
}

/// A decoded frame, in a format GDK can upload.
#[derive(Debug, Clone)]
struct Frame {
    width: u32,
    height: u32,
    rotation: VideoRotation,
    bytes: glib::Bytes,
}

#[derive(Debug, Default)]
struct SinkState {
    frame: Option<Frame>,
    /// Incremented for each new frame.
    serial: u64,
}

/// The latest frame of a video.
///
/// The call task writes frames as they arrive, and the UI reads the newest one
/// when it draws, so slow drawing drops frames instead of piling them up.
#[derive(Debug, Clone, Default)]
pub(crate) struct VideoSink(Arc<Mutex<SinkState>>);

impl VideoSink {
    /// Store a new frame from `LiveKit`.
    fn push(&self, frame: &BoxVideoFrame) {
        self.push_buffer(frame.buffer.as_ref(), frame.rotation);
    }

    /// Store a frame we capture ourselves.
    pub(super) fn push_i420(&self, buffer: &I420Buffer) {
        self.push_buffer(buffer, VideoRotation::VideoRotation0);
    }

    /// Convert a frame to a format GDK can upload, and store it.
    fn push_buffer(&self, buffer: &dyn VideoBuffer, rotation: VideoRotation) {
        let (width, height) = (buffer.width(), buffer.height());
        if width == 0 || height == 0 {
            return;
        }
        let stride = width * 4;
        let mut data = vec![0; (stride * height) as usize];
        // libyuv's ARGB is B, G, R, A in memory.
        buffer.to_argb(
            VideoFormatType::ARGB,
            &mut data,
            stride,
            width.cast_signed(),
            height.cast_signed(),
        );
        let frame = Frame {
            width,
            height,
            rotation,
            bytes: glib::Bytes::from_owned(data),
        };
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.frame = Some(frame);
        state.serial += 1;
    }

    /// The number of frames so far.
    #[cfg(test)]
    pub(super) fn frame_count(&self) -> u64 {
        self.0.lock().unwrap().serial
    }

    /// The size of the newest frame.
    #[cfg(test)]
    pub(super) fn frame_size(&self) -> Option<(u32, u32)> {
        let state = self.0.lock().unwrap();
        state
            .frame
            .as_ref()
            .map(|frame| (frame.width, frame.height))
    }

    /// Get the newest frame as a texture, if it is newer than `serial`.
    ///
    /// Returns the texture with its clockwise rotation in degrees, and
    /// updates `serial`.
    pub(crate) fn texture_since(&self, serial: &mut u64) -> Option<(gdk::Texture, u32)> {
        let frame = {
            let state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.serial == *serial {
                return None;
            }
            *serial = state.serial;
            state.frame.clone()?
        };
        let texture = gdk::MemoryTexture::new(
            frame.width.cast_signed(),
            frame.height.cast_signed(),
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &frame.bytes,
            (frame.width * 4) as usize,
        );
        let rotation = match frame.rotation {
            VideoRotation::VideoRotation0 => 0,
            VideoRotation::VideoRotation90 => 90,
            VideoRotation::VideoRotation180 => 180,
            VideoRotation::VideoRotation270 => 270,
        };
        Some((texture.upcast(), rotation))
    }
}

/// Copy the frames of a remote video track to the sink until the track ends.
pub(super) async fn receive(track: RtcVideoTrack, sink: VideoSink) {
    let mut stream = NativeVideoStream::new(track);
    while let Some(frame) = stream.next().await {
        sink.push(&frame);
    }
}
