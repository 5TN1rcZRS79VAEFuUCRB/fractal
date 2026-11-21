//! The task running a call: `MatrixRTC` membership, media keys and `LiveKit`.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use livekit::{
    e2ee::{
        E2eeOptions, EncryptionType,
        key_provider::{KeyDerivationAlgorithm, KeyProvider, KeyProviderOptions},
    },
    options::TrackPublishOptions,
    prelude::*,
};
use matrix_sdk::{
    Client, Room as MatrixRoom, deserialized_responses::EncryptionInfo,
    event_handler::EventHandlerHandle,
};
use ruma::{
    OwnedUserId, api::client::delayed_events::update_delayed_event::UpdateAction,
    events::AnyToDeviceEvent, serde::Raw,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::rtc::{
    self, CALL_KEYS_EVENT_TYPE, DELAYED_LEAVE_RESTART, MediaKey, Membership, RtcError,
};

/// How long to wait before using a new media key, so other participants
/// receive it before we start encrypting with it.
const USE_KEY_DELAY: Duration = Duration::from_secs(1);

/// A command sent to a running call.
#[derive(Debug)]
pub(super) enum Command {
    /// Mute or unmute the microphone.
    SetMuted(bool),
    /// Leave the call.
    Leave,
}

/// An update about a running call.
#[derive(Debug)]
pub(super) enum Update {
    /// We are connected to the media server.
    Connected,
    /// The participants changed.
    Participants(Vec<Participant>),
    /// The call ended, with an error message if it failed.
    Ended(Option<String>),
}

/// A participant in a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Participant {
    pub user_id: OwnedUserId,
    /// Whether this participant is currently speaking.
    pub speaking: bool,
}

/// Run a call in the given room until it is left or fails.
pub(super) async fn run(
    room: MatrixRoom,
    mut commands: mpsc::UnboundedReceiver<Command>,
    updates: mpsc::UnboundedSender<Update>,
) {
    let result = CallTask::start(room).await;
    let error = match result {
        Ok(mut task) => {
            let _ = updates.send(Update::Connected);
            let error = task.run(&mut commands, &updates).await.err();
            task.leave().await;
            error
        }
        Err(error) => Some(error),
    };

    if let Some(error) = &error {
        warn!("Call failed: {error}");
    }
    let _ = updates.send(Update::Ended(error.map(|error| error.to_string())));
}

/// An error while running a call.
#[derive(Debug, thiserror::Error)]
pub(super) enum CallError {
    #[error("you are not logged in")]
    NoSession,
    #[error(transparent)]
    Rtc(#[from] RtcError),
    #[error("could not connect to the call server: {0}")]
    LiveKit(#[from] livekit::RoomError),
    #[error("could not open the audio devices: {0}")]
    Audio(String),
    #[error("the call server disconnected")]
    Disconnected,
}

/// Removes an event handler when dropped.
struct HandlerGuard {
    client: Client,
    handle: Option<EventHandlerHandle>,
}

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.client.remove_event_handler(handle);
        }
    }
}

/// Our outbound media key.
struct OwnKey {
    index: u8,
    key: Vec<u8>,
}

impl OwnKey {
    fn random(index: u8) -> Self {
        let key: [u8; 32] = rand::random();
        Self {
            index,
            key: key.to_vec(),
        }
    }
}

/// A connected call.
struct CallTask {
    client: Client,
    room: MatrixRoom,
    room_id: String,
    device_id: String,
    own_identity: String,
    state_key: String,
    delay_id: Option<String>,
    /// The key provider, if media is end-to-end encrypted.
    key_provider: Option<KeyProvider>,
    own_key: OwnKey,
    /// The key we will switch to after [`USE_KEY_DELAY`].
    pending_key: Option<OwnKey>,
    keys_rx: mpsc::UnboundedReceiver<MediaKey>,
    _keys_handler: HandlerGuard,
    /// The other devices in the call.
    memberships: HashSet<Membership>,
    speakers: HashSet<String>,
    livekit: Room,
    livekit_events: mpsc::UnboundedReceiver<RoomEvent>,
    microphone: LocalAudioTrack,
    _audio: PlatformAudio,
}

