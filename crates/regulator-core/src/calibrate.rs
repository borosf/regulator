// SPDX-License-Identifier: GPL-2.0
//! Sound-card calibration, ported from tg's calibration_data: measure the
//! audio input's clock error by timing a quartz watch that ticks exactly
//! once per second, then subtract the apparent rate from measurements.


use crate::analyze::Analyzer;

/// tg collects CAL_DATA_SIZE one-second ticks before computing the result.
pub const CAL_DATA_SIZE: usize = 900;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CalStatus {
    /// Collected `have` of `need` one-second samples.
    Collecting { have: usize, need: usize },
    /// Calibration finished; value in seconds per day.
    Done(f64),
    /// This block could not be used (signal lost); try the next second.
    Missed,
    /// Calibration failed the quality check.
    Failed,
}

/// Accumulates (time, phase) samples of the quartz tick and fits the drift.
pub struct CalibrationState {
    points: Vec<(f64, f64)>,
    need: usize,
    start_ts: Option<u64>,
    cal: Option<f64>,
}

impl Default for CalibrationState {
    fn default() -> Self {
        Self::new()
    }
}

impl CalibrationState {
    pub fn new() -> Self {
        CalibrationState::with_size(CAL_DATA_SIZE)
    }

    /// Calibration over a custom number of one-second samples (tests use a
    /// shorter run; tg itself always uses CAL_DATA_SIZE).
    pub fn with_size(need: usize) -> Self {
        CalibrationState {
            points: Vec::new(),
            need,
            start_ts: None,
            cal: None,
        }
    }

    /// The computed calibration in s/day, if finished.
    pub fn calibration(&self) -> Option<f64> {
        self.cal
    }

    /// The (time, phase) samples collected so far, for live feedback.
    pub fn points(&self) -> &[(f64, f64)] {
        &self.points
    }

    /// The current best-fit calibration and its standard error in s/day,
    /// even if the quality gate has not been met yet.
    pub fn estimate(&self) -> Option<(f64, f64)> {
        Self::fit_with_delta(&self.points).ok()
    }

    /// Progress: (collected, needed).
    pub fn progress(&self) -> (usize, usize) {
        (self.points.len(), self.need)
    }

    /// Feed exactly one second of mic input captured while a quartz watch
    /// ticks once per second. `ts` is the sample counter at block start.
    /// The analyzer must be configured for calibration (see
    /// `Analyzer::for_calibration`).
    pub fn add_block(&mut self, an: &Analyzer, samples: &[f32], ts: u64) -> CalStatus {
        let sr = an.sample_rate as usize;
        if samples.len() < 2 * sr {
            return CalStatus::Missed;
        }
        if self.cal.is_some() {
            return CalStatus::Done(self.cal.unwrap());
        }
        let mut s = samples.to_vec();
        let n = s.len();
        // tg's process_cal runs with the noise suppressor disabled.
        an.prepare_data(&mut s);
        let period = match an.compute_period(&s, n) {
            Some(p) => p,
            None => return CalStatus::Missed,
        };
        let (wf, phase, _ps) = an.compute_waveform(&s, period, n);

        // Lock check: the tick must sit near the middle of the folded window.
        let wf_max = wf.iter().cloned().fold(0f32, f32::max);
        let wf_max_i = wf.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap_or(0);
        if wf_max_i < sr * 4 / 10 || wf_max_i > sr * 6 / 10 {
            return CalStatus::Missed;
        }
        // Rising-edge check between the quiet zones.
        let maxa = wf[..sr / 4].iter().cloned().fold(0f32, f32::max);
        let maxb = wf[sr * 3 / 4..].iter().cloned().fold(0f32, f32::max);
        let max = maxa.max(maxb);
        let thd = max + (wf_max - max) * 0.05;
        let mut tick_i = None;
        for i in sr / 4..=wf_max_i {
            if wf[i] > thd {
                if i > sr * 4 / 10 {
                    tick_i = Some(i);
                }
                break;
            }
        }
        let Some(tick_i) = tick_i else {
            return CalStatus::Missed;
        };
        let phase_tick = ((ts + phase as u64 + tick_i as u64) as f64 / sr as f64).fract();

        // One fold-measured phase point per window, on a decoupled time
        // axis (window-boundary seconds): coupling the phase to the
        // tick's own arrival time collapses the fit's information (the
        // phase would be a deterministic function of the time axis, and
        // the drift would never show). This is tg's construction.
        let start = *self.start_ts.get_or_insert(ts);
        let time = (ts - start) as f64 / sr as f64;
        if self.points.is_empty() || time > self.points.last().unwrap().0 + 0.9 {
            self.points.push((time, phase_tick));
        }

        // Try to finish early: once enough points have been collected,
        // refit on every new sample and stop as soon as the confidence
        // interval is tight enough (tg always waits for all 900 points).
        // Points are one per 4-second window; 20 points (~80 s) is the
        // shortest run worth fitting, and the robust inlier cut in the
        // fit tolerates a bad window. The 0.1 s/d gate decides the rest.
        let min_points = self.need.min(20);
        if self.points.len() >= min_points {
            if let Ok(cal) = Self::fit(&self.points) {
                self.cal = Some(cal);
                return CalStatus::Done(cal);
            }
            if self.points.len() >= self.need {
                return CalStatus::Failed;
            }
        }
        let have = self.points.len();
        CalStatus::Collecting { have, need: self.need }
    }

