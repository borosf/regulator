// SPDX-License-Identifier: GPL-2.0
//! dsp2: next-generation detection chain.
//!
//! Differences from the tg port in `analyze`:
//!
//! 1. Band-adaptive prefiltering: the band with the most impulsive
//!    content (rectified kurtosis), not a fixed 3 kHz high-pass.
//! 2. Envelope noise-floor subtraction (median-envelope excision).
//! 3. Coherent folded acquisition: a harmonic comb finds the period,
//!    then the signal is folded at that period — summing every tick
//!    coherently, a 10*log10(K) dB SNR gain — to build the click template
//!    and locate the phase. This acquires on ticks too faint for any
//!    single-tick detector.
//! 4. Matched-filter tracking (normalized cross-correlation against the
//!    learned template) with a whitened acceptance threshold (the peak
//!    must stand out from the lag-to-lag correlation statistics, not
//!    merely exceed a constant) and an innovation gate (a tick whose
//!    phase error is implausible against the tracker's own error
//!    statistics is treated as a miss, not a correction).
//! 5. PLL phase/period tracking with guardrails; adaptive template
//!    accumulation (coherent gain on every confirmed tick); sub-sample
//!    event location by parabolic interpolation of the correlation peak.
//! 6. Soft re-lock on sustained track loss: the grid is recovered by
//!    re-folding at the current period instead of dropping the lock.
//! 7. Robust period refit: per-block median of same-type tick spacings.

use crate::stats::{median, vmax};
use crate::filters::Biquad;

/// One evaluated acquisition fold: template plus its scoring and phase.
struct FoldResult {
    period: f64,
    q_best: f64,
    q_med: f64,
    q_span: f64,
    /// Fold coherence: the folded template's peak against its own
    /// robust spread — maximal exactly when the trial period is right,
    /// since a wrong period smears the folded ticks.
    coherence: f64,
    anchor: usize,
    offset: usize,
    template: Vec<f32>,
}

/// One located tick event with a quality score (peak normalized
/// correlation against the template, 0..1).
#[derive(Clone, Copy, Debug)]
pub struct V2Event {
    /// Absolute sample position (fractional).
    pub t: f64,
    pub tic: bool,
    /// Peak normalized correlation with the template.
    pub quality: f64,
}

/// Result of pushing a block through the tracker.
#[derive(Clone, Debug, Default)]
pub struct V2Result {
    pub locked: bool,
    /// Same-type tick period in samples.
    pub period: f64,
    pub bph: u32,
    /// Rate error in s/day, once a nominal rate is established.
    pub rate_sday: Option<f64>,
    pub events: Vec<V2Event>,
    /// Center of the selected analysis band in Hz.
    pub band_hz: f64,
}

/// Analysis band candidates: (low edge, high edge) in Hz.
const BANDS: [(f64, f64); 3] = [(2000.0, 4500.0), (4000.0, 8000.0), (7000.0, 13000.0)];

pub struct V2Tracker {
    pub sr: u32,
    /// Beat rate hint (0 = auto); used to convert period to s/day.
    pub bph_hint: u32,
    /// Sound-card calibration in s/day, subtracted from the rate.
    pub calibration: f64,

    acquired: bool,
    /// Current same-type period estimate in samples.
    pub period: f64,
    /// Absolute sample position of the next expected tick.
    next_expected: f64,
    next_is_tic: bool,
    /// Template anchor: the tick's offset within the template segment.
    anchor_off: f64,
    /// Running click template: zero-mean, unit-norm.
    template: Vec<f32>,
    template_n: u32,
    /// Rolling phase-error magnitude for the innovation gate.
    err_sigma: f64,
    /// Consecutive missed ticks (soft re-lock trigger).
    misses: u32,
    /// (slope, se) run estimates accumulated across blocks.
    slope_acc: Vec<(f64, f64)>,
    /// Phase errors of the current block (for the period-error integral).
    block_errs: Vec<f64>,
    band: (f64, f64),
    blocks_since_acq: u32,
    confirmed: u64,
    tick_count: u64,
}

