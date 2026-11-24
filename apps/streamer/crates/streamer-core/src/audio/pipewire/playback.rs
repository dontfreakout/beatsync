//! The output (spec §9.6): the one playback stream of a connection. Its callback is the
//! [`Mixer`], which plays every frame at its target server time, measured against PipeWire's
//! output timing.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ::pipewire as pw;
use pw::core::CoreRc;
use pw::properties::{PropertiesBox, properties};
use pw::spa::utils::Direction;
use pw::stream::{Stream, StreamState};

use super::OWN_APP_NAME;
use super::media::{
    self, CHANNELS, LATENCY, MediaStream, STRIDE, StreamSpec, Timing, frames_to_ns,
};
use super::routing::is_own_sink;
use crate::audio::PlayoutShared;
use crate::clock::mono_ns;
use crate::mixer::{Mixer, RENDER_FRAMES};

pub(super) const PLAYBACK_NAME: &str = "beatsync-local";

/// The failure of a stream start when the output would play into the streamer's own sink.
pub(super) const NO_LOCAL_OUTPUT: &str = "No audio output device for the local copy";

pub(super) type PlaybackStream = MediaStream<PlaybackState>;

/// The real-time state of the playback callback.
pub(super) struct PlaybackState {
    mixer: Mixer,
    shared: Arc<PlayoutShared>,
    pcm: Box<[f32; RENDER_FRAMES * CHANNELS]>,
}

/// Whether the output may play to the default output while this process streams: not when
/// that is one of this process's sinks, because the output would loop into the capture
/// (ruling R11-8). `default_audio_sink` is the `default.audio.sink` value of the `default`
/// metadata, `{"name":"<node.name>"}`.
pub(super) fn local_copy_allowed(default_audio_sink: Option<&str>, own_pid: u32) -> bool {
    let Some(value) = default_audio_sink else {
        return true;
    };
    let json: Option<serde_json::Value> = serde_json::from_str(value).ok();
    let name = json
        .as_ref()
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or(value);
    !is_own_sink(name, own_pid)
}

fn playback_properties(target: Option<&str>) -> PropertiesBox {
    let mut props = properties! {
        // Without it, PipeWire names the node after the application.
        *pw::keys::NODE_NAME => PLAYBACK_NAME,
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::NODE_LATENCY => LATENCY,
        // Excluded from the app list and from routing.
        *pw::keys::APP_NAME => OWN_APP_NAME,
        // Without a stored target the stream always follows the default output. WirePlumber
        // 0.4 ignores these string flags (spec 8.3, accepted difference): a volume set on the
        // stream outside the app comes back on the next run, and a playback that was moved
        // outside the app can go to that output on the next run.
        "state.restore-props" => "false",
        "state.restore-target" => "false",
    };
    if let Some(target) = target {
        props.insert(*pw::keys::TARGET_OBJECT, target);
        // A missing target must never send the sound to the default output, and the session
        // manager must not move the stream. WirePlumber 0.4 knows only `node.dont-reconnect`,
        // 0.5 knows both keys; a move from outside the app is ignored.
        props.insert("node.dont-fallback", "true");
        props.insert("node.dont-reconnect", "true");
    }
    props
}

/// Plays `mixer` so that its frames are heard at their server time (`shared` maps the
/// output's clock to server time). Without `target` (a `node.name`) the stream goes to the
/// default output and follows it when it changes.
pub(super) fn connect_playback(
    core: &CoreRc,
    target: Option<&str>,
    mixer: Mixer,
    shared: Arc<PlayoutShared>,
    id: u64,
    on_state: impl FnMut(StreamState) + 'static,
) -> Result<PlaybackStream, String> {
    let spec = StreamSpec {
        name: PLAYBACK_NAME,
        properties: playback_properties(target),
        direction: Direction::Output,
        id,
    };
    let state = PlaybackState {
        mixer,
        shared,
        pcm: Box::new([0.0; RENDER_FRAMES * CHANNELS]),
    };
    media::connect(core, spec, state, process, on_state)
}

