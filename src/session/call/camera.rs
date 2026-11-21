//! Capture the camera for video calls: through the Camera portal on Linux, and
//! with `GStreamer`'s own camera sources elsewhere.

use std::any::Any;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

#[cfg(target_os = "linux")]
use ashpd::desktop::{
    ResponseError,
    camera::{Camera, CameraAccessOptions, OpenPipeWireRemoteOptions},
};
use futures_util::StreamExt;
use gst::prelude::*;
use livekit::webrtc::video_source::{VideoResolution, native::NativeVideoSource};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use super::{screen, video::VideoSink};

/// The size we send. Element Call sends 720p too.
const SIZE: (u32, u32) = (1280, 720);

/// An error while starting the camera.
#[derive(Debug, thiserror::Error)]
pub(super) enum CameraError {
    #[error("no camera found")]
    NoCamera,
    #[error("access to the camera was denied")]
    Denied,
    #[error("could not use the camera portal: {0}")]
    Portal(String),
    #[error("could not capture the camera: {0}")]
    Pipeline(String),
}

#[cfg(target_os = "linux")]
impl From<ashpd::Error> for CameraError {
    fn from(error: ashpd::Error) -> Self {
        match error {
            ashpd::Error::Response(ResponseError::Cancelled | ResponseError::Other) => Self::Denied,
            error => Self::Portal(error.to_string()),
        }
    }
}

/// Lets cameras send big sizes as JPEG, which they often only do. The caps
/// filter keeps the scaling after it from asking the camera for a size it
/// can't do raw.
const DECODE: &str = "capsfilter caps=\"image/jpeg;video/x-raw\" ! decodebin";

/// A running capture of the camera.
pub(super) struct CameraCapture {
    pipeline: gst::Pipeline,
    /// What must stay alive while capturing, like the portal's file
    /// descriptor.
    #[allow(dead_code, reason = "only kept alive")]
    keep: Option<Box<dyn Any + Send + Sync>>,
    /// The source receiving the frames.
    pub(super) source: NativeVideoSource,
    /// The frames, for showing ourselves.
    pub(super) preview: VideoSink,
    /// Resolves when the capture stops by itself, e.g. when the camera is
    /// unplugged.
    pub(super) ended: Option<oneshot::Receiver<()>>,
}

impl CameraCapture {
    /// Ask for access to the camera and start capturing it.
    pub(super) async fn start() -> Result<Self, CameraError> {
        if super::fake_media() {
            return Self::start_pipeline("videotestsrc is-live=true pattern=smpte", None);
        }
        #[cfg(target_os = "linux")]
        return Self::start_portal().await;
        #[cfg(windows)]
        return Self::start_pipeline(&format!("mfvideosrc ! {DECODE}"), None);
        #[cfg(target_os = "macos")]
        return Self::start_pipeline(&format!("avfvideosrc ! {DECODE}"), None);
    }

    /// Ask the Camera portal for access and start capturing the default
    /// camera.
    #[cfg(target_os = "linux")]
    async fn start_portal() -> Result<Self, CameraError> {
        let proxy = Camera::new().await?;
        if !proxy.is_present().await? {
            return Err(CameraError::NoCamera);
        }
        proxy
            .request_access(CameraAccessOptions::default())
            .await?
            .response()?;
        let fd = proxy
            .open_pipe_wire_remote(OpenPipeWireRemoteOptions::default())
            .await?;

        // Without a path, pipewiresrc picks the default camera of the remote,
        // which only has cameras.
        let source = format!(
            "pipewiresrc fd={} always-copy=true ! {DECODE}",
            fd.as_raw_fd()
        );
        Self::start_pipeline(&source, Some(Box::new(fd)))
    }

    /// Start converting the frames of the given `GStreamer` source for
    /// `LiveKit`.
    fn start_pipeline(
        source: &str,
        keep: Option<Box<dyn Any + Send + Sync>>,
    ) -> Result<Self, CameraError> {
        let (width, height) = SIZE;
        let description = format!(
            "{source} ! videoconvert ! videoscale ! videorate \
             ! video/x-raw,format=I420,width={width},height={height},pixel-aspect-ratio=1/1,framerate=30/1 \
             ! appsink name=sink sync=false max-buffers=1 drop=true"
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| CameraError::Pipeline(error.to_string()))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| CameraError::Pipeline("not a pipeline".to_owned()))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|sink| sink.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| CameraError::Pipeline("no sink".to_owned()))?;

        let source = NativeVideoSource::new(VideoResolution { width, height }, false);
        let preview = VideoSink::default();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let source = source.clone();
                    let preview = preview.clone();
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        screen::send_frame(&source, &sample, Some(&preview));
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        );

        let (ended_tx, ended_rx) = oneshot::channel();
        let bus = pipeline
            .bus()
            .ok_or_else(|| CameraError::Pipeline("no bus".to_owned()))?;
        let mut messages = bus.stream_filtered(&[gst::MessageType::Eos, gst::MessageType::Error]);
        tokio::spawn(async move {
            if let Some(message) = messages.next().await {
                if let gst::MessageView::Error(error) = message.view() {
                    warn!("Camera capture failed: {}", error.error());
                } else {
                    debug!("Camera capture ended");
                }
                let _ = ended_tx.send(());
            }
        });

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| CameraError::Pipeline(error.to_string()))?;
        debug!("Capturing the camera");

        Ok(Self {
            pipeline,
            keep,
            source,
            preview,
            ended: Some(ended_rx),
        })
    }

    /// Stop capturing.
    pub(super) fn stop(self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    #[ignore = "needs a camera and the camera portal"]
    fn captures_camera() {
        gst::init().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (frames, size) = runtime.block_on(async {
            let capture = CameraCapture::start().await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
            let result = (capture.preview.frame_count(), capture.preview.frame_size());
            capture.stop();
            result
        });
        println!("{frames} frames of {size:?} in 3 s");
        assert!(frames > 20, "only {frames} frames");
        assert_eq!(size, Some(SIZE));
    }
}
