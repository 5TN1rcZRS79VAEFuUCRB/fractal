//! How loud each person is in calls, kept between calls and restarts.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{LazyLock, Mutex},
};

use ruma::{OwnedUserId, UserId};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::utils::DataType;

/// The highest volume, as a factor of the volume sent.
pub(crate) const MAX_VOLUME: f64 = 2.0;

/// What a remote audio track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AudioKind {
    /// The microphone.
    Voice,
    /// The sound of a shared screen.
    Stream,
}

/// The volumes that are not 1, by kind and person.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Volumes {
    #[serde(default)]
    voice: HashMap<OwnedUserId, f64>,
    #[serde(default)]
    stream: HashMap<OwnedUserId, f64>,
}

impl Volumes {
    fn map(&mut self, kind: AudioKind) -> &mut HashMap<OwnedUserId, f64> {
        match kind {
            AudioKind::Voice => &mut self.voice,
            AudioKind::Stream => &mut self.stream,
        }
    }
}

static VOLUMES: LazyLock<Mutex<Volumes>> = LazyLock::new(|| Mutex::new(load()));

fn path() -> PathBuf {
    DataType::Persistent.dir_path().join("call-volumes.json")
}

fn load() -> Volumes {
    let Ok(data) = std::fs::read(path()) else {
        return Volumes::default();
    };
    serde_json::from_slice(&data).unwrap_or_else(|error| {
        warn!("Could not read the call volumes: {error}");
        Volumes::default()
    })
}

/// The volume of the given person, as a factor from 0 to [`MAX_VOLUME`].
pub(crate) fn volume(user_id: &UserId, kind: AudioKind) -> f64 {
    VOLUMES
        .lock()
        .unwrap()
        .map(kind)
        .get(user_id)
        .copied()
        .unwrap_or(1.0)
}

/// Set the volume of the given person and save it.
pub(crate) fn set_volume(user_id: &UserId, kind: AudioKind, volume: f64) {
    let volume = volume.clamp(0.0, MAX_VOLUME);
    let mut volumes = VOLUMES.lock().unwrap();
    if (volume - 1.0).abs() < 0.01 {
        volumes.map(kind).remove(user_id);
    } else {
        volumes.map(kind).insert(user_id.to_owned(), volume);
    }

    let path = path();
    let result = serde_json::to_vec(&*volumes)
        .map_err(std::io::Error::other)
        .and_then(|data| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, data)
        });
    if let Err(error) = result {
        warn!("Could not save the call volumes: {error}");
    }
}