    /// tg's compute_cal: unwrap the circular phases around their mean
    /// direction, then least-squares fit the drift; the slope in s/d is the
    /// sound card's apparent rate error.
    fn fit(points: &[(f64, f64)]) -> Result<f64, ()> {
        let (cal, delta) = Self::fit_with_delta(points)?;
        // tg's exact gate: the fit must be certain to 0.1 s/d. Loosening
        // this accelerated convergence but let correlated, overlapping
        // windows exit early with a biased fit.
        if delta >= 0.1 {
            return Err(());
        }
        Ok(cal)
    }

    fn fit_with_delta(points: &[(f64, f64)]) -> Result<(f64, f64), ()> {
        let n = points.len();
        if n < 3 {
            return Err(());
        }
        let mut x = 0.0;
        let mut y = 0.0;
        for &(_, p) in points {
            x += (p * 2.0 * std::f64::consts::PI).cos();
            y += (p * 2.0 * std::f64::consts::PI).sin();
        }
        let c = (y.atan2(x) - std::f64::consts::PI) / (2.0 * std::f64::consts::PI);
        // Continuous unwrap: center on the circular mean, then remove
        // +/-1 jumps between successive points. Without this, a phase
        // drift that crosses the 0/1 branch cut mid-run splits the point
        // cluster across the cut and the least-squares fit sees a phase
        // jump of a full second — a garbage slope and a huge delta (the
        // calibration silently fails or exits wildly wrong).
        let mut adjusted: Vec<(f64, f64)> = Vec::with_capacity(n);
        let mut prev: Option<f64> = None;
        for &(t, p) in points.iter() {
            let mut u = (p - c).fract();
            if let Some(pv) = prev {
                while u - pv > 0.5 {
                    u -= 1.0;
                }
                while u - pv < -0.5 {
                    u += 1.0;
                }
            }
            prev = Some(u);
            adjusted.push((t, u));
        }
        // Robust two-pass fit inliers only: a single bad window (a
        // poorly anchored fold, a stray noise tick) would otherwise drag
        // the slope and inflate delta; the regression is re-fit after
        // discarding points whose residual exceeds 4 robust sigmas.
        let inliers = robust_inliers(&adjusted);
        if inliers.len() < 3 {
            return Err(());
        }
        let t_av = inliers.iter().map(|&(t, _)| t).sum::<f64>() / inliers.len() as f64;
        let p_av = inliers.iter().map(|&(_, p)| p).sum::<f64>() / inliers.len() as f64;
        let (mut xx, mut xy, mut yy) = (0.0, 0.0, 0.0);
        for &(t, p) in inliers.iter() {
            let dt = t - t_av;
            let dp = p - p_av;
            xx += dt * dt;
            xy += dt * dp;
            yy += dp * dp;
        }
        if xx == 0.0 {
            return Err(());
        }
        let calibration = xy * 3600.0 * 24.0 / xx;
        let delta = ((xx * yy - xy * xy) / (inliers.len() as f64 - 2.0)).sqrt() / xx;
        Ok((calibration, delta * 3600.0 * 24.0))
    }
}

fn robust_inliers(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    // Initial least-squares fit, residuals, MAD-based outlier cut,
    // second fit on the survivors.
    let n = points.len();
    if n < 5 {
        return points.to_vec();
    }
    let t_av = points.iter().map(|&(t, _)| t).sum::<f64>() / n as f64;
    let p_av = points.iter().map(|&(_, p)| p).sum::<f64>() / n as f64;
    let (mut xx, mut xy) = (0.0, 0.0);
    for &(t, p) in points.iter() {
        let dt = t - t_av;
        xx += dt * dt;
        xy += dt * (p - p_av);
    }
    if xx == 0.0 {
        return points.to_vec();
    }
    let slope = xy / xx;
    let resid: Vec<f64> = points
        .iter()
        .map(|&(t, p)| (p - p_av) - slope * (t - t_av))
        .collect();
    let mut sorted = resid.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mad = (sorted[sorted.len() / 2]).abs().max(1e-12);
    let keep: Vec<usize> = resid
        .iter()
        .enumerate()
        .filter(|(_, r)| r.abs() < 4.0 * mad)
        .map(|(i, _)| i)
        .collect();
    if keep.len() < 5 {
        return points.to_vec();
    }
    keep.into_iter().map(|i| points[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::SyntheticWatch;

    #[test]
    fn calibration_recovers_sound_card_error() {
        // A quartz watch ticking exactly once per second, as recorded by a
        // sound card whose clock runs 15 s/day fast. The ticks then appear
        // 15 s/day slow, so the calibration correction is -15 s/d:
        // measured - calibration = 0 for a perfect source.
        const SR: u32 = 44100;
        let quartz = SyntheticWatch {
            bph: 3600, // one tick per second
            rate_sday: 15.0,
            beat_error_ms: 0.0,
            amplitude_deg: 250.0,
            quartz: true,
        };
        let samples = quartz.generate(SR, 160.0);
        let an = Analyzer::for_calibration(SR);
        let mut state = CalibrationState::with_size(60);
        let win = 4 * SR as usize;
        for i in 0..40 {
            let start = i * win;
            if start + win > samples.len() {
                break;
            }
            let st = state.add_block(&an, &samples[start..start + win], start as u64);
            match st {
                CalStatus::Done(cal) => {
                    assert!((cal - (-15.0)).abs() < 1.0, "calibration: {cal}");
                    return;
                }
                CalStatus::Failed => panic!("calibration failed quality check"),
                _ => {}
            }
        }
        panic!("calibration never completed");
    }
}
