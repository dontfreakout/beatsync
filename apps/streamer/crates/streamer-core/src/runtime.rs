//! Streamer runtime (spec §9.5–§9.6, §10.1): wires the engine, the WebSocket connection and
//! the audio backend.
//!
//! - The engine task owns the [`Engine`]. It feeds it a 10 ms tick, the connection's events,
//!   the user's commands and the audio worker's encoded blocks, and executes its outputs.
//! - The connection task is [`run_connection`]; it ends when the engine task drops its
//!   [`Outgoing`] sender at shutdown.
//! - The audio task runs the backend calls one after another through `spawn_blocking` (each
//!   can block for seconds), so a stop never overtakes the start before it. It opens the
//!   output first and closes it last, after a short fade: one output for the whole
//!   connection.
//! - The audio worker (one std thread per audio session) cuts captured chunks into 20 ms
//!   blocks, stamps them in server time, encodes them for the room and queues the raw PCM for
//!   the local copy in the output's stream slot.
//! - The track loader (app only) downloads the current queue tracks into the cache dir,
//!   decodes them and acknowledges the server's load requests; the engine task feeds it the
//!   room messages. It plays the queue track on this PC: its feeder thread queues the decoded
//!   track in the output's track slot. At shutdown it ends before the output closes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle as ThreadHandle;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::audio::{
    AppInfo, AudioBackend, AudioError, AudioEvent, AudioIo, CHUNK_MAX_FRAMES, CapturedChunk,
    OutputIo, PlayoutShared,
};
use crate::clock::{MonoEpochMap, clock_step_ms, mono_ns};
use crate::config::{Settings, track_cache_dir};
use crate::connection::{
    ConnEvent, JoinKind, Outgoing, build_ws_url, client_compensation_ms, run_connection,
};
use crate::dsp::Metronome;
use crate::encoder::OpusFrameEncoder;
use crate::engine::{Engine, Input, Output, Phase, Status};
use crate::frame::{FRAME_DURATION_MS, FRAME_SAMPLES, MAX_DELAY_MS, MIN_DELAY_MS, SAMPLE_RATE};
use crate::http::http_base;
use crate::mixer::{FLUSH_WAIT_MS, Slot, request_flush, wait_flushed};
use crate::playout::{BLOCK_SAMPLES, CHANNELS, PcmBlock};
use crate::room;
use crate::timeline::CaptureTimeline;
use crate::tracks::TrackStates;
use crate::tracks::loader::{Playback, TrackLoader};

const TICK: Duration = Duration::from_millis(10);
/// The mono→epoch mapping is measured again this often; a wall-clock step shows up as a jump
/// of the mapping at most this long after it happened.
const MONO_EPOCH_INTERVAL: Duration = Duration::from_secs(1);
/// 20 s of frames: more than a stalled socket lasts before its 5 s write timeout.
const OUTGOING_CAPACITY: usize = 1024;
/// Encoded blocks on their way from the worker to the engine.
const BLOCKS_CAPACITY: usize = 256;
/// A chunk is one graph quantum (split only above 1024 frames), so the ring is sized for the
/// smallest quantum: 384 chunks hold 0.5 s even at 64 frames per chunk (about 3 MB).
const CAPTURE_RING: usize = 384;
/// Blocks of 20 ms in each output slot's ring: the largest delay (5 s) plus 1 s, with room to
/// spare.
const PLAYOUT_RING: usize = 512;
const TIMELINE_ALPHA: f64 = 0.002;
const TIMELINE_REANCHOR_MS: f64 = 50.0;
/// A chunk that starts further than this from the end of the previous chunk and the lost
/// frames restarts the block and the timeline (a capture restart).
const CAPTURE_GAP_NS: i64 = 50_000_000;
/// A shorter loss of captured audio plays as silence. A longer loss restarts the block and the
/// timeline: the timestamps then step by far more than the 50 ms at which the web receiver
/// resyncs, so every device resyncs, and no burst of silent blocks goes out.
const MAX_SILENT_FILL_NS: i64 = 200_000_000;
/// The worker sleeps this long when the capture ring is empty.
const WORKER_IDLE: Duration = Duration::from_millis(5);
/// At shutdown, how long queued messages (e.g. STOP_LIVE_STREAM) may take to go out.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// At shutdown, the output's gain ramps to 0 over this many seconds before it closes.
const CLOSE_FADE_S: f32 = 0.005;
/// The longest output delay the shutdown fade waits for.
const CLOSE_FADE_MAX_AHEAD_MS: f64 = 100.0;
/// The shutdown fade also waits this long for the output's next cycle, which renders the
/// ramp (the output asks for 20 ms cycles).
const CLOSE_FADE_CYCLE_MS: f64 = 25.0;

/// Server time at which the local copy plays the first frame of a block stamped `stamp_ms`:
/// the room's delay plus the encoder lookahead (receivers hear the decoded Opus that late).
/// The nudge is not part of it: the output applies the nudge to every source at once.
pub fn local_target_ms(stamp_ms: f64, delay_ms: u32, lookahead_samples: u32) -> f64 {
    let lookahead_ms = f64::from(lookahead_samples) * 1000.0 / f64::from(SAMPLE_RATE);
    stamp_ms + f64::from(delay_ms) + lookahead_ms
}

/// A running streamer: one server connection, and an audio session while streaming.
pub struct StreamerHandle {
    commands: mpsc::UnboundedSender<Command>,
    status: watch::Receiver<Status>,
    tracks: watch::Receiver<TrackStates>,
    backend: Arc<dyn AudioBackend>,
    /// `None` when the server URL is invalid (the status message says why).
    task: Option<JoinHandle<()>>,
}

enum Command {
    Start {
        apps: Vec<String>,
        delay_ms: u32,
    },
    UpdateApps {
        apps: Vec<String>,
    },
    SetNudge {
        nudge_ms: f64,
    },
    SetShuffle(bool),
    /// A room action of the window, already checked.
    Action(serde_json::Value),
    /// Answered when the audio task has done every call queued before it.
    Stop(oneshot::Sender<()>),
    Shutdown,
}

impl StreamerHandle {
    /// Connects to the room in `settings` (and reconnects until [`Self::shutdown`]). An
    /// invalid server URL gives a handle that only reports the error in its status.
    ///
    /// `room_feed` gets the raw room messages for the window ([`room::forwarded`]); the CLI
    /// passes `None`. The app (`JoinKind::App`) also restores the server's stored nudge, asks
    /// for the room's playback state and loads the current tracks ([`Self::tracks`]).
    pub async fn connect(
        settings: Settings,
        backend: Arc<dyn AudioBackend>,
        kind: JoinKind,
        room_feed: Option<mpsc::UnboundedSender<serde_json::Value>>,
    ) -> Self {
        Self::connect_in(settings, backend, kind, room_feed, track_cache_dir()).await
    }

