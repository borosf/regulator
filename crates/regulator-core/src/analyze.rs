// SPDX-License-Identifier: GPL-2.0
//! The timing analysis pipeline, ported from tg's algo.c, with improvements:
//! cached FFT plans, buffer reuse, and sub-sample period refinement.

use crate::filters::Biquad;
use crate::stats::{median, tmean, vmax};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftDirection, FftPlanner};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// Result of one analysis pass over a block of audio. Fields are None when
/// that particular measurement could not be determined from the signal.
#[derive(Clone, Debug, Default)]
pub struct TimingResult {
    /// Detected period in samples (time between same-type ticks).
    pub period: f64,
    /// Rate error in seconds per day (positive = fast).
    pub rate_sday: Option<f64>,
    /// Beat error in milliseconds (tic-to-toc asymmetry).
    pub beat_error_ms: Option<f64>,
    /// Balance amplitude in degrees.
    pub amplitude_deg: Option<f64>,
    /// Standard deviation of the period estimate in samples.
    pub sigma: f64,
    /// Folded average waveform over one period, median-subtracted.
    pub waveform: Vec<f32>,
    /// Beat rate the rate was computed against.
    pub bph: u32,
    /// Position of the tic peak in the folded waveform, if found.
    pub tic: Option<usize>,
    /// Located tick events in this block: (sample position, is_tic).
    /// Sample positions are fractional and relative to the block start.
    pub events: Vec<(f64, bool)>,
}

struct FftCache {
    plans: HashMap<usize, Arc<dyn Fft<f32>>>,
    scratch: Vec<Complex<f32>>,
    /// Reusable complex work buffer (the dominant allocation of the
    /// pipeline: 2n complex values per autocorrelation — 22 MB at the
    /// 16-second cascade window; reusing it removes that from every
    /// cycle).
    work: Vec<Complex<f32>>,
}

impl FftCache {
    fn new() -> Self {
        FftCache {
            plans: HashMap::new(),
            scratch: Vec::new(),
            work: Vec::new(),
        }
    }

    fn plan(&mut self, len: usize, direction: FftDirection) -> Arc<dyn Fft<f32>> {
        // rustfft plans are direction-specific; key on len and direction bit.
        let key = len * 2 + (direction == FftDirection::Inverse) as usize;
        if !self.plans.contains_key(&key) {
            let mut planner = FftPlanner::new();
            let fft = match direction {
                FftDirection::Forward => planner.plan_fft_forward(len),
                FftDirection::Inverse => planner.plan_fft_inverse(len),
            };
            self.plans.insert(key, fft);
        }
        self.plans.get(&key).unwrap().clone()
    }
}

/// Standard beat rates recognized by the auto mode.
pub(crate) const STANDARD_RATES: [f64; 9] = [
    12000.0, 14400.0, 18000.0, 19800.0, 21600.0, 25200.0, 28800.0, 36000.0, 43200.0,
];

/// Snap a detected beats-per-hour value to the nearest standard beat rate.
pub(crate) fn snap_bph(detected: f64) -> f64 {
    STANDARD_RATES
        .iter()
        .copied()
        .min_by(|a, b| (a - detected).abs().partial_cmp(&(b - detected).abs()).unwrap())
        .unwrap_or(28800.0)
}

/// Reusable scratch of the noise suppressor (three window-length vectors
/// per call otherwise).
struct NoiseScratch {
    sq: Vec<f32>,
    b: Vec<f32>,
    a: Vec<f32>,
}

/// Configurable analyzer. Reuse one instance across blocks; it caches FFT
/// plans and reuses scratch buffers.
pub struct Analyzer {
    pub sample_rate: u32,
    /// Beats per hour of the movement under test; 0 = auto-detect.
    pub bph: u32,
    /// Lift angle of the movement in degrees (typically 49-53).
    pub lift_angle: f64,
    /// Enable the impulsive-noise suppressor.
    pub noise_suppressor: bool,
    /// Sound-card calibration in s/day (from CalibrationState); subtracted
    /// from the measured rate.
    pub calibration: f64,
    lpf: Biquad,
    hpf: Biquad,
    fft: RefCell<FftCache>,
    noise: RefCell<NoiseScratch>,
    last_sigma: Cell<f64>,
}

use std::cell::Cell;

