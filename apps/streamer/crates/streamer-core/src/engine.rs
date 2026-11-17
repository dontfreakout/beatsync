//! Pure streamer state machine (spec §9.5). The runtime feeds it inputs from the WebSocket
//! shell, the audio worker, the UI and a ~10 ms tick, and executes the outputs.
//! No I/O, no clocks, no threads: time comes only from [`Input::Tick`].

use serde::Serialize;

use crate::frame::{FRAME_DURATION_MS, FrameHeader, MAX_LABEL_LEN, SAMPLE_RATE, encode_frame};
use crate::ntp::{
    INITIAL_INTERVAL_MS, NtpEstimator, NtpMeasurement, ProbePairValidator, STEADY_INTERVAL_MS,
};
use crate::protocol::{
    ClientData, ClientMessage, LiveStream, RejectReason, RoomEvent, ScheduledAction, ServerMessage,
};

const CODEC: &str = "opus";
/// The app always streams stereo (spec §6).
const CHANNELS: u8 = 2;
/// Ramp of STOP_SPATIAL_AUDIO, the web client's `applyFinalGain` default.
const STOP_SPATIAL_RAMP_S: f32 = 0.1;
/// How long START waits for the echo of our STOP; the server sends none when it has no stream.
const STOP_ECHO_TIMEOUT_MS: f64 = 2000.0;

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// A new WebSocket connection is open.
    Connected,
    /// The WebSocket closed; routing and the local copy keep running.
    Disconnected,
    Server(ServerMessage),
    /// One NTP response; `t3` is the client receive time.
    NtpResponse {
        t0: f64,
        t1: f64,
        t2: f64,
        t3: f64,
        group_id: u64,
        index: u8,
    },
    /// The runtime clock, about every 10 ms.
    Tick {
        now_ms: f64,
    },
    UserStart {
        apps: Vec<String>,
        delay_ms: u32,
    },
    UserUpdateApps {
        apps: Vec<String>,
    },
    UserStop,
    AudioFailed {
        message: String,
    },
    /// One 20 ms Opus packet from the audio worker.
    EncodedBlock {
        capture_server_ms: f64,
        payload: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    SendJson(ClientMessage),
    SendBinary(Vec<u8>),
    /// Two NTP_REQUESTs 25 ms apart; `client_rtt` goes into both as `clientRTT`.
    SendProbePair {
        group_id: u64,
        client_rtt: Option<f64>,
    },
    /// Route the apps into the sink, start capture and the local copy.
    StartAudio {
        apps: Vec<String>,
        delay_ms: u32,
    },
    UpdateAudioApps {
        apps: Vec<String>,
    },
    /// Stop capture and the local copy and restore the routing.
    StopAudio,
    ServerOffset {
        offset_ms: f64,
    },
    SetPlayoutGain {
        gain: f32,
        ramp_s: f32,
    },
    Status(Status),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub connected: bool,
    pub synced: bool,
    pub rtt_ms: Option<f64>,
    pub listeners: usize,
    pub phase: Phase,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Phase {
    Idle,
    /// Audio runs; waiting for sync, START or the server's stream state.
    Starting,
    Streaming {
        stream_id: u32,
    },
}

#[derive(Debug)]
pub struct Engine {
    client_id: String,
    username: String,
    connected: bool,
    validator: ProbePairValidator,
    estimator: NtpEstimator,
    next_group_id: u64,
    /// When the next probe pair is due; `None`: on the next tick.
    next_probe_ms: Option<f64>,
    last_offset_ms: Option<f64>,
    phase: Phase,
    /// The selected apps; every START and UPDATE label is built from them.
    apps: Vec<String>,
    delay_ms: u32,
    /// START was sent on the current connection, so the server has or is creating our
    /// stream (it handles the messages of one socket in order). False whenever `Idle`.
    start_sent: bool,
    /// Until when START waits for the echo of our STOP (a null or foreign stream state), so
    /// that the echo cannot end the next session.
    stop_pending_until_ms: Option<f64>,
    /// The time of the last tick.
    now_ms: f64,
    sequence: u32,
    clients: Vec<ClientData>,
    message: Option<String>,
    volume: f64,
    spatial_gain: f64,
    last_status: Option<Status>,
}

impl Engine {
    pub fn new(client_id: String, username: String) -> Self {
        Self {
            client_id,
            username,
            connected: false,
            validator: ProbePairValidator::default(),
            estimator: NtpEstimator::default(),
            next_group_id: 0,
            next_probe_ms: None,
            last_offset_ms: None,
            phase: Phase::Idle,
            apps: Vec::new(),
            delay_ms: 0,
            start_sent: false,
            stop_pending_until_ms: None,
            now_ms: 0.0,
            sequence: 0,
            clients: Vec::new(),
            message: None,
            volume: 1.0,
            spatial_gain: 1.0,
            last_status: None,
        }
    }

    pub fn handle(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::Connected => {
                self.reset_connection();
                self.connected = true;
            }
            Input::Disconnected => {
                self.reset_connection();
                self.connected = false;
            }
            Input::Server(msg) => self.on_server(msg, &mut out),
            Input::NtpResponse {
                t0,
                t1,
                t2,
                t3,
                group_id,
                index,
            } => self.on_ntp_response(
                NtpMeasurement::new(t0, t1, t2, t3),
                group_id,
                index,
                &mut out,
            ),
            Input::Tick { now_ms } => self.on_tick(now_ms, &mut out),
            Input::UserStart { apps, delay_ms } => {
                if self.phase == Phase::Idle {
                    self.apps = apps.clone();
                    self.delay_ms = delay_ms;
                    self.message = None;
                    self.phase = Phase::Starting;
                    out.push(Output::StartAudio { apps, delay_ms });
                }
            }
            Input::UserUpdateApps { apps } => {
                self.apps = apps.clone();
                if self.phase != Phase::Idle {
                    out.push(Output::UpdateAudioApps { apps });
                    if self.start_sent {
                        let label = self.label();
                        out.push(Output::SendJson(ClientMessage::UpdateLiveStream { label }));
                    }
                }
            }
            Input::UserStop => self.stop(&mut out),
            Input::AudioFailed { message } => {
                self.stop(&mut out);
                self.message = Some(message);
            }
            Input::EncodedBlock {
                capture_server_ms,
                payload,
            } => {
                if let Phase::Streaming { stream_id } = self.phase {
                    let header = FrameHeader {
                        stream_id,
                        sequence: self.sequence,
                        capture_server_time_ms: capture_server_ms,
                    };
                    out.push(Output::SendBinary(encode_frame(&header, &payload)));
                    self.sequence = self.sequence.wrapping_add(1);
                }
            }
        }
        self.send_start_when_ready(&mut out);
        self.emit_status(&mut out);
        out
    }

    /// A new or closed connection: NTP restarts and START must be sent again. The audio
    /// keeps running, so a stream in progress goes back to `Starting`.
    fn reset_connection(&mut self) {
        self.validator = ProbePairValidator::default();
        self.estimator.reset();
        self.next_probe_ms = None;
        self.start_sent = false;
        // The old stream ended with the old socket.
        self.stop_pending_until_ms = None;
        if let Phase::Streaming { .. } = self.phase {
            self.phase = Phase::Starting;
        }
    }

    fn on_server(&mut self, msg: ServerMessage, out: &mut Vec<Output>) {
        match msg {
            ServerMessage::LivenessPing => out.push(Output::SendJson(ClientMessage::LivenessPong)),
            ServerMessage::RoomEvent { event } => match event {
                RoomEvent::ClientChange { clients } => self.clients = clients,
                RoomEvent::LiveStreamState {
                    stream,
                    stopped_by_client_id,
                } => self.on_stream_state(stream, stopped_by_client_id, out),
                RoomEvent::SetPlaybackControls { .. } | RoomEvent::Other => {}
            },
            ServerMessage::ScheduledAction {
                scheduled_action, ..
            } => self.on_scheduled_action(scheduled_action, out),
            ServerMessage::LiveStreamRejected { reason } => self.on_rejected(reason, out),
            // NTP responses come as `Input::NtpResponse` with the receive time.
            ServerMessage::NtpResponse { .. } | ServerMessage::Other => {}
        }
    }

    fn on_ntp_response(
        &mut self,
        m: NtpMeasurement,
        group_id: u64,
        index: u8,
        out: &mut Vec<Output>,
    ) {
        if !self.connected {
            return;
        }
        let Some(best) = self.validator.on_response(m, group_id, index) else {
            return;
        };
        self.estimator.push(best);
        if let Some(offset_ms) = self.estimator.offset_ms()
            && self.last_offset_ms != Some(offset_ms)
        {
            self.last_offset_ms = Some(offset_ms);
            out.push(Output::ServerOffset { offset_ms });
        }
    }

    fn on_tick(&mut self, now_ms: f64, out: &mut Vec<Output>) {
        self.now_ms = now_ms;
        if self
            .stop_pending_until_ms
            .is_some_and(|until| now_ms >= until)
        {
            self.stop_pending_until_ms = None;
        }
        if !self.connected || self.next_probe_ms.is_some_and(|due| now_ms < due) {
            return;
        }
        out.push(Output::SendProbePair {
            group_id: self.next_group_id,
            client_rtt: self.estimator.avg_rtt_ms(),
        });
        self.next_group_id += 1;
        let interval_ms = if self.estimator.is_synced() {
            STEADY_INTERVAL_MS
        } else {
            INITIAL_INTERVAL_MS
        };
        self.next_probe_ms = Some(now_ms + interval_ms as f64);
    }

    /// Before START on this connection (e.g. the join handler's state after a reconnect) and
    /// while idle, states are stale and ignored.
    fn on_stream_state(
        &mut self,
        stream: Option<LiveStream>,
        stopped_by: Option<String>,
        out: &mut Vec<Output>,
    ) {
        let own_stream_id = stream
            .as_ref()
            .filter(|s| s.source_client_id == self.client_id)
            .map(|s| s.stream_id);
        if own_stream_id.is_none() {
            // The echo of our last STOP: START may go out again.
            self.stop_pending_until_ms = None;
        }
        if !self.start_sent {
            return;
        }
        if let Some(stream_id) = own_stream_id {
            let phase = Phase::Streaming { stream_id };
            // The same stream again (a label update) keeps the sequence.
            if self.phase != phase {
                self.phase = phase;
                self.sequence = 0;
            }
            return;
        }
        out.push(Output::StopAudio);
        self.go_idle();
        self.message = match stream {
            // Another source won the race; its REJECTED reply to our START comes later.
            Some(_) => Some("Another stream is active".to_string()),
            None => stopped_by.map(|id| format!("Stopped by {}", self.username_of(&id))),
        };
    }

    fn on_rejected(&mut self, reason: RejectReason, out: &mut Vec<Output>) {
        // A rejection answers a START on this connection; anything else is stale.
        if !self.start_sent {
            return;
        }
        out.push(Output::StopAudio);
        self.go_idle();
        self.message = Some(match reason {
            RejectReason::NotAllowed => {
                format!(
                    "Ask an admin to promote {}",
                    self.username_of(&self.client_id)
                )
            }
            RejectReason::AlreadyActive => "Another stream is active".to_string(),
        });
    }

    /// Applied on receipt, like the web client.
    fn on_scheduled_action(&mut self, action: ScheduledAction, out: &mut Vec<Output>) {
        let ramp_s = match action {
            ScheduledAction::GlobalVolumeConfig { volume, ramp_time } => {
                self.volume = volume;
                ramp_time as f32
            }
            ScheduledAction::SpatialConfig { gains } => match gains.get(&self.client_id) {
                Some(own) => {
                    self.spatial_gain = own.gain;
                    own.ramp_time as f32
                }
                None => return,
            },
            ScheduledAction::StopSpatialAudio => {
                self.spatial_gain = 1.0;
                STOP_SPATIAL_RAMP_S
            }
            ScheduledAction::Other => return,
        };
        let gain = (self.volume * self.spatial_gain) as f32;
        out.push(Output::SetPlayoutGain { gain, ramp_s });
    }

    fn stop(&mut self, out: &mut Vec<Output>) {
        if self.phase == Phase::Idle {
            return;
        }
        if self.start_sent {
            out.push(Output::SendJson(ClientMessage::StopLiveStream));
            self.stop_pending_until_ms = Some(self.now_ms + STOP_ECHO_TIMEOUT_MS);
        }
        out.push(Output::StopAudio);
        self.go_idle();
    }

    fn go_idle(&mut self) {
        self.phase = Phase::Idle;
        self.start_sent = false;
    }

    fn send_start_when_ready(&mut self, out: &mut Vec<Output>) {
        if self.phase != Phase::Starting
            || self.start_sent
            || !self.connected
            || !self.estimator.is_synced()
            || self.stop_pending_until_ms.is_some()
        {
            return;
        }
        self.start_sent = true;
        out.push(Output::SendJson(ClientMessage::StartLiveStream {
            codec: CODEC.to_string(),
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            frame_duration_ms: FRAME_DURATION_MS as u32,
            delay_ms: self.delay_ms,
            label: self.label(),
        }));
    }

    fn emit_status(&mut self, out: &mut Vec<Output>) {
        let status = Status {
            connected: self.connected,
            synced: self.estimator.is_synced(),
            rtt_ms: self.estimator.avg_rtt_ms(),
            listeners: self
                .clients
                .iter()
                .filter(|c| c.client_id != self.client_id)
                .count(),
            phase: self.phase,
            message: self.message.clone(),
        };
        if self.last_status.as_ref() != Some(&status) {
            self.last_status = Some(status.clone());
            out.push(Output::Status(status));
        }
    }

    /// The username from the last CLIENT_CHANGE; fallback the configured name for this
    /// client, else the id.
    fn username_of<'a>(&'a self, client_id: &'a str) -> &'a str {
        match self.clients.iter().find(|c| c.client_id == client_id) {
            Some(c) => &c.username,
            None if client_id == self.client_id => &self.username,
            None => client_id,
        }
    }

    fn label(&self) -> String {
        cut_label(self.apps.join(", "))
    }
}