    /// [`Self::connect`] with the track cache in `cache_dir`.
    pub(crate) async fn connect_in(
        settings: Settings,
        backend: Arc<dyn AudioBackend>,
        kind: JoinKind,
        room_feed: Option<mpsc::UnboundedSender<serde_json::Value>>,
        cache_dir: PathBuf,
    ) -> Self {
        let (tracks_tx, tracks) = watch::channel(TrackStates::new());
        let (status_tx, status) = watch::channel(Status {
            connected: false,
            synced: false,
            synced_once: false,
            rtt_ms: None,
            offset_ms: None,
            sync_measurements: 0,
            nudge_ms: settings.nudge_ms,
            shuffle: false,
            listeners: 0,
            phase: Phase::Idle,
            message: None,
        });
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let url = match build_ws_url(
            &settings.server_url,
            &settings.room_id,
            &settings.username,
            &settings.client_id,
            kind,
        ) {
            Ok(url) => url,
            Err(e) => {
                status_tx.send_modify(|s| s.message = Some(e.to_string()));
                return Self {
                    commands,
                    status,
                    tracks,
                    backend,
                    task: None,
                };
            }
        };

        let shared = Arc::new(PlayoutShared::default());
        let mono_epoch_offset_ms = MonoEpochMap::measure().to_epoch_ms(0);
        // No server offset yet: nothing is stamped before the first one.
        store_f64(&shared.mono_server_offset_ms, mono_epoch_offset_ms);
        // Full volume until the room says otherwise (the default 0 is silence).
        shared.gain.store(1.0_f32.to_bits(), Ordering::Relaxed);
        shared
            .gain_ramp_s
            .store(0.0_f32.to_bits(), Ordering::Relaxed);
        store_f64(&shared.nudge_ms, settings.nudge_ms);
        let offset_known = Arc::new(AtomicBool::new(false));
        let shuffle = Arc::new(AtomicBool::new(false));

        // The output's two slots. The stream producer goes to each stream session's worker in
        // turn, the track producer to the track loader's feeder (the CLI has none).
        let (stream_tx, stream_rx) = rtrb::RingBuffer::<PcmBlock>::new(PLAYOUT_RING);
        let (track_tx, track_rx) = rtrb::RingBuffer::<PcmBlock>::new(PLAYOUT_RING);
        let (output_events, output_events_rx) = mpsc::unbounded_channel();
        let output = Metronome::kick()
            .map(|metronome| OutputIo {
                stream_rx,
                track_rx,
                metronome,
                lowpass: kind == JoinKind::App,
                shared: shared.clone(),
                events: output_events,
            })
            .map_err(|e| format!("cannot decode the metronome sound: {e}"));

        let (out_tx, out_rx) = mpsc::channel(OUTGOING_CAPACITY);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let connection = tokio::spawn(run_connection(url, out_rx, ev_tx, shutdown_rx));
        // The CLI downloads nothing and leaves the cache dir, which it shares with the app,
        // alone.
        let (loader_feed, loader) = match kind {
            JoinKind::App => match http_base(&settings.server_url) {
                Ok(base) => {
                    let (feed, messages) = mpsc::unbounded_channel();
                    let playback = Playback {
                        client_id: settings.client_id.clone(),
                        track_tx,
                        shared: shared.clone(),
                        offset_known: offset_known.clone(),
                        shuffle: shuffle.clone(),
                    };
                    let loader = TrackLoader::spawn(
                        base,
                        cache_dir,
                        tracks_tx,
                        messages,
                        out_tx.clone(),
                        playback,
                    );
                    (Some(feed), Some(loader))
                }
                Err(e) => {
                    eprintln!("streamer: no track downloads: {e}");
                    (None, None)
                }
            },
            JoinKind::Streamer => (None, None),
        };

        let (blocks_tx, blocks_rx) = mpsc::channel(BLOCKS_CAPACITY);
        let (notes_tx, notes_rx) = mpsc::unbounded_channel();
        tokio::spawn(forward_output_events(output_events_rx, notes_tx.clone()));
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let audio = tokio::spawn(audio_task(
            audio_rx,
            AudioContext {
                backend: backend.clone(),
                shared: shared.clone(),
                offset_known: offset_known.clone(),
                stream_tx: Arc::new(Mutex::new(Some(stream_tx))),
                flush_timeout_logged: AtomicBool::new(false),
                blocks: blocks_tx,
                notes: notes_tx,
            },
            output,
        ));

        let engine = EngineTask {
            engine: Engine::new(
                settings.client_id,
                settings.username,
                settings.nudge_ms,
                kind == JoinKind::App,
            ),
            kind,
            out_tx: Some(out_tx),
            room_feed,
            loader_feed,
            audio_tx: Some(audio_tx),
            status_tx,
            shared,
            mono_epoch_offset_ms,
            server_offset_ms: 0.0,
            offset_known,
            next_session_id: 0,
            active_session_id: None,
            outgoing_dropped: 0,
            shuffle,
        };
        let task = tokio::spawn(engine.run(
            Channels {
                commands: commands_rx,
                events: ev_rx,
                blocks: blocks_rx,
                notes: notes_rx,
            },
            Tasks {
                connection,
                shutdown_tx,
                audio,
                loader,
            },
        ));
        Self {
            commands,
            status,
            tracks,
            backend,
            task: Some(task),
        }
    }

    /// Routes `apps` into the sink and streams them once synced. Ignored unless idle.
    pub fn start(&self, apps: Vec<String>, delay_ms: u32) {
        let delay_ms = delay_ms.clamp(MIN_DELAY_MS, MAX_DELAY_MS);
        let _ = self.commands.send(Command::Start { apps, delay_ms });
    }

    pub fn update_apps(&self, apps: Vec<String>) {
        let _ = self.commands.send(Command::UpdateApps { apps });
    }

    /// Sets the nudge: the output uses it at once, and the server learns it.
    pub fn set_nudge(&self, nudge_ms: f64) {
        let _ = self.commands.send(Command::SetNudge { nudge_ms });
    }

    /// Turns shuffle on or off. The state shows in the status and resets on each join.
    pub fn set_shuffle(&self, on: bool) {
        let _ = self.commands.send(Command::SetShuffle(on));
    }

    /// Ends the stream and returns when the routing is restored.
    pub async fn stop(&self) {
        let (done, wait) = oneshot::channel();
        if self.commands.send(Command::Stop(done)).is_ok() {
            let _ = wait.await;
        }
    }

