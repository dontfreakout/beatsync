//! A low-pass filter that matches the Web Audio `BiquadFilterNode` of type "lowpass".

/// Resonance in dB (not linear). The web client sets 0.707 here.
pub const LOW_PASS_Q_DB: f64 = 0.707;
pub const LOW_PASS_MIN_HZ: f32 = 20.0;
/// The "off" value. The filter still runs at this cutoff, as on the web.
pub const LOW_PASS_MAX_HZ: f32 = 20_000.0;

/// Normalized biquad coefficients `[b0, b1, b2, a1, a2]` of the Web Audio low-pass.
/// `q_db` is the resonance in dB. A cutoff at or above the Nyquist frequency passes the signal
/// unchanged. A cutoff at or below 0 Hz gives silence.
pub fn lowpass_coefficients(f0: f64, q_db: f64, fs: f64) -> [f64; 5] {
    let cutoff = f0 / (fs / 2.0);
    if cutoff.is_nan() || cutoff <= 0.0 {
        return [0.0; 5];
    }
    if cutoff >= 1.0 {
        return [1.0, 0.0, 0.0, 0.0, 0.0];
    }
    let w0 = std::f64::consts::TAU * f0 / fs;
    let (sin, cos) = w0.sin_cos();
    let alpha = sin / (2.0 * 10f64.powf(q_db / 20.0));
    let a0 = 1.0 + alpha;
    let b = (1.0 - cos) / 2.0;
    [
        b / a0,
        (1.0 - cos) / a0,
        b / a0,
        -2.0 * cos / a0,
        (1.0 - alpha) / a0,
    ]
}

/// A stereo low-pass filter with an exponential cutoff ramp. Real-time safe.
pub struct LowPass {
    fs: f64,
    /// Cutoff at the start of the ramp.
    start_hz: f64,
    target_hz: f64,
    /// Frames of the ramp already processed.
    done: u32,
    /// Frames of the ramp; 0 when the cutoff is at the target.
    len: u32,
    /// Coefficients for the next frame when no ramp runs.
    coeffs: [f64; 5],
    /// Per channel: `[x1, x2, y1, y2]`.
    state: [[f64; 4]; 2],
}

impl LowPass {
    /// A filter at `freq_hz` with no ramp.
    pub fn new(fs: f32, freq_hz: f32) -> Self {
        let fs = f64::from(fs);
        let hz = clamp_hz(freq_hz);
        Self {
            fs,
            start_hz: hz,
            target_hz: hz,
            done: 0,
            len: 0,
            coeffs: lowpass_coefficients(hz, LOW_PASS_Q_DB, fs),
            state: [[0.0; 4]; 2],
        }
    }

    /// Ramps the cutoff from its current value to `freq_hz` (clamped to 20..20000 Hz) over
    /// `ramp_s` seconds, along an exponential curve. `ramp_s <= 0` jumps. Setting the same
    /// target again keeps the running ramp, so callers can set it every buffer.
    pub fn set_target(&mut self, freq_hz: f32, ramp_s: f32) {
        let target = clamp_hz(freq_hz);
        if target == self.target_hz {
            return;
        }
        let frames = (f64::from(ramp_s) * self.fs).round();
        self.start_hz = self.current_hz();
        self.target_hz = target;
        self.done = 0;
        // `as` saturates; NaN becomes 0, which jumps.
        self.len = if frames >= 1.0 { frames as u32 } else { 0 };
        if self.len == 0 {
            self.start_hz = target;
            self.coeffs = lowpass_coefficients(target, LOW_PASS_Q_DB, self.fs);
        }
    }

    /// Filters interleaved stereo in place. The coefficients change every frame during a ramp.
    /// A sample that is not finite (NaN or infinite) is taken as 0, and a channel whose output
    /// is not finite plays 0 and starts over from silence, so one bad sample never silences
    /// the output for good.
    pub fn process(&mut self, stereo: &mut [f32]) {
        for frame in stereo.as_chunks_mut::<2>().0 {
            if self.done < self.len {
                let hz = self.current_hz();
                self.coeffs = lowpass_coefficients(hz, LOW_PASS_Q_DB, self.fs);
                self.done += 1;
                if self.done == self.len {
                    // The next frame is at the target.
                    self.start_hz = self.target_hz;
                }
            }
            let [b0, b1, b2, a1, a2] = self.coeffs;
            for (sample, s) in frame.iter_mut().zip(self.state.iter_mut()) {
                let x = if sample.is_finite() {
                    f64::from(*sample)
                } else {
                    0.0
                };
                let y = b0 * x + b1 * s[0] + b2 * s[1] - a1 * s[2] - a2 * s[3];
                let out = y as f32;
                if out.is_finite() {
                    *s = [x, s[0], y, s[2]];
                    *sample = out;
                } else {
                    *s = [0.0; 4];
                    *sample = 0.0;
                }
            }
            if self.done == self.len && self.len != 0 {
                // Ramp finished: coefficients for the target.
                self.coeffs = lowpass_coefficients(self.target_hz, LOW_PASS_Q_DB, self.fs);
                self.len = 0;
                self.done = 0;
            }
        }
    }

