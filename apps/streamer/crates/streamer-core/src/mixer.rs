//! The output mixer: the real-time render of the one playback stream. Two slots of timed
//! blocks (the local copy of this PC's live stream and the room's queue track) are summed and
//! low-passed (the CLI's output skips the filter); then the metronome kick is added (it skips
//! the filter), and the gain scales it all. The nudge moves the output time of every slot and
//! of the kick.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::audio::PlayoutShared;
use crate::dsp::{LowPass, Metronome};
use crate::frame::SAMPLE_RATE;
use crate::gain::GainRamp;
use crate::playout::{CHANNELS, PcmBlock, Playout, PlayoutConfig};

/// Frames of the fade-out when a slot is flushed (5 ms).
pub const FADE_FRAMES: usize = 240;
/// How long a producer waits for a flush (see [`wait_flushed`]).
pub const FLUSH_WAIT_MS: u64 = 200;
/// Stereo frames [`Mixer::render`] renders in one part. A longer call is rendered in parts.
pub const RENDER_FRAMES: usize = 1024;

const FRAME_MS: f64 = 1000.0 / SAMPLE_RATE as f64;
const FLUSH_POLL: Duration = Duration::from_millis(1);

/// A block source of the mixer.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The local copy of this PC's live stream.
    Stream = 0,
    /// The room's queue track.
    Track = 1,
}