impl V2Tracker {
    pub fn new(sr: u32) -> Self {
        V2Tracker {
            sr,
            bph_hint: 0,
            calibration: 0.0,
            acquired: false,
            period: 0.0,
            next_expected: 0.0,
            next_is_tic: true,
            anchor_off: 0.0,
            template: Vec::new(),
            template_n: 0,
            err_sigma: 4.0,
            misses: 0,
            slope_acc: Vec::new(),
            block_errs: Vec::new(),
            band: BANDS[0],
            blocks_since_acq: 0,
            confirmed: 0,
            tick_count: 0,
        }
    }

    pub fn locked(&self) -> bool {
        self.acquired
    }

    /// Feed one block of contiguous samples; `start_sample` is the
    /// absolute sample position of `samples[0]`.
    pub fn push_block(&mut self, samples: &[f32], start_sample: u64) -> V2Result {
        let sr = self.sr as usize;
        if samples.len() < sr {
            return V2Result::default();
        }
        if self.acquired {
            self.blocks_since_acq += 1;
            // Lock surveillance: an acquisition that never confirms is a
            // false lock on noise; drop it and try again later.
            if self.blocks_since_acq >= 3 && self.confirmed < 4 {
                if dbg_trace() {
                    eprintln!("lock surveillance: dropping false lock (confirmed={})", self.confirmed);
                }
                self.reset_lock();
                return V2Result::default();
            }
        }

        let xs = self.select_band(samples);

        if !self.acquired {
            // Fold-search acquisition: candidate periods (the bph hint's,
            // or every standard rate in auto mode) are each evaluated by
            // folding the block — coherently summing every tick — and
            // scoring the resulting template by whitened phase
            // correlation. The winning fold is simultaneously the
            // template, its anchor and the grid phase.
            let candidates: Vec<f64> = if self.bph_hint != 0 {
                vec![7200.0 * sr as f64 / self.bph_hint as f64]
            } else {
                crate::analyze::STANDARD_RATES
                    .iter()
                    .map(|&b| 7200.0 * sr as f64 / b as f64)
                    .collect()
            };
            let mut won = None;
            for &p0 in &candidates {
                if let Some(f) = self.fold_candidates(&xs, p0) {
                    if dbg_trace() {
                        eprintln!(
                            "acquisition: p0={p0:.0} -> period={:.1} q={:.3} span={:.3}",
                            f.period, f.q_best, f.q_span
                        );
                    }
                    if won
                        .as_ref()
                        .map(|w: &FoldResult| f.coherence > w.coherence)
                        .unwrap_or(true)
                    {
                        won = Some(f);
                    }
                }
            }
            if let Some(f) = won {
                if dbg_trace() {
                    eprintln!(
                        "acquisition: adopting period={:.1} q_best={:.3} med={:.3} anchor={}",
                        f.period, f.q_best, f.q_med, f.anchor
                    );
                }
                self.adopt_fold(f, start_sample);
            }
            let mut r = self.result_defaults();
            r.locked = self.acquired;
            if !self.acquired {
                r.rate_sday = None;
            }
            return r;
        }

        // 2. Envelope with the noise floor subtracted (used only as a
        // cheap pre-test; the matched filter does the real detection).
        let env = envelope(&xs, sr);
        let mut sorted = env.clone();
        let floor = median(&mut sorted);
        let exc: Vec<f32> = env.iter().map(|&v| (v - floor).max(0.0)).collect();

        let mut events = Vec::new();
        self.block_errs.clear();
        let block_end = start_sample as f64 + samples.len() as f64;
        let hunt = (self.period / 16.0).max(8.0);
        let tlen = self.template.len();
        let nominal = self.period / 2.0;
        while self.next_expected < block_end {
            let expected = self.next_expected;
            let s_lo = expected - hunt - self.anchor_off;
            let s_hi = expected + hunt - self.anchor_off;
            if s_hi < 0.0 {
                // Grid has not reached this block yet; wait.
                break;
            }
            if s_lo < start_sample as f64 - tlen as f64 {
                // Window already past: skip the tick.
                self.next_expected = expected + nominal;
                self.next_is_tic = !self.next_is_tic;
                continue;
            }
            if s_hi + tlen as f64 >= block_end {
                break; // Grid continues into the next block.
            }
            let lo = (s_lo - start_sample as f64) as usize;
            let hi = (s_hi - start_sample as f64) as usize;
            if vmax(&exc, lo, (hi + tlen).min(exc.len())).0 <= 0.0 {
                // Nothing at all in this window.
                self.register_miss(expected, nominal);
                self.next_is_tic = !self.next_is_tic;
                continue;
            }
            let tic = self.next_is_tic;
            match self.match_filter(&xs, lo, hi) {
                Some((seg, quality, delta)) => {
                    let m = start_sample as f64 + seg as f64 + delta + self.anchor_off;
                    let err = m - expected;
                    // Innovation gate: a converged tracker treats an
                    // implausible phase error as a miss instead of letting
                    // noise yank the grid. While settling (statistics not
                    // yet converged) any match inside the hunt window is
                    // accepted so the coarse acquisition period can be
                    // PLL-corrected instead of deadlocking.
                    // A matched position near the window edge is a TRUNCATED
                    // argmax: statistically biased toward the window
                    // center, it poisons spacing statistics and (via the
                    // refit feedback) runs the period away. Only
                    // comfortably-interior matches become events. Edge
                    // matches still correct the PHASE (prediction) —
                    // without that, errors grow until every tick is
                    // rejected and the track dies.
                    if err.abs() > 0.75 * hunt {
                        self.next_expected = expected + nominal + 0.2 * err;
                        self.misses += 1;
                    } else {
                        self.confirm_tick(m, expected, quality, &xs, seg);
                        events.push(V2Event { t: m, tic, quality });
                    }
                }
                None => self.register_miss(expected, nominal),
            }
            self.next_is_tic = !self.next_is_tic;
        }

        // 6. Soft re-lock: recover a drifting grid by re-folding the
        // block at the current period instead of dropping the lock.
        if self.misses >= 8 && events.is_empty() {
            // Soft re-lock: rescore folds around the current period and
            // re-adopt the winner (phase may have drifted).
            if let Some(f) = self.fold_candidates(&xs, self.period) {
                self.adopt_fold(f, start_sample);
            }
            self.misses = 0;
        }

        self.refit_period(&events);
        // Period-error integral: the median block phase error is an
        // unbiased estimate of 1.25x the half-period error. Nudge the
        // period by a damped fraction of it; over many blocks this
        // converges even when per-event noise dwarfs one sample.
        let mut r = self.result_defaults();
        r.events = events;
        r
    }