    /// Sends a room action of the window. Refused while disconnected (it would be lost) and
    /// for the types the window may not send ([`room::check_action`]).
    pub fn send_action(&self, action: serde_json::Value) -> Result<(), String> {
        const NOT_CONNECTED: &str = "Not connected";
        let streaming = {
            let status = self.status.borrow();
            if !status.connected {
                return Err(NOT_CONNECTED.to_string());
            }
            status.phase != Phase::Idle
        };
        room::check_action(&action, streaming)?;
        self.commands
            .send(Command::Action(action))
            .map_err(|_| NOT_CONNECTED.to_string())
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// The load states of the tracks that became current (app only; the CLI has none).
    pub fn tracks(&self) -> watch::Receiver<TrackStates> {
        self.tracks.clone()
    }

    /// Blocks for up to a few seconds; call it from a blocking context.
    pub fn list_apps(&self) -> Result<Vec<AppInfo>, AudioError> {
        self.backend.list_apps()
    }

    /// Stops (restoring the routing), closes the connection and ends all tasks. The track
    /// loader stops its downloads and decodes and clears the cache dir before this returns,
    /// so a following [`Self::connect`] never meets a file of this room.
    pub async fn shutdown(self) {
        self.stop().await;
        let _ = self.commands.send(Command::Shutdown);
        if let Some(task) = self.task {
            let _ = task.await;
        }
    }
}

fn store_f64(atomic: &std::sync::atomic::AtomicU64, value: f64) {
    atomic.store(value.to_bits(), Ordering::Relaxed);
}

fn load_f64(atomic: &std::sync::atomic::AtomicU64) -> f64 {
    f64::from_bits(atomic.load(Ordering::Relaxed))
}

fn mono_ms() -> f64 {
    mono_ns() as f64 / 1_000_000.0
}

/// From the audio task and the worker to the engine task.
enum Note {
    /// The audio session `session_id` cannot continue.
    Failed { session_id: u64, message: String },
    /// The output failed or did not open; it stays closed for this connection.
    OutputFailed { message: String },
}

struct Channels {
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedReceiver<ConnEvent>,
    blocks: mpsc::Receiver<Input>,
    notes: mpsc::UnboundedReceiver<Note>,
}

struct Tasks {
    connection: JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
    audio: JoinHandle<()>,
    /// The track loader; ends when the engine task drops its feed, after its feeder.
    loader: Option<JoinHandle<()>>,
}

struct EngineTask {
    engine: Engine,
    /// The app reports its output latency and nudge in its NTP requests; the CLI does not.
    kind: JoinKind,
    /// Dropped at shutdown: `run_connection` returns when it is gone.
    out_tx: Option<mpsc::Sender<Outgoing>>,
    /// Where the raw room messages go; `None` for the CLI, or once the receiver is gone.
    room_feed: Option<mpsc::UnboundedSender<serde_json::Value>>,
    /// The room messages for the track loader; `None` for the CLI, and from shutdown on.
    loader_feed: Option<mpsc::UnboundedSender<serde_json::Value>>,
    audio_tx: Option<mpsc::UnboundedSender<AudioCmd>>,
    status_tx: watch::Sender<Status>,
    shared: Arc<PlayoutShared>,
    /// The last measured mono→epoch offset.
    mono_epoch_offset_ms: f64,
    /// The last epoch→server offset from the engine.
    server_offset_ms: f64,
    offset_known: Arc<AtomicBool>,
    next_session_id: u64,
    /// The audio session that runs; failures of other sessions are stale.
    active_session_id: Option<u64>,
    outgoing_dropped: u64,
    /// The shuffle state, for the queue player.
    shuffle: Arc<AtomicBool>,
}

impl EngineTask {
    async fn run(mut self, mut ch: Channels, tasks: Tasks) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut remap = tokio::time::interval(MONO_EPOCH_INTERVAL);
        remap.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                command = ch.commands.recv() => match command {
                    Some(Command::Start { apps, delay_ms }) => {
                        self.feed(Input::UserStart { apps, delay_ms });
                    }
                    Some(Command::UpdateApps { apps }) => self.feed(Input::UserUpdateApps { apps }),
                    Some(Command::SetNudge { nudge_ms }) => {
                        store_f64(&self.shared.nudge_ms, nudge_ms);
                        self.feed(Input::UserNudge { nudge_ms });
                    }
                    Some(Command::SetShuffle(on)) => self.feed(Input::UserShuffle { on }),
                    // Actions bypass the engine: it has no state for them.
                    Some(Command::Action(action)) => self.send(Outgoing::Action(action)),
                    Some(Command::Stop(done)) => {
                        self.feed(Input::UserStop);
                        self.audio(AudioCmd::Barrier(done));
                    }
                    // A dropped handle shuts down too.
                    Some(Command::Shutdown) | None => break,
                },
                Some(event) = ch.events.recv() => self.on_conn_event(event),
                Some(block) = ch.blocks.recv() => self.feed(block),
                Some(note) = ch.notes.recv() => match note {
                    Note::Failed { session_id, message } => {
                        if self.active_session_id == Some(session_id) {
                            self.feed(Input::AudioFailed { message });
                        }
                    }
                    Note::OutputFailed { message } => self.feed(Input::OutputFailed { message }),
                },
                _ = tick.tick() => self.feed(Input::Tick { now_ms: mono_ms() }),
                _ = remap.tick() => self.remap_clock(),
            }
        }
        // The loader stops its feeder and its loads and clears the cache while the audio
        // stops.
        self.loader_feed = None;
        self.feed(Input::UserStop);
        // The queue track's feeder ends before the output closes.
        if let Some(loader) = tasks.loader {
            let _ = loader.await;
        }
        // The audio task finishes the queued calls (the stop above included), fades out and
        // closes the output, then ends.
        self.audio_tx = None;
        let _ = tasks.audio.await;
        let out_tx = self.out_tx.take();
        if let Some(out_tx) = &out_tx {
            // Let the queued messages (STOP_LIVE_STREAM) go out before the Close frame.
            let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
            while out_tx.capacity() < out_tx.max_capacity()
                && tokio::time::Instant::now() < deadline
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // The shutdown flag before the sender: a closed queue ends the connection without
        // the Close frame.
        let _ = tasks.shutdown_tx.send(true);
        let _ = tasks.connection.await;
        drop(out_tx);
    }

    /// Measures the mono→epoch offset again. On a wall-clock step the engine moves the server
    /// offset by the opposite amount, so server time stays continuous, and resyncs NTP.
    fn remap_clock(&mut self) {
        let offset_ms = MonoEpochMap::measure().to_epoch_ms(0);
        let step = clock_step_ms(self.mono_epoch_offset_ms, offset_ms);
        self.mono_epoch_offset_ms = offset_ms;
        if let Some(delta_ms) = step {
            eprintln!("streamer: the system clock stepped by {delta_ms:.1} ms");
            // Its `ServerOffset` publishes the new epoch part together with the moved server
            // part, never the new epoch part alone.
            self.feed(Input::ClockStepped { delta_ms });
        }
        self.publish_offset();
    }

    /// Stores the mono→server offset for the audio threads as one value. This task is its
    /// only writer.
    fn publish_offset(&self) {
        store_f64(
            &self.shared.mono_server_offset_ms,
            self.mono_epoch_offset_ms + self.server_offset_ms,
        );
    }

    fn on_conn_event(&mut self, event: ConnEvent) {
        let input = match event {
            // For the track loader and the window only: the engine gets the parsed copy, if
            // any, as the next event.
            ConnEvent::Room(message) => {
                if let Some(loader) = &self.loader_feed
                    && loader.send(message.clone()).is_err()
                {
                    self.loader_feed = None;
                }
                if let Some(feed) = &self.room_feed
                    && feed.send(message).is_err()
                {
                    self.room_feed = None;
                }
                return;
            }
            ConnEvent::Connected => Input::Connected,
            ConnEvent::Disconnected => Input::Disconnected,
            ConnEvent::ConnectFailed(error) => Input::ConnectFailed { error },
            ConnEvent::Message(message) => Input::Server(message),
            ConnEvent::NtpResponse {
                t0,
                t1,
                t2,
                t3,
                group_id,
                index,
            } => Input::NtpResponse {
                t0,
                t1,
                t2,
                t3,
                group_id,
                index,
            },
        };
        self.feed(input);
    }

    fn feed(&mut self, input: Input) {
        for output in self.engine.handle(input) {
            self.dispatch(output);
        }
    }

    fn dispatch(&mut self, output: Output) {
        match output {
            Output::SendJson(message) => self.send(Outgoing::Json(message)),
            Output::SendBinary(frame) => self.send(Outgoing::Binary(frame)),
            Output::SendProbePair {
                group_id,
                client_rtt,
                client_nudge_ms,
            } => {
                let render_ahead_ms = load_f64(&self.shared.render_ahead_ms);
                let client_compensation_ms =
                    client_compensation_ms(self.kind, render_ahead_ms, client_nudge_ms);
                self.send(Outgoing::ProbePair {
                    group_id,
                    client_rtt,
                    client_nudge_ms,
                    client_compensation_ms,
                });
            }
            Output::Nudge { nudge_ms } => store_f64(&self.shared.nudge_ms, nudge_ms),
            Output::StartAudio { apps, delay_ms } => {
                self.next_session_id += 1;
                let session_id = self.next_session_id;
                self.active_session_id = Some(session_id);
                self.audio(AudioCmd::Start {
                    session_id,
                    apps,
                    delay_ms,
                });
            }
            Output::UpdateAudioApps { apps } => {
                if let Some(session_id) = self.active_session_id {
                    self.audio(AudioCmd::Update { session_id, apps });
                }
            }
            Output::StopAudio => {
                self.active_session_id = None;
                self.audio(AudioCmd::Stop);
            }
            Output::ServerOffset { offset_ms } => {
                self.server_offset_ms = offset_ms;
                self.publish_offset();
                self.offset_known.store(true, Ordering::Release);
            }
            Output::SetPlayoutGain { gain, ramp_s } => {
                self.shared
                    .gain_ramp_s
                    .store(ramp_s.to_bits(), Ordering::Relaxed);
                self.shared.gain.store(gain.to_bits(), Ordering::Relaxed);
            }
            Output::SetLowPass { freq_hz, ramp_s } => {
                self.shared
                    .lowpass_ramp_s
                    .store(ramp_s.to_bits(), Ordering::Relaxed);
                self.shared
                    .lowpass_hz
                    .store(freq_hz.to_bits(), Ordering::Relaxed);
            }
            Output::SetMetronome { on } => self.shared.metronome.store(on, Ordering::Relaxed),
            Output::SetShuffle { on } => self.shuffle.store(on, Ordering::Relaxed),
            Output::Status(status) => {
                self.status_tx.send_replace(status);
            }
        }
    }

    /// Never waits: a full queue (a stalled socket) drops the message.
    fn send(&mut self, message: Outgoing) {
        let Some(out_tx) = &self.out_tx else { return };
        if out_tx.try_send(message).is_err() {
            self.outgoing_dropped += 1;
            if self.outgoing_dropped % 250 == 1 {
                eprintln!(
                    "streamer: outgoing queue full, {} messages dropped",
                    self.outgoing_dropped
                );
            }
        }
    }

    fn audio(&self, command: AudioCmd) {
        if let Some(audio_tx) = &self.audio_tx {
            let _ = audio_tx.send(command);
        }
    }
}

