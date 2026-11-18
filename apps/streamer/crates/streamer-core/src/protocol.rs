//! Wire messages for the beatsync WebSocket protocol (subset used by the streamer).
//! Source of truth: `packages/shared/types/WS*.ts`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "SCREAMING_SNAKE_CASE",
    rename_all_fields = "camelCase"
)]
pub enum ClientMessage {
    NtpRequest {
        t0: f64,
        #[serde(rename = "clientRTT", skip_serializing_if = "Option::is_none")]
        client_rtt: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_compensation_ms: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_nudge_ms: Option<f64>,
        probe_group_id: u64,
        probe_group_index: u8,
    },
    LivenessPong,
    StartLiveStream {
        codec: String,
        sample_rate: u32,
        channels: u8,
        frame_duration_ms: u32,
        delay_ms: u32,
        label: String,
    },
    UpdateLiveStream {
        label: String,
    },
    StopLiveStream,
    /// Asks for the room's playback state; while a track plays, the server answers this
    /// client with a scheduled PLAY.
    Sync,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "SCREAMING_SNAKE_CASE",
    rename_all_fields = "camelCase"
)]
pub enum ServerMessage {
    NtpResponse {
        t0: f64,
        t1: f64,
        t2: f64,
        probe_group_id: u64,
        probe_group_index: u8,
    },
    LivenessPing,
    RoomEvent {
        event: RoomEvent,
    },
    ScheduledAction {
        server_time_to_execute: f64,
        scheduled_action: ScheduledAction,
    },
    LiveStreamRejected {
        reason: RejectReason,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "SCREAMING_SNAKE_CASE",
    rename_all_fields = "camelCase"
)]
pub enum RoomEvent {
    ClientChange {
        clients: Vec<ClientData>,
    },
    SetPlaybackControls {
        permissions: String,
    },
    LiveStreamState {
        stream: Option<LiveStream>,
        stopped_by_client_id: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "SCREAMING_SNAKE_CASE",
    rename_all_fields = "camelCase"
)]
pub enum ScheduledAction {
    GlobalVolumeConfig {
        volume: f64,
        ramp_time: f64,
    },
    SpatialConfig {
        gains: HashMap<String, SpatialGain>,
    },
    StopSpatialAudio,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpatialGain {
    pub gain: f64,
    pub ramp_time: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientKind {
    #[default]
    Web,
    Streamer,
    App,
    /// A kind added later: it must not break parsing of the client list.
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientData {
    pub username: String,
    pub client_id: String,
    pub is_admin: bool,
    #[serde(default)]
    pub kind: ClientKind,
    /// The nudge the server stores for this client (0 when absent).
    #[serde(default)]
    pub nudge_ms: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveStream {
    pub stream_id: u32,
    pub source_client_id: String,
    pub source_username: String,
    pub label: String,
    pub delay_ms: f64,
    pub channels: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RejectReason {
    NotAllowed,
    AlreadyActive,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_kind_app_and_unknown() {
        let parse = |kind: &str| -> ClientKind {
            let json =
                format!(r#"{{"username":"a","clientId":"1","isAdmin":false,"kind":"{kind}"}}"#);
            serde_json::from_str::<ClientData>(&json).unwrap().kind
        };
        assert_eq!(parse("app"), ClientKind::App);
        assert_eq!(parse("tv"), ClientKind::Other); // a later kind must not break this build
    }
}
