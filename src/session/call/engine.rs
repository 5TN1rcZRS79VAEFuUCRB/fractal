//! The task running a call: `MatrixRTC` membership, media keys and `LiveKit`.

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    time::Duration,
};

use livekit::{
    e2ee::{
        E2eeOptions, EncryptionType,
        key_provider::{KeyDerivationAlgorithm, KeyProvider, KeyProviderOptions},
    },
    options::{AudioEncoding, TrackPublishOptions, VideoCodec, VideoEncoding},
    prelude::*,
    webrtc::{
        audio_frame::AudioFrame,
        audio_source::{AudioSourceOptions, RtcAudioSource, native::NativeAudioSource},
        video_source::RtcVideoSource,
    },
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
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinError, JoinHandle},
};
use tracing::{debug, info, warn};

use super::{
    app_audio::{self, AppAudioCapture, ShareAudio},
    camera::{CameraCapture, CameraError},
    rtc::{self, CALL_KEYS_EVENT_TYPE, DELAYED_LEAVE_RESTART, MediaKey, Membership, RtcError},
    screen::{ScreenCapture, ScreenError},
    video::{self, Video, VideoKind, VideoSink},
    volume::{self, AudioKind},
};

/// How long to wait before using a new media key, so other participants
/// receive it before we start encrypting with it.
const USE_KEY_DELAY: Duration = Duration::from_secs(1);
/// How long to wait before connecting again to the server of other
/// participants, after the connection failed or dropped.
const SERVER_RETRY_DELAY: Duration = Duration::from_secs(10);

/// A command sent to a running call.
#[derive(Debug)]
pub(super) enum Command {
    /// Mute or unmute the microphone.
    SetMuted(bool),
    /// Ask the user what to share and share their screen, with the given
    /// sound.
    StartScreenShare(ShareAudio),
    /// Stop sharing the screen.
    StopScreenShare,
    /// Turn the camera on or off.
    SetCamera(bool),
    /// Apply the volumes of people again, after one changed.
    UpdateVolumes,
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
    /// Whether we are sharing our screen changed.
    ScreenShare(bool),
    /// Sharing the screen, or its sound, failed.
    ScreenShareFailed(String),
    /// Whether our camera is on changed.
    Camera(bool),
    /// Turning the camera on failed.
    CameraFailed(String),
    /// The videos in the call changed.
    Videos(Vec<Video>),
    /// The call ended, with an error message if it failed.
    Ended(Option<String>),
}

/// A participant in a call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct Participant {
    pub user_id: OwnedUserId,
    /// Whether this participant is currently speaking.
    pub speaking: bool,
    /// Whether we hear the voice of this participant.
    pub voice: bool,
    /// Whether we hear the sound of the screen this participant shares.
    pub stream: bool,
    /// Whether this participant's microphone is muted, or not sent at all.
    pub muted: bool,
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

/// Something that happened on a connection to the `LiveKit` server of other
/// participants.
enum ServerEvent {
    Connected(Room),
    Failed(CallError),
    Event(Box<RoomEvent>),
    Closed,
}

/// A connection to the `LiveKit` server that other participants publish to,
/// only to receive what they publish there.
struct RemoteServer {
    /// Tells the events of this connection from those of an earlier one to
    /// the same server.
    id: u64,
    /// The connection, once it is made.
    livekit: Option<Room>,
    /// The task making the connection and forwarding its events.
    task: JoinHandle<()>,
    /// The identities of who is speaking there.
    speakers: HashSet<String>,
    /// The IDs of the tracks we receive there.
    tracks: HashSet<String>,
}