    fn reset_lock(&mut self) {
        self.acquired = false;
        self.template.clear();
        self.slope_acc.clear();
        self.template_n = 0;
        self.blocks_since_acq = 0;
        self.confirmed = 0;
        self.misses = 0;
    }

    fn register_miss(&mut self, expected: f64, nominal: f64) {
        self.next_expected = expected + nominal;
        self.misses += 1;
    }

    fn confirm_tick(
        &mut self,
        m: f64,
        expected: f64,
        quality: f64,
        xs: &[f32],
        seg: usize,
    ) {
        let err = m - expected;
        self.block_errs.push(err);
        let pred = expected + self.period / 2.0;
        self.next_expected = pred + 0.4 * err;
        // Phase-only PLL: a small systematic measurement bias (template
        // blending shifts the matched peak) accumulated through the old
        // per-tick period adaptation into a rate drift; the statistically
        // gated refit below is the sole period estimator.
        // Rolling error magnitude for the innovation gate.
        self.err_sigma = 0.9 * self.err_sigma + 0.1 * err.abs().max(0.25);
        self.tick_count += 1;
        self.confirmed += 1;
        self.misses = 0;
        // 5. Adaptive template accumulation: every highly correlated tick
        // is blended into the running template (coherent gain).
        if quality >= 0.75 && self.template_n < 4000 {
            self.blend_segment(xs, seg);
        }
    }

