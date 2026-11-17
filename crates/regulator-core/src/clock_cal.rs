// SPDX-License-Identifier: GPL-2.0
//! Clock-based sound-card calibration.
//!
//! tg measures the sound card's clock error against a quartz watch ticking
//! once per second, which needs many minutes of phase samples. But the
//! quantity of interest is simply how far the ADC's sample rate drifts from
//! real time - and the host's system clock (NTP-disciplined) is a better
//! reference than any wristwatch. Regressing the cumulative sample count
//! against batch arrival times gives the same correction passively, with no
//! watch and no calibration mode, converging in a few minutes.

use std::time::Instant;

/// Live clock-calibration accumulator: feed it captured samples as they
/// arrive; it regresses the cumulative count against wall time.
#[derive(Debug)]
pub struct ClockCal {
    sample_rate: f64,
    start: Option<Instant>,
    total: u64,
    /// (t seconds since start, cumulative samples), thinned to ~4 Hz.
    points: Vec<(f64, f64)>,
    last_point: f64,
}

impl ClockCal {
    pub fn new(sample_rate: u32) -> Self {
        ClockCal {
            sample_rate: sample_rate as f64,
            start: None,
            total: 0,
            points: Vec::new(),
            last_point: f64::NEG_INFINITY,
        }
    }

    /// Record `n` newly captured samples.
    pub fn add(&mut self, n: usize) {
        let t = self
            .start
            .get_or_insert_with(Instant::now)
            .elapsed()
            .as_secs_f64();
        self.add_at(n, t);
    }

    /// Record `n` samples captured at `t` seconds since capture start
    /// (separated for testability).
    pub fn add_at(&mut self, n: usize, t: f64) {
        self.total += n as u64;
        if t - self.last_point >= 0.25 {
            self.last_point = t;
            self.points.push((t, self.total as f64));
        }
    }

    /// The sound-card rate error in s/day, with its uncertainty in s/day,
    /// from a least-squares fit of samples against time. Returns None until
    /// at least two seconds of data have been recorded.
    pub fn estimate(&self) -> Option<(f64, f64)> {
        let n = self.points.len();
        if n < 8 || self.points.last()?.0 < 2.0 {
            return None;
        }
        let (t_av, n_av) = (
            self.points.iter().map(|p| p.0).sum::<f64>() / n as f64,
            self.points.iter().map(|p| p.1).sum::<f64>() / n as f64,
        );
        let (mut stt, mut stn, mut sxx) = (0.0, 0.0, 0.0);
        for &(t, c) in &self.points {
            let (dt, dc) = (t - t_av, c - n_av);
            stt += dt * dt;
            stn += dt * dc;
            sxx += dc * dc;
        }
        if stt <= 0.0 {
            return None;
        }
        let slope = stn / stt; // samples per second
        let err = (slope / self.sample_rate - 1.0) * 86400.0;
        // Standard error of the slope from the residuals.
        let syy = sxx - slope * stn;
        let resid_var = (syy / (n as f64 - 2.0)).max(0.0);
        let slope_err = (resid_var / stt).sqrt();
        let unc = slope_err / self.sample_rate * 86400.0;
        Some((err, unc))
    }

    /// True once the fit has converged under `gate` s/day uncertainty.
    pub fn converged(&self, gate: f64) -> Option<f64> {
        let (err, unc) = self.estimate()?;
        if unc <= gate {
            Some(err)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_rate_error() {
        // A card running 12 s/day fast: cumulative samples follow
        // 44100 * (1 + 12/86400) per second.
        let mut cal = ClockCal::new(44100);
        let rate = 44100.0_f64 * (1.0 + 12.0 / 86400.0);
        // 0.25 s batches over 300 s, with +/-1 ms of jitter per batch.
        for i in 0..1200 {
            let t = (i + 1) as f64 * 0.25;
            let jitter = if i % 2 == 0 { 0.001 } else { -0.001 };
            let samples = (rate * (0.25 + jitter)).round() as usize;
            cal.add_at(samples, t);
        }
        let (err, unc) = cal.estimate().expect("estimate");
        assert!((err - 12.0).abs() < 1.0, "err: {err}");
        assert!(unc < 1.0, "unc: {unc}");
        let done = cal.converged(1.0).expect("converged");
        assert!((done - 12.0).abs() < 1.0);
    }

    #[test]
    fn needs_data_to_converge() {
        let mut cal = ClockCal::new(44100);
        assert!(cal.estimate().is_none());
        cal.add(44100);
        assert!(cal.estimate().is_none());
        assert!(cal.converged(0.1).is_none());
    }
}
