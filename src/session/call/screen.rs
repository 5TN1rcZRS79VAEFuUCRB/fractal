//! Capture the screen through the `ScreenCast` portal, for screen sharing.

use std::os::fd::{AsRawFd, OwnedFd};

use ashpd::desktop::{
    CreateSessionOptions, PersistMode, ResponseError, Session,
    screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
        StartCastOptions,
    },
};
use futures_util::StreamExt;
use gst::prelude::*;
use gst_video::prelude::*;
use livekit::webrtc::{
    video_frame::{I420Buffer, VideoFrame, VideoRotation},
    video_source::{VideoResolution, native::NativeVideoSource},
};
use tokio::sync::oneshot;
use tracing::{debug, warn};

/// The largest size we send, to keep encoding cheap.
const MAX_SIZE: (f64, f64) = (1920.0, 1080.0);

/// An error while starting to capture the screen.
#[derive(Debug, thiserror::Error)]
pub(super) enum ScreenError {
    #[error("cancelled")]
    Cancelled,
    #[error("could not use the screen sharing portal: {0}")]
    Portal(ashpd::Error),
    #[error("could not capture the screen: {0}")]
    Pipeline(String),
}

impl From<ashpd::Error> for ScreenError {
    fn from(error: ashpd::Error) -> Self {
        match error {
            ashpd::Error::Response(ResponseError::Cancelled) => Self::Cancelled,
            error => Self::Portal(error),
        }
    }
}

/// A running capture of the screen.
pub(super) struct ScreenCapture {
    pipeline: gst::Pipeline,
    session: Session<Screencast>,
    _fd: OwnedFd,
    /// The source receiving the frames.
    pub(super) source: NativeVideoSource,
    /// The size of the frames we send.
    pub(super) resolution: VideoResolution,
    /// Resolves when the capture stops by itself, e.g. when the user stops
    /// sharing from the system.
    pub(super) ended: Option<oneshot::Receiver<()>>,
}

impl ScreenCapture {
    /// Ask the user what to share and start capturing it.
    pub(super) async fn start() -> Result<Self, ScreenError> {
        let proxy = Screencast::new().await?;
        let session = proxy
            .create_session(CreateSessionOptions::default())
            .await?;

        match Self::start_with_session(&proxy, &session).await {
            Ok((fd, node_id, size)) => match Self::start_pipeline(fd, node_id, size) {
                Ok(parts) => {
                    debug!("Capturing the screen from node {node_id}");
                    Ok(Self::from_parts(parts, session))
                }
                Err(error) => {
                    let _ = session.close().await;
                    Err(error)
                }
            },
            Err(error) => {
                let _ = session.close().await;
                Err(error)
            }
        }
    }

    /// Let the user choose what to share, and get the `PipeWire` remote and
    /// node to capture.
    async fn start_with_session(
        proxy: &Screencast,
        session: &Session<Screencast>,
    ) -> Result<(OwnedFd, u32, Option<(i32, i32)>), ScreenError> {
        let cursor_mode = if proxy
            .available_cursor_modes()
            .await
            .is_ok_and(|modes| modes.contains(CursorMode::EMBEDDED))
        {
            CursorMode::EMBEDDED
        } else {
            CursorMode::HIDDEN
        };
        proxy
            .select_sources(
                session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(cursor_mode)
                    .set_sources(SourceType::MONITOR | SourceType::WINDOW)
                    .set_multiple(false)
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await?
            .response()?;
        let streams = proxy
            .start(session, None, StartCastOptions::default())
            .await?
            .response()?;
        let stream = streams.streams().first().ok_or(ScreenError::Cancelled)?;
        let node_id = stream.pipe_wire_node_id();
        let size = stream.size();
        let fd = proxy
            .open_pipe_wire_remote(session, OpenPipeWireRemoteOptions::default())
            .await?;
        Ok((fd, node_id, size))
    }

    /// Start converting the frames of the given node for `LiveKit`.
    fn start_pipeline(
        fd: OwnedFd,
        node_id: u32,
        size: Option<(i32, i32)>,
    ) -> Result<PipelineParts, ScreenError> {
        let (width, height) = scaled_size(size);
        let description = format!(
            "pipewiresrc fd={} path={node_id} always-copy=true keepalive-time=1000 \
             ! videoconvert ! videoscale \
             ! video/x-raw,format=I420,width={width},height={height},pixel-aspect-ratio=1/1 \
             ! appsink name=sink sync=false max-buffers=1 drop=true",
            fd.as_raw_fd()
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| ScreenError::Pipeline(error.to_string()))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| ScreenError::Pipeline("not a pipeline".to_owned()))?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|sink| sink.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| ScreenError::Pipeline("no sink".to_owned()))?;