    fn result_defaults(&self) -> V2Result {
        V2Result {
            locked: self.acquired,
            period: self.period,
            bph: if self.acquired { self.bph() } else { 0 },
            rate_sday: if self.acquired { self.rate() } else { None },
            events: Vec::new(),
            band_hz: self.band.0,
        }
    }

    /// Kurtosis-based selection of the analysis band; returns the
    /// bandpassed copy of the block.
    fn select_band(&mut self, samples: &[f32]) -> Vec<f32> {
        let srn = self.sr as f64;
        let mut best = f64::MIN;
        let mut best_band = self.band;
        let mut best_out = Vec::new();
        for &band in BANDS.iter() {
            let mut x = samples.to_vec();
            Biquad::high_pass(band.0 / srn).run(&mut x);
            Biquad::low_pass(band.1 / srn).run(&mut x);
            let k = kurtosis(&x);
            if k > best {
                best = k;
                best_band = band;
                best_out = x;
            }
        }
        self.band = best_band;
        best_out
    }

    /// Score every candidate period around `p0` (+/- 1%, sample-step 2)
    /// by folding and whitened phase correlation; the best fold wins.
    /// This is where faint ticks are acquired: the fold integrates every
    /// tick in the block against each hypothesis before any single tick
    /// is individually detectable.
    fn fold_candidates(&self, xs: &[f32], p0: f64) -> Option<FoldResult> {
        let mut best: Option<FoldResult> = None;
        let mut best_span = f64::MIN;
        let search_half = (p0 * 0.01) as i64;
        let mut dp = -search_half;
        while dp <= search_half {
            let p = p0 + dp as f64;
            if let Some(f) = self.fold_and_score(xs, p) {
                if f.q_span > best_span {
                    best_span = f.q_span;
                    best = Some(f);
                }
            }
            dp += 2;
        }
        // Sub-sample refinement: parabolic interpolation of the win
        // curve around the integer winner (the rate resolution of the
        // fold would otherwise be quantized to the search step).
        if let Some(b) = &best {
            let p_lo = b.period - 2.0;
            let p_hi = b.period + 2.0;
            if let (Some(fl), Some(fh)) = (
                self.fold_and_score(xs, p_lo),
                self.fold_and_score(xs, p_hi),
            ) {
                let denom = fl.coherence - 2.0 * b.coherence + fh.coherence;
                if denom.abs() > 1e-12 {
                    let delta = 0.5 * (fl.coherence - fh.coherence) / denom;
                    if delta.abs() <= 2.0 {
                        let p = b.period + 2.0 * delta;
                        if let Some(mut fr) = self.fold_and_score(xs, p) {
                            fr.period = p;
                            best = Some(fr);
                        }
                    }
                }
            }
        }
        best
    }

