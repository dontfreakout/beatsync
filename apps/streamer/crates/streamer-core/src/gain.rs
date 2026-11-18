//! Output gain with a linear per-frame ramp, so volume changes do not click.

/// A gain that moves linearly, frame by frame, from its current value to a target.
/// Both channels of a frame get the same gain. Real-time safe.
pub struct GainRamp {
    /// Gain at the start of the ramp.
    start: f32,
    target: f32,
    /// Frames of the ramp already applied.
    done: u32,
    /// Frames of the ramp; 0 when the gain is at the target.
    len: u32,
}

impl GainRamp {
    pub fn new(initial: f32) -> Self {
        Self {
            start: initial,
            target: initial,
            done: 0,
            len: 0,
        }
    }

    /// Ramps from the current gain to `target` over `ramp_s` seconds (`<= 0`: at once).
    /// Setting the same target again keeps the current ramp, so callers can set it every buffer.
    pub fn set_target(&mut self, target: f32, ramp_s: f32, sample_rate: f32) {
        if target == self.target {
            return;
        }
        let frames = (ramp_s * sample_rate).round();
        self.start = self.current();
        self.target = target;
        self.done = 0;
        // `as` saturates; NaN becomes 0, which jumps.
        self.len = if frames >= 1.0 { frames as u32 } else { 0 };
    }

    /// Multiplies `interleaved` (`channels` samples per frame) by the gain, frame by frame.
    pub fn apply(&mut self, interleaved: &mut [f32], channels: usize) {
        if channels == 0 {
            return;
        }
        for frame in interleaved.chunks_mut(channels) {
            let gain = self.current();
            for sample in frame {
                *sample *= gain;
            }
            if self.done < self.len {
                self.done += 1;
            }
        }
    }

    /// The gain for the next frame.
    fn current(&self) -> f32 {
        if self.done >= self.len {
            self.target
        } else {
            self.start + (self.target - self.start) * (self.done as f32 / self.len as f32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_ramp_reaches_target_linearly() {
        // 0.01 s at 48 kHz = 480 frames. Uneven chunks: the ramp continues across calls.
        let mut g = GainRamp::new(0.0);
        g.set_target(1.0, 0.01, 48_000.0);
        let mut buf = vec![1.0_f32; 2 * 1000];
        let (a, rest) = buf.split_at_mut(2 * 100);
        let (b, c) = rest.split_at_mut(2 * 333);
        g.apply(a, 2);
        g.set_target(1.0, 0.01, 48_000.0); // the same target must not restart the ramp
        g.apply(b, 2);
        g.apply(c, 2);
        for (k, &[left, right]) in buf.as_chunks::<2>().0.iter().enumerate() {
            assert_eq!(left, right, "frame {k}");
            let want = if k >= 480 { 1.0 } else { k as f32 / 480.0 };
            assert!((left - want).abs() < 1e-5, "frame {k}: {left} != {want}");
        }
        assert!((buf[2 * 240] - 0.5).abs() < 1e-5);
        assert!(buf[2 * 480..].iter().all(|&s| s == 1.0));
    }
}