impl Analyzer {
    pub fn new(sample_rate: u32) -> Self {
        Analyzer {
            sample_rate,
            bph: 0,
            lift_angle: 52.0,
            noise_suppressor: true,
            calibration: 0.0,
            lpf: Biquad::low_pass(crate::FILTER_CUTOFF / sample_rate as f64),
            hpf: Biquad::high_pass(crate::FILTER_CUTOFF / sample_rate as f64),
            fft: RefCell::new(FftCache::new()),
            noise: RefCell::new(NoiseScratch {
                sq: Vec::new(),
                b: Vec::new(),
                a: Vec::new(),
            }),
            last_sigma: Cell::new(0.0),
        }
    }

    /// Analyze one block of samples. `samples` should be at least one second
    /// long; several seconds gives more stable results. Returns None only
    /// when no tick train at all could be found; individual measurements
    /// that fail are None inside the result.
    pub fn analyze(&self, samples: &[f32]) -> Option<TimingResult> {
        if samples.len() < self.sample_rate as usize {
            return None;
        }
        let mut s = samples.to_vec();
        let n = s.len();
        let sr = self.sample_rate as usize;

        self.prepare_data(&mut s);
        let period = self.compute_period(&s, n)?;
        let (waveform, _, phase_shift) = self.compute_waveform(&s, period, n);
        let (beat_error_ms, tic, toc) = match self.compute_beat_error(&waveform, period) {
            Some((be, tic, toc)) => (be, Some(tic), Some(toc)),
            None => (None, None, None),
        };
        let tic_idx = tic;
        let amplitude_deg = match (tic, toc) {
            (Some(tic), Some(toc)) => self.compute_amplitude(&waveform, period, tic, toc),
            _ => None,
        };

        // Locate individual tick events by measuring each arrival: search
        // the prepared signal around each expected position (extrapolated
        // from the folded-waveform peaks at the detected period) for the
        // local maximum. The deviation of the measured peak from the
        // expectation is the per-tick phase error that draws the trace.
        let mut events = Vec::new();
        if let (Some(tic), Some(toc)) = (tic, toc) {
            let search = (period / 16.0).max(4.0) as usize;
            let tic0 = (tic + phase_shift) as f64;
            let toc0 = (toc + phase_shift) as f64;
            let cycles = (n as f64 / period).ceil() as usize + 1;
            for k in 0..cycles {
                for (base, is_tic) in [(tic0, true), (toc0, false)] {
                    let expected = base + k as f64 * period;
                    if expected < -(search as f64) || expected > n as f64 {
                        continue;
                    }
                    let a = (expected as isize - search as isize).max(0) as usize;
                    let b = ((expected as usize) + search).min(n - 1);
                    if b <= a {
                        continue;
                    }
                    // Only accept a peak that actually rises above the
                    // local noise floor; otherwise the tick was missed.
                    let (peak, i_max) = vmax(&s, a, b + 1);
                    let med = median(&mut s[a..=b].to_vec());
                    if peak <= 0.0 || peak < 2.0 * med {
                        continue;
                    }
                    events.push((i_max as f64, is_tic));
                }
            }
        }

        let detected_bph = 2.0 * 3600.0 * sr as f64 / period;
        let bph = if self.bph != 0 {
            self.bph as f64
        } else {
            snap_bph(detected_bph)
        };
        let nominal = 7200.0 * sr as f64 / bph;
        let rate_sday = (nominal / period - 1.0) * 86400.0 - self.calibration;

        Some(TimingResult {
            period,
            rate_sday: Some(rate_sday),
            beat_error_ms,
            amplitude_deg,
            sigma: self.last_sigma.get(),
            waveform,
            bph: bph as u32,
            tic: tic_idx,
            events,
        })
    }

    /// An analyzer configured for sound-card calibration against a quartz
    /// watch ticking once per second (tg's 7200 bph nominal period).
    pub fn for_calibration(sample_rate: u32) -> Self {
        let mut an = Analyzer::new(sample_rate);
        an.bph = 7200;
        an.noise_suppressor = false;
        an
    }