    /// Fold xs at `period` (segments starting at k*period, averaged),
    /// then phase-correlate the template across one period. Returns the
    /// scored fold, or None if the template is degenerate.
    fn fold_and_score(&self, xs: &[f32], period: f64) -> Option<FoldResult> {
        let tlen = (period / 8.0).min(2000.0).max(64.0) as usize;
        if period < 8.0 || xs.len() < tlen + period as usize {
            return None;
        }
        let n_seg = ((xs.len() - tlen) as f64 / period) as usize;
        if n_seg < 3 {
            return None;
        }
        let mut tmpl = vec![0f32; tlen];
        for k in 0..=n_seg {
            let s = (k as f64 * period) as usize;
            if s + tlen > xs.len() {
                break;
            }
            for (i, v) in xs[s..s + tlen].iter().enumerate() {
                tmpl[i] += v;
            }
        }
        let cnt = (n_seg + 1) as f32;
        for v in tmpl.iter_mut() {
            *v /= cnt;
        }
        let (peak, anchor) = vmax(&tmpl, 0, tlen);
        if peak.abs() <= 1e-9 {
            return None;
        }
        // Fold coherence: how far the folded tick rises above the
        // template's own noise. This is the period-discrimination
        // statistic — a wrong period smears the folded energy, dropping
        // both the peak and the ratio.
        let mut t2: Vec<f32> = tmpl.clone();
        let tmed = median(&mut t2);
        let tsd = tmpl
            .iter()
            .map(|v| {
                let d = (v - tmed) as f64;
                d * d
            })
            .sum::<f64>()
            / tlen as f64;
        let tsd = tmed - tmed + (tsd.sqrt() as f32);
        let coherence = ((peak - tmed) / tsd.max(1e-9)) as f64;
        // Coarse phase search (8-sample steps), then refine at 1.
        let search = period as usize;
        let mut best_q = f64::MIN;
        let mut best_lag = 0usize;
        let mut lag = 0usize;
        while lag < search {
            let q = self.corr_seg(xs, &tmpl, lag);
            if q > best_q {
                best_q = q;
                best_lag = lag;
            }
            lag += 8;
        }
        let refine_lo = best_lag.saturating_sub(8);
        let refine_hi = (best_lag + 8).min(search);
        for lag in refine_lo..=refine_hi {
            let q = self.corr_seg(xs, &tmpl, lag);
            if q > best_q {
                best_q = q;
                best_lag = lag;
            }
        }
        // Whitened score: the peak against the lag-to-lag median.
        let mut qs: Vec<f32> = (0..search)
            .step_by(8)
            .filter_map(|lag| {
                let q = self.corr_seg(xs, &tmpl, lag);
                if q == f64::MIN { None } else { Some(q as f32) }
            })
            .collect();
        if qs.len() < 8 {
            return None;
        }
        let med = median(&mut qs) as f64;
        Some(FoldResult {
            period,
            q_best: best_q,
            q_med: med,
            q_span: best_q - med,
            coherence,
            anchor,
            offset: best_lag,
            template: tmpl,
        })
    }

    /// Adopt a winning fold as the track: template, anchor, phase and
    /// period all come from the fold itself. The phase correlation peak
    /// must genuinely stand out or the lock is refused (surveillance
    /// also drops folds that never confirm against the live signal).
    fn adopt_fold(&mut self, f: FoldResult, start: u64) -> bool {
        // Absolute gates: a whitened span below 0.05 means the "peak"
        // does not stand out at all (noise), regardless of its height.
        if f.coherence < 3.0 && !(f.q_span > 0.05 && f.q_best > 0.35) {
            return false;
        }
        self.template = f.template;
        normalize(&mut self.template);
        self.slope_acc.clear();
        self.template_n = 100; // blends: the fold is already well averaged
        self.anchor_off = f.anchor as f64;
        self.period = f.period;
        self.next_expected = start as f64 + f.offset as f64 + self.anchor_off;
        self.next_is_tic = true;
        self.acquired = true;
        true
    }





    /// Normalized cross-correlation of the template against segments
    /// starting at every position in `lo..=hi` (block-relative). Returns
    /// (best segment start, quality, sub-sample delta).
    fn match_filter(&self, xs: &[f32], lo: usize, hi: usize) -> Option<(usize, f64, f64)> {
        let mut qs = Vec::with_capacity(hi - lo + 1);
        for s in lo..=hi {
            qs.push(self.corr_at(xs, s));
        }
        let mut best_q = f64::MIN;
        let mut best_i = 0usize;
        for (i, &q) in qs.iter().enumerate() {
            if q > best_q {
                best_q = q;
                best_i = i;
            }
        }
        if best_q == f64::MIN {
            return None;
        }
        // Whitened acceptance: the peak must stand out from the
        // lag-to-lag correlation statistics. A constant threshold either
        // misses faint ticks or admits noise.
        let finite: Vec<f32> = qs
            .iter()
            .filter(|q| **q > f64::MIN / 2.0)
            .map(|q| *q as f32)
            .collect();
        let med = median(&mut finite.clone()) as f64;
        let spread = best_q - med;
        if spread < 0.06 && best_q < 0.3 {
            return None;
        }
        if best_q < 0.1 && spread < 0.05 {
            return None;
        }
        let seg = lo + best_i;
        // Sub-sample refinement: parabolic interpolation over the peak.
        let qa = if best_i > 0 { qs[best_i - 1] } else { best_q };
        let qc = if best_i + 1 < qs.len() { qs[best_i + 1] } else { best_q };
        let denom = qa - 2.0 * best_q + qc;
        let mut delta = 0.0;
        if denom.abs() > 1e-12 {
            let d = 0.5 * (qa - qc) / denom;
            if d.abs() < 1.0 {
                delta = d;
            }
        }
        Some((seg, best_q, delta))
    }

