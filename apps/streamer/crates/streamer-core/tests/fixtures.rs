use serde_json::Value;
use streamer_core::frame::{decode_frame, encode_frame};
use streamer_core::protocol::{
    ClientKind, ClientMessage, RejectReason, RoomEvent, ScheduledAction, ServerMessage,
};

const FRAME_VECTORS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../packages/shared/fixtures/live-frame-vectors.json"
));
const PROTOCOL_MESSAGES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../packages/shared/fixtures/protocol-messages.json"
));

fn hex_decode(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn normalize(v: Value) -> Value {
    match v {
        Value::Number(n) => Value::from(n.as_f64()),
        Value::Array(a) => Value::Array(a.into_iter().map(normalize).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, normalize(v))).collect()),
        other => other,
    }
}

fn protocol() -> Value {
    serde_json::from_str(PROTOCOL_MESSAGES).unwrap()
}

#[test]
fn frame_vectors_decode_and_encode() {
    let vectors: Vec<Value> = serde_json::from_str(FRAME_VECTORS).unwrap();
    assert!(!vectors.is_empty());
    for v in vectors {
        let bytes = hex_decode(v["hex"].as_str().unwrap());
        let payload = hex_decode(v["payloadHex"].as_str().unwrap());
        let (header, got_payload) = decode_frame(&bytes).expect("vector decodes");
        assert_eq!(header.stream_id as u64, v["streamId"].as_u64().unwrap());
        assert_eq!(header.sequence as u64, v["sequence"].as_u64().unwrap());
        assert_eq!(
            header.capture_server_time_ms,
            v["captureServerTimeMs"].as_f64().unwrap()
        );
        assert_eq!(got_payload, payload.as_slice());
        assert_eq!(encode_frame(&header, &payload), bytes);
    }
}

fn build_client_message(name: &str) -> ClientMessage {
    match name {
        "ntpRequestMinimal" => ClientMessage::NtpRequest {
            t0: 1700000000123.5,
            client_rtt: None,
            client_compensation_ms: None,
            client_nudge_ms: None,
            probe_group_id: 7,
            probe_group_index: 0,
        },
        "ntpRequestFull" => ClientMessage::NtpRequest {
            t0: 1700000000148.25,
            client_rtt: Some(12.5),
            client_compensation_ms: Some(0.0),
            client_nudge_ms: Some(0.0),
            probe_group_id: 7,
            probe_group_index: 1,
        },
        "livenessPong" => ClientMessage::LivenessPong,
        "startLiveStream" => ClientMessage::StartLiveStream {
            codec: "opus".into(),
            sample_rate: 48000,
            channels: 2,
            frame_duration_ms: 20,
            delay_ms: 1000,
            label: "Spotify".into(),
        },
        "updateLiveStream" => ClientMessage::UpdateLiveStream {
            label: "Firefox".into(),
        },
        "stopLiveStream" => ClientMessage::StopLiveStream,
        "sync" => ClientMessage::Sync,
        other => panic!("unknown client fixture '{other}': add it to build_client_message"),
    }
}

#[test]
fn client_messages_match_fixtures() {
    let p = protocol();
    let entries = p["clientToServer"].as_array().unwrap();
    assert!(!entries.is_empty());
    for e in entries {
        let name = e["name"].as_str().unwrap();
        let msg = build_client_message(name);
        let got = normalize(serde_json::to_value(msg).unwrap());
        assert_eq!(got, normalize(e["message"].clone()), "fixture {name}");
    }
}

fn server_entries(known: bool) -> Vec<(String, ServerMessage)> {
    let p = protocol();
    p["serverToClient"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["known"].as_bool().unwrap() == known)
        .map(|e| {
            let name = e["name"].as_str().unwrap().to_string();
            let msg: ServerMessage = serde_json::from_value(e["message"].clone())
                .unwrap_or_else(|err| panic!("fixture {name} failed to parse: {err}"));
            (name, msg)
        })
        .collect()
}

fn is_other(m: &ServerMessage) -> bool {
    match m {
        ServerMessage::Other => true,
        ServerMessage::RoomEvent { event } => matches!(event, RoomEvent::Other),
        ServerMessage::ScheduledAction {
            scheduled_action, ..
        } => matches!(scheduled_action, ScheduledAction::Other),
        _ => false,
    }
}

#[test]
fn known_server_messages_parse() {
    let entries = server_entries(true);
    assert!(!entries.is_empty());
    for (name, msg) in &entries {
        assert!(!is_other(msg), "fixture {name} parsed as Other");
    }
    let get = |n: &str| &entries.iter().find(|(name, _)| name == n).unwrap().1;

    match get("liveStreamStateNull") {
        ServerMessage::RoomEvent {
            event: RoomEvent::LiveStreamState { stream, .. },
        } => assert!(stream.is_none()),
        other => panic!("unexpected {other:?}"),
    }
    match get("liveStreamStateStoppedBy") {
        ServerMessage::RoomEvent {
            event:
                RoomEvent::LiveStreamState {
                    stopped_by_client_id,
                    ..
                },
        } => assert_eq!(
            stopped_by_client_id.as_deref(),
            Some("c1a2b3d4-0000-4000-8000-000000000001")
        ),
        other => panic!("unexpected {other:?}"),
    }
    match get("clientChange") {
        ServerMessage::RoomEvent {
            event: RoomEvent::ClientChange { clients },
        } => {
            assert_eq!(clients.len(), 3);
            assert_eq!(clients[0].kind, ClientKind::Web);
            assert_eq!(clients[1].kind, ClientKind::Streamer);
            assert_eq!(clients[2].kind, ClientKind::App);
        }
        other => panic!("unexpected {other:?}"),
    }
    match get("liveStreamRejectedAlreadyActive") {
        ServerMessage::LiveStreamRejected { reason } => {
            assert_eq!(*reason, RejectReason::AlreadyActive)
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn unknown_server_messages_are_ignored() {
    let entries = server_entries(false);
    assert!(!entries.is_empty());
    for (name, msg) in &entries {
        assert!(is_other(msg), "fixture {name} should be Other, got {msg:?}");
    }
}