    /// Rectify, low-pass, normalize and window the input.
    pub(crate) fn prepare_data(&self, s: &mut Vec<f32>) {
        let n = s.len();
        let sr = self.sample_rate as usize;
        self.hpf.run(s);
        if self.noise_suppressor {
            self.noise_suppressor(s);
        }
        for v in s.iter_mut() {
            *v = v.abs();
        }
        self.lpf.run(s);
        let average: f64 = s.iter().map(|&v| v as f64).sum::<f64>() / n as f64;
        for v in s.iter_mut() {
            *v -= average as f32;
        }
        let edge = sr / 10;
        for i in 0..edge.min(n / 2) {
            let k = (1.0 - (i as f64 * std::f64::consts::PI / edge as f64).cos()) / 2.0;
            s[i] *= k as f32;
            let j = n - i - 1;
            s[j] *= k as f32;
        }
    }

    /// Zero out samples in windows whose energy greatly exceeds the median
    /// window energy (impulsive noise gating), ported from tg.
    fn noise_suppressor(self: &Analyzer, s: &mut [f32]) {
        let sr = self.sample_rate as usize;
        let n = s.len();
        let window = sr / 50;
        let scratch = &mut *self.noise.borrow_mut();
        let (sq, b, mut a) = (&mut scratch.sq, &mut scratch.b, &mut scratch.a);
        sq.clear();
        sq.extend(s.iter().map(|v| v * v));
        b.clear();
        b.resize(n, 0.0);
        let mut r_av: f64 = sq[..window].iter().map(|&v| v as f64).sum();
        for i in 0..n {
            b[i] = r_av as f32;
            if i + window == n {
                break;
            }
            r_av += sq[i + window] as f64 - sq[i] as f64;
        }
        let m = n - window + 1;
        let step = sr / 2;
        a.clear();
        a.reserve(m / step + 1);
        let mut i = 0;
        while i + step <= m {
            a.push(vmax(&b, i, i + step).0);
            i += step;
        }
        let k = median(&mut a);
        for i in 0..n {
            let mut j = i as isize - (window / 2) as isize;
            if j < 0 {
                j = 0;
            }
            let upper = n.saturating_sub(window) as isize;
            if j > upper {
                j = upper;
            }
            if b[j as usize] > 2.0 * k {
                s[i] = 0.0;
            }
        }
    }

    /// Autocorrelation of s (length n) via cached FFT plans; result has n
    /// points. Unlike tg, the plans are created once per length and reused.
    pub(crate) fn autocorrelate(&self, s: &[f32], n: usize) -> Vec<f32> {
        let mut fft = self.fft.borrow_mut();
        let forward = fft.plan(2 * n, FftDirection::Forward);
        let inverse = fft.plan(2 * n, FftDirection::Inverse);
        let scratch_len = forward.get_inplace_scratch_len().max(inverse.get_inplace_scratch_len());
        if fft.scratch.len() != scratch_len {
            fft.scratch.resize(scratch_len, Complex::default());
        }
        if fft.work.len() != 2 * n {
            fft.work.clear();
            fft.work.resize(2 * n, Complex::default());
        }
        let cache = &mut *fft;
        let work = &mut cache.work;
        let scratch = &mut cache.scratch;
        for (c, &v) in work.iter_mut().zip(s.iter()) {
            *c = Complex::new(v, 0.0);
        }
        for c in work[s.len()..].iter_mut() {
            *c = Complex::default();
        }
        forward.process_with_scratch(work, scratch);
        for c in work.iter_mut() {
            *c = *c * c.conj();
        }
        inverse.process_with_scratch(work, scratch);
        work.truncate(n);
        let out: Vec<f32> = work.iter().map(|c| c.re).collect();
        // Keep the buffer sized for the next call; truncation above only
        // shrank the logical view, and the capacity is retained.
        work.resize(2 * n, Complex::default());
        out
    }

    /// Parabolic interpolation around an integer peak of `ac` for a
    /// sub-sample peak position. This refines the period estimate beyond
    /// tg's integer-sample resolution, improving rate precision.
    fn interpolate_peak(ac: &[f32], p: usize) -> f64 {
        if p == 0 || p + 1 >= ac.len() {
            return p as f64;
        }
        let (a, b, c) = (ac[p - 1] as f64, ac[p] as f64, ac[p + 1] as f64);
        let denom = a - 2.0 * b + c;
        if denom.abs() < 1e-12 {
            return p as f64;
        }
        let delta = 0.5 * (a - c) / denom;
        if delta.abs() <= 1.0 {
            p as f64 + delta
        } else {
            p as f64
        }
    }

