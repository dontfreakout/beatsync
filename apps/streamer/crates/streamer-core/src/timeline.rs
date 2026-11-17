//! Capture timeline: turns noisy per-block capture times into steady timestamps.

/// Smooths the server time of capture blocks.
///
/// Short-term the output is exactly `frame_ms` apart, apart from a slow offset correction.
/// Long-term it follows the observed times through a first-order loop on the offset.
pub struct CaptureTimeline {
    frame_ms: f64,
    alpha: f64,
    reanchor_ms: f64,
    base: f64,
    n: u64,
    off: f64,
    started: bool,
}

impl CaptureTimeline {
    pub fn new(frame_ms: f64, alpha: f64, reanchor_ms: f64) -> Self {
        Self { frame_ms, alpha, reanchor_ms, base: 0.0, n: 0, off: 0.0, started: false }
    }

    /// Returns the smoothed timestamp for the next block. Call once per block, in order.
    pub fn stamp(&mut self, observed_server_ms: f64) -> f64 {
        if !self.started {
            self.started = true;
            self.base = observed_server_ms;
            self.n = 0;
            self.off = 0.0;
            return observed_server_ms;
        }
        self.n += 1;
        let step = self.n as f64 * self.frame_ms;
        let nominal = self.base + step + self.off;
        let err = observed_server_ms - nominal;
        if err.abs() > self.reanchor_ms {
            self.base = observed_server_ms - step;
            self.off = 0.0;
            return observed_server_ms;
        }
        self.off += self.alpha * err;
        self.base + step + self.off
    }

    /// Forgets all state. Call when capture restarts.
    pub fn reset(&mut self) {
        self.started = false;
        self.base = 0.0;
        self.n = 0;
        self.off = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeline() -> CaptureTimeline {
        CaptureTimeline::new(20.0, 0.002, 50.0)
    }

    /// Deterministic xorshift64, returns a value in [-2.0, 2.0].
    fn jitter(state: &mut u64) -> f64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state % 4001) as f64 / 1000.0 - 2.0
    }

    #[test]
    fn perfect_input_gives_exact_spacing() {
        let mut t = timeline();
        for n in 0..500 {
            let observed = 1000.0 + 20.0 * n as f64;
            assert_eq!(t.stamp(observed), observed);
        }
    }

    #[test]
    fn jitter_is_attenuated() {
        let mut t = timeline();
        let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
        let mut prev = None;
        for n in 0..2000 {
            let observed = 1000.0 + 20.0 * n as f64 + jitter(&mut rng);
            let s = t.stamp(observed);
            if let Some(p) = prev {
                let d: f64 = s - p;
                assert!((d - 20.0).abs() <= 0.01, "block {n}: diff {d}");
            }
            prev = Some(s);
        }
    }

    #[test]
    fn drift_is_tracked() {
        let mut t = timeline();
        let mut last = (0.0, 0.0);
        for n in 0..3000 {
            let observed = 1000.0 + 20.002 * n as f64;
            last = (t.stamp(observed), observed);
        }
        assert!((last.0 - last.1).abs() < 2.0, "error {}", last.0 - last.1);
    }

    #[test]
    fn jump_reanchors() {
        let mut t = timeline();
        let mut prev = None;
        for n in 0..1000 {
            let jump = if n >= 500 { 100.0 } else { 0.0 };
            let observed = 1000.0 + 20.0 * n as f64 + jump;
            let s = t.stamp(observed);
            if n == 500 {
                assert_eq!(s, observed);
            }
            if let Some(p) = prev.filter(|_| n > 500) {
                let d: f64 = s - p;
                assert!((d - 20.0).abs() <= 0.01, "block {n}: diff {d}");
            }
            prev = Some(s);
        }
    }
}
