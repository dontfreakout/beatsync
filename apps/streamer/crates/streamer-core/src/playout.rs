//! Local-copy playout: plays timestamped PCM blocks so that every frame is heard at its target
//! server time, like on every phone.

use crate::frame::{FRAME_SAMPLES, SAMPLE_RATE};

/// Interleaved channels in a [`PcmBlock`].
pub const CHANNELS: usize = 2;
/// Samples in a [`PcmBlock`]: 960 stereo frames.
pub const BLOCK_SAMPLES: usize = FRAME_SAMPLES * CHANNELS;

/// 20 ms of stereo audio and the server time at which its first frame must be heard.
#[derive(Clone, Copy)]
pub struct PcmBlock {
    pub target_start_server_ms: f64,
    /// Stereo interleaved, 960 frames.
    pub samples: [f32; BLOCK_SAMPLES],
}

/// The queue of blocks in target order, e.g. a lock-free ring buffer consumer.
pub trait BlockSource {
    /// The oldest block, if any.
    fn peek(&mut self) -> Option<&PcmBlock>;
    /// Removes the oldest block.
    fn pop(&mut self);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayoutConfig {
    pub sample_rate: f64,
    /// Larger errors flush and restart at the ideal time.
    pub resync_ms: f64,
    /// Larger errors (up to `resync_ms`) are corrected gradually. A larger step between the end
    /// of one block and the start of the next resyncs at the block boundary.
    pub correction_threshold_ms: f64,
    /// While correcting, one source frame is skipped or repeated per this many output frames.
    pub correction_interval_frames: usize,
}

impl Default for PlayoutConfig {
    fn default() -> Self {
        Self {
            sample_rate: f64::from(SAMPLE_RATE),
            resync_ms: 20.0,
            correction_threshold_ms: 0.5,
            correction_interval_frames: 1000,
        }
    }
}

/// What one [`Playout::fill`] call did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FillStats {
    /// Output frames filled with silence (leading silence or underrun).
    pub silent_frames: usize,
    /// Blocks dropped because they ended before the output time.
    pub dropped_blocks: usize,
    /// The cursor was placed at the output time: first call, after `reset`, or a large error.
    pub resynced: bool,
}