/// CLOCK_MONOTONIC ns at which the first frame queued in the cycle `t` is heard (research §7,
/// PipeWire's `pw_time` docs): `now + (delay − elapsed)·1e9/rate + buffered/48000·1e9`, with
/// `elapsed` the ticks since the cycle's snapshot. It cancels out, so the result is
/// `t.now + delay + buffered`.
///
/// `now_ns` is a fresh CLOCK_MONOTONIC read. The callback takes it from [`mono_ns`], not from
/// `pw_stream_get_nsec`: that function is `clock_gettime(CLOCK_MONOTONIC)` in PipeWire, but it
/// exists only since libpipewire 1.1, and a reference to it would stop the binary from loading
/// on PipeWire 1.0 (spec §9.9 supports 1.0).
pub(super) fn heard_at_mono_ns(t: &Timing, now_ns: i64) -> i64 {
    let tick_ns = t.tick_ns();
    let elapsed = (now_ns - t.now_ns) as f64 / tick_ns;
    let latency = (t.delay as f64 - elapsed) * tick_ns + frames_to_ns(t.buffered);
    now_ns + latency.round() as i64
}

impl PlaybackState {
    /// Fills `out` (interleaved F32LE stereo) with the frames heard from `heard_at_ns` on.
    fn render(&mut self, out: &mut [u8], heard_at_ns: i64) {
        let mut done = 0;
        for part in out.chunks_mut(RENDER_FRAMES * STRIDE) {
            let frames = part.len() / STRIDE;
            let pcm = &mut self.pcm[..frames * CHANNELS];
            let at_ns = heard_at_ns + frames_to_ns(done as u64).round() as i64;
            self.mixer.render(pcm, self.shared.mono_to_server_ms(at_ns));
            for (le, sample) in part.as_chunks_mut::<4>().0.iter_mut().zip(pcm.iter()) {
                *le = sample.to_le_bytes();
            }
            done += frames;
        }
    }
}

/// Stores how far ahead of `now_ns` the first frame of this cycle is heard: the track feeder
/// keeps its blocks ahead of what the output renders, and the app reports it to the server.
/// A cycle without timing keeps the last value. Real-time safe.
fn publish_render_ahead(shared: &PlayoutShared, heard_at_ns: Option<i64>, now_ns: i64) {
    if let Some(at) = heard_at_ns {
        let ahead_ms = (at - now_ns) as f64 / 1e6;
        shared
            .render_ahead_ms
            .store(ahead_ms.to_bits(), Ordering::Relaxed);
    }
}