/// What a connection to the server of other participants needs.
struct ServerConnection {
    id: u64,
    client: Client,
    service_url: String,
    room_id: String,
    identity: String,
    device_id: String,
    key_provider: Option<KeyProvider>,
    events_tx: mpsc::UnboundedSender<(u64, ServerEvent)>,
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
    /// The JWT service of the `LiveKit` server we publish to.
    service_url: String,
    livekit: Room,
    livekit_events: mpsc::UnboundedReceiver<RoomEvent>,
    /// The connections to the servers other participants publish to, by JWT
    /// service, when it is not ours.
    servers: HashMap<String, RemoteServer>,
    /// When we may connect again to servers whose connection failed.
    server_retries: HashMap<String, tokio::time::Instant>,
    next_server_id: u64,
    server_events_tx: mpsc::UnboundedSender<(u64, ServerEvent)>,
    server_events_rx: mpsc::UnboundedReceiver<(u64, ServerEvent)>,
    microphone: LocalAudioTrack,
    _audio: PlatformAudio,
    /// The screen capture the user is choosing, with the sound to share.
    share_request: Option<(JoinHandle<Result<ScreenCapture, ScreenError>>, ShareAudio)>,
    /// The screen we are sharing.
    screen: Option<ScreenShare>,
    /// Resolves when the screen capture stops by itself.
    screen_ended: Option<oneshot::Receiver<()>>,
    /// The videos of other participants, by track ID.
    remote_videos: HashMap<String, RemoteVideo>,
    /// The sound of other participants, by track ID.
    remote_audio: HashMap<String, RemoteAudio>,
    /// The camera being turned on.
    camera_request: Option<JoinHandle<Result<CameraCapture, CameraError>>>,
    /// Our camera, with the track we published for it.
    camera: Option<(CameraCapture, TrackSid)>,
    /// Resolves when the camera capture stops by itself.
    camera_ended: Option<oneshot::Receiver<()>>,
}

/// The voice or screen sound of another participant.
struct RemoteAudio {
    user_id: OwnedUserId,
    kind: AudioKind,
    track: RemoteAudioTrack,
    /// Whether the sender muted it for now.
    muted: bool,
}

impl RemoteAudio {
    /// Play this track at the volume chosen for its sender.
    fn apply_volume(&self) {
        let volume = volume::volume(&self.user_id, self.kind);
        self.track.rtc_track().set_volume(volume);
    }
}

/// A video of another participant.
struct RemoteVideo {
    video: Video,
    /// Whether the sender turned it off for now.
    muted: bool,
    /// The task copying its frames.
    task: JoinHandle<()>,
}

/// A screen we are sharing.
struct ScreenShare {
    capture: ScreenCapture,
    /// The tracks we published for it.
    tracks: Vec<TrackSid>,
    /// The capture of the sound of other apps.
    audio: Option<AppAudioCapture>,
    /// The task sending the sound to `LiveKit`.
    audio_pump: Option<JoinHandle<()>>,
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

        let service_url =
            rtc::normalize_service_url(&rtc::discover_livekit_service_url(&client).await?);
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

