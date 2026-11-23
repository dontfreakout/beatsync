//! Audio backend abstraction (spec §9.2): app listing, routing of app streams to a virtual
//! sink, capture of that sink, and the one output that plays the local copy and the queue
//! track. Linux uses PipeWire.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use serde::Serialize;

use crate::dsp::{LOW_PASS_MAX_HZ, Metronome};
use crate::playout::PcmBlock;

pub mod apps;
#[cfg(target_os = "linux")]
pub mod pipewire;

/// One application that plays audio, as the UI lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// The name the user selects; also the routing key ([`apps::app_key`]).
    pub name: String,
    pub binary: Option<String>,
    /// The UI shows the browser loop warning (spec §9.3, C9).
    pub is_browser: bool,
    /// Output streams of the app right now.
    pub streams: usize,
}

/// The user-facing error when the audio system does not fit (spec §9.9, exact copy).
pub const REQUIRED_VERSIONS: &str = "PipeWire 1.0 and WirePlumber 0.4.17 or later required";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioError {
    /// PipeWire or WirePlumber is too old, or WirePlumber is not running.
    UnsupportedVersions,
    /// No PipeWire server to connect to (e.g. a PulseAudio-only system). Shown as
    /// [`REQUIRED_VERSIONS`], with the reason on a second line.
    Unavailable(String),
    /// `start` while routing is already started.
    AlreadyStarted,
    /// Any other backend failure.
    Backend(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersions => f.write_str(REQUIRED_VERSIONS),
            Self::Unavailable(reason) => {
                write!(
                    f,
                    "{REQUIRED_VERSIONS}\nPipeWire is not available: {reason}"
                )
            }
            Self::AlreadyStarted => f.write_str("Audio routing is already started"),
            Self::Backend(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AudioError {}

/// Events from the audio backend to the runtime while started or while the output is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioEvent {
    /// Routing or capture cannot continue (for example, the sink vanished). The runtime
    /// maps it to the engine's `Input::AudioFailed` and then calls `stop`.
    Failed(String),
    /// The output failed and is closed; the text is the reason. Routing and capture go on.
    OutputFailed(String),
}

/// Most stereo frames in one [`CapturedChunk`].
pub const CHUNK_MAX_FRAMES: usize = 1024;

/// Captured PCM from the sink's monitor, stereo interleaved F32 at 48 kHz.
#[derive(Clone, Copy)]
pub struct CapturedChunk {
    /// CLOCK_MONOTONIC time of the first frame, in ns.
    pub mono_ns_first_frame: i64,
    /// Valid stereo frames in `samples` (at most [`CHUNK_MAX_FRAMES`]).
    pub frames: usize,
    pub samples: [f32; CHUNK_MAX_FRAMES * 2],
    /// Stereo frames of captured audio lost just before this chunk (a full ring or no
    /// timing). The worker plays a short loss as silence.
    pub lost_frames: usize,
}

/// Values the runtime updates and the real-time playback callback reads.
#[derive(Debug)]
pub struct PlayoutShared {
    /// f64 bits: server time minus CLOCK_MONOTONIC time, in ms. It is the mono→epoch offset
    /// plus the epoch→server offset, stored as one value: a reader never combines a new part
    /// with an old one (a wall-clock step changes both parts).
    pub mono_server_offset_ms: AtomicU64,
    /// f32 bits: output gain. 0 (the default) is silence.
    pub gain: AtomicU32,
    /// f32 bits: ramp duration of gain changes, in s.
    pub gain_ramp_s: AtomicU32,
    /// f64 bits: the nudge in ms (positive plays earlier). The output reads it for every
    /// buffer and applies it to every source and to the metronome.
    pub nudge_ms: AtomicU64,
    /// Captured chunks the capture callback dropped (full ring or no timing).
    pub capture_dropped: AtomicU64,
    /// f32 bits: the low-pass cutoff in Hz. 20000 (the default) is the "off" value.
    pub lowpass_hz: AtomicU32,
    /// f32 bits: ramp duration of cutoff changes, in s.
    pub lowpass_ramp_s: AtomicU32,
    /// The output adds the metronome kick.
    pub metronome: AtomicBool,
    /// Per output slot ([`crate::mixer::Slot`]): flush requests so far, a wrapping count.
    pub flush_requested: [AtomicU32; 2],
    /// Per output slot: the last flush request the output has done.
    pub flush_done: [AtomicU32; 2],
    /// Per output slot, f64 bits: the server time at which a pending flush stops the slot
    /// (the output plays on until then and fades out over the 5 ms before it); NaN (the
    /// default) flushes at once. Written before the flush request; the output clears it when
    /// the flush is done.
    pub stop_at_ms: [AtomicU64; 2],
    /// f64 bits: how far ahead of now, in ms, the first frame of the output's last render is
    /// heard (device delay and buffered frames); 0 until the output has timing. A cycle
    /// without timing keeps the last value.
    pub render_ahead_ms: AtomicU64,
}

impl Default for PlayoutShared {
    fn default() -> Self {
        Self {
            mono_server_offset_ms: AtomicU64::new(0),
            gain: AtomicU32::new(0.0_f32.to_bits()),
            gain_ramp_s: AtomicU32::new(0.0_f32.to_bits()),
            nudge_ms: AtomicU64::new(0.0_f64.to_bits()),
            capture_dropped: AtomicU64::new(0),
            lowpass_hz: AtomicU32::new(LOW_PASS_MAX_HZ.to_bits()),
            lowpass_ramp_s: AtomicU32::new(0.0_f32.to_bits()),
            metronome: AtomicBool::new(false),
            flush_requested: [AtomicU32::new(0), AtomicU32::new(0)],
            flush_done: [AtomicU32::new(0), AtomicU32::new(0)],
            stop_at_ms: [
                AtomicU64::new(f64::NAN.to_bits()),
                AtomicU64::new(f64::NAN.to_bits()),
            ],
            render_ahead_ms: AtomicU64::new(0.0_f64.to_bits()),
        }
    }
}

impl PlayoutShared {
    /// Real-time safe.
    pub fn nudge_ms(&self) -> f64 {
        f64::from_bits(self.nudge_ms.load(Ordering::Relaxed))
    }

    /// Server time in ms of a CLOCK_MONOTONIC time. Real-time safe.
    pub fn mono_to_server_ms(&self, mono_ns: i64) -> f64 {
        let offset_ms = f64::from_bits(self.mono_server_offset_ms.load(Ordering::Relaxed));
        mono_ns as f64 / 1_000_000.0 + offset_ms
    }
}

/// The audio ends the backend owns while started; the runtime creates one per `start`.
pub struct AudioIo {
    pub capture_tx: rtrb::Producer<CapturedChunk>,
    pub shared: Arc<PlayoutShared>,
    pub events: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
}

/// What the output plays, and where its failure goes. The runtime creates one per connection.
pub struct OutputIo {
    /// Blocks of the local copy ([`crate::mixer::Slot::Stream`]).
    pub stream_rx: rtrb::Consumer<PcmBlock>,
    /// Blocks of the queue track ([`crate::mixer::Slot::Track`]).
    pub track_rx: rtrb::Consumer<PcmBlock>,
    pub metronome: Metronome,
    /// The mix goes through the low-pass filter (the app). The CLI plays its local copy
    /// unfiltered.
    pub lowpass: bool,
    pub shared: Arc<PlayoutShared>,
    pub events: tokio::sync::mpsc::UnboundedSender<AudioEvent>,
}

pub trait AudioBackend: Send + Sync {
    /// Apps that play audio now, sorted by name, without the streamer itself.
    fn list_apps(&self) -> Result<Vec<AppInfo>, AudioError>;
    /// Creates the sink and moves the streams of `apps` (names from [`AppInfo::name`]) to it.
    /// Then captures the sink into `io.capture_tx`. Failures after the answer arrive as
    /// [`AudioEvent::Failed`]: among them "No audio output device for the local copy" when the
    /// default output is one of this process's sinks (the output would play into the
    /// capture), in which case nothing is captured.
    fn start(&self, apps: Vec<String>, io: AudioIo) -> Result<(), AudioError>;
    /// Changes the selection while started; does nothing when stopped.
    fn update_apps(&self, apps: Vec<String>) -> Result<(), AudioError>;
    /// Stops the capture, moves the streams back, clears the app's routing entries and
    /// removes the sink. A stream the user moved elsewhere meanwhile keeps the user's target.
    /// The output stays open. Returns when this is done; does nothing when stopped.
    fn stop(&self) -> Result<(), AudioError>;
    /// Opens the output: one playback stream on the default output, which follows it when it
    /// changes, and plays `io` through a [`crate::mixer::Mixer`]. An open output is closed
    /// first. Works with or without a running `start`. Failures after the answer arrive as
    /// [`AudioEvent::OutputFailed`]; they close the output and leave routing and capture
    /// running.
    fn open_output(&self, io: OutputIo) -> Result<(), AudioError>;
    /// Closes the output; does nothing when none is open.
    fn close_output(&self) -> Result<(), AudioError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pipewire_server_shows_the_required_versions() {
        // Spec §9.9: a PulseAudio-only system shows exactly this sentence; the reason follows
        // on a second line.
        let text =
            AudioError::Unavailable("cannot connect to the PipeWire server: x".into()).to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "PipeWire 1.0 and WirePlumber 0.4.17 or later required",
                "PipeWire is not available: cannot connect to the PipeWire server: x",
            ]
        );
    }

    #[test]
    fn mono_to_server_adds_the_offset() {
        // A sign error here shifts the local copy against the room.
        let shared = PlayoutShared::default();
        let offset = 1_700_000_000_000.0_f64 - 50.0;
        shared
            .mono_server_offset_ms
            .store(offset.to_bits(), Ordering::Relaxed);
        let server = shared.mono_to_server_ms(2_000_000_000);
        assert!((server - (offset + 2000.0)).abs() < 1e-3, "{server}");
    }
}