/// The real-time playback callback: one buffer per cycle, silence on any error.
fn process(stream: &Stream, st: &mut PlaybackState) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = usize::try_from(buffer.requested()).unwrap_or(usize::MAX);
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let now_ns = mono_ns();
    let heard_at = Timing::of(stream).map(|t| heard_at_mono_ns(&t, now_ns));
    publish_render_ahead(&st.shared, heard_at, now_ns);
    let frames = match data.data() {
        Some(bytes) => {
            let capacity = bytes.len() / STRIDE;
            let frames = if requested == 0 {
                capacity
            } else {
                requested.min(capacity)
            };
            let out = &mut bytes[..frames * STRIDE];
            match heard_at {
                Some(at) => st.render(out, at),
                None => {
                    out.fill(0);
                    // The slots did not move: realign exactly at the next timed cycle.
                    st.mixer.reset();
                }
            }
            frames
        }
        None => 0,
    };
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = STRIDE as i32;
    *chunk.size_mut() = (frames * STRIDE) as u32;
    // Dropping `buffer` queues it.
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    #[test]
    fn only_a_pinned_output_refuses_other_targets() {
        ::pipewire::init();
        let pinned = playback_properties(Some("beatsync-test-out"));
        assert_eq!(pinned.get("node.dont-fallback"), Some("true"));
        assert_eq!(pinned.get("node.dont-reconnect"), Some("true"));
        // C3: without a target the output follows the default output, also when it changes.
        let follows = playback_properties(None);
        assert_eq!(follows.get("target.object"), None);
        assert_eq!(follows.get("node.dont-fallback"), None);
        assert_eq!(follows.get("node.dont-reconnect"), None);
    }

    #[test]
    fn heard_at_adds_device_delay_and_resampler_frames() {
        // A sign error is silent on null sinks (delay 0) and makes the laptop early or late on
        // real devices. 2400 ticks at 1/48000 = 50 ms; 48 frames in the resampler = 1 ms.
        let t = Timing {
            now_ns: 5_000 * MS,
            rate_num: 1,
            rate_denom: 48_000,
            delay: 2400,
            buffered: 48,
        };
        for late_ms in [0, 3, 7] {
            let now = t.now_ns + late_ms * MS;
            assert_eq!(heard_at_mono_ns(&t, now), 5_051 * MS, "{late_ms} ms late");
        }
    }

    #[test]
    fn a_cycle_without_timing_keeps_the_render_ahead() {
        // The feeder and the server report use the last measured delay; a cycle without
        // timing must not make it 0.
        let shared = PlayoutShared::default();
        let ahead = || f64::from_bits(shared.render_ahead_ms.load(Ordering::Relaxed));
        publish_render_ahead(&shared, Some(1_030 * MS), 1_000 * MS);
        assert_eq!(ahead(), 30.0);
        publish_render_ahead(&shared, None, 1_020 * MS);
        assert_eq!(ahead(), 30.0);
        publish_render_ahead(&shared, Some(1_062 * MS), 1_040 * MS);
        assert_eq!(ahead(), 22.0);
    }

    #[test]
    fn local_copy_never_plays_into_own_sink() {
        let pid = 4242;
        let json = |name: &str| format!(r#"{{"name":"{name}"}}"#);
        let device = json("alsa_output.pci-0000_65_00.6.analog-stereo");
        assert!(local_copy_allowed(Some(&device), pid));
        assert!(local_copy_allowed(None, pid));
        assert!(!local_copy_allowed(Some(&json("beatsync-4242-3")), pid));
        assert!(
            !local_copy_allowed(Some("beatsync-4242-1"), pid),
            "plain value"
        );
        // Another process's sink, and a name that only starts like ours.
        assert!(local_copy_allowed(Some(&json("beatsync-4243-1")), pid));
        assert!(local_copy_allowed(Some(&json("beatsync-42421-1")), pid));
    }

    /// End-to-end check of both timing rules against a live PipeWire (ignored; run with
    /// `cargo test -p streamer-core -- --ignored loopback`). Each of two test null sinks gets a
    /// loopback: the production output plays clicks scheduled in both mixer slots into the
    /// sink, and the production capture stream on the sink's monitor stamps them. The two
    /// sinks drive independent graphs at different quanta (512 and 2048 frames), so the rules
    /// are checked at two cycle phases and buffer sizes, one of them split into chunks.
    /// Nothing is audible: the output is pinned to a test sink with `node.dont-fallback`, and
    /// the test checks its links before any click is queued.
    mod loopback {
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        use ::pipewire as pw;
        use pw::loop_::Timeout;
        use pw::main_loop::MainLoopRc;
        use pw::node::Node;
        use pw::properties::properties;
        use pw::stream::StreamState;
        use pw::types::ObjectType;

        use crate::audio::pipewire::capture::{CaptureStream, connect_capture};
        use crate::audio::pipewire::playback::{PLAYBACK_NAME, PlaybackStream, connect_playback};
        use crate::audio::pipewire::routing::sink_properties;
        use crate::audio::pipewire::tests::{lock, nodes, own_links, wait_until};
        use crate::audio::{CapturedChunk, PlayoutShared};
        use crate::clock::{MonoEpochMap, mono_ns};
        use crate::dsp::Metronome;
        use crate::mixer::{Mixer, Slot};
        use crate::playout::{BLOCK_SAMPLES, PcmBlock};

        /// Blocks per slot (800 ms), contiguous from the slot's first target time.
        const BLOCKS: u64 = 40;
        /// Blocks that carry a click, at frame [`CLICK_FRAME`].
        const CLICK_BLOCKS: [u64; 4] = [5, 15, 25, 35];
        const CLICK_FRAME: usize = 333;
        /// The track slot's blocks start this much after the stream slot's: the clicks of the
        /// two slots alternate, at least 96 ms apart, at another phase within a block.
        const TRACK_OFFSET_MS: f64 = 103.7;
        /// A captured frame louder than this is a click.
        const THRESHOLD: f32 = 0.25;
        /// The low-pass rings for a few frames after a click; frames this close to the last
        /// onset belong to it.
        const ONSET_GAP_NS: i64 = 10_000_000;
        const MAX_ERROR_MS: f64 = 2.0;
        /// Quantum of each test sink's graph: a common one, and one above
        /// [`crate::audio::CHUNK_MAX_FRAMES`], so captured buffers are split.
        const QUANTA: [u32; 2] = [512, 2048];

        struct Loopback {
            sink: String,
            // The streams drop before the ring ends they own.
            capture: CaptureStream,
            playback: PlaybackStream,
            captured: rtrb::Consumer<CapturedChunk>,
            /// The producers of the stream and the track slot.
            slots: [rtrb::Producer<PcmBlock>; 2],
            shared: Arc<PlayoutShared>,
            states: Rc<RefCell<[Option<StreamState>; 2]>>,
            /// The scheduled clicks in time order, with their slot.
            expected: Vec<(Slot, f64)>,
            measured_ms: Vec<f64>,
            last_onset_ns: Option<i64>,
        }

        impl Loopback {
            fn streaming(&self) -> bool {
                let states = self.states.borrow();
                states.iter().all(|s| *s == Some(StreamState::Streaming))
            }

            /// Queues contiguous blocks with the clicks in each slot, from `first_ms` (server
            /// time) in the stream slot and [`TRACK_OFFSET_MS`] later in the track slot.
            fn schedule(&mut self, first_ms: f64) {
                for (slot, offset_ms) in [(Slot::Stream, 0.0), (Slot::Track, TRACK_OFFSET_MS)] {
                    for n in 0..BLOCKS {
                        let mut samples = [0.0; BLOCK_SAMPLES];
                        let target = first_ms + offset_ms + n as f64 * 20.0;
                        if CLICK_BLOCKS.contains(&n) {
                            samples[CLICK_FRAME * 2] = 1.0;
                            samples[CLICK_FRAME * 2 + 1] = 1.0;
                            self.expected
                                .push((slot, target + CLICK_FRAME as f64 / 48.0));
                        }
                        let block = PcmBlock {
                            target_start_server_ms: target,
                            samples,
                        };
                        let pushed = self.slots[slot as usize].push(block);
                        assert!(pushed.is_ok(), "the {slot:?} ring has room");
                    }
                }
                self.expected.sort_by(|a, b| a.1.total_cmp(&b.1));
            }

            /// Stamps every click onset in the captured chunks with the capture rule.
            fn scan(&mut self) {
                while let Ok(chunk) = self.captured.pop() {
                    let frames = chunk.samples[..chunk.frames * 2].as_chunks::<2>().0;
                    for (k, frame) in frames.iter().enumerate() {
                        if frame[0].abs() <= THRESHOLD {
                            continue;
                        }
                        let offset = k as f64 * 1e9 / 48_000.0;
                        let mono = chunk.mono_ns_first_frame + offset.round() as i64;
                        if self
                            .last_onset_ns
                            .is_some_and(|last| mono - last < ONSET_GAP_NS)
                        {
                            continue;
                        }
                        self.last_onset_ns = Some(mono);
                        self.measured_ms.push(self.shared.mono_to_server_ms(mono));
                    }
                }
            }

            /// Measured − scheduled, in ms, of the clicks of `slot`.
            fn errors(&self, slot: Slot) -> Vec<f64> {
                self.measured_ms
                    .iter()
                    .zip(&self.expected)
                    .filter(|(_, (s, _))| *s == slot)
                    .map(|(m, (_, e))| m - e)
                    .collect()
            }

            /// Frames per cycle the capture saw (`pw_time.size`).
            fn capture_quantum(&self) -> Option<u64> {
                let time = self.capture.stream().time().ok()?;
                Some(time.as_raw().size)
            }

            fn timing(&self) -> String {
                let show = |stream: &pw::stream::Stream| match stream.time() {
                    Ok(t) => {
                        let raw = t.as_raw();
                        format!(
                            "rate {}/{} delay {} buffered {} size {}",
                            raw.rate.num, raw.rate.denom, raw.delay, raw.buffered, raw.size
                        )
                    }
                    Err(e) => format!("no time: {e}"),
                };
                format!(
                    "capture: {}; playback: {}",
                    show(self.capture.stream()),
                    show(self.playback.stream())
                )
            }
        }

        /// Runs the loop until `done` or `timeout`.
        fn pump(main_loop: &MainLoopRc, timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
            let end = Instant::now() + timeout;
            while Instant::now() < end {
                main_loop
                    .loop_()
                    .iterate(Timeout::Finite(Duration::from_millis(5)));
                if done() {
                    return true;
                }
            }
            done()
        }

        #[test]
        #[ignore = "needs a live PipeWire session"]
        fn loopback_timing_error_below_2ms() {
            let _lock = lock();
            let pid = std::process::id();
            let names = ["a", "b"].map(|s| format!("beatsync-test-{pid}-{s}"));
            pw::init();
            let main_loop = MainLoopRc::new(None).expect("main loop");
            let props = properties! { "application.name" => "Beatsync Streamer test" };
            let context = pw::context::ContextRc::new(&main_loop, Some(props)).expect("context");
            let core = context.connect_rc(None).expect("PipeWire");
            let registry = core.get_registry_rc().expect("registry");
            let seen = Rc::new(RefCell::new(Vec::<String>::new()));
            let _registry_listener = registry
                .add_listener_local()
                .global({
                    let seen = seen.clone();
                    move |global| {
                        let name = global.props.and_then(|p| p.get("node.name"));
                        if global.type_ == ObjectType::Node
                            && let Some(name) = name
                        {
                            seen.borrow_mut().push(name.to_string());
                        }
                    }
                })
                .register();

            let sinks: Vec<Node> = names
                .iter()
                .zip(QUANTA)
                .map(|(n, quantum)| {
                    let mut props = sink_properties(n);
                    // Only the graph this test sink drives runs at this quantum.
                    props.insert("node.force-quantum", quantum.to_string());
                    core.create_object("adapter", &props).expect("sink")
                })
                .collect();
            let all_seen = || names.iter().all(|n| seen.borrow().contains(n));
            assert!(
                pump(&main_loop, Duration::from_secs(3), all_seen),
                "test sinks appear"
            );

            let epoch_offset_ms = MonoEpochMap::measure().to_epoch_ms(0);
            let mut loops: Vec<Loopback> = names
                .iter()
                .enumerate()
                .map(|(i, sink)| {
                    let shared = Arc::new(PlayoutShared::default());
                    let store = |a: &std::sync::atomic::AtomicU64, v: f64| {
                        a.store(v.to_bits(), Ordering::Relaxed)
                    };
                    // Server time = local clock time (a server offset of 0).
                    store(&shared.mono_server_offset_ms, epoch_offset_ms);
                    shared.gain.store(1.0_f32.to_bits(), Ordering::Relaxed);
                    let (capture_tx, captured) = rtrb::RingBuffer::new(256);
                    let (stream_tx, stream_rx) = rtrb::RingBuffer::new(64);
                    let (track_tx, track_rx) = rtrb::RingBuffer::new(64);
                    let metronome = Metronome::kick().expect("kick");
                    let mixer = Mixer::new(stream_rx, track_rx, metronome, shared.clone(), true);
                    let states = Rc::new(RefCell::new([None, None]));
                    let recorder = |k: usize| {
                        let states = states.clone();
                        move |s| states.borrow_mut()[k] = Some(s)
                    };
                    let id = 2 * i as u64;
                    let capture =
                        connect_capture(&core, sink, capture_tx, shared.clone(), id, recorder(0))
                            .expect("capture");
                    let playback = connect_playback(
                        &core,
                        Some(sink),
                        mixer,
                        shared.clone(),
                        id + 1,
                        recorder(1),
                    )
                    .expect("playback");
                    Loopback {
                        sink: sink.clone(),
                        capture,
                        playback,
                        captured,
                        slots: [stream_tx, track_tx],
                        shared,
                        states,
                        expected: Vec::new(),
                        measured_ms: Vec::new(),
                        last_onset_ns: None,
                    }
                })
                .collect();
            let streaming = pump(&main_loop, Duration::from_secs(3), || {
                loops.iter().all(Loopback::streaming)
            });
            let states: Vec<_> = loops
                .iter()
                .map(|l| format!("{:?}", l.states.borrow()))
                .collect();
            assert!(streaming, "all streams stream: {states:?}");

            // Safety: every playback link goes to a test sink before anything but silence plays.
            let prefix = format!("beatsync-test-{pid}-");
            let linked = wait_until(Duration::from_secs(3), || {
                let links = own_links(PLAYBACK_NAME);
                links.len() == 4 && links.iter().all(|p| p.starts_with(&prefix))
            });
            assert!(
                linked,
                "playback only to test sinks: {:?}",
                own_links(PLAYBACK_NAME)
            );

            for (i, l) in loops.iter_mut().enumerate() {
                // Different sub-block phases for the two loopbacks.
                let first = l.shared.mono_to_server_ms(mono_ns()) + 300.0 + 7.3 * i as f64;
                l.schedule(first);
            }
            let clicks = 2 * CLICK_BLOCKS.len();
            let all_found = pump(&main_loop, Duration::from_secs(3), || {
                loops.iter_mut().for_each(Loopback::scan);
                loops.iter().all(|l| l.measured_ms.len() >= clicks)
            });

            let mut report = Vec::new();
            for l in &loops {
                let errors = [Slot::Stream, Slot::Track].map(|slot| l.errors(slot));
                let dropped = l.shared.capture_dropped.load(Ordering::Relaxed);
                println!(
                    "{}: click errors (measured − scheduled) ms: stream slot {:.4?}, track \
                     slot {:.4?}; {}; dropped {dropped}",
                    l.sink,
                    errors[0],
                    errors[1],
                    l.timing()
                );
                let quantum = l.capture_quantum();
                report.push((l.sink.clone(), l.measured_ms.len(), errors, quantum));
            }
            drop(loops);
            drop(sinks);
            pump(&main_loop, Duration::from_millis(200), || false);
            let gone = wait_until(Duration::from_secs(2), || {
                nodes().iter().all(|(_, n)| !n.starts_with(&prefix))
            });

            assert!(all_found, "every click is captured: {report:?}");
            for ((sink, found, errors, quantum), forced) in report.iter().zip(QUANTA) {
                assert_eq!(
                    *quantum,
                    Some(u64::from(forced)),
                    "{sink}: the forced quantum"
                );
                assert_eq!(*found, clicks, "{sink}: one onset per click: {errors:?}");
                for (slot, errors) in [Slot::Stream, Slot::Track].iter().zip(errors) {
                    assert_eq!(errors.len(), CLICK_BLOCKS.len(), "{sink} {slot:?}");
                    assert!(
                        errors.iter().all(|e| e.abs() < MAX_ERROR_MS),
                        "{sink} {slot:?}: timing error below {MAX_ERROR_MS} ms: {errors:?}"
                    );
                }
            }
            assert!(gone, "test sinks are removed");
        }
    }
}