        let (server_events_tx, server_events_rx) = mpsc::unbounded_channel();
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
            service_url,
            livekit,
            livekit_events,
            servers: HashMap::new(),
            server_retries: HashMap::new(),
            next_server_id: 0,
            server_events_tx,
            server_events_rx,
            microphone,
            _audio: audio,
            share_request: None,
            screen: None,
            screen_ended: None,
            remote_videos: HashMap::new(),
            remote_audio: HashMap::new(),
            camera_request: None,
            camera: None,
            camera_ended: None,
        })
    }

    /// Run the call until it is left or fails.
    #[allow(clippy::too_many_lines)]
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
        self.sync_servers(updates);

        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::SetMuted(true)) => {
                        self.microphone.mute();
                        self.send_participants(updates);
                    }
                    Some(Command::SetMuted(false)) => {
                        self.microphone.unmute();
                        self.send_participants(updates);
                    }
                    Some(Command::StartScreenShare(audio)) => self.request_screen_share(audio),
                    Some(Command::StopScreenShare) => {
                        if let Some((request, _)) = self.share_request.take() {
                            request.abort();
                        }
                        self.stop_screen_share().await;
                        let _ = updates.send(Update::ScreenShare(false));
                    }
                    Some(Command::SetCamera(true)) => {
                        if self.camera.is_none() && self.camera_request.is_none() {
                            self.camera_request = Some(tokio::spawn(CameraCapture::start()));
                        }
                    }
                    Some(Command::SetCamera(false)) => {
                        if let Some(request) = self.camera_request.take() {
                            request.abort();
                        }
                        self.stop_camera(updates).await;
                    }
                    Some(Command::UpdateVolumes) => {
                        for audio in self.remote_audio.values() {
                            audio.apply_volume();
                        }
                    }
                    Some(Command::Leave) | None => return Ok(()),
                },
                event = self.livekit_events.recv() => match event {
                    Some(RoomEvent::Disconnected { reason }) => {
                        debug!("LiveKit disconnected: {reason:?}");
                        return Err(CallError::Disconnected);
                    }
                    Some(event) => self.handle_livekit_event(None, event, updates),
                    None => return Err(CallError::Disconnected),
                },
                Some((id, event)) = self.server_events_rx.recv() => {
                    self.handle_server_event(id, event, updates);
                }
                Some(key) = self.keys_rx.recv() => self.add_remote_key(key),
                Some(_) = room_info.next() => {
                    if let Some(deadline) = self.update_memberships(updates).await {
                        use_key.as_mut().reset(deadline);
                    }
                    self.sync_servers(updates);
                }
                _ = refresh.tick() => {
                    if let Some(deadline) = self.update_memberships(updates).await {
                        use_key.as_mut().reset(deadline);
                    }
                    // Also retries the servers we could not connect to.
                    self.sync_servers(updates);
                }
                result = share_result(&mut self.share_request), if self.share_request.is_some() => {
                    if let Some((_, audio)) = self.share_request.take() {
                        self.start_screen_share(result, audio, updates).await;
                    }
                }
                result = finished(&mut self.camera_request), if self.camera_request.is_some() => {
                    self.camera_request = None;
                    self.start_camera(result, updates).await;
                }
                () = capture_ended(&mut self.camera_ended) => self.stop_camera(updates).await,
                () = capture_ended(&mut self.screen_ended) => {
                    self.stop_screen_share().await;
                    let _ = updates.send(Update::ScreenShare(false));
                }
                _ = heartbeat.tick() => self.restart_delayed_leave().await,
                () = &mut use_key => {
                    self.use_pending_key();
                    use_key.as_mut().reset(far_future());
                }
            }
        }
    }

    /// Handle an event of the `LiveKit` server we publish to, or with `server`,
    /// of the server other participants publish to.
    fn handle_livekit_event(
        &mut self,
        server: Option<&str>,
        event: RoomEvent,
        updates: &mpsc::UnboundedSender<Update>,
    ) {
        let on = server.unwrap_or("our server");
        match event {
            RoomEvent::ActiveSpeakersChanged { speakers } => {
                let speakers = speakers
                    .iter()
                    .map(|participant| participant.identity().to_string())
                    .collect();
                match server {
                    None => self.speakers = speakers,
                    Some(url) => {
                        if let Some(server) = self.servers.get_mut(url) {
                            server.speakers = speakers;
                        }
                    }
                }
                self.send_participants(updates);
            }
            RoomEvent::ParticipantConnected(participant) => {
                debug!(
                    "LiveKit participant connected on {on}: {}",
                    participant.identity()
                );
            }
            RoomEvent::ParticipantDisconnected(participant) => {
                debug!(
                    "LiveKit participant disconnected on {on}: {}",
                    participant.identity()
                );
            }
            RoomEvent::TrackSubscribed {
                track,
                publication,
                participant,
            } => {
                debug!(
                    "Subscribed to {:?} track of {} on {on}",
                    publication.source(),
                    participant.identity()
                );
                if let Some(url) = server
                    && let Some(server) = self.servers.get_mut(url)
                {
                    server.tracks.insert(publication.sid().to_string());
                }
                match track {
                    RemoteTrack::Video(track) => {
                        self.add_remote_video(&track, &publication, &participant, updates);
                    }
                    RemoteTrack::Audio(track) => {
                        self.add_remote_audio(track, &publication, &participant, updates);
                    }
                }
            }
            RoomEvent::TrackUnsubscribed { publication, .. } => {
                let sid = publication.sid().to_string();
                if let Some(url) = server
                    && let Some(server) = self.servers.get_mut(url)
                {
                    server.tracks.remove(&sid);
                }
                self.remove_track(&sid, updates);
            }
            RoomEvent::TrackMuted { publication, .. } => {
                self.set_track_muted(&publication.sid(), true, updates);
            }
            RoomEvent::TrackUnmuted { publication, .. } => {
                self.set_track_muted(&publication.sid(), false, updates);
            }
            RoomEvent::TrackSubscriptionFailed {
                participant, error, ..
            } => {
                warn!(
                    "Could not subscribe to track of {} on {on}: {error}",
                    participant.identity()
                );
            }
            RoomEvent::E2eeStateChanged { participant, state } => {
                debug!(
                    "Encryption state of {} on {on}: {state:?}",
                    participant.identity()
                );
            }
            _ => {}
        }
    }

    /// Stop playing or showing a track of another participant.
    fn remove_track(&mut self, sid: &str, updates: &mpsc::UnboundedSender<Update>) {
        if let Some(video) = self.remote_videos.remove(sid) {
            video.task.abort();
            self.send_videos(updates);
        }
        if self.remote_audio.remove(sid).is_some() {
            self.send_participants(updates);
        }
    }

    /// Connect to the servers that other participants publish to, and
    /// disconnect from those nobody publishes to anymore.
    ///
    /// Each device publishes to the server of its own homeserver, so to hear
    /// people from other homeservers, we receive from their servers too.
    fn sync_servers(&mut self, updates: &mpsc::UnboundedSender<Update>) {
        let wanted = self
            .memberships
            .iter()
            .filter_map(|membership| membership.service_url.clone())
            .filter(|url| *url != self.service_url)
            .collect::<HashSet<_>>();
        let unwanted = self
            .servers
            .keys()
            .filter(|url| !wanted.contains(*url))
            .cloned()
            .collect::<Vec<_>>();
        for url in unwanted {
            debug!("Nobody in the call publishes to {url} anymore");
            self.close_server(&url, updates);
        }
        let now = tokio::time::Instant::now();
        self.server_retries.retain(|_, retry| *retry > now);
        for url in wanted {
            if !self.servers.contains_key(&url) && !self.server_retries.contains_key(&url) {
                self.connect_server(url);
            }
        }
    }

    /// Start connecting to the server behind the given JWT service.
    fn connect_server(&mut self, service_url: String) {
        let id = self.next_server_id;
        self.next_server_id += 1;
        debug!("Connecting to the call server of {service_url} to receive media");
        let task = tokio::spawn(run_server(ServerConnection {
            id,
            client: self.client.clone(),
            service_url: service_url.clone(),
            room_id: self.room_id.clone(),
            identity: self.own_identity.clone(),
            device_id: self.device_id.clone(),
            key_provider: self.key_provider.clone(),
            events_tx: self.server_events_tx.clone(),
        }));
        self.servers.insert(
            service_url,
            RemoteServer {
                id,
                livekit: None,
                task,
                speakers: HashSet::new(),
                tracks: HashSet::new(),
            },
        );
    }

    /// Disconnect from the server behind the given JWT service, dropping what
    /// we receive there.
    fn close_server(&mut self, service_url: &str, updates: &mpsc::UnboundedSender<Update>) {
        let Some(server) = self.servers.remove(service_url) else {
            return;
        };
        server.task.abort();
        for sid in &server.tracks {
            self.remove_track(sid, updates);
        }
        if !server.speakers.is_empty() {
            self.send_participants(updates);
        }
        if let Some(livekit) = server.livekit {
            tokio::spawn(async move {
                if let Err(error) = livekit.close().await {
                    debug!("Could not close LiveKit connection: {error}");
                }
            });
        }
    }

    /// Handle what happened on a connection to the server of other
    /// participants.
    fn handle_server_event(
        &mut self,
        id: u64,
        event: ServerEvent,
        updates: &mpsc::UnboundedSender<Update>,
    ) {
        let Some(url) = self
            .servers
            .iter()
            .find(|(_, server)| server.id == id)
            .map(|(url, _)| url.clone())
        else {
            // A connection we dropped.
            if let ServerEvent::Connected(livekit) = event {
                tokio::spawn(async move {
                    let _ = livekit.close().await;
                });
            }
            return;
        };
        match event {
            ServerEvent::Connected(livekit) => {
                info!("Receiving call media from the call server of {url}");
                if let Some(server) = self.servers.get_mut(&url) {
                    server.livekit = Some(livekit);
                }
            }
            ServerEvent::Failed(error) => {
                warn!("Could not connect to the call server of {url}: {error}");
                self.retry_server(&url, updates);
            }
            ServerEvent::Event(event) if matches!(*event, RoomEvent::Disconnected { .. }) => {
                if let RoomEvent::Disconnected { reason } = *event {
                    debug!("The call server of {url} disconnected: {reason:?}");
                }
                self.retry_server(&url, updates);
            }
            ServerEvent::Event(event) => self.handle_livekit_event(Some(&url), *event, updates),
            ServerEvent::Closed => {
                debug!("The connection to the call server of {url} closed");
                self.retry_server(&url, updates);
            }
        }
    }

    /// Drop the connection to the server behind the given JWT service, to
    /// connect again after [`SERVER_RETRY_DELAY`] if someone still publishes
    /// there.
    fn retry_server(&mut self, service_url: &str, updates: &mpsc::UnboundedSender<Update>) {
        self.close_server(service_url, updates);
        self.server_retries.insert(
            service_url.to_owned(),
            tokio::time::Instant::now() + SERVER_RETRY_DELAY,
        );
    }

    /// Whether the participant with the given identity is speaking, on any
    /// server.
    fn is_speaking(&self, identity: &str) -> bool {
        self.speakers.contains(identity)
            || self
                .servers
                .values()
                .any(|server| server.speakers.contains(identity))
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
        self.own_key = key;
        self.sync_key_index();
        debug!("Now encrypting call media with key {}", self.own_key.index);
    }

    /// Make all our tracks use our current key.
    ///
    /// New tracks start with the key at index 0.
    fn sync_key_index(&self) {
        let identity = ParticipantIdentity(self.own_identity.clone());
        for ((participant, _), cryptor) in self.livekit.e2ee_manager().frame_cryptors() {
            if participant == identity {
                cryptor.set_key_index(self.own_key.index.into());
            }
        }
    }

    /// Ask the user what to share, unless we are already sharing.
    fn request_screen_share(&mut self, audio: ShareAudio) {
        if self.screen.is_some() || self.share_request.is_some() {
            return;
        }
        self.share_request = Some((tokio::spawn(ScreenCapture::start()), audio));
    }

    /// Publish the screen capture the user chose.
    async fn start_screen_share(
        &mut self,
        result: Result<Result<ScreenCapture, ScreenError>, JoinError>,
        audio: ShareAudio,
        updates: &mpsc::UnboundedSender<Update>,
    ) {
        let mut capture = match result {
            Ok(Ok(capture)) => capture,
            Ok(Err(ScreenError::Cancelled)) => {
                let _ = updates.send(Update::ScreenShare(false));
                return;
            }
            Ok(Err(error)) => {
                warn!("Could not share screen: {error}");
                let _ = updates.send(Update::ScreenShareFailed(error.to_string()));
                return;
            }
            Err(error) => {
                warn!("Screen sharing request crashed: {error}");
                let _ = updates.send(Update::ScreenShareFailed(error.to_string()));
                return;
            }
        };

        let track = LocalVideoTrack::create_video_track(
            "screen",
            RtcVideoSource::Native(capture.source.clone()),
        );
        let pixels = u64::from(capture.resolution.width) * u64::from(capture.resolution.height);
        let options = TrackPublishOptions {
            source: TrackSource::Screenshare,
            video_codec: VideoCodec::VP8,
            simulcast: false,
            // About 1.5 bits per pixel per second, so 3 Mb/s for 1080p.
            video_encoding: Some(VideoEncoding {
                max_bitrate: (pixels * 3 / 2).clamp(500_000, 3_000_000),
                max_framerate: 30.0,
            }),
            ..Default::default()
        };
        let publication = match self
            .livekit
            .local_participant()
            .publish_track(LocalTrack::Video(track), options)
            .await
        {
            Ok(publication) => publication,
            Err(error) => {
                warn!("Could not publish screen: {error}");
                capture.stop().await;
                let _ = updates.send(Update::ScreenShareFailed(error.to_string()));
                return;
            }
        };

        self.screen_ended = capture.ended.take();
        let mut share = ScreenShare {
            capture,
            tracks: vec![publication.sid()],
            audio: None,
            audio_pump: None,
        };
        if audio != ShareAudio::None
            && let Err(error) = self.share_audio(&mut share, audio).await
        {
            warn!("Could not share screen sound: {error}");
            let _ = updates.send(Update::ScreenShareFailed(error));
        }
        self.screen = Some(share);
        self.sync_key_index();
        let _ = updates.send(Update::ScreenShare(true));
    }

    /// Capture and publish the sound of other apps along with the screen.
    async fn share_audio(&self, share: &mut ScreenShare, audio: ShareAudio) -> Result<(), String> {
        let source = NativeAudioSource::new(
            AudioSourceOptions {
                echo_cancellation: false,
                noise_suppression: false,
                auto_gain_control: false,
            },
            app_audio::SAMPLE_RATE,
            app_audio::CHANNELS,
            0,
        );
        let (samples_tx, samples_rx) = mpsc::unbounded_channel::<Vec<i16>>();
        let capture = AppAudioCapture::start(audio, move |samples| {
            #[allow(clippy::cast_possible_truncation)]
            let samples = samples
                .iter()
                .map(|sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16)
                .collect();
            let _ = samples_tx.send(samples);
        })?;
        share.audio = Some(capture);
        share.audio_pump = Some(tokio::spawn(pump_audio(source.clone(), samples_rx)));

        let track =
            LocalAudioTrack::create_audio_track("screen-audio", RtcAudioSource::Native(source));
        let publication = self
            .livekit
            .local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: TrackSource::ScreenshareAudio,
                    audio_encoding: Some(AudioEncoding {
                        max_bitrate: 128_000,
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        share.tracks.push(publication.sid());
        Ok(())
    }

    /// Stop sharing the screen, if we are.
    async fn stop_screen_share(&mut self) {
        self.screen_ended = None;
        let Some(share) = self.screen.take() else {
            return;
        };
        for sid in &share.tracks {
            if let Err(error) = self.livekit.local_participant().unpublish_track(sid).await {
                debug!("Could not unpublish screen track: {error}");
            }
        }
        // Stopping the capture ends the pump.
        drop(share.audio);
        if let Some(pump) = share.audio_pump {
            let _ = pump.await;
        }
        share.capture.stop().await;
        debug!("Stopped sharing the screen");
    }

    /// Publish the camera, once it is on.
    async fn start_camera(
        &mut self,
        result: Result<Result<CameraCapture, CameraError>, JoinError>,
        updates: &mpsc::UnboundedSender<Update>,
    ) {
        let mut capture = match result {
            Ok(Ok(capture)) => capture,
            Ok(Err(error)) => {
                warn!("Could not turn on camera: {error}");
                let _ = updates.send(Update::CameraFailed(error.to_string()));
                return;
            }
            Err(error) => {
                if !error.is_cancelled() {
                    warn!("Camera request crashed: {error}");
                    let _ = updates.send(Update::CameraFailed(error.to_string()));
                }
                return;
            }
        };

        let track = LocalVideoTrack::create_video_track(
            "camera",
            RtcVideoSource::Native(capture.source.clone()),
        );
        let options = TrackPublishOptions {
            source: TrackSource::Camera,
            video_codec: VideoCodec::VP8,
            simulcast: true,
            video_encoding: Some(VideoEncoding {
                max_bitrate: 1_700_000,
                max_framerate: 30.0,
            }),
            ..Default::default()
        };
        match self
            .livekit
            .local_participant()
            .publish_track(LocalTrack::Video(track), options)
            .await
        {
            Ok(publication) => {
                self.camera_ended = capture.ended.take();
                self.camera = Some((capture, publication.sid()));
                self.sync_key_index();
                let _ = updates.send(Update::Camera(true));
                self.send_videos(updates);
            }
            Err(error) => {
                warn!("Could not publish camera: {error}");
                capture.stop();
                let _ = updates.send(Update::CameraFailed(error.to_string()));
            }
        }
    }

    /// Turn off the camera, if it is on.
    async fn stop_camera(&mut self, updates: &mpsc::UnboundedSender<Update>) {
        self.camera_ended = None;
        if let Some((capture, sid)) = self.camera.take() {
            if let Err(error) = self.livekit.local_participant().unpublish_track(&sid).await {
                debug!("Could not unpublish camera: {error}");
            }
            capture.stop();
            debug!("Turned off the camera");
            self.send_videos(updates);
        }
        let _ = updates.send(Update::Camera(false));
    }

    /// The user of the given participant.
    fn user_of(&self, participant: &RemoteParticipant) -> Option<OwnedUserId> {
        let identity = participant.identity().to_string();
        self.memberships
            .iter()
            .find(|membership| membership.identity == identity)
            .map(|membership| membership.user_id.clone())
            .or_else(|| {
                // Identities are `@user:server:DEVICE`.
                let (user_id, _) = identity.rsplit_once(':')?;
                OwnedUserId::try_from(user_id).ok()
            })
    }

    /// Play the voice or screen sound of another participant at its volume.
    fn add_remote_audio(
        &mut self,
        track: RemoteAudioTrack,
        publication: &RemoteTrackPublication,
        participant: &RemoteParticipant,
        updates: &mpsc::UnboundedSender<Update>,
    ) {
        let Some(user_id) = self.user_of(participant) else {
            warn!(
                "Unknown participant {}: its sound has no volume",
                participant.identity()
            );
            return;
        };
        let kind = match publication.source() {
            TrackSource::ScreenshareAudio => AudioKind::Stream,
            _ => AudioKind::Voice,
        };
        let audio = RemoteAudio {
            user_id,
            kind,
            track,
            muted: publication.is_muted(),
        };
        audio.apply_volume();
        self.remote_audio
            .insert(publication.sid().to_string(), audio);
        self.send_participants(updates);
    }

    /// Show a video track of another participant.
    fn add_remote_video(
        &mut self,
        track: &RemoteVideoTrack,
        publication: &RemoteTrackPublication,
        participant: &RemoteParticipant,
        updates: &mpsc::UnboundedSender<Update>,
    ) {
        let Some(user_id) = self.user_of(participant) else {
            warn!(
                "Ignoring video of unknown participant {}",
                participant.identity()
            );
            return;
        };
        let kind = match publication.source() {
            TrackSource::Screenshare => VideoKind::Screen,
            _ => VideoKind::Camera,
        };
        let id = publication.sid().to_string();
        let sink = VideoSink::default();
        let task = tokio::spawn(video::receive(track.rtc_track(), sink.clone()));
        let video = Video {
            id: id.clone(),
            user_id,
            local: false,
            kind,
            sink,
        };
        if let Some(old) = self.remote_videos.insert(
            id,
            RemoteVideo {
                video,
                muted: publication.is_muted(),
                task,
            },
        ) {
            old.task.abort();
        }
        self.send_videos(updates);
    }

    /// Hide or show a video that its sender turned off or on, or note that
    /// a sender muted or unmuted its sound.
    fn set_track_muted(
        &mut self,
        sid: &TrackSid,
        muted: bool,
        updates: &mpsc::UnboundedSender<Update>,
    ) {
        let sid = sid.to_string();
        if let Some(video) = self.remote_videos.get_mut(&sid)
            && video.muted != muted
        {
            video.muted = muted;
            self.send_videos(updates);
        }
        if let Some(audio) = self.remote_audio.get_mut(&sid)
            && audio.muted != muted
        {
            audio.muted = muted;
            self.send_participants(updates);
        }
    }

    /// Tell the UI which videos to show.
    fn send_videos(&self, updates: &mpsc::UnboundedSender<Update>) {
        let mut videos = self
            .remote_videos
            .values()
            .filter(|video| !video.muted)
            .map(|video| video.video.clone())
            .collect::<Vec<_>>();
        if let (Some((capture, _)), Some(user_id)) = (&self.camera, self.client.user_id()) {
            videos.push(Video {
                id: "local-camera".to_owned(),
                user_id: user_id.to_owned(),
                local: true,
                kind: VideoKind::Camera,
                sink: capture.preview.clone(),
            });
        }
        // Screens first, then others by person, then us, so the order is
        // stable.
        videos.sort_by(|a, b| {
            (a.kind != VideoKind::Screen, a.local, &a.user_id, &a.id).cmp(&(
                b.kind != VideoKind::Screen,
                b.local,
                &b.user_id,
                &b.id,
            ))
        });
        let _ = updates.send(Update::Videos(videos));
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
        let own_user_id = self.client.user_id().map(ToOwned::to_owned);
        if let Some(user_id) = &own_user_id {
            let speaking = self
                .speakers
                .contains(&self.livekit.local_participant().identity().to_string());
            seen.insert(user_id.clone(), speaking);
        }
        for membership in &self.memberships {
            let speaking = self.is_speaking(&membership.identity);
            let entry = seen.entry(membership.user_id.clone()).or_insert(false);
            *entry |= speaking;
        }
        let mut participants = seen
            .into_iter()
            .map(|(user_id, speaking)| {
                let has = |kind| {
                    self.remote_audio
                        .values()
                        .any(|audio| audio.user_id == user_id && audio.kind == kind)
                };
                let muted = if own_user_id.as_ref() == Some(&user_id) {
                    self.microphone.is_muted()
                } else {
                    !self.remote_audio.values().any(|audio| {
                        audio.user_id == user_id && audio.kind == AudioKind::Voice && !audio.muted
                    })
                };
                Participant {
                    voice: has(AudioKind::Voice),
                    stream: has(AudioKind::Stream),
                    muted,
                    user_id,
                    speaking,
                }
            })
            .collect::<Vec<_>>();
        participants.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        let _ = updates.send(Update::Participants(participants));
    }

    /// Leave the call.
    async fn leave(mut self) {
        if let Some((request, _)) = self.share_request.take() {
            request.abort();
        }
        self.stop_screen_share().await;
        if let Some(request) = self.camera_request.take() {
            request.abort();
        }
        if let Some((capture, _)) = self.camera.take() {
            capture.stop();
        }
        for (_, video) in self.remote_videos.drain() {
            video.task.abort();
        }
        for (_, server) in self.servers.drain() {
            server.task.abort();
            if let Some(livekit) = server.livekit
                && let Err(error) = livekit.close().await
            {
                debug!("Could not close LiveKit connection: {error}");
            }
        }
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

/// Wait for the screen capture the user is choosing.
async fn share_result(
    request: &mut Option<(JoinHandle<Result<ScreenCapture, ScreenError>>, ShareAudio)>,
) -> Result<Result<ScreenCapture, ScreenError>, JoinError> {
    match request {
        Some((handle, _)) => handle.await,
        None => std::future::pending().await,
    }
}

/// Wait for a task to finish.
async fn finished<T>(handle: &mut Option<JoinHandle<T>>) -> Result<T, JoinError> {
    match handle {
        Some(handle) => handle.await,
        None => std::future::pending().await,
    }
}

/// Wait until a capture stops by itself.
async fn capture_ended(ended: &mut Option<oneshot::Receiver<()>>) {
    let Some(receiver) = ended else {
        return std::future::pending().await;
    };
    let result = receiver.await;
    *ended = None;
    if result.is_err() {
        // The capture was dropped without ending by itself.
        std::future::pending::<()>().await;
    }
}

/// Send captured sound to `LiveKit` in 10 ms frames.
async fn pump_audio(source: NativeAudioSource, mut samples_rx: mpsc::UnboundedReceiver<Vec<i16>>) {
    let samples_per_channel = app_audio::SAMPLE_RATE / 100;
    let frame_len = (samples_per_channel * app_audio::CHANNELS) as usize;
    let mut pending = Vec::new();
    while let Some(samples) = samples_rx.recv().await {
        pending.extend_from_slice(&samples);
        let mut start = 0;
        while pending.len() - start >= frame_len {
            let frame = AudioFrame {
                data: Cow::Borrowed(&pending[start..start + frame_len]),
                sample_rate: app_audio::SAMPLE_RATE,
                num_channels: app_audio::CHANNELS,
                samples_per_channel,
            };
            if let Err(error) = source.capture_frame(&frame).await {
                debug!("Could not send screen sound: {error}");
            }
            start += frame_len;
        }
        pending.drain(..start);
    }
}

/// Connect to the server of other participants, only to receive, and forward
/// what happens on the connection.
async fn run_server(connection: ServerConnection) {
    let ServerConnection {
        id,
        client,
        service_url,
        room_id,
        identity,
        device_id,
        key_provider,
        events_tx,
    } = connection;

    let connect = async {
        let sfu =
            rtc::get_subscriber_sfu_config(&client, &service_url, &room_id, &identity, &device_id)
                .await?;
        let mut options = RoomOptions::default();
        options.auto_subscribe = true;
        // The same keys as on our server: participants use one key for all.
        options.encryption = key_provider.clone().map(|key_provider| E2eeOptions {
            encryption_type: EncryptionType::Gcm,
            key_provider,
        });
        let (livekit, events) = Room::connect(&sfu.url, &sfu.jwt, options).await?;
        if key_provider.is_some() {
            livekit.e2ee_manager().set_enabled(true);
        }
        Ok::<_, CallError>((livekit, events))
    };
    let (livekit, mut events) = match connect.await {
        Ok(connection) => connection,
        Err(error) => {
            let _ = events_tx.send((id, ServerEvent::Failed(error)));
            return;
        }
    };
    if events_tx
        .send((id, ServerEvent::Connected(livekit)))
        .is_err()
    {
        return;
    }
    while let Some(event) = events.recv().await {
        if events_tx
            .send((id, ServerEvent::Event(Box::new(event))))
            .is_err()
        {
            return;
        }
    }
    let _ = events_tx.send((id, ServerEvent::Closed));
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
