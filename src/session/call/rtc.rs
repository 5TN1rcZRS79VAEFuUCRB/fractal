//! `MatrixRTC` ([MSC4143]) signalling for group calls over `LiveKit`.
//!
//! This speaks the same "compatibility" dialect as Element Call: memberships
//! are `org.matrix.msc3401.call.member` state events, the media backend is a
//! `LiveKit` SFU reached through a JWT service, and per-participant media keys
//! are exchanged as encrypted `io.element.call.encryption_keys` to-device
//! messages.
//!
//! [MSC4143]: https://github.com/matrix-org/matrix-spec-proposals/pull/4143

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use matrix_sdk::{Client, Room as MatrixRoom, deserialized_responses::RawAnySyncOrStrippedState};
use ruma::{
    OwnedDeviceId, OwnedUserId, UserId,
    api::client::{
        account::request_openid_token,
        delayed_events::{
            DelayParameters, delayed_state_event,
            update_delayed_event::{UpdateAction, unstable_v1, unstable_v2},
        },
        rtc::{RtcTransport, transports},
    },
    events::StateEventType,
    serde::Raw,
};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use tracing::{debug, warn};

/// The state event type of call memberships.
pub(super) const CALL_MEMBER_EVENT_TYPE: &str = "org.matrix.msc3401.call.member";
/// The to-device event type of call media keys.
pub(super) const CALL_KEYS_EVENT_TYPE: &str = "io.element.call.encryption_keys";
/// How long a membership is valid if it is never refreshed.
const MEMBERSHIP_EXPIRES: Duration = Duration::from_hours(4);
/// How long the homeserver waits before sending our delayed leave event.
pub(super) const DELAYED_LEAVE_TIMEOUT: Duration = Duration::from_secs(8);
/// How often we restart the delayed leave event.
pub(super) const DELAYED_LEAVE_RESTART: Duration = Duration::from_secs(5);

