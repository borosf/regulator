// SPDX-License-Identifier: GPL-2.0
//! Synthetic watch signal generator, used by the tests and the TUI demo
//! mode. Produces a train of decaying tick bursts at the requested rate.

/// Parameters of the synthetic movement.
#[derive(Clone, Copy, Debug)]
pub struct SyntheticWatch {
    /// Beats per hour, e.g. 28800.
    pub bph: u32,
    /// Rate error in seconds per day (positive = fast).
    pub rate_sday: f64,
    /// Beat error in milliseconds.
    pub beat_error_ms: f64,
    /// Balance amplitude in degrees (affects pulse width).
    pub amplitude_deg: f64,
    /// Quartz mode: a sharp click once per second (no escapement build-up),
    /// for sound-card calibration against a quartz watch.
    pub quartz: bool,
}

impl SyntheticWatch {
    /// Generate `duration_s` seconds of audio at `sample_rate`.
    pub fn generate(&self, sample_rate: u32, duration_s: f64) -> Vec<f32> {
        // Ticks (tic, toc, tic, ...) occur every 3600/bph seconds; the
        // same-type period the analyzer locks onto is twice that.
        let nominal_period = 2.0 * 3600.0 / self.bph as f64;
        let period = nominal_period * 86400.0 / (86400.0 + self.rate_sday);
        let spacing = period / 2.0;
        let n = (duration_s * sample_rate as f64) as usize;
        let mut out = vec![0f32; n];
        let sr = sample_rate as f64;
        let burst_len = (0.02 * sr) as usize;
        let mut t = 0.0f64;
        let mut flip = false;
        let mut rng = 0x1234_5678u64;
        while t < duration_s {
            let idx = (t * sr) as usize;
            let shift = if flip {
                self.beat_error_ms / 2000.0
            } else {
                -self.beat_error_ms / 2000.0
            };
            let start = idx as isize + (shift * sr) as isize;
            if start >= 0 && (start as usize) < n.saturating_sub(burst_len) {
                let base = start as usize;
                // Pulse width follows the lift-angle/impulse relation the
                // analyzer inverts: amp_abs = 0.5 / sin(pi * pulse / period).
                // A 52 deg lift angle is assumed here.
                let amp_abs = self.amplitude_deg / 52.0;
                let width = (period
                    * (0.5 / amp_abs).asin()
                    / std::f64::consts::PI
                    * sr) as usize;
                let width = width.min(burst_len);
                // Real escapement sounds build up before the main impulse:
                // a rising pre-hump about one pulse width before the tick,
                // then the sharp attack at the tick itself. tg's amplitude
                // scan measures the distance from the pre-hump's local
                // maximum to the tick, which tracks `width`.
                if !self.quartz {
                    for i in 0..width {
                        let tt = i as f64 / sr;
                        let bump = (std::f64::consts::PI * i as f64 / width as f64).sin();
                        let env = bump * bump * 0.35;
                        let carrier = (2.0 * std::f64::consts::PI * 3500.0 * tt).sin();
                        let idx = base as isize + i as isize - 3 * width as isize / 2;
                        if idx >= 0 && (idx as usize) < n {
                            out[idx as usize] += (env * carrier) as f32;
                        }
                    }
                }
                for i in 0..width / 3 {
                    let tt = i as f64 / sr;
                    let env = (-4.0 * i as f64 / (width / 3) as f64).exp();
                    let carrier = (2.0 * std::f64::consts::PI * 3500.0 * tt).sin();
                    let idx = base as isize + i as isize;
                    if idx >= 0 && (idx as usize) < n {
                        out[idx as usize] += (env * carrier * 0.8) as f32;
                    }
                }
            }
            flip = !flip;
            t += spacing;
            // A little jitter so the signal is not perfectly periodic.
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            t += ((rng >> 33) as f64 / u32::MAX as f64 - 0.5) * 1e-5;
        }
        // Background noise.
        let mut noise_rng = 0xdead_beefu64;
        for v in out.iter_mut() {
            noise_rng = noise_rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = ((noise_rng >> 11) as i64 % 2000) as f64 / 1000.0 - 1.0;
            *v += (r * 0.02) as f32;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::Analyzer;

    const SR: u32 = 44100;

    #[test]
    fn detects_rate_error() {
        let watch = SyntheticWatch {
            bph: 28800,
            rate_sday: 12.0,
            beat_error_ms: 0.4,
            amplitude_deg: 270.0,
            quartz: false,
        };
        let samples = watch.generate(SR, 4.0);
        let mut an = Analyzer::new(SR);
        an.bph = 28800;
        let r = an.analyze(&samples).expect("analysis failed");
        assert!((r.rate_sday.unwrap() - 12.0).abs() < 3.0, "rate: {:?}", r.rate_sday);
    }

    #[test]
    fn auto_detects_beat_rate() {
        let watch = SyntheticWatch {
            bph: 18000,
            rate_sday: -5.0,
            beat_error_ms: 0.0,
            amplitude_deg: 250.0,
            quartz: false,
        };
        let samples = watch.generate(SR, 6.0);
        let an = Analyzer::new(SR);
        let r = an.analyze(&samples).expect("analysis failed");
        let detected_bph = 2.0 * 3600.0 * SR as f64 / r.period;
        assert!(
            (detected_bph - 18000.0).abs() < 100.0,
            "detected bph: {detected_bph}"
        );
    }

    #[test]
    fn detects_beat_error() {
        let watch = SyntheticWatch {
            bph: 21600,
            rate_sday: 0.0,
            beat_error_ms: 1.2,
            amplitude_deg: 260.0,
            quartz: false,
        };
        let samples = watch.generate(SR, 4.0);
        let mut an = Analyzer::new(SR);
        an.bph = 21600;
        let r = an.analyze(&samples).expect("analysis failed");
        assert!(
            (r.beat_error_ms.unwrap() - 1.2).abs() < 0.8,
            "beat error: {:?}",
            r.beat_error_ms
        );
    }

    #[test]
    fn detects_amplitude() {
        let watch = SyntheticWatch {
            bph: 28800,
            rate_sday: 0.0,
            beat_error_ms: 0.0,
            amplitude_deg: 270.0,
            quartz: false,
        };
        let samples = watch.generate(SR, 4.0);
        let mut an = Analyzer::new(SR);
        an.bph = 28800;
        let r = an.analyze(&samples).expect("analysis failed");
        let amp = r.amplitude_deg.expect("amplitude not found");
        assert!((amp - 270.0).abs() < 25.0, "amplitude: {amp}");
    }

    #[test]
    fn rejects_silence() {
        let an = Analyzer::new(SR);
        assert!(an.analyze(&vec![0.0; SR as usize * 2]).is_none());
    }
}