#[derive(Debug, Clone, Copy)]
enum Cursor {
    /// No position (start, after `reset`, or after an underrun): the next block found is placed
    /// at the output time (resync).
    Unset,
    /// Placed before the head block, which starts in the future: silence until its start.
    Waiting,
    /// At frame `index` of the source's head block.
    Block { index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Correction {
    None,
    /// The cursor is late: skip one source frame per interval.
    Skip,
    /// The cursor is early: repeat one source frame per interval.
    Repeat,
}

/// Plays [`PcmBlock`]s back to back from a cursor (head block + frame index) and keeps the
/// cursor at the time the output is heard: small errors are corrected gradually; large errors,
/// timestamp steps between blocks (above the correction threshold) and the end of an underrun
/// resync. Real-time safe: `fill` does not allocate, lock or panic.
pub struct Playout {
    frame_ms: f64,
    resync_ms: f64,
    threshold_ms: f64,
    interval: usize,
    cursor: Cursor,
    /// Output frames still to play, while correcting, before the next skip or repeat.
    until_correction: usize,
    last_error_ms: Option<f64>,
}

impl Playout {
    pub fn new(cfg: PlayoutConfig) -> Self {
        let interval = cfg.correction_interval_frames.max(1);
        Self {
            frame_ms: 1000.0 / cfg.sample_rate,
            resync_ms: cfg.resync_ms,
            threshold_ms: cfg.correction_threshold_ms,
            interval,
            cursor: Cursor::Unset,
            until_correction: interval,
            last_error_ms: None,
        }
    }

    /// Fills `out` (stereo interleaved) from `src`. `heard_at_server_ms` is the server time at
    /// which `out[0]` will be heard. A trailing odd sample is zeroed.
    pub fn fill(
        &mut self,
        out: &mut [f32],
        heard_at_server_ms: f64,
        src: &mut impl BlockSource,
    ) -> FillStats {
        let mut stats = FillStats::default();
        let frames = out.len() / CHANNELS;
        let (out, tail) = out.split_at_mut(frames * CHANNELS);
        tail.fill(0.0);
        let (pos, correction) = self.align(out, heard_at_server_ms, src, &mut stats);
        self.play(out, heard_at_server_ms, pos, correction, src, &mut stats);
        stats
    }

    /// Forgets the cursor. The next `fill` resyncs.
    pub fn reset(&mut self) {
        self.cursor = Cursor::Unset;
        self.until_correction = self.interval;
        self.last_error_ms = None;
    }

    /// The error (output time − cursor time, ms; > 0: the cursor is late) at the start of the
    /// last `fill`, or `None` if the cursor was not on a block then.
    pub fn last_error_ms(&self) -> Option<f64> {
        self.last_error_ms
    }

    /// Checks the cursor against the output time. Returns the output frame from which to play
    /// the cursor (frames before it are silent) and the correction for this call.
    fn align(
        &mut self,
        out: &mut [f32],
        heard_at: f64,
        src: &mut impl BlockSource,
        stats: &mut FillStats,
    ) -> (usize, Correction) {
        self.last_error_ms = None;
        let index = match self.cursor {
            Cursor::Unset => {
                if src.peek().is_none() {
                    return (silence(out, 0, stats), Correction::None);
                }
                return (self.resync(out, heard_at, src, stats), Correction::None);
            }
            Cursor::Waiting => {
                let Some(head) = src.peek() else {
                    self.cursor = Cursor::Unset;
                    return (silence(out, 0, stats), Correction::None);
                };
                let lead = ((head.target_start_server_ms - heard_at) / self.frame_ms).round();
                if lead > 0.0 {
                    return (self.wait(out, lead, stats), Correction::None);
                }
                // The block is due: check it like a playing cursor.
                self.cursor = Cursor::Block { index: 0 };
                0
            }
            Cursor::Block { index } => index,
        };
        let Some(head) = src.peek() else {
            return (0, Correction::None);
        };
        let error = heard_at - (head.target_start_server_ms + index as f64 * self.frame_ms);
        self.last_error_ms = Some(error);
        if error.abs() > self.resync_ms {
            (self.resync(out, heard_at, src, stats), Correction::None)
        } else if error > self.threshold_ms {
            (0, Correction::Skip)
        } else if error < -self.threshold_ms {
            (0, Correction::Repeat)
        } else {
            (0, Correction::None)
        }
    }

    /// Drops blocks that end before `heard_at` (the time of `out[0]`) and puts the cursor at
    /// `heard_at`, mid-block or after leading silence. Returns the first output frame to play.
    fn resync(
        &mut self,
        out: &mut [f32],
        heard_at: f64,
        src: &mut impl BlockSource,
        stats: &mut FillStats,
    ) -> usize {
        stats.resynced = true;
        self.until_correction = self.interval;
        loop {
            let Some(head) = src.peek() else {
                self.cursor = Cursor::Unset;
                return silence(out, 0, stats);
            };
            let offset = ((heard_at - head.target_start_server_ms) / self.frame_ms).round();
            if offset >= FRAME_SAMPLES as f64 {
                src.pop();
                stats.dropped_blocks += 1;
            } else if offset >= 0.0 {
                let index = offset as usize;
                self.cursor = Cursor::Block { index };
                return 0;
            } else {
                return self.wait(out, -offset, stats);
            }
        }
    }

    /// Writes `lead` frames of silence before the head block, or silence to the end of `out`
    /// if the block starts later. Returns the output frame where the block starts, or the frame
    /// count of `out`.
    fn wait(&mut self, out: &mut [f32], lead: f64, stats: &mut FillStats) -> usize {
        let frames = out.len() / CHANNELS;
        if lead >= frames as f64 {
            self.cursor = Cursor::Waiting;
            return silence(out, 0, stats);
        }
        let lead = lead as usize;
        silence(&mut out[..lead * CHANNELS], 0, stats);
        self.cursor = Cursor::Block { index: 0 };
        lead
    }

    /// Plays the cursor back to back into output frames `pos..`, with one skip or repeat per
    /// `interval` frames if correcting. Pops each block after its last frame. A next block that
    /// starts more than `correction_threshold_ms` away from the end of the previous one (lost
    /// block, capture re-anchor) resyncs at once, so it is never heard early.
    fn play(
        &mut self,
        out: &mut [f32],
        heard_at: f64,
        mut pos: usize,
        mut correction: Correction,
        src: &mut impl BlockSource,
        stats: &mut FillStats,
    ) {
        let Cursor::Block { mut index } = self.cursor else {
            return;
        };
        let frames = out.len() / CHANNELS;
        while pos < frames {
            let Some(block) = src.peek() else {
                // Underrun: the next block found resyncs.
                silence(out, pos, stats);
                self.cursor = Cursor::Unset;
                return;
            };
            let end_ms = block.target_start_server_ms + FRAME_SAMPLES as f64 * self.frame_ms;
            let mut run = (frames - pos).min(FRAME_SAMPLES - index);
            if correction != Correction::None {
                run = run.min(self.until_correction);
            }
            out[pos * CHANNELS..(pos + run) * CHANNELS]
                .copy_from_slice(&block.samples[index * CHANNELS..(index + run) * CHANNELS]);
            pos += run;
            index += run;
            if correction != Correction::None {
                self.until_correction -= run;
                if self.until_correction == 0 {
                    self.until_correction = self.interval;
                    match correction {
                        Correction::Skip => index += 1,
                        Correction::Repeat => index -= 1,
                        Correction::None => {}
                    }
                }
            }
            if index < FRAME_SAMPLES {
                continue;
            }
            index -= FRAME_SAMPLES;
            src.pop();
            if let Some(next) = src.peek()
                && (next.target_start_server_ms - end_ms).abs() > self.threshold_ms
            {
                correction = Correction::None;
                let at = heard_at + pos as f64 * self.frame_ms;
                pos += self.resync(&mut out[pos * CHANNELS..], at, src, stats);
                let Cursor::Block { index: placed } = self.cursor else {
                    return;
                };
                index = placed;
            }
        }
        self.cursor = Cursor::Block { index };
    }
}

/// Writes silence to output frames `from..` and returns the frame count of `out`.
fn silence(out: &mut [f32], from: usize, stats: &mut FillStats) -> usize {
    let frames = out.len() / CHANNELS;
    out[from * CHANNELS..].fill(0.0);
    stats.silent_frames += frames - from;
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const T0: f64 = 1_700_000_000_000.0;
    const FRAME_MS: f64 = 1000.0 / 48_000.0;

    struct Queue(VecDeque<PcmBlock>);

    impl BlockSource for Queue {
        fn peek(&mut self) -> Option<&PcmBlock> {
            self.0.front()
        }

        fn pop(&mut self) {
            self.0.pop_front();
        }
    }

    /// Block `n` of a stream: frame `i` holds `n * 960 + i + 1` (left) and its negation (right),
    /// so a zero frame is silence.
    fn block(n: u64, start_ms: f64) -> PcmBlock {
        let mut samples = [0.0; BLOCK_SAMPLES];
        let (frames, _) = samples.as_chunks_mut::<CHANNELS>();
        for (i, frame) in frames.iter_mut().enumerate() {
            let v = (n * FRAME_SAMPLES as u64 + i as u64 + 1) as f32;
            *frame = [v, -v];
        }
        PcmBlock {
            target_start_server_ms: start_ms,
            samples,
        }
    }

    /// Blocks `range` of a contiguous stream that starts at `T0`.
    fn contiguous(range: std::ops::Range<u64>) -> Queue {
        Queue(range.map(|n| block(n, T0 + 20.0 * n as f64)).collect())
    }

    /// An output buffer filled with a non-silent frame, so the tests see what `fill` writes.
    fn buf(frames: usize) -> Vec<f32> {
        [1e6, -1e6].repeat(frames)
    }

    /// The source frame number in output frame `k`, `None` for silence.
    fn source_frame(out: &[f32], k: usize) -> Option<u64> {
        let (l, r) = (out[2 * k], out[2 * k + 1]);
        assert_eq!(l, -r, "channels disagree at output frame {k}");
        (l != 0.0).then(|| l as u64 - 1)
    }

    fn playout() -> Playout {
        Playout::new(PlayoutConfig::default())
    }

    /// Plays `calls` buffers of `frames` frames back to back from `T0` and returns the source
    /// frame of every output frame. `before_call` can feed the source; `check` sees each result.
    fn run(
        p: &mut Playout,
        src: &mut Queue,
        frames: usize,
        calls: usize,
        mut before_call: impl FnMut(usize, &mut Queue),
        mut check: impl FnMut(usize, FillStats, &Playout),
    ) -> Vec<Option<u64>> {
        let mut out = buf(frames);
        let mut played = Vec::new();
        for call in 0..calls {
            before_call(call, src);
            let heard_at = T0 + (call * frames) as f64 * FRAME_MS;
            let stats = p.fill(&mut out, heard_at, src);
            check(call, stats, p);
            played.extend((0..frames).map(|k| source_frame(&out, k)));
        }
        played
    }

    #[test]
    fn future_block_gets_exact_leading_silence() {
        // 10 ms = 480 frames of silence, longer than one 256-frame buffer.
        let mut p = playout();
        let mut src = Queue(VecDeque::from([block(0, T0 + 10.0)]));
        let mut silent = 0;
        let count = |_: usize, s: FillStats, _: &Playout| silent += s.silent_frames;
        let played = run(&mut p, &mut src, 256, 2, |_, _| {}, count);
        assert_eq!(silent, 480);
        assert!(played[..480].iter().all(Option::is_none));
        assert_eq!(played[480], Some(0));
        assert_eq!(played[511], Some(31));
    }

    #[test]
    fn joins_mid_block() {
        let mut p = playout();
        let mut src = contiguous(0..2);
        let mut out = buf(256);
        let stats = p.fill(&mut out, T0 + 5.0, &mut src);
        assert_eq!(source_frame(&out, 0), Some(240));
        assert_eq!(source_frame(&out, 255), Some(495));
        let want = FillStats {
            silent_frames: 0,
            dropped_blocks: 0,
            resynced: true,
        };
        assert_eq!(stats, want);
    }

    #[test]
    fn past_blocks_are_dropped() {
        // Blocks 0 and 1 end at 20 and 40 ms; 50 ms is frame 480 of block 2.
        let mut p = playout();
        let mut src = contiguous(0..4);
        let mut out = buf(256);
        let stats = p.fill(&mut out, T0 + 50.0, &mut src);
        assert_eq!(stats.dropped_blocks, 2);
        assert_eq!(source_frame(&out, 0), Some(2 * 960 + 480));
    }

    #[test]
    fn drift_is_corrected_gradually() {
        // The output clock runs 100 ppm fast: each buffer spans a little less server time.
        const FRAMES: usize = 1024;
        let calls = 60 * 48_000 / FRAMES;
        let mut p = playout();
        let mut src = Queue(VecDeque::new());
        let mut next = 0;
        let mut out = buf(FRAMES);
        let mut prev: Option<u64> = None;
        let mut repeats = 0;
        for call in 0..calls {
            while src.0.len() < 4 {
                src.0.push_back(block(next, T0 + 20.0 * next as f64));
                next += 1;
            }
            let heard_at = T0 + call as f64 * FRAMES as f64 / 48.0 / (1.0 + 100e-6);
            let stats = p.fill(&mut out, heard_at, &mut src);
            assert_eq!(stats.silent_frames, 0, "call {call}");
            if call > 0 {
                assert!(!stats.resynced, "call {call}");
                let error = p.last_error_ms().expect("cursor on a block");
                assert!(error.abs() < 1.0, "call {call}: error {error} ms");
            }
            for k in 0..FRAMES {
                let f = source_frame(&out, k).expect("no silence");
                if let Some(last) = prev {
                    match f as i64 - last as i64 {
                        0 => repeats += 1,
                        1 => {}
                        step => panic!("call {call}, frame {k}: source step {step}"),
                    }
                }
                prev = Some(f);
            }
        }
        // 60 s at 100 ppm is 6 ms = 288 frames; all but the last ~0.5 ms must be repeated.
        assert!(repeats > 200, "{repeats} repeated frames");
    }

    #[test]
    fn large_error_resyncs() {
        // From call 5 on, the output time is 100 ms (4800 frames) later: the cursor is 100 ms
        // late, so blocks 5..10 (ending before the new output time) are dropped and the cursor
        // joins block 10 mid-block, at the output time.
        const FRAMES: usize = 1024;
        let mut p = playout();
        let mut src = contiguous(0..20);
        let mut out = buf(FRAMES);
        for call in 0..7 {
            let frame = call * FRAMES + if call >= 5 { 4800 } else { 0 };
            let stats = p.fill(&mut out, T0 + frame as f64 * FRAME_MS, &mut src);
            assert_eq!(stats.resynced, call == 0 || call == 5, "call {call}");
            let dropped = if call == 5 { 5 } else { 0 };
            assert_eq!(stats.dropped_blocks, dropped, "call {call}");
            assert_eq!(source_frame(&out, 0), Some(frame as u64), "call {call}");
        }
        let error = p.last_error_ms().expect("cursor on a block");
        assert!(error.abs() < FRAME_MS / 2.0, "error {error} ms");
    }

    #[test]
    fn forward_jump_between_blocks_waits_for_target() {
        // A capture re-anchor: from block 5 on, the targets are 100 ms later. Block 4 ends at
        // output frame 4800, inside call 4; block 5 must wait for its target, T0 + 200 ms =
        // output frame 9600, and must not play early, not even until the end of call 4.
        const FRAMES: usize = 1024;
        let mut p = playout();
        let jump = |n: u64| if n >= 5 { 100.0 } else { 0.0 };
        let blocks = (0..12).map(|n| block(n, T0 + 20.0 * n as f64 + jump(n)));
        let mut src = Queue(blocks.collect());
        let check = |call: usize, s: FillStats, p: &Playout| {
            assert_eq!(s.resynced, call == 0 || call == 4, "call {call}");
            if call == 10 {
                let error = p.last_error_ms().expect("cursor on a block");
                assert!(error.abs() < FRAME_MS / 2.0, "error {error} ms");
            }
        };
        let played = run(&mut p, &mut src, FRAMES, 11, |_, _| {}, check);
        for (k, &f) in played.iter().enumerate() {
            let want = match k {
                ..4800 => Some(k as u64),
                4800..9600 => None,
                _ => Some(k as u64 - 4800),
            };
            assert_eq!(f, want, "output frame {k}");
        }
    }

    #[test]
    fn block_after_underrun_is_aligned_exactly() {
        // Blocks 0..3 cover output frames 0..2880, so call 2 runs dry 192 frames before its end.
        // Block 3 (due at 2880) arrives before call 3, 4 ms late: the cursor joins it at the
        // output time (frame 192) instead of starting it late and correcting gradually.
        const FRAMES: usize = 1024;
        let mut p = playout();
        let mut src = contiguous(0..3);
        let feed = |call: usize, src: &mut Queue| {
            if call == 3 {
                for n in 3..8 {
                    src.0.push_back(block(n, T0 + 20.0 * n as f64));
                }
            }
        };
        let check = |call: usize, s: FillStats, p: &Playout| {
            assert_eq!(s.resynced, call == 0 || call == 3, "call {call}");
            if call == 4 {
                let error = p.last_error_ms().expect("cursor on a block");
                assert!(error.abs() < FRAME_MS / 2.0, "error {error} ms");
            }
        };
        let played = run(&mut p, &mut src, FRAMES, 5, feed, check);
        for (k, &f) in played.iter().enumerate() {
            let want = if (2880..3072).contains(&k) {
                None
            } else {
                Some(k as u64)
            };
            assert_eq!(f, want, "output frame {k}");
        }
    }

    #[test]
    fn single_missing_block_leaves_a_silent_gap() {
        // Block 2 is lost: the step from the end of block 1 to block 3 is exactly 20 ms. Output
        // frames 1920..2880 (block 2's slot) are silent and block 3 starts exactly at its
        // target, output frame 2880; none of its frames plays earlier.
        const FRAMES: usize = 1024;
        let mut p = playout();
        let blocks = [0, 1, 3, 4].map(|n| block(n, T0 + 20.0 * n as f64));
        let mut src = Queue(VecDeque::from(blocks));
        let mut silent = 0;
        let check = |call: usize, s: FillStats, _: &Playout| {
            assert_eq!(s.resynced, call == 0 || call == 1, "call {call}");
            silent += s.silent_frames;
        };
        let played = run(&mut p, &mut src, FRAMES, 4, |_, _| {}, check);
        for (k, &f) in played.iter().enumerate() {
            let want = if (1920..2880).contains(&k) {
                None
            } else {
                Some(k as u64)
            };
            assert_eq!(f, want, "output frame {k}");
        }
        assert_eq!(silent, 960);
    }

    #[test]
    fn underrun_outputs_silence_then_resumes_on_time() {
        // Blocks 0..3 cover output frames 0..2880. The producer comes back before call 6 with
        // block 8 (blocks 3..8 are lost), due at T0 + 160 ms = output frame 7680.
        const FRAMES: usize = 1024;
        let mut p = playout();
        let mut src = contiguous(0..3);
        let mut silent = 0;
        let feed = |call: usize, src: &mut Queue| {
            if call == 6 {
                for n in 8..12 {
                    src.0.push_back(block(n, T0 + 20.0 * n as f64));
                }
            }
        };
        let check = |call: usize, s: FillStats, _: &Playout| {
            // Call 6 finds block 8 after the underrun and resyncs to wait for its target.
            assert_eq!(s.resynced, call == 0 || call == 6, "call {call}");
            silent += s.silent_frames;
        };
        let played = run(&mut p, &mut src, FRAMES, 10, feed, check);
        for (k, &f) in played.iter().enumerate() {
            let want = if (2880..7680).contains(&k) {
                None
            } else {
                Some(k as u64)
            };
            assert_eq!(f, want, "output frame {k}");
        }
        assert_eq!(silent, 7680 - 2880);
    }
}