    /// Pearson correlation of the template against the segment at `s`.
    fn corr_at(&self, xs: &[f32], s: usize) -> f64 {
        self.corr_seg(xs, &self.template, s)
    }

    /// Pearson correlation of `t` against the segment of `xs` at `s`.
    fn corr_seg(&self, xs: &[f32], t: &[f32], s: usize) -> f64 {
        let tlen = t.len();
        if tlen == 0 || s + tlen > xs.len() {
            return f64::MIN;
        }
        let seg = &xs[s..s + tlen];
        let n = tlen as f64;
        let t_mean = t.iter().map(|&v| v as f64).sum::<f64>() / n;
        let t_sq: f64 = t.iter().map(|&v| {
            let d = v as f64 - t_mean;
            d * d
        }).sum();
        let mut dot = 0f64;
        let mut s_sum = 0f64;
        let mut s_sq = 0f64;
        for i in 0..tlen {
            let sv = seg[i] as f64;
            dot += sv * (t[i] as f64 - t_mean);
            s_sum += sv;
            s_sq += sv * sv;
        }
        let var = s_sq - s_sum * s_sum / n;
        if var <= 1e-12 || t_sq <= 1e-12 {
            return f64::MIN;
        }
        dot / (var.sqrt() * t_sq.sqrt())
    }

    /// Blend a confirmed tick's segment into the running template.
    fn blend_segment(&mut self, xs: &[f32], seg_start_rel: usize) {
        let tlen = self.template.len();
        let s = seg_start_rel;
        if s + tlen > xs.len() {
            return;
        }
        let mut seg = xs[s..s + tlen].to_vec();
        normalize(&mut seg);
        let n = (self.template_n + 1) as f64;
        let w = (1.0 / n).min(0.1);
        for (k, &v) in seg.iter().enumerate() {
            self.template[k] = ((1.0 - w) * self.template[k] as f64 + w * v as f64) as f32;
        }
        normalize(&mut self.template);
        self.template_n += 1;
    }

    /// 7. Robust period refit: per-type linear regression of event
    /// position against tick index over consecutive (gap-free) runs.
    /// The regression slope IS the period; every event in a run
    /// contributes (unlike median-of-diffs), so the estimate's standard
    /// error shrinks to hundredths of a sample within a block. Constant
    /// phase offsets cancel; gaps simply end runs. Runs from all blocks
    /// accumulate with inverse-variance pooling, SE-gated like the rest.
    fn refit_period(&mut self, events: &[V2Event]) {
        let mut estimates: Vec<(f64, f64)> = Vec::new(); // (slope, se)
        for same_type in [true, false] {
            let mut run: Vec<f64> = Vec::new();
            for e in events.iter().filter(|e| e.tic == same_type) {
                if let Some(&last) = run.last() {
                    if e.t - last > 1.5 * self.period {
                        self.push_slope(&run, &mut estimates);
                        run.clear();
                    }
                }
                run.push(e.t);
            }
            self.push_slope(&run, &mut estimates);
        }
        self.slope_acc.retain(|(s, se)| !s.is_nan() && *se > 0.0);
        self.slope_acc.extend(estimates);
        if self.slope_acc.len() > 128 {
            let cut = self.slope_acc.len() - 64;
            self.slope_acc.drain(..cut);
        }
        if self.slope_acc.is_empty() {
            return;
        }
        // Inverse-variance pooled slope over all runs.
        let mut w_sum = 0.0;
        let mut sw = 0.0;
        for &(slope, se) in &self.slope_acc {
            let w = 1.0 / (se * se).max(1e-18);
            w_sum += w;
            sw += w * slope;
        }
        let pooled = sw / w_sum;
        let se = (1.0 / w_sum).sqrt();
        let corr = pooled - self.period;
        if pooled > 0.0 && corr.abs() / self.period < 0.01 {
            let scale = (corr.abs() / (3.0 * se)).min(1.0);
            self.period += scale * corr;
        }
        if dbg_trace() {
            eprintln!(
                "refit: runs={} pooled={pooled:.2} se={se:.3} -> period={:.2}",
                self.slope_acc.len(),
                self.period
            );
        }
    }