enum AudioCmd {
    Start {
        session_id: u64,
        apps: Vec<String>,
        delay_ms: u32,
    },
    Update {
        session_id: u64,
        apps: Vec<String>,
    },
    Stop,
    Barrier(oneshot::Sender<()>),
}

struct AudioContext {
    backend: Arc<dyn AudioBackend>,
    shared: Arc<PlayoutShared>,
    offset_known: Arc<AtomicBool>,
    /// The producer of the output's stream slot while no worker has it.
    stream_tx: StreamTx,
    /// A flush that timed out was logged.
    flush_timeout_logged: AtomicBool,
    blocks: mpsc::Sender<Input>,
    notes: mpsc::UnboundedSender<Note>,
}

/// The producer of the output's stream slot, shared by the stream sessions of a connection.
type StreamTx = Arc<Mutex<Option<rtrb::Producer<PcmBlock>>>>;

/// The stream slot's producer while a worker has it. Dropping the lease gives the producer
/// back, also when the worker panics or its thread does not start.
struct StreamLease {
    tx: Option<rtrb::Producer<PcmBlock>>,
    home: StreamTx,
}

impl StreamLease {
    fn take(home: &StreamTx) -> Self {
        let tx = home.lock().unwrap_or_else(PoisonError::into_inner).take();
        Self {
            tx,
            home: home.clone(),
        }
    }

    /// Queues `block` for the output; false when it is not queued (a full ring).
    fn push(&mut self, block: PcmBlock) -> bool {
        self.tx.as_mut().is_some_and(|tx| tx.push(block).is_ok())
    }
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            *self.home.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx);
        }
    }
}

/// One audio session: its worker thread.
struct Session {
    stop: Arc<AtomicBool>,
    worker: ThreadHandle<WorkerStats>,
}

/// Passes the output's failures to the engine task until the backend drops the output.
async fn forward_output_events(
    mut events: mpsc::UnboundedReceiver<AudioEvent>,
    notes: mpsc::UnboundedSender<Note>,
) {
    while let Some(event) = events.recv().await {
        let (AudioEvent::Failed(message) | AudioEvent::OutputFailed(message)) = event;
        if notes.send(Note::OutputFailed { message }).is_err() {
            return;
        }
    }
}

async fn audio_task(
    mut commands: mpsc::UnboundedReceiver<AudioCmd>,
    cx: AudioContext,
    output: Result<OutputIo, String>,
) {
    // Without an output the connection goes on, silent on this PC.
    let opened = match output {
        Ok(io) => {
            let backend = cx.backend.clone();
            blocking(move || backend.open_output(io)).await
        }
        Err(message) => Err(message),
    };
    if let Err(message) = opened {
        let _ = cx.notes.send(Note::OutputFailed { message });
    }
    let mut session: Option<Session> = None;
    while let Some(command) = commands.recv().await {
        match command {
            AudioCmd::Start {
                session_id,
                apps,
                delay_ms,
            } => {
                if session.is_some() {
                    stop_session(&cx, &mut session).await;
                }
                session = start_session(&cx, session_id, apps, delay_ms).await;
            }
            AudioCmd::Update { session_id, apps } => {
                if session.is_some() {
                    let backend = cx.backend.clone();
                    let result = blocking(move || backend.update_apps(apps)).await;
                    if let Err(message) = result {
                        let _ = cx.notes.send(Note::Failed {
                            session_id,
                            message,
                        });
                    }
                }
            }
            AudioCmd::Stop => stop_session(&cx, &mut session).await,
            AudioCmd::Barrier(done) => {
                let _ = done.send(());
            }
        }
    }
    // The engine task has ended: never leave the apps routed into the sink.
    if session.is_some() {
        stop_session(&cx, &mut session).await;
    }
    fade_out_output(&cx.shared).await;
    let backend = cx.backend.clone();
    if let Err(message) = blocking(move || backend.close_output()).await {
        eprintln!("streamer: closing the audio output failed: {message}");
    }
}

