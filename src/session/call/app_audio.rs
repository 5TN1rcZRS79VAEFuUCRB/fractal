//! Capture the sound of other apps, to share it along with the screen.
//!
//! On Windows, `GStreamer` captures the sound of the whole system except our
//! own process. macOS can't share sound yet.
//!
//! On Linux, the capture itself runs in the `fractalplus-screen-audio` helper,
//! because the WebRTC library linked into Fractal shadows the `PipeWire`
//! functions. The helper links the chosen apps to its own capture stream, so
//! the apps keep playing on their usual device, and our own sound (the other
//! people in the call) is never captured, so they don't hear themselves.

#[cfg(target_os = "linux")]
use std::{
    io::Read,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
};

#[cfg(target_os = "linux")]
use tracing::debug;

/// The sample rate of the captured sound.
pub(super) const SAMPLE_RATE: u32 = 48_000;
/// The number of channels of the captured sound.
pub(super) const CHANNELS: u32 = 2;

/// The name of the helper program.
#[cfg(target_os = "linux")]
const HELPER: &str = "fractalplus-screen-audio";

/// Which sound to share with the screen.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) enum ShareAudio {
    /// No sound.
    #[default]
    None,
    /// The sound of every app, except this one.
    System,
    /// Only the sound of the app with the given name.
    App(String),
}

/// An app that is playing sound.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AudioApp {
    /// The name of the app.
    pub name: String,
}

/// The path of the helper program.
///
/// It is installed next to Fractal. In tests, Fractal is one directory deeper.
#[cfg(target_os = "linux")]
fn helper_path() -> PathBuf {
    let exe = std::env::current_exe().ok();
    exe.iter()
        .flat_map(|exe| exe.ancestors().skip(1).take(2))
        .map(|dir| dir.join(HELPER))
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(HELPER))
}

/// List the apps that are playing sound right now, except this one.
///
/// This blocks while it runs the helper.
#[cfg(target_os = "linux")]
pub(crate) fn playing_apps() -> Result<Vec<AudioApp>, String> {
    let output = Command::new(helper_path())
        .arg("list")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("could not run {HELPER}: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|name| AudioApp {
            name: name.to_owned(),
        })
        .collect())
}

