//! Secret API for Windows and macOS.
//!
//! The passphrase of each session is stored in the system keychain (Windows
//! Credential Manager or macOS Keychain), which cannot list its items, so the
//! rest of the session data is kept in a list next to the session data.

use std::path::PathBuf;

use matrix_sdk::authentication::oauth::ClientId;
use ruma::{OwnedDeviceId, OwnedUserId};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, error, warn};
use url::Url;
use zeroize::Zeroizing;

use super::{SecretError, SecretExt, StoredSession};
use crate::{APP_ID, spawn_tokio, utils::DataType};

/// A session as stored in the list, without its passphrase.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ListedSession {
    homeserver: Url,
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
    id: String,
    client_id: Option<ClientId>,
}

impl From<&StoredSession> for ListedSession {
    fn from(session: &StoredSession) -> Self {
        Self {
            homeserver: session.homeserver.clone(),
            user_id: session.user_id.clone(),
            device_id: session.device_id.clone(),
            id: session.id.clone(),
            client_id: session.client_id.clone(),
        }
    }
}

/// The path of the list of sessions.
fn list_path() -> PathBuf {
    let mut path = DataType::Persistent.dir_path();
    path.push("sessions.json");
    path
}

/// Read the list of sessions.
async fn read_list() -> Result<Vec<ListedSession>, SecretError> {
    match fs::read(list_path()).await {
        Ok(data) => serde_json::from_slice(&data)
            .map_err(|error| SecretError::Service(format!("invalid list of sessions: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(SecretError::Service(format!(
            "could not read the list of sessions: {error}"
        ))),
    }
}

/// Write the list of sessions.
async fn write_list(sessions: &[ListedSession]) -> Result<(), SecretError> {
    let path = list_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent).await;
    }
    let data = serde_json::to_vec_pretty(sessions)
        .map_err(|error| SecretError::Service(error.to_string()))?;
    // Write a copy first, so a crash never leaves a broken list.
    let temporary = path.with_extension("json.new");
    fs::write(&temporary, data)
        .await
        .map_err(|error| SecretError::Service(format!("could not save sessions: {error}")))?;
    fs::rename(&temporary, &path)
        .await
        .map_err(|error| SecretError::Service(format!("could not save sessions: {error}")))
}

/// The keychain entry of the session with the given ID.
fn entry(id: &str) -> Result<keyring::Entry, SecretError> {
    keyring::Entry::new(APP_ID, id).map_err(|error| SecretError::Service(error.to_string()))
}

/// Secret API under Windows and macOS.
#[derive(Debug)]
pub(crate) struct KeychainSecret;

impl SecretExt for KeychainSecret {
    async fn restore_sessions() -> Result<Vec<StoredSession>, SecretError> {
        spawn_tokio!(async move {
            let mut sessions = Vec::new();
            for listed in read_list().await? {
                let passphrase = tokio::task::spawn_blocking({
                    let id = listed.id.clone();
                    move || {
                        entry(&id)?
                            .get_password()
                            .map_err(|error| SecretError::Service(error.to_string()))
                    }
                })
                .await
                .map_err(|error| SecretError::Service(error.to_string()))?;
                match passphrase {
                    Ok(passphrase) => sessions.push(StoredSession {
                        homeserver: listed.homeserver,
                        user_id: listed.user_id,
                        device_id: listed.device_id,
                        id: listed.id,
                        client_id: listed.client_id,
                        passphrase: Zeroizing::new(passphrase),
                    }),
                    Err(error) => warn!(
                        "Could not restore session {} of {}: {error}",
                        listed.id, listed.user_id
                    ),
                }
            }
            Ok(sessions)
        })
        .await
        .expect("task was not aborted")
    }

    async fn store_session(session: StoredSession) -> Result<(), SecretError> {
        spawn_tokio!(async move {
            let listed = ListedSession::from(&session);
            let id = session.id.clone();
            let passphrase = session.passphrase.clone();
            tokio::task::spawn_blocking(move || {
                entry(&id)?
                    .set_password(&passphrase)
                    .map_err(|error| SecretError::Service(error.to_string()))
            })
            .await
            .map_err(|error| SecretError::Service(error.to_string()))??;

            let mut list = read_list().await?;
            list.retain(|other| other.id != listed.id);
            list.push(listed);
            write_list(&list).await
        })
        .await
        .expect("task was not aborted")
    }

    async fn delete_session(session: StoredSession) {
        let id = session.id.clone();
        spawn_tokio!(async move {
            match read_list().await {
                Ok(mut list) => {
                    list.retain(|other| other.id != id);
                    if let Err(error) = write_list(&list).await {
                        error!("Could not remove session from list: {error}");
                    }
                }
                Err(error) => error!("Could not remove session from list: {error}"),
            }
            let result = tokio::task::spawn_blocking(move || {
                entry(&id)?
                    .delete_credential()
                    .map_err(|error| SecretError::Service(error.to_string()))
            })
            .await;
            match result {
                Ok(Ok(())) => debug!("Removed session passphrase from the keychain"),
                Ok(Err(error)) => error!("Could not remove session passphrase: {error}"),
                Err(error) => error!("Could not remove session passphrase: {error}"),
            }
        })
        .await
        .expect("task was not aborted");
    }
}
