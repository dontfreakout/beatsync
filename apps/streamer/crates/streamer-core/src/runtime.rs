//! Streamer runtime (spec §9.5–§9.6, §10.1): wires the engine, the WebSocket connection and
//! the audio backend.
//!
//! - The engine task owns the [`Engine`]. It feeds it a 10 ms tick, the connection's events,
//!   the user's commands and the audio worker's encoded blocks, and executes its outputs.
//! - The connection task is [`run_connection`]; it ends when the engine task drops its
//!   [`Outgoing`] sender at shutdown.
//! - The audio task runs the backend calls one after another through `spawn_blocking` (each
//!   can block for seconds), so a stop never overtakes the start before it.
//! - The audio worker (one std thread per audio session) cuts captured chunks into 20 ms
//!   blocks, stamps them in server time, encodes them for the room and queues the raw PCM for
//!   the local copy.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle as ThreadHandle;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::audio::{
    AppInfo, AudioBackend, AudioError, AudioEvent, AudioIo, CHUNK_MAX_FRAMES, CapturedChunk,
    PlayoutShared,
};
use crate::clock::{MonoEpochMap, clock_step_ms, mono_ns};
use crate::config::Settings;
use crate::connection::{ConnEvent, Outgoing, build_ws_url, run_connection};
use crate::encoder::OpusFrameEncoder;
use crate::engine::{Engine, Input, Output, Phase, Status};
use crate::frame::{FRAME_DURATION_MS, FRAME_SAMPLES, MAX_DELAY_MS, MIN_DELAY_MS, SAMPLE_RATE};
use crate::playout::{BLOCK_SAMPLES, CHANNELS, PcmBlock};
use crate::timeline::CaptureTimeline;

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
/// 512 blocks of 20 ms: the largest delay (5 s) plus 1 s, with room to spare.
const PLAYOUT_RING: usize = 512;
const TIMELINE_ALPHA: f64 = 0.002;
const TIMELINE_REANCHOR_MS: f64 = 50.0;
/// A larger jump between consecutive chunks restarts the block and the timeline.
const CAPTURE_GAP_NS: i64 = 50_000_000;
/// The worker sleeps this long when the capture ring is empty.
const WORKER_IDLE: Duration = Duration::from_millis(5);
/// At shutdown, how long queued messages (e.g. STOP_LIVE_STREAM) may take to go out.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Server time at which the local copy plays the first frame of a block stamped `stamp_ms`:
/// the room's delay plus the encoder lookahead (receivers hear the decoded Opus that late)
/// plus the manual local offset.
pub fn local_target_ms(
    stamp_ms: f64,
    delay_ms: u32,
    lookahead_samples: u32,
    local_offset_ms: f64,
) -> f64 {
    let lookahead_ms = f64::from(lookahead_samples) * 1000.0 / f64::from(SAMPLE_RATE);
    stamp_ms + f64::from(delay_ms) + lookahead_ms + local_offset_ms
}

/// A running streamer: one server connection, and an audio session while streaming.
pub struct StreamerHandle {
    commands: mpsc::UnboundedSender<Command>,
    status: watch::Receiver<Status>,
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
    /// Answered when the audio task has done every call queued before it.
    Stop(oneshot::Sender<()>),
    Shutdown,
}