/// Ramps the output's gain to 0 over [`CLOSE_FADE_S`] and waits until the output has played
/// the ramp, so a running metronome kick, or any sound still queued, does not end with a
/// click when the output closes. The output renders [`PlayoutShared::render_ahead_ms`] ahead
/// of what is heard; a longer delay than [`CLOSE_FADE_MAX_AHEAD_MS`] is not waited for.
async fn fade_out_output(shared: &PlayoutShared) {
    shared
        .gain_ramp_s
        .store(CLOSE_FADE_S.to_bits(), Ordering::Relaxed);
    shared.gain.store(0.0_f32.to_bits(), Ordering::Relaxed);
    let ahead_ms = load_f64(&shared.render_ahead_ms);
    let ahead_ms = if ahead_ms.is_finite() {
        ahead_ms.clamp(0.0, CLOSE_FADE_MAX_AHEAD_MS)
    } else {
        0.0
    };
    let fade_ms = f64::from(CLOSE_FADE_S) * 1000.0;
    let wait_ms = ahead_ms + fade_ms + CLOSE_FADE_CYCLE_MS;
    tokio::time::sleep(Duration::from_secs_f64(wait_ms / 1000.0)).await;
}

/// Runs a backend call on the blocking pool; a panic becomes an error message.
async fn blocking(
    call: impl FnOnce() -> Result<(), AudioError> + Send + 'static,
) -> Result<(), String> {
    match tokio::task::spawn_blocking(call).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(e) => Err(format!("Audio backend call failed: {e}")),
    }
}

/// Creates a fresh capture ring and a worker, which takes the stream slot's producer, then
/// starts the backend. A failure is reported to the engine; the session still exists, so the
/// engine's stop cleans it up.
async fn start_session(
    cx: &AudioContext,
    session_id: u64,
    apps: Vec<String>,
    delay_ms: u32,
) -> Option<Session> {
    let (capture_tx, capture_rx) = rtrb::RingBuffer::<CapturedChunk>::new(CAPTURE_RING);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let notes = cx.notes.clone();
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            let note = match event {
                AudioEvent::Failed(message) => Note::Failed {
                    session_id,
                    message,
                },
                AudioEvent::OutputFailed(message) => Note::OutputFailed { message },
            };
            if notes.send(note).is_err() {
                return;
            }
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let worker = Worker {
        session_id,
        capture_rx,
        local_copy: StreamLease::take(&cx.stream_tx),
        shared: cx.shared.clone(),
        offset_known: cx.offset_known.clone(),
        blocks: cx.blocks.clone(),
        notes: cx.notes.clone(),
        delay_ms,
        stop: stop.clone(),
    };
    let worker = match std::thread::Builder::new()
        .name("streamer-audio".to_string())
        .spawn(move || worker.run())
    {
        Ok(handle) => handle,
        Err(e) => {
            let message = format!("Cannot start the audio worker: {e}");
            let _ = cx.notes.send(Note::Failed {
                session_id,
                message,
            });
            return None;
        }
    };

    let io = AudioIo {
        capture_tx,
        shared: cx.shared.clone(),
        events: events_tx,
    };
    let backend = cx.backend.clone();
    if let Err(message) = blocking(move || backend.start(apps, io)).await {
        let _ = cx.notes.send(Note::Failed {
            session_id,
            message,
        });
    }
    Some(Session { stop, worker })
}

/// Stops the backend (routing restored when it returns), then the worker, then drops the local
/// copy the output still has queued.
async fn stop_session(cx: &AudioContext, session: &mut Option<Session>) {
    let backend = cx.backend.clone();
    if let Err(message) = blocking(move || backend.stop()).await {
        eprintln!("streamer: stopping the audio failed: {message}");
    }
    let Some(Session { stop, worker }) = session.take() else {
        return;
    };
    stop.store(true, Ordering::Relaxed);
    if let Ok(Ok(stats)) = tokio::task::spawn_blocking(move || worker.join()).await {
        let capture_dropped = cx.shared.capture_dropped.load(Ordering::Relaxed);
        eprintln!(
            "streamer: audio stopped: {} blocks, {} local-copy drops, {} send drops, \
             {} chunks before the first server offset, {} capture drops in total",
            stats.blocks,
            stats.playout_dropped,
            stats.send_dropped,
            stats.no_offset_chunks,
            capture_dropped,
        );
    }
    // The worker has ended and its lease has given the producer back, so every block of this
    // session was pushed before the flush request.
    flush_local_copy(cx).await;
}

/// Asks the output to fade out and drop the queued local copy, and waits for it. An output
/// that does not run (closed, or no device) does the flush when it runs again.
async fn flush_local_copy(cx: &AudioContext) {
    let shared = cx.shared.clone();
    let seq = request_flush(&shared, Slot::Stream);
    let wait = Duration::from_millis(FLUSH_WAIT_MS);
    let flushed =
        tokio::task::spawn_blocking(move || wait_flushed(&shared, Slot::Stream, seq, wait))
            .await
            .unwrap_or(false);
    if !flushed && !cx.flush_timeout_logged.swap(true, Ordering::Relaxed) {
        eprintln!(
            "streamer: the audio output did not drop the local copy within {FLUSH_WAIT_MS} ms"
        );
    }
}

#[derive(Debug, Default)]
struct WorkerStats {
    blocks: u64,
    playout_dropped: u64,
    send_dropped: u64,
    no_offset_chunks: u64,
}

/// The audio worker of one session (a std thread: it must not wait on the async runtime).
struct Worker {
    session_id: u64,
    capture_rx: rtrb::Consumer<CapturedChunk>,
    local_copy: StreamLease,
    shared: Arc<PlayoutShared>,
    offset_known: Arc<AtomicBool>,
    blocks: mpsc::Sender<Input>,
    notes: mpsc::UnboundedSender<Note>,
    delay_ms: u32,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn run(mut self) -> WorkerStats {
        let mut stats = WorkerStats::default();
        let mut encoder = match OpusFrameEncoder::new(CHANNELS as u8) {
            Ok(encoder) => encoder,
            Err(e) => {
                let message = format!("Cannot create the Opus encoder: {e}");
                let _ = self.notes.send(Note::Failed {
                    session_id: self.session_id,
                    message,
                });
                return stats;
            }
        };
        let lookahead = encoder.lookahead_samples();
        let mut timeline =
            CaptureTimeline::new(FRAME_DURATION_MS, TIMELINE_ALPHA, TIMELINE_REANCHOR_MS);
        let mut assembler = BlockAssembler::new();
        while !self.stop.load(Ordering::Relaxed) {
            let Ok(chunk) = self.capture_rx.pop() else {
                std::thread::sleep(WORKER_IDLE);
                continue;
            };
            // Stamps need the server offset; until the first one, nothing is stamped.
            if !self.offset_known.load(Ordering::Acquire) {
                assembler.clear();
                stats.no_offset_chunks += 1;
                continue;
            }
            if assembler.is_gap(&chunk) {
                assembler.clear();
                timeline.reset();
            }
            assembler.push(&chunk, |first_ns, samples| {
                stats.blocks += 1;
                let stamp = timeline.stamp(self.shared.mono_to_server_ms(first_ns));
                match encoder.encode(samples) {
                    Ok(payload) => {
                        let block = Input::EncodedBlock {
                            capture_server_ms: stamp,
                            payload,
                        };
                        if self.blocks.try_send(block).is_err() {
                            stats.send_dropped += 1;
                        }
                    }
                    Err(e) => eprintln!("streamer: cannot encode a block: {e}"),
                }
                let block = PcmBlock {
                    target_start_server_ms: local_target_ms(stamp, self.delay_ms, lookahead),
                    samples: *samples,
                };
                if !self.local_copy.push(block) {
                    stats.playout_dropped += 1;
                }
            });
        }
        stats
    }
}