    /// The cutoff for the next frame.
    fn current_hz(&self) -> f64 {
        if self.done >= self.len {
            self.target_hz
        } else {
            let t = f64::from(self.done) / f64::from(self.len);
            self.start_hz * (self.target_hz / self.start_hz).powf(t)
        }
    }
}

fn clamp_hz(freq_hz: f32) -> f64 {
    // `clamp` keeps NaN, so map it to the "off" value.
    if freq_hz.is_nan() {
        return f64::from(LOW_PASS_MAX_HZ);
    }
    f64::from(freq_hz.clamp(LOW_PASS_MIN_HZ, LOW_PASS_MAX_HZ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coefficients_match_web_audio() {
        let c = lowpass_coefficients(1000.0, 0.707, 48_000.0);
        let want = [
            0.0040348285,
            0.0080696571,
            0.0040348285,
            -1.8703659559,
            0.8865052701,
        ];
        for (got, want) in c.iter().zip(want) {
            assert!((got - want).abs() < 1e-9, "{c:?}");
        }
        assert_eq!(
            lowpass_coefficients(24_000.0, 0.707, 48_000.0),
            [1.0, 0.0, 0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn steady_state_gain_matches_chrome() {
        for (hz, want_db) in [(100.0, 0.0497), (1000.0, 0.7070), (5000.0, -28.3860)] {
            let mut f = LowPass::new(48_000.0, 1000.0);
            let n = 2000 + 24_000;
            let mut buf = Vec::with_capacity(2 * n);
            for i in 0..n {
                let s = (std::f64::consts::TAU * hz * i as f64 / 48_000.0).sin() as f32;
                buf.extend([s, s]);
            }
            let input = buf.clone();
            f.process(&mut buf);
            let rms = |b: &[f32]| {
                let sum: f64 = b[2 * 2000..]
                    .chunks(2)
                    .map(|x| f64::from(x[0]).powi(2))
                    .sum();
                (sum / (n - 2000) as f64).sqrt()
            };
            let db = 20.0 * (rms(&buf) / rms(&input)).log10();
            println!("{hz} Hz: {db:.4} dB");
            assert!(
                (db - want_db).abs() < 0.01,
                "{hz} Hz: {db} dB, want {want_db}"
            );
        }
    }

    #[test]
    fn ramp_midpoint_is_the_geometric_mean() {
        let mut f = LowPass::new(48_000.0, 20_000.0);
        f.set_target(500.0, 1.0);
        f.set_target(500.0, 1.0); // the same target keeps the ramp
        let mut buf = vec![0.0_f32; 2 * 24_000];
        f.process(&mut buf);
        let mid = f.current_hz();
        println!("midpoint {mid} Hz");
        assert!((mid - 3162.3).abs() < 1.0, "{mid}");
        let mut rest = vec![0.0_f32; 2 * 24_000];
        f.process(&mut rest);
        assert_eq!(f.current_hz(), 500.0);
        assert_eq!(
            f.coeffs,
            lowpass_coefficients(500.0, LOW_PASS_Q_DB, 48_000.0)
        );
    }

    #[test]
    fn a_nan_sample_does_not_poison_the_filter() {
        // One bad sample must not make the output silent or garbage until Leave: it plays
        // as silence, and the filter goes on as if it had been 0.
        let signal = |i: usize| (std::f64::consts::TAU * 440.0 * i as f64 / 48_000.0).sin() as f32;
        let frames = 4800;
        let mut bad = Vec::with_capacity(2 * frames);
        let mut clean = Vec::with_capacity(2 * frames);
        for i in 0..frames {
            let s = signal(i);
            let (left, right) = match i {
                100 => (f32::NAN, f32::INFINITY),
                200 => (f32::NEG_INFINITY, f32::NAN),
                _ => (s, s),
            };
            bad.extend([left, right]);
            clean.extend(if i == 100 || i == 200 {
                [0.0, 0.0]
            } else {
                [s, s]
            });
        }
        let mut filter = LowPass::new(48_000.0, 1000.0);
        filter.process(&mut bad);
        LowPass::new(48_000.0, 1000.0).process(&mut clean);
        assert!(bad.iter().all(|s| s.is_finite()), "every sample is finite");
        assert_eq!(bad, clean);

        // A state that went bad (an output sample that is not finite) starts over.
        filter.state[0] = [f64::NAN; 4];
        filter.state[1] = [f64::INFINITY, 0.0, f64::NEG_INFINITY, 0.0];
        let mut after = vec![0.5_f32; 2 * 100];
        filter.process(&mut after);
        assert!(after.iter().all(|s| s.is_finite()), "{after:?}");
        assert!(
            after[2 * 99] > 0.1,
            "the filter plays again: {}",
            after[2 * 99]
        );
    }

    #[test]
    fn a_new_target_mid_ramp_starts_from_the_current_cutoff() {
        let mut f = LowPass::new(48_000.0, 20_000.0);
        f.set_target(500.0, 1.0);
        f.process(&mut vec![0.0_f32; 2 * 24_000]);
        let mid = f.current_hz();
        f.set_target(10_000.0, 1.0);
        assert!((f.current_hz() - mid).abs() < 1e-9);
        f.set_target(100.0, 0.0);
        assert_eq!(f.current_hz(), 100.0);
    }
}