impl StreamerHandle {
    /// Connects to the room in `settings` (and reconnects until [`Self::shutdown`]). An
    /// invalid server URL gives a handle that only reports the error in its status.
    pub async fn connect(settings: Settings, backend: Arc<dyn AudioBackend>) -> Self {
        let (status_tx, status) = watch::channel(Status {
            connected: false,
            synced: false,
            synced_once: false,
            rtt_ms: None,
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
        ) {
            Ok(url) => url,
            Err(e) => {
                status_tx.send_modify(|s| s.message = Some(e.to_string()));
                return Self {
                    commands,
                    status,
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
        let offset_known = Arc::new(AtomicBool::new(false));

        let (out_tx, out_rx) = mpsc::channel(OUTGOING_CAPACITY);
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let connection = tokio::spawn(run_connection(url, out_rx, ev_tx, shutdown_rx));

        let (blocks_tx, blocks_rx) = mpsc::channel(BLOCKS_CAPACITY);
        let (notes_tx, notes_rx) = mpsc::unbounded_channel();
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let audio = tokio::spawn(audio_task(
            audio_rx,
            AudioContext {
                backend: backend.clone(),
                shared: shared.clone(),
                offset_known: offset_known.clone(),
                blocks: blocks_tx,
                notes: notes_tx,
                local_offset_ms: settings.local_offset_ms,
            },
        ));

        let engine = EngineTask {
            engine: Engine::new(settings.client_id, settings.username),
            out_tx: Some(out_tx),
            audio_tx: Some(audio_tx),
            status_tx,
            shared,
            mono_epoch_offset_ms,
            server_offset_ms: 0.0,
            offset_known,
            next_session_id: 0,
            active_session_id: None,
            outgoing_dropped: 0,
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
            },
        ));
        Self {
            commands,
            status,
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

    /// Ends the stream and returns when the routing is restored.
    pub async fn stop(&self) {
        let (done, wait) = oneshot::channel();
        if self.commands.send(Command::Stop(done)).is_ok() {
            let _ = wait.await;
        }
    }

    pub fn status(&self) -> watch::Receiver<Status> {
        self.status.clone()
    }

    /// Blocks for up to a few seconds; call it from a blocking context.
    pub fn list_apps(&self) -> Result<Vec<AppInfo>, AudioError> {
        self.backend.list_apps()
    }

    /// Stops (restoring the routing), closes the connection and ends all tasks.
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

fn mono_ms() -> f64 {
    mono_ns() as f64 / 1_000_000.0
}

/// From the audio task and the worker to the engine task.
enum Note {
    /// The audio session `session_id` cannot continue.
    Failed { session_id: u64, message: String },
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
}

struct EngineTask {
    engine: Engine,
    /// Dropped at shutdown: `run_connection` returns when it is gone.
    out_tx: Option<mpsc::Sender<Outgoing>>,
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
                    Some(Command::Stop(done)) => {
                        self.feed(Input::UserStop);
                        self.audio(AudioCmd::Barrier(done));
                    }
                    // A dropped handle shuts down too.
                    Some(Command::Shutdown) | None => break,
                },
                Some(event) = ch.events.recv() => self.feed(conn_input(event)),
                Some(block) = ch.blocks.recv() => self.feed(block),
                Some(note) = ch.notes.recv() => match note {
                    Note::Failed { session_id, message } => {
                        if self.active_session_id == Some(session_id) {
                            self.feed(Input::AudioFailed { message });
                        }
                    }
                },
                _ = tick.tick() => self.feed(Input::Tick { now_ms: mono_ms() }),
                _ = remap.tick() => self.remap_clock(),
            }
        }
        self.feed(Input::UserStop);
        // The audio task finishes the queued calls (the stop above included), then ends.
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
            } => self.send(Outgoing::ProbePair {
                group_id,
                client_rtt,
            }),
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

fn conn_input(event: ConnEvent) -> Input {
    match event {
        ConnEvent::Connected => Input::Connected,
        ConnEvent::Disconnected => Input::Disconnected,
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
    blocks: mpsc::Sender<Input>,
    notes: mpsc::UnboundedSender<Note>,
    local_offset_ms: f64,
}

/// One audio session: its worker thread.
struct Session {
    stop: Arc<AtomicBool>,
    worker: ThreadHandle<WorkerStats>,
}

async fn audio_task(mut commands: mpsc::UnboundedReceiver<AudioCmd>, cx: AudioContext) {
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

/// Creates fresh rings and a worker, then starts the backend. A failure is reported to the
/// engine; the session still exists, so the engine's stop cleans it up.
async fn start_session(
    cx: &AudioContext,
    session_id: u64,
    apps: Vec<String>,
    delay_ms: u32,
) -> Option<Session> {
    let (capture_tx, capture_rx) = rtrb::RingBuffer::<CapturedChunk>::new(CAPTURE_RING);
    let (playout_tx, playout_rx) = rtrb::RingBuffer::<PcmBlock>::new(PLAYOUT_RING);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let notes = cx.notes.clone();
    tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            let AudioEvent::Failed(message) = event;
            if notes
                .send(Note::Failed {
                    session_id,
                    message,
                })
                .is_err()
            {
                return;
            }
        }
    });
    let stop = Arc::new(AtomicBool::new(false));
    let worker = Worker {
        session_id,
        capture_rx,
        playout_tx,
        shared: cx.shared.clone(),
        offset_known: cx.offset_known.clone(),
        blocks: cx.blocks.clone(),
        notes: cx.notes.clone(),
        delay_ms,
        local_offset_ms: cx.local_offset_ms,
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
        playout_rx,
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

/// Stops the backend (routing restored when it returns), then the worker.
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
    playout_tx: rtrb::Producer<PcmBlock>,
    shared: Arc<PlayoutShared>,
    offset_known: Arc<AtomicBool>,
    blocks: mpsc::Sender<Input>,
    notes: mpsc::UnboundedSender<Note>,
    delay_ms: u32,
    local_offset_ms: f64,
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
                    target_start_server_ms: local_target_ms(
                        stamp,
                        self.delay_ms,
                        lookahead,
                        self.local_offset_ms,
                    ),
                    samples: *samples,
                };
                if self.playout_tx.push(block).is_err() {
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

    /// The chunk does not continue the previous one: audio was lost before it, or its time
    /// does not follow (a capture restart).
    fn is_gap(&self, chunk: &CapturedChunk) -> bool {
        chunk.after_loss
            || self
                .next_ns
                .is_some_and(|next| (chunk.mono_ns_first_frame - next).abs() > CAPTURE_GAP_NS)
    }

    /// Forgets the partial block and the expected next chunk.
    fn clear(&mut self) {
        self.filled = 0;
        self.next_ns = None;
    }

    fn push(&mut self, chunk: &CapturedChunk, mut emit: impl FnMut(i64, &[f32; BLOCK_SAMPLES])) {
        let frames = chunk.frames.min(CHUNK_MAX_FRAMES);
        self.next_ns = Some(chunk.mono_ns_first_frame + frames_to_ns(frames));
        let mut index = 0;
        while index < frames {
            if self.filled == 0 {
                self.first_ns = chunk.mono_ns_first_frame + frames_to_ns(index);
            }
            let take = (FRAME_SAMPLES - self.filled).min(frames - index);
            self.samples[self.filled * CHANNELS..(self.filled + take) * CHANNELS]
                .copy_from_slice(&chunk.samples[index * CHANNELS..(index + take) * CHANNELS]);
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

    #[test]
    fn local_target_includes_lookahead() {
        // Without the lookahead the laptop is 6.5 ms ahead of every phone.
        assert_eq!(local_target_ms(1000.0, 1000, 312, 0.0), 2006.5);
        assert_eq!(local_target_ms(1000.0, 1000, 312, 40.0), 2046.5);
    }

    /// A chunk of `frames` stereo frames whose samples count up from `first_frame`.
    fn chunk(mono_ns: i64, first_frame: usize, frames: usize) -> CapturedChunk {
        let mut c = CapturedChunk {
            mono_ns_first_frame: mono_ns,
            frames,
            samples: [0.0; CHUNK_MAX_FRAMES * 2],
            after_loss: false,
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

        // 100 ms of lost audio: the partial block is dropped, the next block starts after it.
        let resume_ns = t0 + frames_to_ns(15 * 1024) + 100_000_000;
        let c = chunk(resume_ns, 0, 1024);
        assert!(asm.is_gap(&c));
        asm.clear();
        blocks.clear();
        asm.push(&c, |first_ns, s| blocks.push((first_ns, s[0], 0.0)));
        assert_eq!(blocks, vec![(resume_ns, 0.0, 0.0)]);
    }

    #[test]
    fn a_chunk_after_lost_audio_is_a_gap() {
        // One lost 1024-frame chunk moves the next chunk by only 21 ms. Without the mark the
        // assembler joins audio across the hole, and the timeline stamps the following
        // blocks up to 21 ms early for seconds.
        let mut asm = BlockAssembler::new();
        let t0 = 7_000_000_000_i64;
        asm.push(&chunk(t0, 0, 1024), |_, _| {});
        let mut next = chunk(t0 + frames_to_ns(2 * 1024), 2 * 1024, 1024);
        assert!(!asm.is_gap(&next), "too short for the time check");
        next.after_loss = true;
        assert!(asm.is_gap(&next));
    }
}