fn frames_to_ns(frames: usize) -> i64 {
    (frames as f64 * 1e9 / f64::from(SAMPLE_RATE)).round() as i64
}

/// Cuts captured chunks into 960-frame blocks and gives the CLOCK_MONOTONIC time of each
/// block's first frame.
struct BlockAssembler {
    samples: [f32; BLOCK_SAMPLES],
    /// Frames in `samples`.
    filled: usize,
    first_ns: i64,
    /// Where the next chunk should start; `None` before the first chunk.
    next_ns: Option<i64>,
}

impl BlockAssembler {
    fn new() -> Self {
        Self {
            samples: [0.0; BLOCK_SAMPLES],
            filled: 0,
            first_ns: 0,
            next_ns: None,
        }
    }

    /// The chunk does not continue the previous one: too much audio was lost before it, or
    /// its time does not follow the previous chunk and the lost frames (a capture restart).
    fn is_gap(&self, chunk: &CapturedChunk) -> bool {
        frames_to_ns(chunk.lost_frames) > MAX_SILENT_FILL_NS
            || self.next_ns.is_some_and(|next| {
                let expected_ns = next + frames_to_ns(chunk.lost_frames);
                (chunk.mono_ns_first_frame - expected_ns).abs() > CAPTURE_GAP_NS
            })
    }

    /// Forgets the partial block and the expected next chunk.
    fn clear(&mut self) {
        self.filled = 0;
        self.next_ns = None;
    }

    /// Adds the chunk. If it continues the previous chunk, silence comes first in place of the
    /// lost frames: the timestamps stay continuous, and every device plays the same silence.
    /// Check [`Self::is_gap`] first.
    fn push(&mut self, chunk: &CapturedChunk, mut emit: impl FnMut(i64, &[f32; BLOCK_SAMPLES])) {
        if let Some(next) = self.next_ns {
            self.append(next, None, chunk.lost_frames, &mut emit);
        }
        let frames = chunk.frames.min(CHUNK_MAX_FRAMES);
        let samples = &chunk.samples[..frames * CHANNELS];
        self.append(chunk.mono_ns_first_frame, Some(samples), frames, &mut emit);
        self.next_ns = Some(chunk.mono_ns_first_frame + frames_to_ns(frames));
    }

