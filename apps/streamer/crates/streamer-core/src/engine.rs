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
    /// A connect attempt failed with `error`; the connection tries again.
    ConnectFailed {
        error: String,
    },
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
    /// The wall clock, which NTP measures, stepped by `delta_ms`. The runtime has moved its
    /// mono→epoch mapping by the same amount.
    ClockStepped {
        delta_ms: f64,
    },
    UserStart {
        apps: Vec<String>,
        delay_ms: u32,
    },
    UserUpdateApps {
        apps: Vec<String>,
    },
    UserStop,
    /// The user set a new nudge (positive plays the local copy earlier).
    UserNudge {
        nudge_ms: f64,
    },
    /// The user turned shuffle on or off.
    UserShuffle {
        on: bool,
    },
    AudioFailed {
        message: String,
    },
    /// The audio output failed and stays closed for this connection. A stream goes on.
    OutputFailed {
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
        client_nudge_ms: f64,
    },
    /// The nudge changed because the server's stored value was restored.
    Nudge {
        nudge_ms: f64,
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
    /// The low-pass filter goes to `freq_hz` over `ramp_s` (app only).
    SetLowPass {
        freq_hz: f32,
        ramp_s: f32,
    },
    /// The metronome plays or not: the room enabled it and the clock is synced (app only).
    SetMetronome {
        on: bool,
    },
    /// The shuffle state changed.
    SetShuffle {
        on: bool,
    },
    Status(Status),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub connected: bool,
    pub synced: bool,
    /// Synced at least once since this engine was created (a disconnect or a clock step does
    /// not clear it). Until then the server offset may be unknown, and a start would mute the
    /// apps without a local copy.
    pub synced_once: bool,
    pub rtt_ms: Option<f64>,
    /// Server offset of the last estimate; kept over a reconnect.
    pub offset_ms: Option<f64>,
    /// Accepted NTP measurements, 0..=16.
    pub sync_measurements: usize,
    pub nudge_ms: f64,
    /// Shuffle is on; false on a new engine.
    pub shuffle: bool,
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
    /// Responses of older probe pairs are ignored: they were sent before the NTP state was
    /// last reset (a clock step), so they may measure across the step.
    first_valid_group_id: u64,
    /// When the next probe pair is due; `None`: on the next tick.
    next_probe_ms: Option<f64>,
    last_offset_ms: Option<f64>,
    synced_once: bool,
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
    /// The message of the last failed connect attempt; a connect clears it from `message`.
    connect_error: Option<String>,
    /// "Audio output failed: …": the output stays closed for the rest of the connection, so
    /// the status shows it whenever no other message does.
    output_failure: Option<String>,
    volume: f64,
    spatial_gain: f64,
    last_status: Option<Status>,
    nudge_ms: f64,
    /// The server's stored nudge was restored, or the user set one: later CLIENT_CHANGEs
    /// must not overwrite it (the web client's `shouldRestoreNudge`).
    nudge_restored: bool,
    /// The desktop app, like the web client: it restores the server's stored nudge once
    /// (spec §6.2) and asks for the room's playback state (SYNC) once the clock is synced on
    /// each connection (spec C7). The CLI does neither: its `--local-offset` wins, and it plays
    /// no tracks.
    is_app: bool,
    /// SYNC was sent on the current connection.
    room_sync_sent: bool,
    /// The last low-pass frequency sent out; the first one after a join is always sent.
    last_lowpass_hz: Option<f64>,
    /// The room has the metronome on.
    metronome_enabled: bool,
    /// The last metronome state sent out.
    metronome_on: bool,
    shuffle: bool,
}