/// An error during call signalling.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RtcError {
    #[error("the homeserver does not advertise a LiveKit call backend")]
    NoTransport,
    #[error(transparent)]
    Matrix(#[from] matrix_sdk::Error),
    #[error(transparent)]
    Http(#[from] matrix_sdk::HttpError),
    #[error("call backend request failed: {0}")]
    Backend(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// The milliseconds since the Unix epoch, now.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Find the `LiveKit` JWT service URL to use for our own membership.
///
/// Tries the [MSC4143] `rtc/transports` endpoint first, then the
/// `org.matrix.msc4143.rtc_foci` field of the server's `.well-known`.
///
/// [MSC4143]: https://github.com/matrix-org/matrix-spec-proposals/pull/4143
pub(super) async fn discover_livekit_service_url(client: &Client) -> Result<String, RtcError> {
    match client.send(transports::v1::Request::new()).await {
        Ok(response) => {
            if let Some(url) = response.rtc_transports.iter().find_map(livekit_service_url) {
                return Ok(url);
            }
            debug!("No LiveKit transport in rtc/transports, trying .well-known");
        }
        Err(error) => debug!("rtc/transports is not available, trying .well-known: {error}"),
    }

    let server_name = client
        .user_id()
        .map(|user_id| user_id.server_name().to_owned())
        .ok_or(RtcError::NoTransport)?;
    let url = format!("https://{server_name}/.well-known/matrix/client");
    let response = client
        .http_client()
        .get(url)
        .send()
        .await
        .map_err(|error| RtcError::Backend(error.to_string()))?;
    let body = response
        .bytes()
        .await
        .map_err(|error| RtcError::Backend(error.to_string()))?;
    let well_known: JsonValue = serde_json::from_slice(&body)?;

    ["org.matrix.msc4143.rtc_foci", "m.rtc_foci"]
        .iter()
        .filter_map(|key| well_known.get(key)?.as_array())
        .flatten()
        .find_map(|focus| {
            let kind = focus.get("type")?.as_str()?;
            if kind != "livekit" && kind != "m.livekit" {
                return None;
            }
            Some(focus.get("livekit_service_url")?.as_str()?.to_owned())
        })
        .ok_or(RtcError::NoTransport)
}

/// The JWT service URL of the given transport, if it is a `LiveKit` one.
fn livekit_service_url(transport: &RtcTransport) -> Option<String> {
    if let RtcTransport::LiveKit(livekit) = transport {
        return Some(livekit.service_url.clone());
    }
    if !matches!(transport.transport_type(), "livekit" | "m.livekit") {
        return None;
    }
    transport
        .data()
        .get("livekit_service_url")?
        .as_str()
        .map(ToOwned::to_owned)
}

/// The state key of our membership event in the given room.
pub(super) fn membership_state_key(room: &MatrixRoom, user_id: &UserId, device_id: &str) -> String {
    let key = format!("{user_id}_{device_id}_m.call");
    // Rooms versions from MSC3757/MSC3779 allow owned state keys without the
    // leading underscore; every other room version needs it.
    let info = room.clone_info();
    let version = info
        .room_version()
        .map(ruma::RoomVersionId::as_str)
        .unwrap_or_default();
    if version.starts_with("org.matrix.msc3757") || version.starts_with("org.matrix.msc3779") {
        key
    } else {
        format!("_{key}")
    }
}

/// The `LiveKit` participant identity of the given device.
pub(super) fn participant_identity(user_id: &UserId, device_id: &str) -> String {
    format!("{user_id}:{device_id}")
}

/// Our membership event content.
pub(super) fn membership_content(
    identity: &str,
    device_id: &str,
    service_url: &str,
    room_id: &str,
    created_ts: Option<u64>,
) -> JsonValue {
    let mut content = json!({
        "application": "m.call",
        "call_id": "",
        "scope": "m.room",
        "device_id": device_id,
        "membershipID": identity,
        "expires": u64::try_from(MEMBERSHIP_EXPIRES.as_millis()).unwrap_or(u64::MAX),
        "m.call.intent": "audio",
        "focus_active": { "type": "livekit", "focus_selection": "multi_sfu" },
        "foci_preferred": [{
            "type": "livekit",
            "livekit_service_url": service_url,
            "livekit_alias": room_id,
        }],
    });
    if let Some(created_ts) = created_ts {
        content["created_ts"] = created_ts.into();
    }
    content
}

/// Send our membership state event.
pub(super) async fn send_membership(
    room: &MatrixRoom,
    state_key: &str,
    content: JsonValue,
) -> Result<(), RtcError> {
    let raw = Raw::new(&content)?.cast_unchecked();
    room.send_state_event_raw(CALL_MEMBER_EVENT_TYPE, state_key, raw)
        .await?;
    Ok(())
}

/// Schedule an empty membership event with the homeserver, sent if we stop
/// restarting it, so we leave the call even if we crash or lose the network.
///
/// Returns the delay ID, or `None` if the homeserver does not support delayed
/// events ([MSC4140]).
///
/// [MSC4140]: https://github.com/matrix-org/matrix-spec-proposals/pull/4140
pub(super) async fn schedule_delayed_leave(room: &MatrixRoom, state_key: &str) -> Option<String> {
    let body = match Raw::new(&json!({})) {
        Ok(body) => body.cast_unchecked(),
        Err(error) => {
            warn!("Could not serialize delayed leave: {error}");
            return None;
        }
    };
    let request = delayed_state_event::unstable::Request::new_raw(
        room.room_id().to_owned(),
        state_key.to_owned(),
        StateEventType::from(CALL_MEMBER_EVENT_TYPE),
        DelayParameters::Timeout {
            timeout: DELAYED_LEAVE_TIMEOUT,
        },
        body,
    );
    match room.client().send(request).await {
        Ok(response) => Some(response.delay_id),
        Err(error) => {
            warn!("Homeserver did not accept a delayed leave event: {error}");
            None
        }
    }
}

/// Apply an action to a delayed event.
pub(super) async fn update_delayed_event(
    client: &Client,
    delay_id: &str,
    action: UpdateAction,
) -> Result<(), RtcError> {
    let v1 = unstable_v1::Request::new(delay_id.to_owned(), action.clone());
    match client.send(v1).await {
        Ok(_) => Ok(()),
        Err(error) => {
            debug!("Delayed event update failed, trying the newer endpoint: {error}");
            client
                .send(unstable_v2::Request::new(delay_id.to_owned(), action))
                .await?;
            Ok(())
        }
    }
}

/// The `LiveKit` server URL and access token for a call.
#[derive(Deserialize)]
pub(super) struct SfuConfig {
    pub url: String,
    pub jwt: String,
}

/// Get a `LiveKit` access token from the JWT service, as a publisher.
///
/// This uses the legacy `/sfu/get` endpoint, which gives us the
/// `{user_id}:{device_id}` participant identity that other clients match our
/// membership against.
pub(super) async fn get_sfu_config(
    client: &Client,
    service_url: &str,
    room_id: &str,
    device_id: &str,
) -> Result<SfuConfig, RtcError> {
    let user_id = client.user_id().ok_or(RtcError::NoTransport)?.to_owned();
    let openid = client
        .send(request_openid_token::v3::Request::new(user_id))
        .await?;

    let body = json!({
        "room": room_id,
        "device_id": device_id,
        "openid_token": {
            "access_token": openid.access_token,
            "token_type": "Bearer",
            "matrix_server_name": openid.matrix_server_name,
            "expires_in": openid.expires_in.as_secs(),
        },
    });

    let url = format!("{}/sfu/get", service_url.trim_end_matches('/'));
    let response = client
        .http_client()
        .post(url)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .map_err(|error| RtcError::Backend(error.to_string()))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| RtcError::Backend(error.to_string()))?;
    if !status.is_success() {
        return Err(RtcError::Backend(format!(
            "{status}: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }

    Ok(serde_json::from_slice(&bytes)?)
}

/// An active call membership of another device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct Membership {
    pub user_id: OwnedUserId,
    pub device_id: OwnedDeviceId,
    /// The `LiveKit` participant identity of this membership.
    pub identity: String,
}

/// The active room call memberships in the given room.
pub(super) async fn active_memberships(room: &MatrixRoom) -> Vec<Membership> {
    #[derive(Deserialize)]
    struct StateEvent {
        sender: OwnedUserId,
        origin_server_ts: u64,
        content: Content,
    }
    #[derive(Deserialize)]
    struct Content {
        application: Option<String>,
        call_id: Option<String>,
        scope: Option<String>,
        device_id: Option<OwnedDeviceId>,
        expires: Option<u64>,
        created_ts: Option<u64>,
        #[serde(rename = "membershipID")]
        membership_id: Option<String>,
    }

    let events = match room
        .get_state_events(StateEventType::from(CALL_MEMBER_EVENT_TYPE))
        .await
    {
        Ok(events) => events,
        Err(error) => {
            warn!("Could not load call memberships: {error}");
            return Vec::new();
        }
    };

    let now = now_ms();
    events
        .into_iter()
        .filter_map(|event| {
            let RawAnySyncOrStrippedState::Sync(raw) = event else {
                return None;
            };
            let event = raw.deserialize_as_unchecked::<StateEvent>().ok()?;
            let content = event.content;
            if content.application.as_deref() != Some("m.call")
                || content.call_id.as_deref().is_some_and(|id| !id.is_empty())
                || content
                    .scope
                    .as_deref()
                    .is_some_and(|scope| scope != "m.room")
            {
                return None;
            }
            let device_id = content.device_id?;
            let expires = content
                .expires
                .unwrap_or(u64::try_from(MEMBERSHIP_EXPIRES.as_millis()).unwrap_or(u64::MAX));
            let joined = content
                .created_ts
                .unwrap_or(event.origin_server_ts)
                .min(event.origin_server_ts);
            if joined.saturating_add(expires) < now {
                return None;
            }
            let identity = content
                .membership_id
                .unwrap_or_else(|| participant_identity(&event.sender, device_id.as_str()));
            Some(Membership {
                user_id: event.sender,
                device_id,
                identity,
            })
        })
        .collect()
}

/// A media key of a call participant.
#[derive(Debug, Clone)]
pub(super) struct MediaKey {
    pub user_id: OwnedUserId,
    pub device_id: OwnedDeviceId,
    /// The `LiveKit` participant identity of the key's owner.
    pub identity: String,
    pub index: u8,
    pub key: Vec<u8>,
}

/// The content of a to-device message sharing our media key.
pub(super) fn media_key_content(
    room_id: &str,
    identity: &str,
    device_id: &str,
    index: u8,
    key: &[u8],
) -> JsonValue {
    let media_key = json!({ "index": index, "key": BASE64.encode(key) });
    json!({
        "room_id": room_id,
        "member_id": identity,
        "media_key": media_key,
        "keys": media_key,
        "member": {
            "claimed_device_id": device_id,
            "id": identity,
        },
    })
}

/// Parse a media key to-device message from `sender` for the given room.
pub(super) fn parse_media_key(
    sender: OwnedUserId,
    room_id: &str,
    content: &JsonValue,
) -> Option<MediaKey> {
    if content.get("room_id")?.as_str()? != room_id {
        return None;
    }
    let media_key = content
        .get("media_key")
        .filter(|key| key.is_object())
        .or_else(|| content.get("keys"))?;
    let index = u8::try_from(media_key.get("index")?.as_u64()?).ok()?;
    let encoded = media_key.get("key")?.as_str()?;
    let key = BASE64
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded))
        .ok()?;
    let member = content.get("member");
    let device_id: OwnedDeviceId = member?.get("claimed_device_id")?.as_str()?.into();
    let identity = content
        .get("member_id")
        .or_else(|| member?.get("id"))
        .and_then(JsonValue::as_str)
        .map_or_else(
            || participant_identity(&sender, device_id.as_str()),
            ToOwned::to_owned,
        );
    Some(MediaKey {
        user_id: sender,
        device_id,
        identity,
        index,
        key,
    })
}