    /// tg's peak_detector: the maximum of buff[a..=b] is accepted only if it
    /// is a real peak (rises above the local median, not part of a sustained
    /// plateau). Returns the index or None.
    fn peak_detector(buff: &[f32], a: usize, b: usize) -> Option<usize> {
        if b <= a || b >= buff.len() {
            return None;
        }
        let (max, i_max) = vmax(buff, a, b + 1);
        if max <= 0.0 {
            return None;
        }
        let med = median(&mut buff[a..=b].to_vec());
        if i_max > a + 1 && buff[a + 1..i_max].iter().all(|&v| v > med) {
            return None;
        }
        if i_max == b || buff[i_max + 1..=b].iter().all(|&v| v > med) {
            return None;
        }
        let mut cnt = 0;
        let mut down = true;
        for &v in &buff[a + 1..=b] {
            if v > (max + med) / 2.0 {
                if down {
                    cnt += 1;
                }
                down = false;
            }
            if v < med {
                down = true;
            }
        }
        if cnt > 20 {
            return None;
        }
        Some(i_max)
    }

    /// Autocorrelation-based period estimation, ported from tg's
    /// estimate_period().
    fn estimate_period(&self, ac: &[f32], sr: usize) -> Option<usize> {
        let first = vmax(ac, sr / 12, sr.min(ac.len())).1;
        let a = (sr / 12).max(first.saturating_sub(sr / 12));
        let b = (first + sr / 12).min(ac.len() - 1);
        let first_estimate = Self::peak_detector(ac, a, b)?;
        let mut estimate = first_estimate;
        let mut factor = 1;
        let mut fct = 2;
        while first_estimate / fct > sr / 12 {
            let c = first_estimate / fct;
            if c > sr / 50 {
                if let Some(ne) =
                    Self::peak_detector(ac, c - sr / 50, (c + sr / 50).min(ac.len() - 1))
                {
                    if ac[ne] > 0.9 * ac[first_estimate] {
                        estimate = ne;
                        factor = fct;
                    }
                }
            }
            fct += 1;
        }
        let a = estimate * 3 / 2;
        let max = if a < ac.len() {
            vmax(ac, a.saturating_sub(sr / 50), (a + sr / 50).min(ac.len())).0
        } else {
            0.0
        };
        if max < 0.2 * ac[estimate] {
            let doubled = first_estimate * 2 / factor;
            if doubled < sr {
                return Self::peak_detector(
                    ac,
                    doubled.saturating_sub(sr / 50),
                    (doubled + sr / 50).min(ac.len() - 1),
                );
            }
            return None;
        }
        Some(estimate)
    }

    /// Refine the period estimate by locating peaks at successive multiples
    /// and averaging the per-cycle estimates, ported from tg's
    /// compute_period(). Unlike tg, the final estimate is refined with
    /// parabolic interpolation for sub-sample period resolution.
    pub(crate) fn compute_period(&self, s: &[f32], n: usize) -> Option<f64> {
        let sr = self.sample_rate as usize;
        let ac = self.autocorrelate(s, n);
        let estimate = if self.bph != 0 {
            let c = 7200 * sr / self.bph as usize;
            if c <= sr / 50 || c + sr / 50 >= ac.len() {
                return None;
            }
            Self::peak_detector(&ac, c - sr / 50, c + sr / 50)? as f64
        } else {
            self.estimate_period(&ac, sr)? as f64
        };
        let delta = sr as f64 * 0.02;
        let mut new_estimate = estimate;
        let mut sum = 0.0;
        let mut sq_sum = 0.0;
        let mut count = 0;
        let mut cycle = 1usize;
        let mut last_peak = 0usize;
        loop {
            let inf = (new_estimate * cycle as f64 - delta).floor() as usize;
            let sup = (new_estimate * cycle as f64 + delta).ceil() as usize;
            if sup > n * 2 / 3 || sup >= ac.len() {
                break;
            }
            let peak = Self::peak_detector(&ac, inf, sup)?;
            new_estimate = peak as f64;
            new_estimate /= cycle as f64;
            if new_estimate < estimate - delta || new_estimate > estimate + delta {
                return None;
            }
            if inf > n / 3 {
                sum += new_estimate;
                sq_sum += new_estimate * new_estimate;
                count += 1;
            }
            cycle += 1;
            last_peak = peak;
        }
        let period = if count > 0 { sum / count as f64 } else { estimate };
        self.last_sigma.set(if count > 1 {
            ((sq_sum - count as f64 * period * period) / (count - 1) as f64).sqrt()
        } else {
            0.0
        });
        if period >= 1.2 * 3600.0 * 2.0 * sr as f64 / if self.bph != 0 { self.bph } else { 18000 } as f64
        {
            return None;
        }
        // Sub-sample refinement: interpolate the autocorrelation around the
        // last accepted peak, expressed back as a per-period offset.
        if last_peak > 0 && count > 0 {
            let cycles = (last_peak as f64 / period).round().max(1.0);
            let refined = Self::interpolate_peak(&ac, last_peak) / cycles;
            if (refined - period).abs() < delta {
                return Some(refined);
            }
        }
        Some(period)
    }