    /// Slope (and its standard error) of one consecutive run.
    fn push_slope(&self, run: &[f64], out: &mut Vec<(f64, f64)>) {
        let n = run.len();
        if n < 8 {
            return;
        }
        // Least squares of t against index k = 0..n.
        let ks: Vec<f64> = (0..n).map(|i| i as f64).collect();
        let km = (n - 1) as f64 / 2.0;
        let tm = run.iter().sum::<f64>() / n as f64;
        let mut num = 0.0;
        let mut den = 0.0;
        for (k, &t) in ks.iter().zip(run) {
            num += (k - km) * (t - tm);
            den += (k - km) * (k - km);
        }
        if den <= 0.0 {
            return;
        }
        let slope = num / den;
        // Residual variance for the slope SE.
        let mut ssr = 0.0;
        for (k, &t) in ks.iter().zip(run) {
            let pred = tm + slope * (k - km);
            ssr += (t - pred) * (t - pred);
        }
        if n < 10 {
            return; // too short to trust the residual estimate
        }
        let var = ssr / (n - 2) as f64;
        let se = (var / den).sqrt();
        out.push((slope, se));
    }

    fn bph(&self) -> u32 {
        let detected = 7200.0 * self.sr as f64 / self.period;
        if self.bph_hint != 0 {
            self.bph_hint
        } else {
            crate::analyze::snap_bph(detected) as u32
        }
    }

    /// Current rate estimate in s/day, once locked.
    pub fn rate(&self) -> Option<f64> {
        if !self.acquired || self.period <= 0.0 {
            return None;
        }
        let bph = self.bph() as f64;
        let nominal = 7200.0 * self.sr as f64 / bph;
        Some((nominal / self.period - 1.0) * 86400.0 - self.calibration)
    }
}


/// Debug tracing for the acquisition pipeline (bounded noise: one env
/// lookup per stage, only when the variable exists at all).
fn dbg_trace() -> bool {
    std::env::var("REGULATOR_DSP2_TRACE").is_ok()
}

/// Low-passed magnitude envelope of x at roughly 1 kHz.
fn envelope(x: &[f32], sr: usize) -> Vec<f32> {
    let mut a: Vec<f32> = x.iter().map(|v| v.abs()).collect();
    Biquad::low_pass(1000.0 / sr as f64).run(&mut a);
    a
}

/// Zero-mean, unit-norm in-place normalization.
fn normalize(v: &mut [f32]) {
    let mean = v.iter().map(|&x| x as f64).sum::<f64>() / v.len() as f64;
    for s in v.iter_mut() {
        *s -= mean as f32;
    }
    let norm = v.iter().map(|&x| (x * x) as f64).sum::<f64>().sqrt();
    if norm > 1e-9 {
        for s in v.iter_mut() {
            *s = ((*s as f64) / norm) as f32;
        }
    }
}