/// Cuts to at most `MAX_LABEL_LEN` UTF-16 code units (Zod's `.max()` counts JS string
/// length) at a char boundary.
fn cut_label(mut label: String) -> String {
    let mut units = 0;
    let end = label
        .char_indices()
        .find(|&(_, c)| {
            units += c.len_utf16();
            units > MAX_LABEL_LEN
        })
        .map_or(label.len(), |(i, _)| i);
    label.truncate(end);
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FrameHeader, decode_frame};
    use crate::protocol::{
        ClientData, ClientKind, LiveStream, RejectReason, RoomEvent, ScheduledAction, SpatialGain,
    };

    const ME: &str = "me-id";
    const PAYLOAD: [u8; 3] = [0xAB, 0xCD, 0xEF];

    /// Drives an engine with a test clock and remembers the last emitted `Status`.
    struct Harness {
        engine: Engine,
        now_ms: f64,
        status: Option<Status>,
    }

    impl Harness {
        /// Engine for client `ME` named "alice", not connected.
        fn new() -> Self {
            Self {
                engine: Engine::new(ME.into(), "alice".into()),
                now_ms: 1_000_000.0,
                status: None,
            }
        }

        fn connected() -> Self {
            let mut h = Self::new();
            h.feed(Input::Connected);
            h
        }

        /// Connected, synced, START sent and own stream `stream_id` active.
        fn streaming(stream_id: u32) -> Self {
            let mut h = Self::connected();
            h.sync();
            h.feed(user_start(&["Spotify"]));
            h.server(own_state(stream_id));
            assert_eq!(h.phase(), Phase::Streaming { stream_id });
            h
        }

        fn feed(&mut self, input: Input) -> Vec<Output> {
            let out = self.engine.handle(input);
            if let Some(&s) = statuses(&out).last() {
                self.status = Some(s.clone());
            }
            out
        }

        fn server(&mut self, msg: ServerMessage) -> Vec<Output> {
            self.feed(Input::Server(msg))
        }

        fn tick(&mut self, now_ms: f64) -> Vec<Output> {
            self.now_ms = now_ms;
            self.feed(Input::Tick { now_ms })
        }

        /// Answers probe pair `group_id` with a pure pair of the given rtt and offset.
        fn answer(&mut self, group_id: u64, rtt: f64, offset: f64) -> Vec<Output> {
            let mut out = Vec::new();
            for index in 0..2u8 {
                let t0 = self.now_ms + 25.0 * f64::from(index);
                let t1 = t0 + rtt / 2.0 + offset;
                out.extend(self.feed(Input::NtpResponse {
                    t0,
                    t1,
                    t2: t1,
                    t3: t0 + rtt,
                    group_id,
                    index,
                }));
            }
            out
        }

        /// `n` ticks 50 ms apart, each answered with an accepted pair (rtt 20, offset 500).
        fn probe_pairs(&mut self, n: usize) -> Vec<Output> {
            let mut out = Vec::new();
            for _ in 0..n {
                let tick = self.tick(self.now_ms + 50.0);
                let [(group_id, _)] = probes(&tick)[..] else {
                    panic!("one probe pair is due: {tick:?}");
                };
                out.extend(tick);
                out.extend(self.answer(group_id, 20.0, 500.0));
            }
            out
        }

        fn sync(&mut self) -> Vec<Output> {
            self.probe_pairs(16)
        }

        fn status(&self) -> &Status {
            self.status.as_ref().expect("a Status was emitted")
        }

        fn phase(&self) -> Phase {
            self.status().phase
        }

        fn message(&self) -> Option<&str> {
            self.status().message.as_deref()
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn user_start(apps: &[&str]) -> Input {
        Input::UserStart {
            apps: strings(apps),
            delay_ms: 1000,
        }
    }

    fn block(capture_server_ms: f64) -> Input {
        Input::EncodedBlock {
            capture_server_ms,
            payload: PAYLOAD.to_vec(),
        }
    }

    fn live_stream(stream_id: u32, source: &str) -> LiveStream {
        LiveStream {
            stream_id,
            source_client_id: source.into(),
            source_username: "someone".into(),
            label: "Spotify".into(),
            delay_ms: 1000.0,
            channels: 2,
        }
    }

    fn state(stream: Option<LiveStream>, stopped_by: Option<&str>) -> ServerMessage {
        ServerMessage::RoomEvent {
            event: RoomEvent::LiveStreamState {
                stream,
                stopped_by_client_id: stopped_by.map(String::from),
            },
        }
    }

    fn own_state(stream_id: u32) -> ServerMessage {
        state(Some(live_stream(stream_id, ME)), None)
    }

    fn client_change(clients: &[(&str, &str)]) -> ServerMessage {
        let clients = clients
            .iter()
            .map(|(id, name)| ClientData {
                username: name.to_string(),
                client_id: id.to_string(),
                is_admin: false,
                kind: ClientKind::Web,
            })
            .collect();
        ServerMessage::RoomEvent {
            event: RoomEvent::ClientChange { clients },
        }
    }

    fn scheduled(action: ScheduledAction) -> ServerMessage {
        ServerMessage::ScheduledAction {
            server_time_to_execute: 0.0,
            scheduled_action: action,
        }
    }

    fn volume(volume: f64, ramp_time: f64) -> ServerMessage {
        scheduled(ScheduledAction::GlobalVolumeConfig { volume, ramp_time })
    }

    fn spatial(gains: &[(&str, f64, f64)]) -> ServerMessage {
        let gains = gains
            .iter()
            .map(|&(id, gain, ramp_time)| (id.to_string(), SpatialGain { gain, ramp_time }))
            .collect();
        scheduled(ScheduledAction::SpatialConfig { gains })
    }

    fn gain(gain: f32, ramp_s: f32) -> Output {
        Output::SetPlayoutGain { gain, ramp_s }
    }

    fn statuses(out: &[Output]) -> Vec<&Status> {
        out.iter()
            .filter_map(|o| match o {
                Output::Status(s) => Some(s),
                _ => None,
            })
            .collect()
    }

    fn without_status(out: Vec<Output>) -> Vec<Output> {
        out.into_iter()
            .filter(|o| !matches!(o, Output::Status(_)))
            .collect()
    }

    fn probes(out: &[Output]) -> Vec<(u64, Option<f64>)> {
        out.iter()
            .filter_map(|o| match o {
                Output::SendProbePair {
                    group_id,
                    client_rtt,
                } => Some((*group_id, *client_rtt)),
                _ => None,
            })
            .collect()
    }

    fn offsets(out: &[Output]) -> Vec<f64> {
        out.iter()
            .filter_map(|o| match o {
                Output::ServerOffset { offset_ms } => Some(*offset_ms),
                _ => None,
            })
            .collect()
    }

    fn starts(out: &[Output]) -> Vec<&ClientMessage> {
        out.iter()
            .filter_map(|o| match o {
                Output::SendJson(m @ ClientMessage::StartLiveStream { .. }) => Some(m),
                _ => None,
            })
            .collect()
    }

    fn start_label(out: &[Output]) -> String {
        match starts(out)[..] {
            [ClientMessage::StartLiveStream { label, .. }] => label.clone(),
            _ => panic!("one START expected: {out:?}"),
        }
    }

    fn frames(out: &[Output]) -> Vec<(FrameHeader, Vec<u8>)> {
        out.iter()
            .filter_map(|o| match o {
                Output::SendBinary(bytes) => {
                    let (h, payload) = decode_frame(bytes).expect("valid frame");
                    Some((h, payload.to_vec()))
                }
                _ => None,
            })
            .collect()
    }

    fn frame(stream_id: u32, sequence: u32, capture_server_time_ms: f64) -> (FrameHeader, Vec<u8>) {
        let header = FrameHeader {
            stream_id,
            sequence,
            capture_server_time_ms,
        };
        (header, PAYLOAD.to_vec())
    }

    #[test]
    fn start_waits_for_sync() {
        let mut h = Harness::connected();
        let out = h.feed(Input::UserStart {
            apps: strings(&["Spotify"]),
            delay_ms: 1500,
        });
        let start_audio = Output::StartAudio {
            apps: strings(&["Spotify"]),
            delay_ms: 1500,
        };
        assert_eq!(without_status(out), vec![start_audio]);
        assert_eq!(h.phase(), Phase::Starting);

        assert!(starts(&h.probe_pairs(15)).is_empty());
        let out = h.probe_pairs(1);
        let start = ClientMessage::StartLiveStream {
            codec: "opus".into(),
            sample_rate: 48_000,
            channels: 2,
            frame_duration_ms: 20,
            delay_ms: 1500,
            label: "Spotify".into(),
        };
        assert_eq!(starts(&out), vec![&start]);
        // START goes out once per connection.
        let mut out = h.tick(h.now_ms + 2500.0);
        out.extend(h.answer(16, 20.0, 500.0));
        assert!(starts(&out).is_empty());
        assert_eq!(h.phase(), Phase::Starting);
    }

    #[test]
    fn probe_pairs_follow_the_sync_schedule() {
        let mut h = Harness::new();
        assert!(
            probes(&h.tick(0.0)).is_empty(),
            "no probes while disconnected"
        );
        h.feed(Input::Connected);
        // The first pair goes out on the first tick, then every 50 ms until synced.
        let out = h.sync();
        let ids: Vec<u64> = probes(&out).iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, (0..16).collect::<Vec<u64>>());
        assert_eq!(probes(&out)[0].1, None);
        assert_eq!(probes(&out)[1].1, Some(20.0));
        assert!(h.status().synced);
        assert_eq!(h.status().rtt_ms, Some(20.0));
        // The offset is reported once while it stays the same.
        assert_eq!(offsets(&out), vec![500.0]);

        let t = h.now_ms;
        assert!(probes(&h.tick(t + 49.0)).is_empty());
        assert_eq!(probes(&h.tick(t + 50.0)), vec![(16, Some(20.0))]);
        // A lower-RTT measurement moves the offset.
        assert_eq!(offsets(&h.answer(16, 10.0, 480.0)), vec![480.0]);
        // Synced: every 2500 ms.
        assert!(probes(&h.tick(t + 2549.0)).is_empty());
        assert_eq!(probes(&h.tick(t + 2550.0)), vec![(17, Some(19.375))]);

        h.feed(Input::Disconnected);
        assert!(!h.status().synced);
        assert!(probes(&h.tick(t + 5100.0)).is_empty());
        h.feed(Input::Connected);
        // Group ids keep increasing, so a late response of an old pair cannot match.
        assert_eq!(probes(&h.tick(t + 5101.0)), vec![(18, None)]);
    }

    #[test]
    fn frames_only_after_own_state() {
        let mut h = Harness::connected();
        h.sync();
        h.feed(user_start(&["Spotify"]));
        assert!(frames(&h.feed(block(10.0))).is_empty());

        h.server(own_state(4));
        assert_eq!(h.phase(), Phase::Streaming { stream_id: 4 });
        assert_eq!(frames(&h.feed(block(30.0))), vec![frame(4, 0, 30.0)]);
        assert_eq!(frames(&h.feed(block(50.0))), vec![frame(4, 1, 50.0)]);
        // The same stream again (e.g. after a label update) keeps the sequence.
        h.server(own_state(4));
        assert_eq!(frames(&h.feed(block(70.0))), vec![frame(4, 2, 70.0)]);
    }

    #[test]
    fn rejection_not_allowed_stops_audio_with_message() {
        let mut h = Harness::connected();
        h.server(client_change(&[(ME, "Alice"), ("web-1", "Bob")]));
        h.sync();
        h.feed(user_start(&["Spotify"]));
        let out = h.server(ServerMessage::LiveStreamRejected {
            reason: RejectReason::NotAllowed,
        });
        assert_eq!(without_status(out), vec![Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
        // The own name from CLIENT_CHANGE, not the configured one.
        assert_eq!(h.message(), Some("Ask an admin to promote Alice"));

        h.feed(user_start(&["Spotify"]));
        assert_eq!(h.message(), None, "a new start clears the message");
    }

    #[test]
    fn rejection_already_active_message() {
        let mut h = Harness::connected();
        h.sync();
        h.feed(user_start(&["Spotify"]));
        let out = h.server(ServerMessage::LiveStreamRejected {
            reason: RejectReason::AlreadyActive,
        });
        assert_eq!(without_status(out), vec![Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
        assert_eq!(h.message(), Some("Another stream is active"));
    }

    #[test]
    fn stopped_by_web_user_restores_routing() {
        let mut h = Harness::streaming(3);
        h.server(client_change(&[(ME, "alice"), ("web-1", "Bob")]));
        let out = h.server(state(None, Some("web-1")));
        assert_eq!(without_status(out), vec![Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
        assert_eq!(h.message(), Some("Stopped by Bob"));
        assert!(frames(&h.feed(block(0.0))).is_empty());

        // Unknown client: the id.
        let mut h = Harness::streaming(3);
        h.server(state(None, Some("ghost-id")));
        assert_eq!(h.message(), Some("Stopped by ghost-id"));

        // Ended without stoppedBy (e.g. by the server): stop without a message.
        let mut h = Harness::streaming(3);
        let out = h.server(state(None, None));
        assert_eq!(without_status(out), vec![Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
        assert_eq!(h.message(), None);
    }

    #[test]
    fn competing_stream_after_start_is_named() {
        let mut h = Harness::connected();
        h.sync();
        h.feed(user_start(&["Spotify"]));
        // Another source's stream wins the race before the server answers our START.
        let out = h.server(state(Some(live_stream(5, "web-1")), None));
        assert_eq!(without_status(out), vec![Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
        assert_eq!(h.message(), Some("Another stream is active"));
        // The REJECTED reply that follows is stale.
        let out = h.server(ServerMessage::LiveStreamRejected {
            reason: RejectReason::AlreadyActive,
        });
        assert!(without_status(out).is_empty());
        assert_eq!(h.message(), Some("Another stream is active"));
    }

    #[test]
    fn stop_then_start_waits_for_the_stop_echo() {
        let mut h = Harness::streaming(3);
        h.feed(Input::UserStop);
        let out = h.feed(user_start(&["Spotify"]));
        let start_audio = Output::StartAudio {
            apps: strings(&["Spotify"]),
            delay_ms: 1000,
        };
        assert_eq!(
            without_status(out),
            vec![start_audio],
            "audio starts at once"
        );
        assert!(starts(&h.tick(h.now_ms + 10.0)).is_empty());

        // The STOP's echo ends the old stream only: the new session goes on and sends START.
        let out = h.server(state(None, None));
        assert!(!out.contains(&Output::StopAudio));
        assert_eq!(start_label(&out), "Spotify");
        assert_eq!(h.phase(), Phase::Starting);
        h.server(own_state(4));
        assert_eq!(h.phase(), Phase::Streaming { stream_id: 4 });
        assert_eq!(frames(&h.feed(block(0.0))), vec![frame(4, 0, 0.0)]);
    }

    #[test]
    fn stop_without_echo_holds_start_for_2000_ms() {
        // START sent but no stream yet; the server sends nothing for a STOP without a stream.
        let mut h = Harness::connected();
        h.sync();
        h.feed(user_start(&["Spotify"]));
        let t = h.now_ms;
        h.feed(Input::UserStop);
        h.feed(user_start(&["Spotify"]));
        assert!(starts(&h.tick(t + 1999.0)).is_empty());
        assert_eq!(start_label(&h.tick(t + 2000.0)), "Spotify");

        // A reconnect ends the wait: the old stream ended with the old socket.
        let mut h = Harness::streaming(3);
        h.feed(Input::UserStop);
        h.feed(Input::Disconnected);
        h.feed(Input::Connected);
        h.feed(user_start(&["Spotify"]));
        // The resync takes 800 ms, less than the 2000 ms wait.
        assert_eq!(start_label(&h.sync()), "Spotify");
    }

    /// after a reconnect and resync, START is sent again and frames use the new streamId from sequence 0
    #[test]
    fn reconnect_restarts_stream() {
        let mut h = Harness::streaming(3);
        let mut all = Vec::new();
        all.extend(h.feed(block(0.0)));
        all.extend(h.feed(block(20.0)));

        let out = h.feed(Input::Disconnected);
        all.extend(out);
        assert_eq!(h.phase(), Phase::Starting);
        assert!(!h.status().connected);
        assert!(!h.status().synced);
        let out = h.feed(block(40.0));
        assert!(frames(&out).is_empty(), "no frames while disconnected");
        all.extend(out);

        all.extend(h.feed(Input::Connected));
        let out = h.probe_pairs(15);
        assert!(starts(&out).is_empty(), "START waits for the resync");
        all.extend(out);
        let out = h.probe_pairs(1);
        assert_eq!(start_label(&out), "Spotify");
        all.extend(out);

        all.extend(h.server(own_state(9)));
        assert_eq!(h.phase(), Phase::Streaming { stream_id: 9 });
        assert_eq!(frames(&h.feed(block(60.0))), vec![frame(9, 0, 60.0)]);
        assert_eq!(frames(&h.feed(block(80.0))), vec![frame(9, 1, 80.0)]);
        assert!(!all.contains(&Output::StopAudio), "audio keeps running");
    }

    #[test]
    fn stale_state_after_reconnect_is_ignored() {
        let mut h = Harness::streaming(3);
        h.feed(Input::Disconnected);
        h.feed(Input::Connected);
        // The join handler's state: the old stream ended with the old socket.
        let out = h.server(state(None, None));
        assert!(without_status(out).is_empty());
        assert_eq!(h.phase(), Phase::Starting);
        // A state that still names the old stream does not resume it either.
        let out = h.server(own_state(3));
        assert!(without_status(out).is_empty());
        assert_eq!(h.phase(), Phase::Starting);
        assert!(frames(&h.feed(block(0.0))).is_empty());

        assert_eq!(start_label(&h.sync()), "Spotify");
    }

    #[test]
    fn user_stop_sends_stop_and_stops_audio() {
        let stop = Output::SendJson(ClientMessage::StopLiveStream);
        let mut h = Harness::streaming(3);
        let out = h.feed(Input::UserStop);
        assert_eq!(without_status(out), vec![stop.clone(), Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
        // Late states are ignored once idle.
        assert!(without_status(h.server(own_state(3))).is_empty());
        assert_eq!(h.phase(), Phase::Idle);
        assert!(frames(&h.feed(block(0.0))).is_empty());

        // START sent but no state yet: the server may be creating the stream.
        let mut h = Harness::connected();
        h.sync();
        h.feed(user_start(&["Spotify"]));
        let out = h.feed(Input::UserStop);
        assert_eq!(without_status(out), vec![stop, Output::StopAudio]);

        // START not sent yet: only the audio stops.
        let mut h = Harness::connected();
        h.feed(user_start(&["Spotify"]));
        let out = h.feed(Input::UserStop);
        assert_eq!(without_status(out), vec![Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
    }

    #[test]
    fn audio_failure_stops_the_stream_with_message() {
        let mut h = Harness::streaming(3);
        let out = h.feed(Input::AudioFailed {
            message: "capture failed".into(),
        });
        let stop = Output::SendJson(ClientMessage::StopLiveStream);
        assert_eq!(without_status(out), vec![stop, Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
        assert_eq!(h.message(), Some("capture failed"));
    }

    #[test]
    fn updated_apps_reach_audio_and_label() {
        let mut h = Harness::connected();
        // Idle: the selection is only stored.
        let out = h.feed(Input::UserUpdateApps {
            apps: strings(&["Firefox"]),
        });
        assert!(without_status(out).is_empty());

        h.feed(user_start(&["Spotify"]));
        // Before START: audio follows, the START carries the label.
        let apps = strings(&["Spotify", "Firefox"]);
        let out = h.feed(Input::UserUpdateApps { apps: apps.clone() });
        assert_eq!(without_status(out), vec![Output::UpdateAudioApps { apps }]);
        assert_eq!(start_label(&h.sync()), "Spotify, Firefox");

        h.server(own_state(3));
        let apps = strings(&["Firefox"]);
        let out = h.feed(Input::UserUpdateApps { apps: apps.clone() });
        let update = ClientMessage::UpdateLiveStream {
            label: "Firefox".into(),
        };
        let expected = vec![Output::UpdateAudioApps { apps }, Output::SendJson(update)];
        assert_eq!(without_status(out), expected);

        // A START after a reconnect carries the current selection.
        h.feed(Input::Disconnected);
        h.feed(Input::Connected);
        assert_eq!(start_label(&h.sync()), "Firefox");
    }

    #[test]
    fn liveness_ping_is_answered() {
        let mut h = Harness::connected();
        let out = h.server(ServerMessage::LivenessPing);
        let pong = Output::SendJson(ClientMessage::LivenessPong);
        assert_eq!(without_status(out), vec![pong]);
    }

    #[test]
    fn listeners_exclude_self() {
        let mut h = Harness::new();
        let first = Status {
            connected: false,
            synced: false,
            rtt_ms: None,
            listeners: 0,
            phase: Phase::Idle,
            message: None,
        };
        assert_eq!(
            statuses(&h.tick(0.0)),
            vec![&first],
            "the first call emits a Status"
        );
        let clients = [(ME, "alice"), ("web-1", "Bob"), ("web-2", "Carol")];
        h.server(client_change(&clients));
        assert_eq!(h.status().listeners, 2);
        // Unchanged status: no new Status.
        assert!(statuses(&h.server(client_change(&clients))).is_empty());
    }

    #[test]
    fn gain_combines_volume_and_own_spatial_gain() {
        let mut h = Harness::connected();
        assert_eq!(
            without_status(h.server(volume(0.5, 0.25))),
            vec![gain(0.5, 0.25)]
        );
        let out = h.server(spatial(&[(ME, 0.5, 0.125), ("web-1", 1.0, 1.0)]));
        assert_eq!(without_status(out), vec![gain(0.25, 0.125)]);
        // A config without an entry for this client changes nothing.
        assert!(without_status(h.server(spatial(&[("web-1", 0.0, 1.0)]))).is_empty());
        // The volume keeps the spatial gain.
        assert_eq!(
            without_status(h.server(volume(1.0, 0.5))),
            vec![gain(0.5, 0.5)]
        );
        // 0 is a real gain (the web client's `|| 1` would play it at full volume).
        let out = h.server(spatial(&[(ME, 0.0, 0.25)]));
        assert_eq!(without_status(out), vec![gain(0.0, 0.25)]);
        let out = h.server(scheduled(ScheduledAction::StopSpatialAudio));
        assert_eq!(without_status(out), vec![gain(1.0, 0.1)]);
    }

    #[test]
    fn label_is_cut_to_100_chars() {
        let label = |apps: &[&str]| {
            let mut h = Harness::connected();
            h.sync();
            start_label(&h.feed(user_start(apps)))
        };
        assert_eq!(label(&["Spotify", "Firefox"]), "Spotify, Firefox");
        let (a, b) = ("a".repeat(60), "b".repeat(60));
        assert_eq!(label(&[&a, &b]), format!("{a}, {}", "b".repeat(38)));
        // Zod counts UTF-16 code units: an emoji is 2, "é" is 1; never split a char.
        assert_eq!(label(&[&format!("{}🎵", "a".repeat(99))]), "a".repeat(99));
        let accents = "é".repeat(100);
        assert_eq!(label(&[&accents]), accents);
    }
}