    /// Fold the rectified signal at `period` and average into a one-period
    /// waveform, ported from tg's compute_phase()/compute_waveform().
    pub(crate) fn compute_waveform(&self, s: &[f32], period: f64, n: usize) -> (Vec<f32>, f64, usize) {
        let count = period as usize;
        let mut wf = vec![0f64; count];
        let mut j = 0usize;
        let mut i = 0usize;
        while (j as f64 * period).round() < n as f64 {
            let start = (j as f64 * period).round() as usize;
            i = 0;
            while i < count && start + i < n {
                wf[i] += s[start + i] as f64;
                i += 1;
            }
            j += 1;
        }
        let lastfull = i;
        let mut x = 0.0;
        let mut y = 0.0;
        for (i, &w) in wf.iter().enumerate() {
            let a = i as f64 * (2.0 * std::f64::consts::PI / period);
            let w = w / (j as f64 - if i >= lastfull { 1.0 } else { 0.0 });
            x += w * a.cos();
            y += w * a.sin();
        }
        let phase = period * (std::f64::consts::PI + y.atan2(x)) / (2.0 * std::f64::consts::PI);

        let wf_size = count;
        let binsize = 1 + n / wf_size;
        let mut bin = vec![0f32; binsize + 1];
        let mut wf2 = vec![0f32; wf_size];
        let phase_shift = phase.round() as usize;
        for i in 0..wf_size {
            let k = (i + phase_shift) % wf_size;
            let mut j = 0;
            loop {
                let idx = k + j * wf_size;
                if idx >= n {
                    break;
                }
                bin[j] = s[idx];
                j += 1;
            }
            wf2[i] = tmean(&mut bin[..j]);
        }
        let step = (wf_size / 100).max(1);
        let mut sc = Vec::new();
        let mut i = 0;
        while i * step < wf_size {
            sc.push(wf2[i * step]);
            i += 1;
        }
        let nl = median(&mut sc);
        for v in wf2.iter_mut() {
            *v -= nl;
        }
        (wf2, phase, phase_shift)
    }

    /// Beat error: distance between the tic and toc peaks in the folded
    /// waveform, found via the waveform autocorrelation, ported from tg's
    /// compute_parameters(). Returns (beat error ms, tic index, toc index).
    fn compute_beat_error(&self, wf: &[f32], period: f64) -> Option<(Option<f64>, usize, usize)> {
        let sr = self.sample_rate as usize;
        let n = wf.len();
        let ac = self.autocorrelate(wf, n);
        let half = (period / 2.0).floor() as usize;
        let a = half.saturating_sub(sr / 50);
        let b = (half + sr / 50).min(n - 1);
        if b <= a {
            return None;
        }
        let tic_to_toc = Self::peak_detector(&ac, a, b)?;
        let be = period / 2.0 - tic_to_toc as f64;

        // Fold tic and toc together and smooth to locate the tic peak.
        let m = n - tic_to_toc;
        let mut fold = vec![0f32; m];
        for i in 0..m {
            fold[i] = wf[i] + wf[i + tic_to_toc];
        }
        let window = (sr / 2000).max(1);
        if m <= window {
            return None;
        }
        let smooth = Self::smooth(&fold, window);
        let (max, max_i) = vmax(&smooth, 0, smooth.len());
        if max <= 0.0 {
            return None;
        }
        let tic = max_i;
        let toc = tic + tic_to_toc;
        Some((Some(be * 1000.0 / sr as f64), tic, toc))
    }