        let resolution = VideoResolution { width, height };
        let source = NativeVideoSource::new(resolution.clone(), true);
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample({
                    let source = source.clone();
                    move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        send_frame(&source, &sample);
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .build(),
        );

        let (ended_tx, ended_rx) = oneshot::channel();
        let bus = pipeline
            .bus()
            .ok_or_else(|| ScreenError::Pipeline("no bus".to_owned()))?;
        let mut messages = bus.stream_filtered(&[gst::MessageType::Eos, gst::MessageType::Error]);
        tokio::spawn(async move {
            if let Some(message) = messages.next().await {
                if let gst::MessageView::Error(error) = message.view() {
                    warn!("Screen capture failed: {}", error.error());
                } else {
                    debug!("Screen capture ended");
                }
                let _ = ended_tx.send(());
            }
        });

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| ScreenError::Pipeline(error.to_string()))?;

        Ok(PipelineParts {
            pipeline,
            fd,
            source,
            resolution,
            ended: Some(ended_rx),
        })
    }

    /// Stop capturing.
    pub(super) async fn stop(self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Err(error) = self.session.close().await {
            debug!("Could not close screen sharing session: {error}");
        }
    }
}

/// A capture without its portal session yet.
struct PipelineParts {
    pipeline: gst::Pipeline,
    fd: OwnedFd,
    source: NativeVideoSource,
    resolution: VideoResolution,
    ended: Option<oneshot::Receiver<()>>,
}

impl ScreenCapture {
    /// Combine the pipeline parts with the portal session.
    fn from_parts(parts: PipelineParts, session: Session<Screencast>) -> Self {
        Self {
            pipeline: parts.pipeline,
            session,
            _fd: parts.fd,
            source: parts.source,
            resolution: parts.resolution,
            ended: parts.ended,
        }
    }
}

/// Scale the given size to fit in [`MAX_SIZE`], with even dimensions.
fn scaled_size(size: Option<(i32, i32)>) -> (u32, u32) {
    let (width, height) = size
        .filter(|(width, height)| *width > 0 && *height > 0)
        .map_or(MAX_SIZE, |(width, height)| {
            (f64::from(width), f64::from(height))
        });
    let scale = (MAX_SIZE.0 / width).min(MAX_SIZE.1 / height).min(1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let even = |value: f64| (((value * scale) / 2.0).round() as u32 * 2).max(2);
    (even(width), even(height))
}

/// Copy an I420 sample to the video source.
fn send_frame(source: &NativeVideoSource, sample: &gst::Sample) {
    let (Some(buffer), Some(caps)) = (sample.buffer(), sample.caps()) else {
        return;
    };
    let Ok(info) = gst_video::VideoInfo::from_caps(caps) else {
        return;
    };
    let Ok(frame) = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info) else {
        return;
    };

    let (width, height) = (frame.width(), frame.height());
    let mut i420 = I420Buffer::new(width, height);
    let (stride_y, stride_u, stride_v) = i420.strides();
    let (y, u, v) = i420.data_mut();
    let src_strides = frame.plane_stride();
    let chroma = (width.div_ceil(2), height.div_ceil(2));
    for (plane, dst, dst_stride, (w, h)) in [
        (0, y, stride_y, (width, height)),
        (1, u, stride_u, chroma),
        (2, v, stride_v, chroma),
    ] {
        let Ok(src) = frame.plane_data(plane) else {
            return;
        };
        let Ok(src_stride) = usize::try_from(src_strides[plane as usize]) else {
            return;
        };
        copy_plane(
            src,
            src_stride,
            dst,
            dst_stride as usize,
            w as usize,
            h as usize,
        );
    }

    let frame = VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        timestamp_us: gtk::glib::monotonic_time(),
        frame_metadata: None,
        buffer: i420,
    };
    source.capture_frame(&frame);
}

/// Copy the rows of a plane between buffers with different strides.
fn copy_plane(
    src: &[u8],
    src_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    width: usize,
    height: usize,
) {
    for row in 0..height {
        let (Some(src_row), Some(dst_row)) = (
            src.get(row * src_stride..row * src_stride + width),
            dst.get_mut(row * dst_stride..row * dst_stride + width),
        ) else {
            return;
        };
        dst_row.copy_from_slice(src_row);
    }
}