impl CallTask {
    /// Join the call in the given room.
    #[allow(clippy::too_many_lines)]
    async fn start(room: MatrixRoom) -> Result<Self, CallError> {
        let client = room.client();
        let user_id = client.user_id().ok_or(CallError::NoSession)?.to_owned();
        let device_id = client.device_id().ok_or(CallError::NoSession)?.to_string();
        let room_id = room.room_id().to_string();
        let own_identity = rtc::participant_identity(&user_id, &device_id);
        let encrypted = room
            .latest_encryption_state()
            .await
            .is_ok_and(|state| state.is_encrypted());

        let service_url = rtc::discover_livekit_service_url(&client).await?;
        info!("Joining call in {room_id} through {service_url} (encrypted: {encrypted})");

        // Listen for media keys before announcing ourselves, so we don't miss
        // the keys other participants send us when we join.
        let (keys_tx, keys_rx) = mpsc::unbounded_channel();
        let handle = client.add_event_handler({
            let room_id = room_id.clone();
            move |raw: Raw<AnyToDeviceEvent>, encryption_info: Option<EncryptionInfo>| {
                let keys_tx = keys_tx.clone();
                let room_id = room_id.clone();
                async move {
                    handle_to_device(&raw, encryption_info, &room_id, &keys_tx);
                }
            }
        });
        let keys_handler = HandlerGuard {
            client: client.clone(),
            handle: Some(handle),
        };

        let state_key = rtc::membership_state_key(&room, &user_id, &device_id);
        let delay_id = rtc::schedule_delayed_leave(&room, &state_key).await;
        rtc::send_membership(
            &room,
            &state_key,
            rtc::membership_content(&own_identity, &device_id, &service_url, &room_id, None),
        )
        .await?;

        let sfu = match rtc::get_sfu_config(&client, &service_url, &room_id, &device_id).await {
            Ok(sfu) => sfu,
            Err(error) => {
                leave_membership(&client, &room, &state_key, delay_id.as_deref()).await;
                return Err(error.into());
            }
        };

        let own_key = OwnKey::random(0);
        let key_provider = encrypted.then(|| {
            let key_provider = KeyProvider::new(KeyProviderOptions {
                ratchet_window_size: 10,
                key_ring_size: 256,
                key_derivation_algorithm: KeyDerivationAlgorithm::HKDF,
                ..Default::default()
            });
            key_provider.set_key(
                &ParticipantIdentity(own_identity.clone()),
                own_key.index.into(),
                own_key.key.clone(),
            );
            key_provider
        });

        let mut options = RoomOptions::default();
        options.auto_subscribe = true;
        options.encryption = key_provider.clone().map(|key_provider| E2eeOptions {
            encryption_type: EncryptionType::Gcm,
            key_provider,
        });

        let (livekit, livekit_events) = match Room::connect(&sfu.url, &sfu.jwt, options).await {
            Ok(connection) => connection,
            Err(error) => {
                leave_membership(&client, &room, &state_key, delay_id.as_deref()).await;
                return Err(error.into());
            }
        };
        if key_provider.is_some() {
            livekit.e2ee_manager().set_enabled(true);
        }

        let audio = match PlatformAudio::new() {
            Ok(audio) => audio,
            Err(error) => {
                let _ = livekit.close().await;
                leave_membership(&client, &room, &state_key, delay_id.as_deref()).await;
                return Err(CallError::Audio(error.to_string()));
            }
        };
        let microphone = LocalAudioTrack::create_audio_track("microphone", audio.rtc_source());
        if let Err(error) = livekit
            .local_participant()
            .publish_track(
                LocalTrack::Audio(microphone.clone()),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    dtx: true,
                    red: true,
                    ..Default::default()
                },
            )
            .await
        {
            let _ = livekit.close().await;
            leave_membership(&client, &room, &state_key, delay_id.as_deref()).await;
            return Err(error.into());
        }