    /// Amplitude from the tic/toc pulse widths, tg's algorithm: scan the
    /// eighth before each tick for a threshold crossing, follow the
    /// increasing run to its local maximum, and take the distance from
    /// that local max to the tick as the pulse width. Real escapement
    /// sounds have a build-up before the main impulse, so this distance
    /// tracks the impulse duration. A/la = 0.5 / sin(pi * pulse / period).
    fn compute_amplitude(&self, wf: &[f32], period: f64, tic: usize, toc: usize) -> Option<f64> {
        let sr = self.sample_rate as usize;
        let wf_size = wf.len();
        let window = sr / 1000;
        let mut ext = wf.to_vec();
        ext.extend_from_within(0..window.min(wf_size));
        let smooth = Self::smooth(&ext, window);
        if smooth.is_empty() {
            return None;
        }
        let wlen = smooth.len();
        let eighth = (period / 8.0) as usize;
        if eighth == 0 || tic >= wlen || toc >= wlen {
            return None;
        }
        let glob_max = smooth.iter().cloned().fold(0f32, f32::max);
        if glob_max <= 0.0 {
            return None;
        }
        // Noise level in the eighths following each tick.
        let mut quiet = 0.0f32;
        for &t in &[tic, toc] {
            let mut j = ((t as f64 + period / 8.0) % wlen as f64) as usize;
            for _ in 0..eighth {
                if smooth[j] > quiet {
                    quiet = smooth[j];
                }
                j = (j + 1) % wlen;
            }
        }
        let mut threshold = (0.01 * glob_max).max(1.4 * quiet);
        while threshold < 0.2 * glob_max {
            let mut pulses = [0.0f64; 2];
            let mut ok = true;
            for (k, &t) in [toc, tic].iter().enumerate() {
                let mut j = ((t as f64 + 7.0 * period / 8.0) % wlen as f64) as usize;
                let mut i = 0usize;
                while i < eighth {
                    if smooth[j] > threshold {
                        break;
                    }
                    i += 1;
                    j = (j + 1) % wlen;
                }
                let mut max = 0.0f32;
                while i < eighth {
                    let x = smooth[j];
                    if x > max {
                        max = x;
                    } else {
                        break;
                    }
                    i += 1;
                    j = (j + 1) % wlen;
                }
                if i < eighth {
                    pulses[k] = period / 8.0 - i as f64 - 1.0;
                } else {
                    ok = false;
                    break;
                }
            }
            if ok {
                let tic_amp_abs = 0.5 / (std::f64::consts::PI * pulses[0] / period).sin();
                let toc_amp_abs = 0.5 / (std::f64::consts::PI * pulses[1] / period).sin();
                let tic_amp = self.lift_angle * tic_amp_abs;
                let toc_amp = self.lift_angle * toc_amp_abs;
                if (135.0..360.0).contains(&tic_amp)
                    && (135.0..360.0).contains(&toc_amp)
                    && (tic_amp - toc_amp).abs() < 60.0
                {
                    return Some(self.lift_angle * (tic_amp_abs + toc_amp_abs) / 2.0);
                }
            }
            threshold *= 1.4;
        }
        None
    }

    /// Exponential-decay peak smoother, ported from tg's smooth().
    fn smooth(input: &[f32], window: usize) -> Vec<f32> {
        let size = input.len();
        if window == 0 || window >= size {
            return Vec::new();
        }
        let k = 1.0 - 1.0 / window as f64;
        let mut r_av = 0.0f64;
        let mut u = 0.0f64;
        for &x in input.iter().take(window) {
            u *= k;
            if x as f64 > u {
                u = x as f64;
            }
            r_av += u;
        }
        let mut w = 0.0f64;
        let mut out = vec![0f32; size - window];
        for i in 0..size - window {
            out[i] = r_av as f32;
            u *= k;
            w *= k;
            let x = input[i + window];
            let y = input[i];
            if x as f64 > u {
                u = x as f64;
            }
            if y as f64 > w {
                w = y as f64;
            }
            r_av += u - w;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_detector_degenerate_ranges() {
        let v = vec![1.0, 5.0, 1.0];
        assert_eq!(Analyzer::peak_detector(&v, 1, 1), None);
        assert_eq!(Analyzer::peak_detector(&v, 2, 1), None);
        assert_eq!(Analyzer::peak_detector(&v, 0, 3), None);
        assert_eq!(Analyzer::peak_detector(&v, 0, 2), Some(1));
    }

    #[test]
    fn peak_detector_rejects_edge_peak() {
        let v = vec![1.0, 2.0, 5.0];
        assert_eq!(Analyzer::peak_detector(&v, 0, 2), None);
    }
}