/// A running capture of the sound of other apps.
///
/// The capture stops when this is dropped.
#[cfg(target_os = "linux")]
pub(super) struct AppAudioCapture {
    child: Child,
    reader: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
impl AppAudioCapture {
    /// Start capturing the given sound.
    ///
    /// `on_samples` is called from another thread with interleaved stereo
    /// samples at [`SAMPLE_RATE`].
    pub(super) fn start(
        audio: ShareAudio,
        mut on_samples: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<Self, String> {
        let mut command = Command::new(helper_path());
        match audio {
            ShareAudio::None => return Err("no sound to capture".to_owned()),
            ShareAudio::System => command.arg("system"),
            ShareAudio::App(name) => command.arg("app").arg(name),
        };
        let mut child = command
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not run {HELPER}: {error}"))?;
        let mut stdout = child.stdout.take().ok_or("no output from the helper")?;

        let reader = thread::Builder::new()
            .name("screen-audio".to_owned())
            .spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                let mut filled = 0;
                let mut samples = Vec::new();
                loop {
                    let n = match stdout.read(&mut buf[filled..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    filled += n;
                    let whole = filled - filled % 4;
                    samples.clear();
                    samples.extend(
                        buf[..whole]
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|b| f32::from_le_bytes(*b)),
                    );
                    buf.copy_within(whole..filled, 0);
                    filled -= whole;
                    on_samples(&samples);
                }
                debug!("Screen sharing sound capture ended");
            })
            .map_err(|error| error.to_string())?;

        Ok(Self {
            child,
            reader: Some(reader),
        })
    }
}

#[cfg(target_os = "linux")]
impl Drop for AppAudioCapture {
    fn drop(&mut self) {
        // Closing stdin asks the helper to stop.
        drop(self.child.stdin.take());
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// List the apps that are playing sound right now, except this one.
///
/// Only the sound of the whole system can be shared here, so this is empty.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn playing_apps() -> Result<Vec<AudioApp>, String> {
    Ok(Vec::new())
}

/// A running capture of the sound of other apps.
///
/// The capture stops when this is dropped.
#[cfg(not(target_os = "linux"))]
pub(super) struct AppAudioCapture {
    pipeline: gst::Pipeline,
}

#[cfg(not(target_os = "linux"))]
impl AppAudioCapture {
    /// Start capturing the given sound.
    ///
    /// `on_samples` is called from another thread with interleaved stereo
    /// samples at [`SAMPLE_RATE`].
    pub(super) fn start(
        audio: ShareAudio,
        on_samples: impl FnMut(&[f32]) + Send + 'static,
    ) -> Result<Self, String> {
        use std::sync::Mutex;

        use gst::prelude::*;

        let source = match audio {
            ShareAudio::None => return Err("no sound to capture".to_owned()),
            #[cfg(windows)]
            ShareAudio::System => format!(
                "wasapi2src loopback=true loopback-mode=exclude-process-tree loopback-target-pid={}",
                std::process::id()
            ),
            #[cfg(not(windows))]
            ShareAudio::System => {
                return Err("sharing sound is not supported on this system yet".to_owned());
            }
            ShareAudio::App(_) => {
                return Err(
                    "sharing the sound of one app is not supported on this system yet".to_owned(),
                );
            }
        };
        let description = format!(
            "{source} ! audioconvert ! audioresample \
             ! audio/x-raw,format=F32LE,layout=interleaved,rate={SAMPLE_RATE},channels={CHANNELS} \
             ! appsink name=sink sync=false"
        );
        let pipeline = gst::parse::launch(&description)
            .map_err(|error| error.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_owned())?;
        let sink = pipeline
            .by_name("sink")
            .and_then(|sink| sink.downcast::<gst_app::AppSink>().ok())
            .ok_or("no sink")?;
        let on_samples = Mutex::new(on_samples);
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(buffer) = sample.buffer()
                        && let Ok(map) = buffer.map_readable()
                    {
                        let samples = map
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|bytes| f32::from_le_bytes(*bytes))
                            .collect::<Vec<_>>();
                        if let Ok(mut on_samples) = on_samples.lock() {
                            on_samples(&samples);
                        }
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|error| error.to_string())?;
        Ok(Self { pipeline })
    }
}

#[cfg(not(target_os = "linux"))]
impl Drop for AppAudioCapture {
    fn drop(&mut self) {
        use gst::prelude::*;

        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        process::Command,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::*;

    /// Play a tone into a silent sink while running `f`.
    fn with_tone<T>(f: impl FnOnce() -> T) -> T {
        let path = std::env::temp_dir().join("fractal-test-tone.wav");
        let samples = (0..48_000 * 10)
            .map(|i| (f64::from(i) * 440.0 * std::f64::consts::TAU / 48_000.0).sin())
            .flat_map(|v| {
                #[allow(clippy::cast_possible_truncation)]
                let v = ((v * 12_000.0) as i16).to_le_bytes();
                [v, v].concat()
            })
            .collect::<Vec<u8>>();
        let mut wav = Vec::new();
        let len = u32::try_from(samples.len()).unwrap();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        for value in [16u32, 0x0002_0001, 48_000, 48_000 * 4, 0x0010_0004] {
            wav.extend_from_slice(&value.to_le_bytes());
        }
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&len.to_le_bytes());
        wav.extend_from_slice(&samples);
        std::fs::write(&path, wav).unwrap();

        let module = Command::new("pactl")
            .args([
                "load-module",
                "module-null-sink",
                "sink_name=fractal_test_sink",
            ])
            .output()
            .unwrap();
        let module = String::from_utf8(module.stdout).unwrap().trim().to_owned();
        let mut player = Command::new("pw-play")
            .args(["--target", "fractal_test_sink"])
            .arg(&path)
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_secs(1));

        let result = f();

        let _ = player.kill();
        let _ = player.wait();
        let _ = Command::new("pactl")
            .args(["unload-module", &module])
            .status();
        let _ = std::fs::remove_file(path);
        result
    }

    /// Capture the given sound for 2 seconds and return its peak.
    fn capture_peak(audio: ShareAudio) -> f32 {
        let peak = Arc::new(Mutex::new(0.0f32));
        let capture = AppAudioCapture::start(audio, {
            let peak = peak.clone();
            move |samples| {
                let mut peak = peak.lock().unwrap();
                *peak = samples.iter().fold(*peak, |max, s| max.max(s.abs()));
            }
        })
        .unwrap();
        thread::sleep(Duration::from_secs(2));
        drop(capture);
        *peak.lock().unwrap()
    }

    #[test]
    #[ignore = "needs a PipeWire session"]
    fn captures_other_apps() {
        with_tone(|| {
            let apps = playing_apps().unwrap();
            println!("Playing apps: {apps:?}");
            assert!(apps.iter().any(|app| app.name == "pw-play"));

            let system = capture_peak(ShareAudio::System);
            let app = capture_peak(ShareAudio::App("pw-play".to_owned()));
            let other = capture_peak(ShareAudio::App("not-playing".to_owned()));
            println!("Peaks: system {system}, app {app}, other app {other}");
            assert!(system > 0.3, "system sound not captured");
            assert!(app > 0.3, "app sound not captured");
            assert!(other < 0.01, "captured the wrong app");
        });
    }
}