        Ok(Self {
            client,
            room,
            room_id,
            device_id,
            own_identity,
            state_key,
            delay_id,
            key_provider,
            own_key,
            pending_key: None,
            keys_rx,
            _keys_handler: keys_handler,
            memberships: HashSet::new(),
            speakers: HashSet::new(),
            livekit,
            livekit_events,
            microphone,
            _audio: audio,
        })
    }

    /// Run the call until it is left or fails.
    async fn run(
        &mut self,
        commands: &mut mpsc::UnboundedReceiver<Command>,
        updates: &mpsc::UnboundedSender<Update>,
    ) -> Result<(), CallError> {
        let mut room_info = self.room.subscribe_info();
        let mut heartbeat = tokio::time::interval(DELAYED_LEAVE_RESTART);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The memberships are refreshed on room info changes, but also
        // periodically so expired memberships are dropped.
        let mut refresh = tokio::time::interval(Duration::from_secs(30));
        let use_key = tokio::time::sleep_until(far_future());
        tokio::pin!(use_key);

        self.update_memberships(updates).await;

        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::SetMuted(true)) => self.microphone.mute(),
                    Some(Command::SetMuted(false)) => self.microphone.unmute(),
                    Some(Command::Leave) | None => return Ok(()),
                },
                event = self.livekit_events.recv() => match event {
                    Some(RoomEvent::ActiveSpeakersChanged { speakers }) => {
                        self.speakers = speakers
                            .iter()
                            .map(|participant| participant.identity().to_string())
                            .collect();
                        self.send_participants(updates);
                    }
                    Some(RoomEvent::ParticipantConnected(participant)) => {
                        debug!("LiveKit participant connected: {}", participant.identity());
                    }
                    Some(RoomEvent::ParticipantDisconnected(participant)) => {
                        debug!("LiveKit participant disconnected: {}", participant.identity());
                    }
                    Some(RoomEvent::TrackSubscribed { publication, participant, .. }) => {
                        debug!(
                            "Subscribed to {:?} track of {}",
                            publication.source(),
                            participant.identity()
                        );
                    }
                    Some(RoomEvent::TrackSubscriptionFailed { participant, error, .. }) => {
                        warn!("Could not subscribe to track of {}: {error}", participant.identity());
                    }
                    Some(RoomEvent::E2eeStateChanged { participant, state }) => {
                        debug!("Encryption state of {}: {state:?}", participant.identity());
                    }
                    Some(RoomEvent::Disconnected { reason }) => {
                        debug!("LiveKit disconnected: {reason:?}");
                        return Err(CallError::Disconnected);
                    }
                    Some(_) => {}
                    None => return Err(CallError::Disconnected),
                },
                Some(key) = self.keys_rx.recv() => self.add_remote_key(key),
                Some(_) = room_info.next() => {
                    if let Some(deadline) = self.update_memberships(updates).await {
                        use_key.as_mut().reset(deadline);
                    }
                }
                _ = refresh.tick() => {
                    if let Some(deadline) = self.update_memberships(updates).await {
                        use_key.as_mut().reset(deadline);
                    }
                }
                _ = heartbeat.tick() => self.restart_delayed_leave().await,
                () = &mut use_key => {
                    self.use_pending_key();
                    use_key.as_mut().reset(far_future());
                }
            }
        }
    }

    /// Reload the memberships, share our key with new devices and rotate it
    /// when devices leave.
    ///
    /// Returns when to start using a new key, if it was rotated.
    async fn update_memberships(
        &mut self,
        updates: &mpsc::UnboundedSender<Update>,
    ) -> Option<tokio::time::Instant> {
        let memberships = rtc::active_memberships(&self.room)
            .await
            .into_iter()
            .filter(|membership| membership.identity != self.own_identity)
            .collect::<HashSet<_>>();
        if memberships == self.memberships {
            return None;
        }

        debug!(
            "Call memberships: {:?}",
            memberships
                .iter()
                .map(|m| m.identity.as_str())
                .collect::<Vec<_>>()
        );
        let joined = memberships
            .difference(&self.memberships)
            .cloned()
            .collect::<Vec<_>>();
        let left = self.memberships.difference(&memberships).count();
        self.memberships = memberships;
        self.send_participants(updates);

        self.key_provider.as_ref()?;

        if left > 0 {
            // Someone left: they must not be able to decrypt what comes next.
            let index = self
                .pending_key
                .as_ref()
                .unwrap_or(&self.own_key)
                .index
                .wrapping_add(1);
            let key = OwnKey::random(index);
            let targets = self.memberships.iter().cloned().collect::<Vec<_>>();
            self.share_key(&key, &targets).await;
            self.pending_key = Some(key);
            return Some(tokio::time::Instant::now() + USE_KEY_DELAY);
        }

        if !joined.is_empty() {
            // Newcomers get our current key, and the next one if we are
            // about to switch to it.
            let current = OwnKey {
                index: self.own_key.index,
                key: self.own_key.key.clone(),
            };
            self.share_key(&current, &joined).await;
            if let Some(pending) = self.pending_key.take() {
                self.share_key(&pending, &joined).await;
                self.pending_key = Some(pending);
            }
        }
        None
    }

    /// Send our media key to the given devices.
    async fn share_key(&self, key: &OwnKey, targets: &[Membership]) {
        if targets.is_empty() {
            return;
        }
        let encryption = self.client.encryption();

        let mut devices = Vec::new();
        for target in targets {
            match encryption
                .get_device(&target.user_id, &target.device_id)
                .await
            {
                Ok(Some(device)) => devices.push(device),
                Ok(None) => warn!(
                    "Unknown device {} of {}, cannot send call key",
                    target.device_id, target.user_id
                ),
                Err(error) => warn!("Could not load device of {}: {error}", target.user_id),
            }
        }
        if devices.is_empty() {
            return;
        }

        let content = rtc::media_key_content(
            &self.room_id,
            &self.own_identity,
            &self.device_id,
            key.index,
            &key.key,
        );
        let content = match Raw::new(&content) {
            Ok(content) => content.cast_unchecked(),
            Err(error) => {
                warn!("Could not serialize call key: {error}");
                return;
            }
        };
        if let Err(error) = encryption
            .encrypt_and_send_raw_to_device(
                devices.iter().collect(),
                CALL_KEYS_EVENT_TYPE,
                content,
                // The default strategy shares with all devices, like Element Call.
                Default::default(),
            )
            .await
        {
            warn!("Could not send call key: {error}");
        } else {
            debug!("Sent call key {} to {} devices", key.index, devices.len());
        }
    }

    /// Start encrypting with the pending key.
    fn use_pending_key(&mut self) {
        let (Some(key_provider), Some(key)) = (&self.key_provider, self.pending_key.take()) else {
            return;
        };
        let identity = ParticipantIdentity(self.own_identity.clone());
        key_provider.set_key(&identity, key.index.into(), key.key.clone());
        for ((participant, _), cryptor) in self.livekit.e2ee_manager().frame_cryptors() {
            if participant == identity {
                cryptor.set_key_index(key.index.into());
            }
        }
        debug!("Now encrypting call media with key {}", key.index);
        self.own_key = key;
    }

    /// Use a key received from another participant.
    fn add_remote_key(&self, key: MediaKey) {
        let Some(key_provider) = &self.key_provider else {
            return;
        };
        debug!(
            "Received call key {} from {} ({})",
            key.index, key.user_id, key.device_id
        );
        key_provider.set_key(
            &ParticipantIdentity(key.identity),
            key.index.into(),
            key.key,
        );
    }

    /// Keep our delayed leave event from firing.
    async fn restart_delayed_leave(&mut self) {
        let Some(delay_id) = &self.delay_id else {
            return;
        };
        if let Err(error) =
            rtc::update_delayed_event(&self.client, delay_id, UpdateAction::Restart).await
        {
            warn!("Could not restart delayed leave, scheduling a new one: {error}");
            self.delay_id = rtc::schedule_delayed_leave(&self.room, &self.state_key).await;
        }
    }

    /// Tell the UI who is in the call.
    fn send_participants(&self, updates: &mpsc::UnboundedSender<Update>) {
        let mut seen = HashMap::new();
        if let Some(user_id) = self.client.user_id() {
            let speaking = self
                .speakers
                .contains(&self.livekit.local_participant().identity().to_string());
            seen.insert(user_id.to_owned(), speaking);
        }
        for membership in &self.memberships {
            let speaking = self.speakers.contains(&membership.identity);
            let entry = seen.entry(membership.user_id.clone()).or_insert(false);
            *entry |= speaking;
        }
        let mut participants = seen
            .into_iter()
            .map(|(user_id, speaking)| Participant { user_id, speaking })
            .collect::<Vec<_>>();
        participants.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        let _ = updates.send(Update::Participants(participants));
    }

    /// Leave the call.
    async fn leave(self) {
        if let Err(error) = self.livekit.close().await {
            debug!("Could not close LiveKit connection: {error}");
        }
        leave_membership(
            &self.client,
            &self.room,
            &self.state_key,
            self.delay_id.as_deref(),
        )
        .await;
    }
}