    /// Adds `frames` frames that start at `start_ns`: from `samples`, or silence for `None`.
    fn append(
        &mut self,
        start_ns: i64,
        samples: Option<&[f32]>,
        frames: usize,
        emit: &mut impl FnMut(i64, &[f32; BLOCK_SAMPLES]),
    ) {
        let mut index = 0;
        while index < frames {
            if self.filled == 0 {
                self.first_ns = start_ns + frames_to_ns(index);
            }
            let take = (FRAME_SAMPLES - self.filled).min(frames - index);
            let to = &mut self.samples[self.filled * CHANNELS..(self.filled + take) * CHANNELS];
            match samples {
                Some(from) => {
                    to.copy_from_slice(&from[index * CHANNELS..(index + take) * CHANNELS]);
                }
                None => to.fill(0.0),
            }
            self.filled += take;
            index += take;
            if self.filled == FRAME_SAMPLES {
                emit(self.first_ns, &self.samples);
                self.filled = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::{Mixer, RENDER_FRAMES, Slot};
    use crate::test_support::{
        FakeBackend, WAIT, accept, answer_ntp, connect_app, server, wait_until,
    };
    use crate::test_util::TempDir;
    use futures_util::{SinkExt, StreamExt};
    use std::time::Instant;

    #[test]
    fn local_target_has_no_nudge() {
        // Without the lookahead the laptop is 6.5 ms ahead of every phone. The mixer applies the
        // nudge to its output time, so a target with the nudge would move the local copy twice.
        assert_eq!(local_target_ms(1000.0, 1000, 312), 2006.5);
    }

    /// A chunk of `frames` stereo frames whose samples count up from `first_frame`.
    fn chunk(mono_ns: i64, first_frame: usize, frames: usize) -> CapturedChunk {
        let mut c = CapturedChunk {
            mono_ns_first_frame: mono_ns,
            frames,
            samples: [0.0; CHUNK_MAX_FRAMES * 2],
            lost_frames: 0,
        };
        for i in 0..frames {
            let v = (first_frame + i) as f32;
            c.samples[2 * i] = v;
            c.samples[2 * i + 1] = -v;
        }
        c
    }

    #[test]
    fn blocks_carry_their_first_frame_time_across_chunks() {
        // Stamping a block with its chunk's time instead of its own first frame would add up
        // to 21 ms of jitter that the timeline only partly hides.
        let mut asm = BlockAssembler::new();
        let t0 = 7_000_000_000_i64;
        let mut blocks: Vec<(i64, f32, f32)> = Vec::new();
        for n in 0..15 {
            let c = chunk(t0 + frames_to_ns(n * 1024), n * 1024, 1024);
            assert!(!asm.is_gap(&c));
            asm.push(&c, |first_ns, s| {
                blocks.push((first_ns, s[0], s[BLOCK_SAMPLES - 1]))
            });
        }
        assert_eq!(blocks.len(), 15 * 1024 / FRAME_SAMPLES);
        for (k, &(first_ns, first, last)) in blocks.iter().enumerate() {
            let expected_ns = t0 + frames_to_ns(k * FRAME_SAMPLES);
            assert!((first_ns - expected_ns).abs() <= 1, "block {k}");
            assert_eq!(
                first,
                (k * FRAME_SAMPLES) as f32,
                "block {k} starts in order"
            );
            assert_eq!(
                last,
                -(((k + 1) * FRAME_SAMPLES - 1) as f32),
                "block {k} ends"
            );
        }

        // A 100 ms jump with no lost frames counted (a capture restart): the partial block is
        // dropped, the next block starts after it.
        let resume_ns = t0 + frames_to_ns(15 * 1024) + 100_000_000;
        let c = chunk(resume_ns, 0, 1024);
        assert!(asm.is_gap(&c));
        asm.clear();
        blocks.clear();
        asm.push(&c, |first_ns, s| blocks.push((first_ns, s[0], 0.0)));
        assert_eq!(blocks, vec![(resume_ns, 0.0, 0.0)]);
    }

    #[test]
    fn a_long_loss_is_a_gap() {
        // Filled, a long loss goes out as a burst of silent blocks. A restart steps the
        // timestamps by far more than 50 ms, so every device resyncs.
        let mut asm = BlockAssembler::new();
        let t0 = 7_000_000_000_i64;
        asm.push(&chunk(t0, 0, 1024), |_, _| {});
        // 213 ms lost, and the time of the next chunk agrees.
        let mut resumed = chunk(t0 + frames_to_ns(11 * 1024), 11 * 1024, 1024);
        resumed.lost_frames = 10 * 1024;
        assert!(asm.is_gap(&resumed));
    }

    #[test]
    fn lost_audio_becomes_silence_in_time() {
        // The web receiver plays consecutive sequence numbers back to back through any
        // timestamp step below 50 ms, but the local copy resyncs on a step above 0.5 ms. So a
        // short loss must not step the timestamps: both play the lost frames as silence.
        let mut asm = BlockAssembler::new();
        let t0 = 7_000_000_000_i64;
        // Frames 1024..4096 (64 ms) are lost, more than the 50 ms time check.
        let mut resumed = chunk(t0 + frames_to_ns(4 * 1024), 4 * 1024, 1024);
        resumed.lost_frames = 3 * 1024;
        let chunks = [
            chunk(t0, 0, 1024),
            resumed,
            chunk(t0 + frames_to_ns(5 * 1024), 5 * 1024, 1024),
        ];
        let mut blocks: Vec<i64> = Vec::new();
        let mut left: Vec<f32> = Vec::new();
        for c in &chunks {
            assert!(!asm.is_gap(c), "chunk at frame {}", c.samples[0]);
            asm.push(c, |first_ns, s| {
                blocks.push(first_ns);
                left.extend(s.iter().step_by(CHANNELS));
            });
        }
        assert_eq!(blocks.len(), 6 * 1024 / FRAME_SAMPLES);
        for (k, &first_ns) in blocks.iter().enumerate() {
            let expected_ns = t0 + frames_to_ns(k * FRAME_SAMPLES);
            assert!((first_ns - expected_ns).abs() <= 1, "block {k}");
        }
        for (k, &v) in left.iter().enumerate() {
            let want = if (1024..4 * 1024).contains(&k) {
                0.0
            } else {
                k as f32
            };
            assert_eq!(v, want, "frame {k}");
        }
    }

    #[tokio::test]
    async fn room_feed_receives_room_messages() {
        let (listener, settings) = server().await;
        let (feed_tx, mut feed_rx) = mpsc::unbounded_channel();
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-feed");
        let handle = connect_app(settings, backend, Some(feed_tx), &cache).await;
        let mut ws = accept(&listener).await;
        let text = r#"{"type":"ROOM_EVENT","event":{"type":"CLIENT_CHANGE","clients":[{"username":"bob","clientId":"b","isAdmin":true,"joinedAt":1}]}}"#;
        ws.send(tokio_tungstenite::tungstenite::Message::text(text))
            .await
            .unwrap();
        let message = tokio::time::timeout(WAIT, feed_rx.recv())
            .await
            .expect("the feed gets the message")
            .unwrap();
        assert_eq!(
            message,
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn actions_are_refused_while_disconnected() {
        // A chat message or an admin change typed while the socket is down must show an
        // error, not vanish.
        let (listener, settings) = server().await;
        drop(listener);
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-refused");
        let handle = connect_app(settings, backend, None, &cache).await;
        let chat = serde_json::json!({ "type": "SEND_CHAT_MESSAGE", "text": "hi" });
        assert_eq!(handle.send_action(chat), Err("Not connected".to_string()));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_failed_connect_shows_the_reason_in_the_status() {
        // A wrong but parsable server address: the sync screen must say why it waits.
        let (listener, settings) = server().await;
        drop(listener);
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-connect-error");
        let handle = connect_app(settings, backend, None, &cache).await;
        let status = handle.status();
        wait_until("the status has the connect error", || {
            status
                .borrow()
                .message
                .as_deref()
                .is_some_and(|m| m.starts_with("Cannot reach the server: "))
        })
        .await;
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_stops_the_audio_before_returning() {
        // Leave while this PC streams: the join screen shows when `shutdown` returns, and the
        // apps must not stay routed into the sink.
        let (listener, settings) = server().await;
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-shutdown");
        let handle = connect_app(settings, backend.clone(), None, &cache).await;
        let _ws = accept(&listener).await;
        handle.start(vec!["app".to_string()], 1000);
        wait_until("the backend starts", || backend.calls().contains(&"start")).await;
        handle.shutdown().await;
        assert_eq!(
            backend.calls(),
            ["open_output", "start", "stop", "close_output"]
        );
    }

    #[tokio::test]
    async fn leave_fades_the_output_out_before_it_closes() {
        // A running metronome kick (or any sound still queued) must not end with a click:
        // the gain ramps to 0 over 5 ms, and the output closes after the ramp has played.
        let (listener, settings) = server().await;
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-fade");
        let handle = connect_app(settings, backend.clone(), None, &cache).await;
        let _ws = accept(&listener).await;
        wait_until("the output opens", || {
            backend.calls().contains(&"open_output")
        })
        .await;
        let (shared, _) = backend.output_track().expect("the output is open");
        let silent_at = std::thread::spawn(move || {
            let end = Instant::now() + WAIT;
            while Instant::now() < end {
                if f32::from_bits(shared.gain.load(Ordering::Relaxed)) == 0.0 {
                    return Some(Instant::now());
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            None
        });
        handle.shutdown().await;
        let closed = backend.output_closed().expect("the output closes");
        assert_eq!((closed.gain, closed.gain_ramp_s), (0.0, 0.005));
        let silent_at = silent_at.join().unwrap().expect("the gain goes to 0");
        let played = closed.at.saturating_duration_since(silent_at);
        assert!(
            played >= Duration::from_millis(5),
            "closed {played:?} after the fade began"
        );
    }

    #[tokio::test]
    async fn the_output_opens_on_connect_and_closes_on_shutdown() {
        // App and CLI: one output for the whole connection, open with no stream (the queue
        // track and the metronome need it), closed when the connection ends.
        for kind in [JoinKind::App, JoinKind::Streamer] {
            let (listener, settings) = server().await;
            let backend = Arc::new(FakeBackend::default());
            let cache = TempDir::new("runtime-output");
            let handle =
                StreamerHandle::connect_in(settings, backend.clone(), kind, None, cache.0.clone())
                    .await;
            let _ws = accept(&listener).await;
            wait_until("the output opens", || backend.calls() == ["open_output"]).await;
            // Only the app's output low-passes: the CLI plays its local copy unfiltered.
            let lowpass = backend.output_lowpass();
            assert_eq!(lowpass, Some(kind == JoinKind::App), "{kind:?}");
            handle.shutdown().await;
            assert_eq!(backend.calls(), ["open_output", "close_output"], "{kind:?}");
        }
    }

    #[tokio::test]
    async fn the_app_reports_its_compensation_and_the_cli_does_not() {
        // The server schedules a PLAY far enough ahead for the app's output delay and nudge.
        // The fake output measures no delay, so the app reports its nudge.
        for (kind, want) in [
            (JoinKind::App, Some(serde_json::json!(12.5))),
            (JoinKind::Streamer, None),
        ] {
            let (listener, mut settings) = server().await;
            settings.nudge_ms = 12.5;
            let backend = Arc::new(FakeBackend::default());
            let cache = TempDir::new("runtime-compensation");
            let handle =
                StreamerHandle::connect_in(settings, backend, kind, None, cache.0.clone()).await;
            let mut ws = accept(&listener).await;
            let request = tokio::time::timeout(WAIT, async {
                loop {
                    let message = ws.next().await.expect("open").expect("a message");
                    let tokio_tungstenite::tungstenite::Message::Text(text) = message else {
                        continue;
                    };
                    let value: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                    if value["type"] == "NTP_REQUEST" {
                        return value;
                    }
                }
            })
            .await
            .expect("an NTP request");
            assert_eq!(request["clientNudgeMs"], 12.5, "{kind:?}");
            assert_eq!(
                request.get("clientCompensationMs"),
                want.as_ref(),
                "{kind:?}"
            );
            handle.shutdown().await;
        }
    }

    /// Plays the output as PipeWire does: renders a part at the current server time every
    /// 5 ms until `stop` is set, then gives the mixer back.
    fn run_output(
        mut mixer: Mixer,
        shared: Arc<PlayoutShared>,
        stop: Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<Mixer> {
        std::thread::spawn(move || {
            let mut out = vec![0.0_f32; CHANNELS * RENDER_FRAMES];
            while !stop.load(Ordering::Relaxed) {
                mixer.render(&mut out, shared.mono_to_server_ms(mono_ns()));
                std::thread::sleep(Duration::from_millis(5));
            }
            mixer
        })
    }

    /// The loudest sample of the next `seconds` the mixer plays, rendered at once from now on.
    fn peak_ahead(mixer: &mut Mixer, shared: &PlayoutShared, seconds: f64) -> f32 {
        let start_ms = shared.mono_to_server_ms(mono_ns());
        let mut out = vec![0.0_f32; CHANNELS * RENDER_FRAMES];
        let parts = (seconds * f64::from(SAMPLE_RATE) / RENDER_FRAMES as f64).ceil() as usize;
        let mut peak = 0.0_f32;
        for k in 0..parts {
            let at_ms = start_ms + (k * RENDER_FRAMES) as f64 * 1000.0 / f64::from(SAMPLE_RATE);
            mixer.render(&mut out, at_ms);
            peak = out.iter().fold(peak, |p, s| p.max(s.abs()));
        }
        peak
    }

    /// Captures 200 ms of a constant `level` from now on, and waits until the worker has made
    /// blocks of it.
    async fn capture(tx: &mut rtrb::Producer<CapturedChunk>, level: f32) {
        let first_ns = mono_ns();
        for k in 0..10 {
            let mut chunk = CapturedChunk {
                mono_ns_first_frame: first_ns + frames_to_ns(k * FRAME_SAMPLES),
                frames: FRAME_SAMPLES,
                samples: [0.0; CHUNK_MAX_FRAMES * 2],
                lost_frames: 0,
            };
            chunk.samples[..FRAME_SAMPLES * CHANNELS].fill(level);
            assert!(tx.push(chunk).is_ok(), "the capture ring has room");
        }
        let capacity = tx.buffer().capacity();
        wait_until("the worker takes the captured audio", || {
            tx.slots() == capacity
        })
        .await;
        // The worker pushes a block right after it takes its chunk.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn stream_stop_flushes_the_local_copy() {
        // The local copy is queued a delay ahead. After a stop none of it may play, also not
        // seconds later or at the next start; and the next stream's local copy must play.
        let (listener, settings) = server().await;
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-flush");
        let handle = connect_app(settings, backend.clone(), None, &cache).await;
        let _ntp = answer_ntp(accept(&listener).await);
        let status = handle.status();
        wait_until("the server offset is known", || {
            status.borrow().offset_ms.is_some()
        })
        .await;
        wait_until("the output opens", || {
            backend.calls().contains(&"open_output")
        })
        .await;
        let output = backend.take_output().expect("the output ends");
        let shared = output.shared.clone();
        let mixer = Mixer::new(
            output.stream_rx,
            output.track_rx,
            output.metronome,
            shared.clone(),
            output.lowpass,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let player = run_output(mixer, shared.clone(), stop.clone());

        // Blocks are due 5 s after their capture: all are still queued at the stop.
        handle.start(vec!["app".to_string()], MAX_DELAY_MS);
        wait_until("the stream starts", || backend.calls().contains(&"start")).await;
        let mut io = backend.take_io().expect("the audio ends");
        capture(&mut io.capture_tx, 0.5).await;
        handle.stop().await;
        let stream = Slot::Stream as usize;
        let requested = shared.flush_requested[stream].load(Ordering::Acquire);
        let done = shared.flush_done[stream].load(Ordering::Acquire);
        assert_eq!((requested, done), (1, 1), "flushed before the stop returns");
        stop.store(true, Ordering::Relaxed);
        let mut mixer = player.join().unwrap();
        let peak = peak_ahead(&mut mixer, &shared, 6.0);
        assert!(
            peak < 1e-6,
            "no block of the stopped stream plays: peak {peak}"
        );

        handle.start(vec!["app".to_string()], MAX_DELAY_MS);
        wait_until("the stream starts again", || {
            backend.calls().iter().filter(|c| **c == "start").count() == 2
        })
        .await;
        let mut io = backend.take_io().expect("the audio ends");
        capture(&mut io.capture_tx, 0.5).await;
        let peak = peak_ahead(&mut mixer, &shared, 6.0);
        assert!(peak > 0.4, "the next stream plays: peak {peak}");
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_output_failure_does_not_stop_the_stream() {
        // The room still hears the stream when the PC's speakers fail; the status says why
        // the PC is silent.
        let (listener, settings) = server().await;
        let backend = Arc::new(FakeBackend::default());
        let cache = TempDir::new("runtime-output-failure");
        let handle = connect_app(settings, backend.clone(), None, &cache).await;
        let _ws = accept(&listener).await;
        wait_until("the output opens", || {
            backend.calls().contains(&"open_output")
        })
        .await;
        handle.start(vec!["app".to_string()], 1000);
        wait_until("the stream starts", || backend.calls().contains(&"start")).await;
        let output = backend.take_output().expect("the output ends");
        let failed = AudioEvent::OutputFailed("device gone".to_string());
        output.events.send(failed).unwrap();
        let status = handle.status();
        wait_until("the status shows the failure", || {
            status.borrow().message.as_deref() == Some("Audio output failed: device gone")
        })
        .await;
        assert_eq!(status.borrow().phase, Phase::Starting);
        assert_eq!(
            backend.calls(),
            ["open_output", "start"],
            "the stream goes on"
        );
        handle.shutdown().await;
    }
}