impl Engine {
    pub fn new(client_id: String, username: String, nudge_ms: f64, is_app: bool) -> Self {
        Self {
            client_id,
            username,
            connected: false,
            validator: ProbePairValidator::default(),
            estimator: NtpEstimator::default(),
            next_group_id: 0,
            first_valid_group_id: 0,
            next_probe_ms: None,
            last_offset_ms: None,
            synced_once: false,
            phase: Phase::Idle,
            apps: Vec::new(),
            delay_ms: 0,
            start_sent: false,
            stop_pending_until_ms: None,
            now_ms: 0.0,
            sequence: 0,
            clients: Vec::new(),
            message: None,
            connect_error: None,
            output_failure: None,
            volume: 1.0,
            spatial_gain: 1.0,
            last_status: None,
            nudge_ms,
            nudge_restored: false,
            is_app,
            room_sync_sent: false,
            last_lowpass_hz: None,
            metronome_enabled: false,
            metronome_on: false,
            shuffle: false,
        }
    }

    pub fn handle(&mut self, input: Input) -> Vec<Output> {
        let mut out = Vec::new();
        match input {
            Input::Connected => {
                self.reset_connection();
                self.connected = true;
                // The connect error is over; a message set since then stays.
                if let Some(error) = self.connect_error.take()
                    && self.message.as_ref() == Some(&error)
                {
                    self.message = None;
                }
            }
            Input::Disconnected => {
                self.reset_connection();
                self.connected = false;
            }
            Input::ConnectFailed { error } => {
                let message = format!("Cannot reach the server: {error}");
                self.message = Some(message.clone());
                self.connect_error = Some(message);
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
            Input::ClockStepped { delta_ms } => self.on_clock_step(delta_ms, &mut out),
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
            Input::UserNudge { nudge_ms } => {
                self.nudge_ms = nudge_ms;
                self.nudge_restored = true;
            }
            Input::UserShuffle { on } => {
                if self.shuffle != on {
                    self.shuffle = on;
                    out.push(Output::SetShuffle { on });
                }
            }
            Input::AudioFailed { message } => {
                self.stop(&mut out);
                self.message = Some(message);
            }
            Input::OutputFailed { message } => {
                let failure = format!("Audio output failed: {message}");
                self.message = Some(failure.clone());
                self.output_failure = Some(failure);
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
        self.update_metronome(&mut out);
        self.emit_status(&mut out);
        out
    }

    /// A new or closed connection: NTP restarts and START must be sent again. The audio
    /// keeps running, so a stream in progress goes back to `Starting`.
    fn reset_connection(&mut self) {
        self.reset_ntp();
        self.start_sent = false;
        self.room_sync_sent = false;
        // The old stream ended with the old socket.
        self.stop_pending_until_ms = None;
        if let Phase::Streaming { .. } = self.phase {
            self.phase = Phase::Starting;
        }
    }

    /// NTP starts over: probe pairs every 50 ms from the next tick until synced again.
    fn reset_ntp(&mut self) {
        self.validator = ProbePairValidator::default();
        self.estimator.reset();
        self.next_probe_ms = None;
        self.first_valid_group_id = self.next_group_id;
    }

    /// The epoch moved by `delta_ms` against the monotonic clock, so the server offset moves
    /// by `-delta_ms`: server time, and with it the stamps and the local copy, stays
    /// continuous. The window holds measurements of the old clock, so NTP starts over; the
    /// phase and the stream go on.
    fn on_clock_step(&mut self, delta_ms: f64, out: &mut Vec<Output>) {
        if let Some(offset_ms) = self.last_offset_ms {
            let offset_ms = offset_ms - delta_ms;
            self.last_offset_ms = Some(offset_ms);
            out.push(Output::ServerOffset { offset_ms });
        }
        self.reset_ntp();
    }

    fn on_server(&mut self, msg: ServerMessage, out: &mut Vec<Output>) {
        match msg {
            ServerMessage::LivenessPing => out.push(Output::SendJson(ClientMessage::LivenessPong)),
            ServerMessage::RoomEvent { event } => match event {
                RoomEvent::ClientChange { clients } => {
                    self.clients = clients;
                    self.restore_nudge(out);
                }
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

    /// The server keeps a nonzero nudge of this client from an earlier session: the app takes
    /// it once.
    fn restore_nudge(&mut self, out: &mut Vec<Output>) {
        if !self.is_app || self.nudge_restored {
            return;
        }
        let stored = self
            .clients
            .iter()
            .find(|c| c.client_id == self.client_id)
            .map_or(0.0, |c| c.nudge_ms);
        if stored != 0.0 {
            self.nudge_restored = true;
            self.nudge_ms = stored;
            out.push(Output::Nudge { nudge_ms: stored });
        }
    }

    fn on_ntp_response(
        &mut self,
        m: NtpMeasurement,
        group_id: u64,
        index: u8,
        out: &mut Vec<Output>,
    ) {
        if !self.connected || group_id < self.first_valid_group_id {
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
        self.request_room_sync_when_synced(out);
        if !self.connected || self.next_probe_ms.is_some_and(|due| now_ms < due) {
            return;
        }
        out.push(Output::SendProbePair {
            group_id: self.next_group_id,
            client_rtt: self.estimator.avg_rtt_ms(),
            client_nudge_ms: self.nudge_ms,
        });
        self.next_group_id += 1;
        let interval_ms = if self.estimator.is_synced() {
            STEADY_INTERVAL_MS
        } else {
            INITIAL_INTERVAL_MS
        };
        self.next_probe_ms = Some(now_ms + interval_ms as f64);
    }

    /// SYNC once per connection, at the first tick with a synced clock: while a track plays,
    /// the server answers with a PLAY scheduled in server time, which needs the synced clock.
    fn request_room_sync_when_synced(&mut self, out: &mut Vec<Output>) {
        if self.is_app && self.connected && !self.room_sync_sent && self.estimator.is_synced() {
            self.room_sync_sent = true;
            out.push(Output::SendJson(ClientMessage::Sync));
        }
    }

    /// While `Starting`, only our own state or REJECTED decides: a null or foreign state may be
    /// the end of another source's stream, and the server may create ours right after it.
    /// States before START on this connection (e.g. the join handler's after a reconnect) are
    /// stale.
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
        let Some(stream_id) = own_stream_id else {
            // The echo of our last STOP: START may go out again.
            self.stop_pending_until_ms = None;
            if let Phase::Streaming { .. } = self.phase {
                out.push(Output::StopAudio);
                self.go_idle();
                if stream.is_none()
                    && let Some(id) = stopped_by
                {
                    self.message = Some(format!("Stopped by {}", self.username_of(&id)));
                }
            }
            return;
        };
        match self.phase {
            Phase::Idle => {
                // An orphaned stream of ours (e.g. stopped during a disconnect): end it once.
                if self.stop_pending_until_ms.is_none() {
                    self.send_stop(out);
                }
            }
            Phase::Starting if !self.start_sent => {}
            Phase::Starting | Phase::Streaming { .. } => {
                let phase = Phase::Streaming { stream_id };
                // The same stream again (a label update) keeps the sequence.
                if self.phase != phase {
                    self.phase = phase;
                    self.sequence = 0;
                }
            }
        }
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
            ScheduledAction::LowPassConfig { freq, ramp_time } => {
                // Only the app filters. A new ramp time alone does not move the filter.
                if self.is_app && self.last_lowpass_hz != Some(freq) {
                    self.last_lowpass_hz = Some(freq);
                    out.push(Output::SetLowPass {
                        freq_hz: freq as f32,
                        ramp_s: ramp_time as f32,
                    });
                }
                return;
            }
            ScheduledAction::MetronomeConfig { enabled } => {
                self.metronome_enabled = enabled;
                return;
            }
            ScheduledAction::Other => return,
        };
        let gain = (self.volume * self.spatial_gain) as f32;
        out.push(Output::SetPlayoutGain { gain, ramp_s });
    }

    /// The metronome plays in the app only, while the room has it on and the clock is synced.
    fn update_metronome(&mut self, out: &mut Vec<Output>) {
        let on = self.is_app && self.metronome_enabled && self.estimator.is_synced();
        if on != self.metronome_on {
            self.metronome_on = on;
            out.push(Output::SetMetronome { on });
        }
    }

    fn stop(&mut self, out: &mut Vec<Output>) {
        if self.phase == Phase::Idle {
            return;
        }
        if self.start_sent {
            self.send_stop(out);
        }
        out.push(Output::StopAudio);
        self.go_idle();
    }

    fn send_stop(&mut self, out: &mut Vec<Output>) {
        out.push(Output::SendJson(ClientMessage::StopLiveStream));
        self.stop_pending_until_ms = Some(self.now_ms + STOP_ECHO_TIMEOUT_MS);
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
        self.synced_once |= self.estimator.is_synced();
        let status = Status {
            connected: self.connected,
            synced: self.estimator.is_synced(),
            synced_once: self.synced_once,
            rtt_ms: self.estimator.avg_rtt_ms(),
            offset_ms: self.last_offset_ms,
            sync_measurements: self.estimator.measurements(),
            nudge_ms: self.nudge_ms,
            shuffle: self.shuffle,
            listeners: self
                .clients
                .iter()
                .filter(|c| c.client_id != self.client_id)
                .count(),
            phase: self.phase,
            message: self.message.clone().or_else(|| self.output_failure.clone()),
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
            Self::with_nudge(0.0)
        }

        fn with_nudge(nudge_ms: f64) -> Self {
            Self::with_engine(Engine::new(ME.into(), "alice".into(), nudge_ms, false))
        }

        /// Like [`Self::new`], for the desktop app: it restores the server's nudge and
        /// requests the room's playback state.
        fn app() -> Self {
            Self::app_with_nudge(0.0)
        }

        fn app_with_nudge(nudge_ms: f64) -> Self {
            Self::with_engine(Engine::new(ME.into(), "alice".into(), nudge_ms, true))
        }

        fn with_engine(engine: Engine) -> Self {
            Self {
                engine,
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
            self.probe_pairs_at(n, 500.0)
        }

        /// `n` ticks 50 ms apart, each answered with an accepted pair (rtt 20, `offset`).
        fn probe_pairs_at(&mut self, n: usize, offset: f64) -> Vec<Output> {
            let mut out = Vec::new();
            for _ in 0..n {
                let tick = self.tick(self.now_ms + 50.0);
                let [(group_id, _)] = probes(&tick)[..] else {
                    panic!("one probe pair is due: {tick:?}");
                };
                out.extend(tick);
                out.extend(self.answer(group_id, 20.0, offset));
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
                nudge_ms: 0.0,
            })
            .collect();
        ServerMessage::RoomEvent {
            event: RoomEvent::ClientChange { clients },
        }
    }

    /// A CLIENT_CHANGE whose only client is this one, with the server's stored `nudge_ms`.
    fn own_client_change(nudge_ms: f64) -> ServerMessage {
        ServerMessage::RoomEvent {
            event: RoomEvent::ClientChange {
                clients: vec![ClientData {
                    username: "alice".into(),
                    client_id: ME.into(),
                    is_admin: false,
                    kind: ClientKind::App,
                    nudge_ms,
                }],
            },
        }
    }

    fn nudges(out: &[Output]) -> Vec<f64> {
        out.iter()
            .filter_map(|o| match o {
                Output::Nudge { nudge_ms } => Some(*nudge_ms),
                _ => None,
            })
            .collect()
    }

    fn probe_nudges(out: &[Output]) -> Vec<f64> {
        out.iter()
            .filter_map(|o| match o {
                Output::SendProbePair {
                    client_nudge_ms, ..
                } => Some(*client_nudge_ms),
                _ => None,
            })
            .collect()
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
                    ..
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

    fn room_syncs(out: &[Output]) -> usize {
        out.iter()
            .filter(|o| matches!(o, Output::SendJson(ClientMessage::Sync)))
            .count()
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
    fn clock_step_keeps_server_time_and_resyncs() {
        // NTP measures the wall clock. After a step of +1000 ms the window still holds offsets
        // of the old clock for up to 40 s, so the stamps would move 1000 ms against the local
        // copy: the laptop would play ahead of the room.
        let mut h = Harness::streaming(3);
        let t = h.now_ms;
        let in_flight = h.tick(t + 2500.0);
        let [(stale, _)] = probes(&in_flight)[..] else {
            panic!("a steady probe pair is due: {in_flight:?}");
        };
        let out = h.feed(Input::ClockStepped { delta_ms: 1000.0 });
        assert_eq!(offsets(&out), vec![-500.0], "server time stays continuous");
        assert!(!h.status().synced);
        assert!(h.status().synced_once, "the offset is still known");
        assert_eq!(h.phase(), Phase::Streaming { stream_id: 3 });
        assert_eq!(frames(&h.feed(block(0.0))), vec![frame(3, 0, 0.0)]);
        // The pair sent before the step was measured across it.
        let out = h.answer(stale, 1.0, -9000.0);
        assert!(
            offsets(&out).is_empty(),
            "a pair from before the step is ignored"
        );
        assert_eq!(h.status().rtt_ms, None);

        // New pairs go out at once, then every 50 ms until 16 new measurements.
        let first = h.tick(h.now_ms + 10.0);
        let [(group_id, _)] = probes(&first)[..] else {
            panic!("a probe pair goes out at once: {first:?}");
        };
        let mut out = h.answer(group_id, 20.0, -500.0);
        out.extend(h.probe_pairs_at(14, -500.0));
        assert!(
            offsets(&out).is_empty(),
            "the shifted offset was the new one"
        );
        assert!(!h.status().synced, "15 new measurements");
        h.probe_pairs_at(1, -500.0);
        assert!(h.status().synced);
        assert_eq!(h.phase(), Phase::Streaming { stream_id: 3 });
        assert_eq!(frames(&h.feed(block(20.0))), vec![frame(3, 1, 20.0)]);

        // Without a known offset there is nothing to shift.
        let mut h = Harness::connected();
        assert!(offsets(&h.feed(Input::ClockStepped { delta_ms: 1000.0 })).is_empty());
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
    fn competing_stream_waits_for_the_rejection() {
        let mut h = Harness::connected();
        h.sync();
        h.feed(user_start(&["Spotify"]));
        // Another source's stream: only the server's answer to our START decides.
        let out = h.server(state(Some(live_stream(5, "web-1")), None));
        assert!(without_status(out).is_empty());
        assert_eq!(h.phase(), Phase::Starting);
        let out = h.server(ServerMessage::LiveStreamRejected {
            reason: RejectReason::AlreadyActive,
        });
        assert_eq!(without_status(out), vec![Output::StopAudio]);
        assert_eq!(h.phase(), Phase::Idle);
        assert_eq!(h.message(), Some("Another stream is active"));
    }

    #[test]
    fn foreign_stream_ending_during_start_keeps_the_session() {
        let mut h = Harness::connected();
        h.server(client_change(&[(ME, "alice"), ("web-1", "Bob")]));
        h.sync();
        h.feed(user_start(&["Spotify"]));
        // Bob stops another stream; the server handles our START right after.
        let out = h.server(state(None, Some("web-1")));
        assert!(without_status(out).is_empty());
        assert_eq!(h.phase(), Phase::Starting);
        assert_eq!(h.message(), None);
        let out = h.server(own_state(7));
        assert!(without_status(out).is_empty());
        assert_eq!(h.phase(), Phase::Streaming { stream_id: 7 });
        assert_eq!(h.message(), None);
        assert_eq!(frames(&h.feed(block(0.0))), vec![frame(7, 0, 0.0)]);
    }

    #[test]
    fn own_stream_while_idle_is_stopped() {
        // Stopped during a disconnect: no STOP could go out, and the server kept the stream.
        let mut h = Harness::streaming(3);
        h.feed(Input::Disconnected);
        assert_eq!(
            without_status(h.feed(Input::UserStop)),
            vec![Output::StopAudio]
        );
        h.feed(Input::Connected);
        let out = h.server(own_state(3));
        let stop = Output::SendJson(ClientMessage::StopLiveStream);
        assert_eq!(without_status(out), vec![stop], "STOP only, no audio");
        assert_eq!(h.phase(), Phase::Idle);
        // Once: the STOP is pending.
        assert!(without_status(h.server(own_state(3))).is_empty());
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
        assert!(
            h.status().synced_once,
            "a start may go on: the offset is known"
        );
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
    fn output_failure_shows_in_the_status() {
        // The PC's speakers are gone, but the room still hears the stream: it goes on, and
        // the status says why the PC is silent.
        let mut h = Harness::streaming(3);
        let out = h.feed(Input::OutputFailed {
            message: "device gone".into(),
        });
        assert!(without_status(out).is_empty(), "no stop");
        assert_eq!(h.phase(), Phase::Streaming { stream_id: 3 });
        assert_eq!(h.message(), Some("Audio output failed: device gone"));
    }

    #[test]
    fn the_output_failure_stays_in_the_status_while_the_output_is_closed() {
        // The output stays closed for the rest of the connection, so every later stream and
        // track is silent on this PC: the status must go on saying why.
        let failure = Some("Audio output failed: device gone");
        let mut h = Harness::connected();
        h.sync();
        h.feed(Input::OutputFailed {
            message: "device gone".into(),
        });
        h.feed(user_start(&["Spotify"]));
        assert_eq!(h.message(), failure, "a new start keeps it");

        // A newer message shows first; the failure is back when that one clears.
        h.server(ServerMessage::LiveStreamRejected {
            reason: RejectReason::AlreadyActive,
        });
        assert_eq!(h.message(), Some("Another stream is active"));
        h.feed(user_start(&["Spotify"]));
        assert_eq!(h.message(), failure);

        // A reconnect does not reopen the output.
        h.feed(Input::ConnectFailed {
            error: "refused".into(),
        });
        h.feed(Input::Connected);
        assert_eq!(h.message(), failure);
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
            synced_once: false,
            rtt_ms: None,
            offset_ms: None,
            sync_measurements: 0,
            nudge_ms: 0.0,
            shuffle: false,
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

    fn low_pass(freq: f64, ramp_time: f64) -> ServerMessage {
        scheduled(ScheduledAction::LowPassConfig { freq, ramp_time })
    }

    fn metronome(enabled: bool) -> ServerMessage {
        scheduled(ScheduledAction::MetronomeConfig { enabled })
    }

    fn metronome_outputs(out: &[Output]) -> Vec<bool> {
        out.iter()
            .filter_map(|o| match o {
                Output::SetMetronome { on } => Some(*on),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn low_pass_config_reaches_the_output() {
        let mut h = Harness::app();
        h.feed(Input::Connected);
        let out = h.server(low_pass(800.0, 0.2));
        assert_eq!(
            without_status(out),
            vec![Output::SetLowPass {
                freq_hz: 800.0,
                ramp_s: 0.2
            }]
        );
    }

    #[test]
    fn an_unchanged_low_pass_frequency_is_ignored() {
        let mut h = Harness::app();
        h.feed(Input::Connected);
        assert_eq!(without_status(h.server(low_pass(800.0, 0.2))).len(), 1);
        // The same frequency with another ramp time does nothing.
        assert!(without_status(h.server(low_pass(800.0, 0.5))).is_empty());
        assert_eq!(without_status(h.server(low_pass(900.0, 0.5))).len(), 1);
        // The first one after a new join is sent again.
        let mut fresh = Harness::app();
        fresh.feed(Input::Connected);
        assert_eq!(without_status(fresh.server(low_pass(900.0, 0.5))).len(), 1);
    }

    #[test]
    fn the_metronome_plays_only_when_enabled_and_synced() {
        let mut h = Harness::app();
        h.feed(Input::Connected);
        // Enabled, but the clock is not synced: nothing yet.
        assert!(metronome_outputs(&h.server(metronome(true))).is_empty());
        assert_eq!(metronome_outputs(&h.sync()), vec![true]);
        assert!(metronome_outputs(&h.server(metronome(true))).is_empty());
        assert_eq!(metronome_outputs(&h.server(metronome(false))), vec![false]);
        assert_eq!(metronome_outputs(&h.server(metronome(true))), vec![true]);
        // The clock step ends the sync: the metronome stops.
        let out = h.feed(Input::ClockStepped { delta_ms: 1000.0 });
        assert_eq!(metronome_outputs(&out), vec![false]);
    }

    #[test]
    fn the_cli_ignores_low_pass_and_metronome() {
        let mut h = Harness::new();
        h.feed(Input::Connected);
        let mut all = h.server(low_pass(800.0, 0.2));
        all.extend(h.server(metronome(true)));
        all.extend(h.sync());
        assert!(
            without_status(all)
                .iter()
                .all(|o| !matches!(o, Output::SetLowPass { .. } | Output::SetMetronome { .. }))
        );
    }

    #[test]
    fn shuffle_shows_in_the_status() {
        let mut h = Harness::app();
        h.feed(Input::Connected);
        assert!(!h.status().shuffle);
        let out = h.feed(Input::UserShuffle { on: true });
        assert!(out.contains(&Output::SetShuffle { on: true }));
        assert!(h.status().shuffle);
        let out = h.feed(Input::UserShuffle { on: true });
        assert!(!out.contains(&Output::SetShuffle { on: true }));
        h.feed(Input::UserShuffle { on: false });
        assert!(!h.status().shuffle);
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

    #[test]
    fn probe_pairs_carry_the_nudge() {
        let mut h = Harness::with_nudge(20.0);
        h.feed(Input::Connected);
        let first = h.tick(h.now_ms + 10.0);
        assert_eq!(probe_nudges(&first), vec![20.0]);
        h.feed(Input::UserNudge { nudge_ms: -10.0 });
        let next = h.tick(h.now_ms + 60.0);
        assert_eq!(probe_nudges(&next), vec![-10.0]);
    }

    #[test]
    fn the_server_nudge_is_restored_once() {
        let mut h = Harness::app();
        h.feed(Input::Connected);
        let out = h.server(own_client_change(30.0));
        assert_eq!(nudges(&out), vec![30.0]);
        assert_eq!(h.status().nudge_ms, 30.0);
        let out = h.server(own_client_change(50.0));
        assert!(nudges(&out).is_empty());
        assert_eq!(h.status().nudge_ms, 30.0);
    }

    #[test]
    fn the_cli_never_restores_the_server_nudge() {
        // Spec §6.2: the app restores the stored nudge; the CLI's `--local-offset` wins.
        let mut h = Harness::with_nudge(20.0);
        h.feed(Input::Connected);
        let out = h.server(own_client_change(30.0));
        assert!(nudges(&out).is_empty());
        assert_eq!(h.status().nudge_ms, 20.0);
    }

    #[test]
    fn a_zero_server_nudge_is_not_restored() {
        let mut h = Harness::app_with_nudge(20.0);
        h.feed(Input::Connected);
        let out = h.server(own_client_change(0.0));
        assert!(nudges(&out).is_empty());
        assert_eq!(h.status().nudge_ms, 20.0);
    }

    #[test]
    fn a_user_nudge_is_not_overwritten_by_a_stale_client_change() {
        let mut h = Harness::app();
        h.feed(Input::Connected);
        h.feed(Input::UserNudge { nudge_ms: 15.0 });
        let out = h.server(own_client_change(5.0));
        assert!(nudges(&out).is_empty());
        assert_eq!(h.status().nudge_ms, 15.0);
    }

    #[test]
    fn a_failed_connect_shows_the_reason_until_connected() {
        // A wrong but parsable server address gave "Connecting..." forever with no reason.
        let mut h = Harness::new();
        h.feed(Input::ConnectFailed {
            error: "Connection refused".into(),
        });
        assert_eq!(
            h.message(),
            Some("Cannot reach the server: Connection refused")
        );
        h.feed(Input::Connected);
        assert_eq!(h.message(), None);

        // A connect clears only the connect error: a message set after it stays.
        h.feed(Input::Disconnected);
        h.feed(Input::ConnectFailed {
            error: "Connection refused".into(),
        });
        h.feed(user_start(&["Spotify"]));
        h.feed(Input::AudioFailed {
            message: "no sink".into(),
        });
        h.feed(Input::Connected);
        assert_eq!(h.message(), Some("no sink"));
    }

    #[test]
    fn status_reports_sync_measurements_and_offset() {
        let mut h = Harness::connected();
        assert_eq!(h.status().sync_measurements, 0);
        assert_eq!(h.status().offset_ms, None);
        h.probe_pairs(3);
        assert_eq!(h.status().sync_measurements, 3);
        assert_eq!(h.status().offset_ms, Some(500.0));
        h.probe_pairs(13);
        assert_eq!(h.status().sync_measurements, 16);
        // A 17th measurement does not raise the count.
        let tick = h.tick(h.now_ms + STEADY_INTERVAL_MS as f64);
        let [(group_id, _)] = probes(&tick)[..] else {
            panic!("a steady probe pair is due: {tick:?}");
        };
        h.answer(group_id, 20.0, 500.0);
        assert_eq!(h.status().sync_measurements, 16);
    }

    #[test]
    fn sync_is_requested_once_per_connection_after_the_clock_syncs() {
        // Spec C7: like the web client, the app asks for the playback state once its clock is
        // synced; while a track plays, the server answers with a PLAY scheduled in server
        // time. One request per connection; a new connection asks again.
        let mut h = Harness::app();
        h.feed(Input::Connected);
        let mut out = h.probe_pairs(15);
        // The 16th pair goes out at 15 measurements.
        out.extend(h.probe_pairs(1));
        assert_eq!(room_syncs(&out), 0, "not synced before 16 measurements");
        assert!(h.status().synced);
        let out = h.tick(h.now_ms + 10.0);
        assert_eq!(room_syncs(&out), 1, "{out:?}");

        let mut later = Vec::new();
        for _ in 0..3 {
            later.extend(h.tick(h.now_ms + STEADY_INTERVAL_MS as f64));
        }
        // A clock step resyncs on the same connection.
        later.extend(h.feed(Input::ClockStepped { delta_ms: 1000.0 }));
        later.extend(h.sync());
        later.extend(h.tick(h.now_ms + 10.0));
        assert!(h.status().synced);
        assert_eq!(room_syncs(&later), 0, "once per connection");

        h.feed(Input::Disconnected);
        h.feed(Input::Connected);
        let mut out = h.sync();
        assert_eq!(room_syncs(&out), 0, "not synced on the new connection yet");
        out.extend(h.tick(h.now_ms + 10.0));
        out.extend(h.tick(h.now_ms + STEADY_INTERVAL_MS as f64));
        assert_eq!(room_syncs(&out), 1, "once more on the new connection");
    }

    #[test]
    fn the_cli_never_requests_sync() {
        // The CLI keeps its behavior: it shows no room and plays no tracks.
        let mut h = Harness::new();
        h.feed(Input::Connected);
        let mut out = h.sync();
        out.extend(h.tick(h.now_ms + 10.0));
        h.feed(Input::Disconnected);
        h.feed(Input::Connected);
        out.extend(h.sync());
        out.extend(h.tick(h.now_ms + 10.0));
        assert!(h.status().synced);
        assert_eq!(room_syncs(&out), 0);
    }
}