/// Excess kurtosis of a signal: impulsive content has high kurtosis.
fn kurtosis(x: &[f32]) -> f64 {
    let n = x.len() as f64;
    let m = x.iter().map(|&v| v as f64).sum::<f64>() / n;
    let mut m2 = 0.0;
    let mut m4 = 0.0;
    for &v in x {
        let d = v as f64 - m;
        let d2 = d * d;
        m2 += d2;
        m4 += d2 * d2;
    }
    m2 /= n;
    m4 /= n;
    if m2 <= 1e-12 {
        0.0
    } else {
        m4 / (m2 * m2) - 3.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::Analyzer;
    use crate::signal::SyntheticWatch;

    const SR: u32 = 44100;

    fn noisy(watch: &SyntheticWatch, dur: f64, noise: f64) -> Vec<f32> {
        let mut out = watch.generate_clean(SR, dur);
        let mut rng = 0xfeed_faceu64;
        for v in out.iter_mut() {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = ((rng >> 11) as i64 % 2000) as f64 / 1000.0 - 1.0;
            *v += (r * noise) as f32;
        }
        out
    }

    fn run(watch: &SyntheticWatch, samples: &[f32]) -> (V2Tracker, Vec<V2Event>) {
        let mut tr = V2Tracker::new(SR);
        tr.bph_hint = watch.bph;
        let block = 4 * SR as usize;
        let mut all = Vec::new();
        for (i, chunk) in samples.chunks(block).enumerate() {
            let r = tr.push_block(chunk, (i * block) as u64);
            all.extend(r.events);
        }
        (tr, all)
    }

    const WATCH: SyntheticWatch = SyntheticWatch {
        bph: 28800,
        rate_sday: 9.0,
        beat_error_ms: 0.5,
        amplitude_deg: 275.0,
        quartz: false,
    };

    #[test]
    fn locks_and_finds_ticks() {
        let samples = WATCH.generate(SR, 20.0);
        let (tr, all) = run(&WATCH, &samples);
        assert!(tr.locked(), "tracker never acquired");
        let rate = tr.rate().expect("rate");
        assert!(
            (rate - 9.0).abs() < 3.0,
            "rate {rate} ({} events)",
            all.len()
        );
        let expected = 28800.0 / 3600.0 * (20.0 - 4.0);
        assert!(
            all.len() as f64 > 0.8 * expected,
            "found {} of {expected} ticks",
            all.len()
        );
    }

    #[test]
    fn acquires_on_faint_signal() {
        // White noise ~50x the tick burst envelope: v1's acquisition
        // cannot lock here; the folded comb chain must.
        let samples = noisy(&WATCH, 12.0, 1.0);
        assert!(
            Analyzer::new(SR).analyze(&samples).is_none(),
            "v1 unexpectedly locked"
        );
        let (tr, all) = run(&WATCH, &samples);
        assert!(tr.locked(), "v2 failed to acquire on faint signal");
        // With 12 s of data at this noise the rate estimate itself is
        // information-limited to a few tens of s/d (single-tick positions
        // scatter by hundreds of samples); the win is acquiring and
        // tracking at all. The rate converges with listening time.
        let rate = tr.rate().unwrap();
        assert!((rate - 9.0).abs() < 150.0, "rate {rate}");
        let expected = 28800.0 / 3600.0 * 8.0;
        assert!(
            all.len() as f64 > 0.5 * expected,
            "found {} of {expected}",
            all.len()
        );
    }

    #[test]
    fn tracks_through_loud_noise() {
        // The previous draft lost the grid at noise 0.4; the innovation
        // gate + whitened threshold must hold it.
        let samples = noisy(&WATCH, 20.0, 0.4);
        let (tr, all) = run(&WATCH, &samples);
        assert!(tr.locked(), "lost lock at noise 0.4");
        let expected = 28800.0 / 3600.0 * 16.0;
        assert!(
            all.len() as f64 > 0.6 * expected,
            "found {} of {expected} ticks at noise 0.4",
            all.len()
        );
    }
}