/// A deadline that never comes.
fn far_future() -> tokio::time::Instant {
    tokio::time::Instant::now() + Duration::from_hours(8760)
}

/// Remove our membership from the room, after the call task failed.
pub(super) async fn clean_up_membership(room: &MatrixRoom) {
    let client = room.client();
    let (Some(user_id), Some(device_id)) = (client.user_id(), client.device_id()) else {
        return;
    };
    let state_key = rtc::membership_state_key(room, user_id, device_id.as_str());
    leave_membership(&client, room, &state_key, None).await;
}

/// Remove our membership.
async fn leave_membership(
    client: &Client,
    room: &MatrixRoom,
    state_key: &str,
    delay_id: Option<&str>,
) {
    if let Some(delay_id) = delay_id {
        match rtc::update_delayed_event(client, delay_id, UpdateAction::Send).await {
            Ok(()) => return,
            Err(error) => debug!("Could not send delayed leave, sending it directly: {error}"),
        }
    }
    if let Err(error) = rtc::send_membership(room, state_key, serde_json::json!({})).await {
        warn!("Could not leave call: {error}");
    }
}

/// Handle a to-device event, forwarding call media keys for our room.
fn handle_to_device(
    raw: &Raw<AnyToDeviceEvent>,
    encryption_info: Option<EncryptionInfo>,
    room_id: &str,
    keys_tx: &mpsc::UnboundedSender<MediaKey>,
) {
    #[derive(Deserialize)]
    struct ToDevice {
        #[serde(rename = "type")]
        event_type: String,
        content: JsonValue,
    }

    let Ok(event) = raw.deserialize_as_unchecked::<ToDevice>() else {
        return;
    };
    if event.event_type != CALL_KEYS_EVENT_TYPE {
        return;
    }
    let Some(encryption_info) = encryption_info else {
        warn!("Ignoring call key sent without encryption");
        return;
    };
    if let Some(key) = rtc::parse_media_key(encryption_info.sender, room_id, &event.content) {
        let _ = keys_tx.send(key);
    }
}