/// Asks the mixer to fade out `slot`, drop its queued blocks and forget its position. Returns
/// the sequence number to pass to [`wait_flushed`]. Blocks pushed before this call are dropped.
/// The fade starts at once, or ends exactly at the slot's stop time
/// ([`PlayoutShared::stop_at_ms`]) if one is set before this call.
pub fn request_flush(shared: &PlayoutShared, slot: Slot) -> u32 {
    // `fetch_add` wraps on overflow.
    shared.flush_requested[slot as usize]
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

/// Waits until the mixer has done flush `seq` of `slot` (or a later one), polling every 1 ms.
/// Returns false at the timeout, e.g. when no output runs. Not for the real-time thread.
pub fn wait_flushed(shared: &PlayoutShared, slot: Slot, seq: u32, timeout: Duration) -> bool {
    // A timeout too large for an `Instant` waits without end.
    let end = Instant::now().checked_add(timeout);
    loop {
        let done = shared.flush_done[slot as usize].load(Ordering::Acquire);
        // Wrapping order: `done` is at or after `seq` when it is less than half the range ahead.
        if done.wrapping_sub(seq) as i32 >= 0 {
            return true;
        }
        let left = end.map_or(FLUSH_POLL, |end| {
            end.saturating_duration_since(Instant::now())
        });
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(FLUSH_POLL.min(left));
    }
}

/// One slot: a ring of timed blocks and the playout that keeps it on time.
struct SlotState {
    rx: rtrb::Consumer<PcmBlock>,
    playout: Playout,
    /// Frames of the flush fade already rendered; `None` when no fade runs.
    fade: Option<usize>,
    /// The stop time (server ms) the pending flush plays on to; `None` when there is none.
    stop_ms: Option<f64>,
}

impl SlotState {
    fn new(rx: rtrb::Consumer<PcmBlock>) -> Self {
        Self {
            rx,
            playout: Playout::new(PlayoutConfig::default()),
            fade: None,
            stop_ms: None,
        }
    }

    /// Fills `pcm` from the slot, heard from `at` (server ms, with the nudge). A flush request
    /// fades the slot out over [`FADE_FRAMES`], also across calls: at once, or ending exactly
    /// at the slot's stop time. At the end of the fade the slot drops its queued blocks,
    /// forgets its position and marks the flush done.
    fn render(&mut self, pcm: &mut [f32], at: f64, index: usize, shared: &PlayoutShared) {
        let requested = &shared.flush_requested[index];
        let done = &shared.flush_done[index];
        if self.fade.is_none() && requested.load(Ordering::Acquire) != done.load(Ordering::Relaxed)
        {
            let stop = f64::from_bits(shared.stop_at_ms[index].load(Ordering::Acquire));
            self.fade = self.follow_stop(stop, at);
        }
        self.playout.fill(pcm, at, &mut self.rx);
        let frames = pcm.as_chunks_mut::<CHANNELS>().0;
        if self.fade.is_none()
            && let Some(stop) = self.stop_ms
        {
            // Frame k is heard at `at + k·FRAME_MS`: the fade ends at the frame heard at the
            // stop time, and that frame and all after it are silent.
            let left = frames_until(stop, at);
            for (k, frame) in frames.iter_mut().enumerate() {
                let left = left - k as f64;
                if left < FADE_FRAMES as f64 {
                    let gain = left.max(0.0) as f32 / FADE_FRAMES as f32;
                    frame.iter_mut().for_each(|sample| *sample *= gain);
                }
            }
            if left <= frames.len() as f64 {
                self.finish(index, shared);
            }
            return;
        }
        let Some(faded) = self.fade else {
            return;
        };
        for (k, frame) in frames.iter_mut().enumerate() {
            let left = FADE_FRAMES.saturating_sub(faded + k);
            let gain = left as f32 / FADE_FRAMES as f32;
            for sample in frame {
                *sample *= gain;
            }
        }
        let faded = faded + frames.len();
        if faded < FADE_FRAMES {
            self.fade = Some(faded);
            return;
        }
        self.finish(index, shared);
    }

    /// How a pending flush goes on in a part heard from `at`, given the slot's stop time now
    /// (NaN: none): `None` to play on to the stop time in `self.stop_ms`, or the frames of an
    /// at-once fade already done. A stop time too close for its fade when first seen (a late
    /// stop) fades at once. A stop time that is cleared or moved while its fade runs goes on
    /// at once from the level it reached, so the level never steps up.
    fn follow_stop(&mut self, stop: f64, at: f64) -> Option<usize> {
        if self.stop_ms == Some(stop) {
            return None;
        }
        if let Some(old) = self.stop_ms.take() {
            let left = frames_until(old, at);
            if left < FADE_FRAMES as f64 {
                return Some(FADE_FRAMES - left.max(0.0) as usize);
            }
        }
        if frames_until(stop, at) >= FADE_FRAMES as f64 {
            self.stop_ms = Some(stop);
            return None;
        }
        Some(0)
    }

    /// Ends the flush: drops the queued blocks, forgets the position, clears the slot's stop
    /// time (unless a new one replaced it meanwhile) and marks the flush done. Every flush
    /// requested so far is done then, also one whose stop time was too close for its fade
    /// or came while the fade ran, so no stop time stays behind for a later flush.
    fn finish(&mut self, index: usize, shared: &PlayoutShared) {
        // Read before the request: a stop time set after it belongs to a later request.
        let stop = shared.stop_at_ms[index].load(Ordering::Acquire);
        // The request read here covers every block pushed before it.
        let seq = shared.flush_requested[index].load(Ordering::Acquire);
        let queued = self.rx.slots();
        if let Ok(chunk) = self.rx.read_chunk(queued) {
            chunk.commit_all();
        }
        self.playout.reset();
        self.fade = None;
        self.stop_ms = None;
        if !f64::from_bits(stop).is_nan() {
            let _ = shared.stop_at_ms[index].compare_exchange(
                stop,
                f64::NAN.to_bits(),
                Ordering::AcqRel,
                Ordering::Relaxed,
            );
        }
        shared.flush_done[index].store(seq, Ordering::Release);
    }
}

/// The frames from the one heard at `at` to the one heard at `stop_ms`, rounded; NaN for no
/// stop time.
fn frames_until(stop_ms: f64, at: f64) -> f64 {
    ((stop_ms - at) / FRAME_MS).round()
}

/// The real-time render of the output: the two slots, the low-pass, the metronome kick and
/// the gain, with targets read from [`PlayoutShared`] on every call.
pub struct Mixer {
    slots: [SlotState; 2],
    /// `None` for the CLI's output, which plays its local copy unfiltered.
    lowpass: Option<LowPass>,
    metronome: Metronome,
    gain: GainRamp,
    shared: Arc<PlayoutShared>,
    /// The nudge of the last call. A change resyncs both slots.
    nudge_ms: f64,
    /// One slot's audio of one part.
    pcm: Box<[f32; RENDER_FRAMES * CHANNELS]>,
}

impl Mixer {
    /// Builds the mixer off the real-time thread, with the gain and the cutoff of `shared`.
    /// Without `lowpass` the mix skips the low-pass filter (the CLI's output).
    pub fn new(
        stream_rx: rtrb::Consumer<PcmBlock>,
        track_rx: rtrb::Consumer<PcmBlock>,
        metronome: Metronome,
        shared: Arc<PlayoutShared>,
        lowpass: bool,
    ) -> Self {
        Self {
            slots: [SlotState::new(stream_rx), SlotState::new(track_rx)],
            lowpass: lowpass
                .then(|| LowPass::new(SAMPLE_RATE as f32, load_f32(&shared.lowpass_hz))),
            metronome,
            gain: GainRamp::new(load_f32(&shared.gain)),
            nudge_ms: nudge_ms(&shared),
            shared,
            pcm: Box::new([0.0; RENDER_FRAMES * CHANNELS]),
        }
    }

    /// Forgets where both slots are, so the next render places each one at its time again.
    /// For a cycle the output played without timing. A flush that is asked for or running
    /// goes on at the next render. Real-time safe.
    pub fn reset(&mut self) {
        for slot in &mut self.slots {
            slot.playout.reset();
        }
    }

    /// Fills `out` (interleaved stereo) with the frames heard from `heard_at_server_ms` on.
    /// A call of more than [`RENDER_FRAMES`] frames is rendered in parts of that size, back to
    /// back. A trailing odd sample is zeroed. Real-time safe: no allocation, lock or panic.
    pub fn render(&mut self, out: &mut [f32], heard_at_server_ms: f64) {
        let shared = &*self.shared;
        let gain = load_f32(&shared.gain);
        let gain_ramp_s = load_f32(&shared.gain_ramp_s);
        self.gain.set_target(gain, gain_ramp_s, SAMPLE_RATE as f32);
        if let Some(lowpass) = &mut self.lowpass {
            let lowpass_hz = load_f32(&shared.lowpass_hz);
            let lowpass_ramp_s = load_f32(&shared.lowpass_ramp_s);
            lowpass.set_target(lowpass_hz, lowpass_ramp_s);
        }
        let metronome = shared.metronome.load(Ordering::Relaxed);
        let nudge = nudge_ms(shared);
        if nudge != self.nudge_ms {
            // Every slot moves to the new time at once (a step the playout would otherwise
            // correct slowly).
            self.nudge_ms = nudge;
            for slot in &mut self.slots {
                slot.playout.reset();
            }
        }
        // Positive plays earlier: the frames due at a later server time play now.
        let at = heard_at_server_ms + nudge;

        let frames = out.len() / CHANNELS;
        let (out, tail) = out.split_at_mut(frames * CHANNELS);
        tail.fill(0.0);
        let mut done = 0;
        for part in out.chunks_mut(RENDER_FRAMES * CHANNELS) {
            let part_at = at + done as f64 * FRAME_MS;
            part.fill(0.0);
            let pcm = &mut self.pcm[..part.len()];
            for (index, slot) in self.slots.iter_mut().enumerate() {
                slot.render(pcm, part_at, index, shared);
                for (o, s) in part.iter_mut().zip(pcm.iter()) {
                    *o += *s;
                }
            }
            if let Some(lowpass) = &mut self.lowpass {
                lowpass.process(part);
            }
            if metronome {
                self.metronome.add(part, part_at);
            }
            self.gain.apply(part, CHANNELS);
            done += part.len() / CHANNELS;
        }
    }
}

fn load_f32(atomic: &AtomicU32) -> f32 {
    f32::from_bits(atomic.load(Ordering::Relaxed))
}

/// The nudge in ms; a non-finite value counts as 0.
fn nudge_ms(shared: &PlayoutShared) -> f64 {
    let nudge = shared.nudge_ms();
    if nudge.is_finite() { nudge } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playout::BLOCK_SAMPLES;
    use std::sync::atomic::AtomicU64;

    /// A beat time: 11 beats are exactly 12 s, so it is also a whole frame count.
    const BEAT_MS: f64 = 1_700_000_004_000.0;
    const T0: f64 = BEAT_MS - 1000.0;
    const RING: usize = 16;

    struct Rig {
        mixer: Mixer,
        stream: rtrb::Producer<PcmBlock>,
        track: rtrb::Producer<PcmBlock>,
        shared: Arc<PlayoutShared>,
    }

    /// A mixer of the app at full gain with empty rings. Everything else has its default:
    /// low-pass at 20000 Hz, no metronome, no nudge.
    fn rig() -> Rig {
        rig_with(true)
    }

    /// [`rig`], with or without the low-pass filter.
    fn rig_with(lowpass: bool) -> Rig {
        let shared = Arc::new(PlayoutShared::default());
        store_f32(&shared.gain, 1.0);
        let (stream, stream_rx) = rtrb::RingBuffer::new(RING);
        let (track, track_rx) = rtrb::RingBuffer::new(RING);
        let metronome = Metronome::kick().unwrap();
        let mixer = Mixer::new(stream_rx, track_rx, metronome, shared.clone(), lowpass);
        Rig {
            mixer,
            stream,
            track,
            shared,
        }
    }

    fn store_f32(atomic: &AtomicU32, value: f32) {
        atomic.store(value.to_bits(), Ordering::Relaxed);
    }

    fn store_f64(atomic: &AtomicU64, value: f64) {
        atomic.store(value.to_bits(), Ordering::Relaxed);
    }

    /// A block whose frame `i` is `frame(i)`.
    fn block(target_ms: f64, frame: impl Fn(usize) -> [f32; 2]) -> PcmBlock {
        let mut samples = [0.0; BLOCK_SAMPLES];
        for (i, f) in samples.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            *f = frame(i);
        }
        PcmBlock {
            target_start_server_ms: target_ms,
            samples,
        }
    }

    fn push(tx: &mut rtrb::Producer<PcmBlock>, block: PcmBlock) {
        assert!(tx.push(block).is_ok(), "the ring has room");
    }

    /// Renders `calls` buffers of `frames` frames back to back, the first heard at `start_ms`.
    fn play(mixer: &mut Mixer, start_ms: f64, frames: usize, calls: usize) -> Vec<f32> {
        let mut all = Vec::new();
        let mut out = vec![0.0_f32; 2 * frames];
        for call in 0..calls {
            // Every sample must be written.
            out.fill(9.0);
            mixer.render(&mut out, start_ms + (call * frames) as f64 * FRAME_MS);
            all.extend_from_slice(&out);
        }
        all
    }

    /// The first frame whose `channel` is not zero.
    fn first_sound(out: &[f32], channel: usize) -> Option<usize> {
        out.as_chunks::<2>()
            .0
            .iter()
            .position(|f| f[channel] != 0.0)
    }

    /// The kick alone, as heard from `start_ms` on, for `samples` interleaved samples.
    fn kick_alone(start_ms: f64, samples: usize) -> Vec<f32> {
        let mut kick = vec![0.0_f32; samples];
        Metronome::kick().unwrap().add(&mut kick, start_ms);
        assert!(kick.iter().any(|s| s.abs() > 0.1), "a loud kick in range");
        kick
    }

    fn peak(samples: impl IntoIterator<Item = f32>) -> f32 {
        samples.into_iter().fold(0.0, |p, s| p.max(s.abs()))
    }

    #[test]
    fn both_slots_play_at_their_targets() {
        // The local copy on the left channel, the track on the right. The low-pass is causal,
        // so the first frame with sound on a channel is its block's target.
        let mut r = rig();
        push(&mut r.stream, block(T0 + 5.0, |_| [0.25, 0.0]));
        push(&mut r.track, block(T0 + 10.0, |_| [0.0, 0.5]));
        let out = play(&mut r.mixer, T0, 512, 4);
        assert_eq!(first_sound(&out, 0), Some(240));
        assert_eq!(first_sound(&out, 1), Some(480));
        // Both play at once at their level (the filter passes DC), each for 20 ms.
        let at = |frame: usize, channel: usize| out[2 * frame + channel];
        assert!((at(1000, 0) - 0.25).abs() < 1e-3, "{}", at(1000, 0));
        assert!((at(1000, 1) - 0.5).abs() < 1e-3, "{}", at(1000, 1));
        assert!(at(1300, 0).abs() < 1e-3, "{}", at(1300, 0));
        assert!((at(1300, 1) - 0.5).abs() < 1e-3, "{}", at(1300, 1));
        assert!(at(1500, 1).abs() < 1e-3, "{}", at(1500, 1));
    }

    /// The level a constant 0.5 has at frame `k` of a flush fade.
    fn faded(k: usize) -> f32 {
        0.5 * FADE_FRAMES.saturating_sub(k) as f32 / FADE_FRAMES as f32
    }

    #[test]
    fn flush_fades_out_and_drops_queued_blocks() {
        let mut r = rig();
        for n in 0..8 {
            push(&mut r.track, block(T0 + 20.0 * n as f64, |_| [0.5, 0.5]));
        }
        let playing = play(&mut r.mixer, T0, 1024, 1);
        assert!((playing[2 * 1000] - 0.5).abs() < 1e-3, "the track plays");
        let seq = request_flush(&r.shared, Slot::Track);
        let out = play(&mut r.mixer, T0 + 1024.0 * FRAME_MS, 1024, 1);
        for (k, frame) in out.as_chunks::<2>().0.iter().enumerate() {
            let want = faded(k);
            assert!(
                (frame[0] - want).abs() < 0.01,
                "frame {k}: {frame:?}, want {want}"
            );
        }
        assert!(wait_flushed(&r.shared, Slot::Track, seq, Duration::ZERO));
        assert_eq!(r.track.slots(), RING, "every queued block is dropped");
        // Blocks 2..8 were due in this call.
        let next = play(&mut r.mixer, T0 + 2048.0 * FRAME_MS, 1024, 1);
        assert!(peak(next) < 1e-6, "silent after the flush");
    }

    #[test]
    fn a_flush_fade_spans_short_calls() {
        // Calls of 64 frames: the fade runs on over four calls, and the flush is done when
        // the fade ends.
        let mut r = rig();
        for n in 0..8 {
            push(&mut r.track, block(T0 + 20.0 * n as f64, |_| [0.5, 0.5]));
        }
        play(&mut r.mixer, T0, 64, 16);
        let seq = request_flush(&r.shared, Slot::Track);
        let mut out = Vec::new();
        for call in 0..6 {
            let at = T0 + (1024 + 64 * call) as f64 * FRAME_MS;
            out.extend(play(&mut r.mixer, at, 64, 1));
            let done = wait_flushed(&r.shared, Slot::Track, seq, Duration::ZERO);
            assert_eq!(done, call >= 3, "call {call}");
        }
        for (k, frame) in out.as_chunks::<2>().0.iter().enumerate() {
            let want = faded(k);
            assert!(
                (frame[0] - want).abs() < 0.01,
                "frame {k}: {frame:?}, want {want}"
            );
        }
        assert_eq!(r.track.slots(), RING, "every queued block is dropped");
    }

    #[test]
    fn a_flush_of_an_empty_slot_is_acknowledged() {
        let mut r = rig();
        // The counters are about to wrap: sequence numbers compare with wrapping arithmetic.
        let i = Slot::Stream as usize;
        r.shared.flush_requested[i].store(u32::MAX, Ordering::Relaxed);
        r.shared.flush_done[i].store(u32::MAX, Ordering::Relaxed);
        let seq = request_flush(&r.shared, Slot::Stream);
        assert_eq!(seq, 0);
        let early = Duration::from_millis(5);
        assert!(!wait_flushed(&r.shared, Slot::Stream, seq, early));
        play(&mut r.mixer, T0, 1024, 1);
        assert!(wait_flushed(&r.shared, Slot::Stream, seq, Duration::ZERO));
        let track = Slot::Track as usize;
        assert_eq!(r.shared.flush_done[track].load(Ordering::Relaxed), 0);

        // A waiting producer sees the flush done by a later render.
        let seq = request_flush(&r.shared, Slot::Stream);
        let shared = r.shared.clone();
        let waiter = std::thread::spawn(move || {
            wait_flushed(&shared, Slot::Stream, seq, Duration::from_secs(5))
        });
        std::thread::sleep(Duration::from_millis(20));
        play(&mut r.mixer, T0 + 1024.0 * FRAME_MS, 1024, 1);
        assert!(waiter.join().unwrap());
    }

    /// A track slot alone (no filter, no gain) with eight blocks at the level 0.5 from `T0`
    /// on, and a pending flush that stops at `stop_ms` (NaN: at once).
    fn stopping_slot(stop_ms: f64) -> (SlotState, rtrb::Producer<PcmBlock>, Arc<PlayoutShared>) {
        let shared = Arc::new(PlayoutShared::default());
        let (mut tx, rx) = rtrb::RingBuffer::new(RING);
        for n in 0..8 {
            push(&mut tx, block(T0 + 20.0 * n as f64, |_| [0.5, 0.5]));
        }
        store_f64(&shared.stop_at_ms[Slot::Track as usize], stop_ms);
        request_flush(&shared, Slot::Track);
        (SlotState::new(rx), tx, shared)
    }

    /// Renders `frames` frames of the slot from `T0 + from` frames on, in calls of `size`
    /// frames; returns the left channel.
    fn render_slot(
        slot: &mut SlotState,
        shared: &PlayoutShared,
        from: usize,
        frames: usize,
        size: usize,
    ) -> Vec<f32> {
        let mut left = Vec::new();
        let mut done = 0;
        while done < frames {
            let n = size.min(frames - done);
            let mut pcm = vec![9.0_f32; 2 * n];
            let at = T0 + (from + done) as f64 * FRAME_MS;
            slot.render(&mut pcm, at, Slot::Track as usize, shared);
            left.extend(pcm.iter().step_by(2));
            done += n;
        }
        left
    }

    fn stop_time(shared: &PlayoutShared) -> f64 {
        f64::from_bits(shared.stop_at_ms[Slot::Track as usize].load(Ordering::Relaxed))
    }

    #[test]
    fn a_stop_time_fades_out_exactly_at_its_target() {
        // A PAUSE at T while the blocks after T are queued already: the track plays on until
        // T and fades out over the 5 ms that end exactly at T, whatever the call size.
        // T is 50 ms after T0: frame 2400.
        for size in [64, 333, 1024, 2400] {
            let (mut slot, tx, shared) = stopping_slot(T0 + 50.0);
            let before = render_slot(&mut slot, &shared, 0, 2048, size);
            assert!(
                !wait_flushed(&shared, Slot::Track, 1, Duration::ZERO),
                "size {size}: the flush waits for the stop"
            );
            let mut out = before;
            out.extend(render_slot(&mut slot, &shared, 2048, 1000, size));
            for (k, &sample) in out.iter().enumerate() {
                let want = match k {
                    ..2160 => 0.5,
                    2160..2400 => 0.5 * ((2400 - k) as f32 / FADE_FRAMES as f32),
                    _ => 0.0,
                };
                assert!(
                    (sample - want).abs() < 1e-6,
                    "size {size}, frame {k}: {sample}, want {want}"
                );
            }
            assert_eq!(out[2399], 0.5 / FADE_FRAMES as f32, "size {size}");
            assert!(wait_flushed(&shared, Slot::Track, 1, Duration::ZERO));
            assert_eq!(
                tx.slots(),
                RING,
                "size {size}: the queued blocks are dropped"
            );
            assert!(
                stop_time(&shared).is_nan(),
                "size {size}: the stop time is cleared"
            );
        }
    }

    #[test]
    fn a_flush_after_a_stop_time_stops_at_once() {
        // A PLAY after a PAUSE whose time is still ahead: the new flush fades out at once
        // and does not wait for the old stop time.
        let (mut slot, tx, shared) = stopping_slot(T0 + 100.0);
        let mut out = render_slot(&mut slot, &shared, 0, 1000, 512);
        store_f64(&shared.stop_at_ms[Slot::Track as usize], f64::NAN);
        let seq = request_flush(&shared, Slot::Track);
        out.extend(render_slot(&mut slot, &shared, 1000, 1000, 512));
        for (k, &sample) in out.iter().enumerate() {
            let want = 0.5 * (FADE_FRAMES.saturating_sub(k.saturating_sub(1000))) as f32
                / FADE_FRAMES as f32;
            let want = if k < 1000 { 0.5 } else { want };
            assert!(
                (sample - want).abs() < 1e-6,
                "frame {k}: {sample}, want {want}"
            );
        }
        assert!(wait_flushed(&shared, Slot::Track, seq, Duration::ZERO));
        assert_eq!(tx.slots(), RING, "the queued blocks are dropped");

        // A PLAY within the fade to the stop time: the fade goes on from its level, so the
        // level never steps up.
        let (mut slot, _tx, shared) = stopping_slot(T0 + 100.0);
        let mut out = render_slot(&mut slot, &shared, 0, 4700, 470);
        store_f64(&shared.stop_at_ms[Slot::Track as usize], f64::NAN);
        request_flush(&shared, Slot::Track);
        out.extend(render_slot(&mut slot, &shared, 4700, 300, 300));
        for k in 4561..5000 {
            assert!(
                out[k] <= out[k - 1],
                "frame {k}: {} after {}",
                out[k],
                out[k - 1]
            );
        }
        assert!(
            out[4700] > 0.0 && out[4800] == 0.0,
            "{} {}",
            out[4700],
            out[4800]
        );

        // A stop time already too close for its fade when the output first sees it (a late
        // PAUSE): it fades out at once instead of cutting.
        let (mut slot, _tx, shared) = stopping_slot(T0 + 2.0);
        let out = render_slot(&mut slot, &shared, 0, 480, 480);
        assert_eq!(out[0], 0.5);
        assert!(out[200] > 0.0 && out[200] < 0.5, "{}", out[200]);
        assert!(out[240..].iter().all(|&s| s == 0.0));
    }

    #[test]
    fn a_finished_flush_leaves_no_stop_time() {
        // A stop time too close for its fade fades out at once. Its flush is done, so its stop
        // time must not stay behind for a later flush that sets none.
        let (mut slot, _tx, shared) = stopping_slot(T0 + 2.0);
        render_slot(&mut slot, &shared, 0, 480, 480);
        assert!(wait_flushed(&shared, Slot::Track, 1, Duration::ZERO));
        assert!(stop_time(&shared).is_nan(), "{}", stop_time(&shared));

        // A later flush plays on to its own stop time (500 ms, frame 24 000), then clears it.
        store_f64(&shared.stop_at_ms[Slot::Track as usize], T0 + 500.0);
        let seq = request_flush(&shared, Slot::Track);
        render_slot(&mut slot, &shared, 480, 480, 480);
        assert_eq!(stop_time(&shared), T0 + 500.0);
        assert!(!wait_flushed(&shared, Slot::Track, seq, Duration::ZERO));
        render_slot(&mut slot, &shared, 960, 24_000, 1024);
        assert!(wait_flushed(&shared, Slot::Track, seq, Duration::ZERO));
        assert!(stop_time(&shared).is_nan(), "{}", stop_time(&shared));

        // A stop time set for a newer flush while the fade runs: the end of the fade does that
        // flush too, so its stop time does not stay behind either.
        let (mut slot, _tx, shared) = stopping_slot(T0 + 2.0);
        render_slot(&mut slot, &shared, 0, 64, 64);
        store_f64(&shared.stop_at_ms[Slot::Track as usize], T0 + 500.0);
        let seq = request_flush(&shared, Slot::Track);
        render_slot(&mut slot, &shared, 64, 480, 64);
        assert!(wait_flushed(&shared, Slot::Track, seq, Duration::ZERO));
        assert!(stop_time(&shared).is_nan(), "{}", stop_time(&shared));
    }

    #[derive(Debug, Clone, Copy)]
    enum Source {
        Stream,
        Track,
        Kick,
    }

    /// The first frame with sound when `source` is due 15 ms after the output starts.
    fn first_heard(source: Source, nudge_ms: f64) -> usize {
        let mut r = rig();
        store_f64(&r.shared.nudge_ms, nudge_ms);
        let due = BEAT_MS;
        match source {
            Source::Stream => push(&mut r.stream, block(due, |_| [0.5, 0.5])),
            Source::Track => push(&mut r.track, block(due, |_| [0.5, 0.5])),
            Source::Kick => r.shared.metronome.store(true, Ordering::Relaxed),
        }
        let out = play(&mut r.mixer, due - 15.0, 1024, 2);
        first_sound(&out, 0).expect("heard")
    }

    #[test]
    fn the_nudge_moves_every_slot_and_the_kick() {
        // Positive plays earlier: +10 ms is 480 frames earlier.
        assert_eq!(first_heard(Source::Stream, 0.0), 720);
        assert_eq!(first_heard(Source::Track, 0.0), 720);
        for source in [Source::Stream, Source::Track, Source::Kick] {
            let plain = first_heard(source, 0.0);
            let nudged = first_heard(source, 10.0);
            assert_eq!(nudged + 480, plain, "{source:?}");
        }
    }

    #[test]
    fn a_nudge_change_acts_at_once() {
        // Contiguous blocks, silent except block 3 (due at frame 2880). After the first call
        // the nudge becomes +10 ms: block 3 is heard 480 frames earlier, at once, and not
        // through the slow drift correction.
        let mut r = rig();
        for n in 0..8 {
            let level = if n == 3 { 0.5 } else { 0.0 };
            push(
                &mut r.track,
                block(T0 + 20.0 * n as f64, |_| [level, level]),
            );
        }
        let mut out = play(&mut r.mixer, T0, 1024, 1);
        store_f64(&r.shared.nudge_ms, 10.0);
        out.extend(play(&mut r.mixer, T0 + 1024.0 * FRAME_MS, 1024, 2));
        assert_eq!(first_sound(&out, 0), Some(2880 - 480));
    }

    #[test]
    fn reset_realigns_both_slots_at_the_next_render() {
        // The output had a cycle without timing, and its time moved on by 5 ms. After a reset
        // both slots are placed at the new time at once; the playout alone would correct the
        // 5 ms slowly. Block 3 of each slot is the only sound, due at frame 2880.
        let mut r = rig();
        for n in 0..8 {
            let level = if n == 3 { 0.5 } else { 0.0 };
            let target = T0 + 20.0 * n as f64;
            push(&mut r.stream, block(target, |_| [level, 0.0]));
            push(&mut r.track, block(target, |_| [0.0, level]));
        }
        let mut out = play(&mut r.mixer, T0, 1024, 1);
        r.mixer.reset();
        out.extend(play(
            &mut r.mixer,
            T0 + (1024.0 + 240.0) * FRAME_MS,
            1024,
            2,
        ));
        assert_eq!(first_sound(&out, 0), Some(2880 - 240));
        assert_eq!(first_sound(&out, 1), Some(2880 - 240));
    }

    #[test]
    fn reset_keeps_a_pending_flush() {
        // A stream stop asks for a flush while the output has no timing: the flush still
        // happens at the next render, so no old block plays.
        let mut r = rig();
        for n in 0..8 {
            push(&mut r.stream, block(T0 + 20.0 * n as f64, |_| [0.5, 0.5]));
        }
        play(&mut r.mixer, T0, 1024, 1);
        let seq = request_flush(&r.shared, Slot::Stream);
        r.mixer.reset();
        assert!(!wait_flushed(&r.shared, Slot::Stream, seq, Duration::ZERO));
        play(&mut r.mixer, T0 + 1024.0 * FRAME_MS, 1024, 1);
        assert!(wait_flushed(&r.shared, Slot::Stream, seq, Duration::ZERO));
        assert_eq!(r.stream.slots(), RING, "every queued block is dropped");
    }

    #[test]
    fn the_kick_skips_the_low_pass() {
        // Cutoff 20 Hz: a 5 kHz tone in the track slot is near silent, the kick is not changed.
        let mut r = rig();
        store_f32(&r.shared.lowpass_hz, 20.0);
        r.shared.metronome.store(true, Ordering::Relaxed);
        let start = BEAT_MS - 5.0;
        for n in 0..4 {
            let tone = move |i: usize| {
                let k = (n * 960 + i) as f64;
                let s = (0.5 * (std::f64::consts::TAU * 5000.0 * k / 48_000.0).sin()) as f32;
                [s, s]
            };
            push(&mut r.track, block(start + 20.0 * n as f64, tone));
        }
        let out = play(&mut r.mixer, start, 1024, 3);
        let kick = kick_alone(start, out.len());
        let tone_left = peak(out.iter().zip(&kick).map(|(o, k)| o - k));
        println!("tone after the filter: peak {tone_left}");
        assert!(tone_left < 0.005, "the tone is filtered: peak {tone_left}");
    }

    #[test]
    fn the_cli_output_skips_the_low_pass() {
        // The CLI plays its local copy as captured: no filter, also not at a low cutoff.
        let tone = |i: usize| {
            let s = (0.5 * (std::f64::consts::TAU * 5000.0 * i as f64 / 48_000.0).sin()) as f32;
            [s, -s]
        };
        let mut r = rig_with(false);
        store_f32(&r.shared.lowpass_hz, 20.0);
        for n in 0..4 {
            push(
                &mut r.stream,
                block(T0 + 20.0 * n as f64, move |i| tone(n * 960 + i)),
            );
        }
        let out = play(&mut r.mixer, T0, 960, 4);
        for (k, frame) in out.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(*frame, tone(k), "frame {k}");
        }
    }

    #[test]
    fn gain_scales_the_kick() {
        let mut r = rig();
        store_f32(&r.shared.gain, 0.5);
        r.shared.metronome.store(true, Ordering::Relaxed);
        let start = BEAT_MS - 5.0;
        let out = play(&mut r.mixer, start, 1024, 3);
        let kick = kick_alone(start, out.len());
        for (k, (o, s)) in out.iter().zip(&kick).enumerate() {
            assert!((o - 0.5 * s).abs() < 1e-6, "sample {k}: {o} != 0.5 × {s}");
        }
    }

    #[test]
    fn a_long_call_is_rendered_in_parts() {
        // More frames than one part: the parts play back to back, as separate calls would.
        // A trailing odd sample is zeroed.
        let feed = |r: &mut Rig| {
            r.shared.metronome.store(true, Ordering::Relaxed);
            for n in 0..6 {
                let ramp = move |i: usize| {
                    let v = ((n * 960 + i) % 97) as f32 / 97.0;
                    [v, -v]
                };
                push(&mut r.track, block(BEAT_MS - 7.0 + 20.0 * n as f64, ramp));
            }
        };
        let start = BEAT_MS - 10.0;
        let mut a = rig();
        feed(&mut a);
        let mut long = vec![9.0_f32; 2 * 2500 + 1];
        a.mixer.render(&mut long, start);
        let mut b = rig();
        feed(&mut b);
        let mut parts = play(&mut b.mixer, start, 1024, 2);
        parts.extend(play(&mut b.mixer, start + 2048.0 * FRAME_MS, 452, 1));
        assert_eq!(long[2 * 2500], 0.0);
        assert!(peak(parts.iter().copied()) > 0.1);
        assert!(long[..2 * 2500] == parts[..], "the same samples");
    }
}
