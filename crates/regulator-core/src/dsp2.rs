// SPDX-License-Identifier: GPL-2.0
//! dsp2: rough draft of the next-generation detection chain.
//!
//! Ideas under test (see the bench_dsp2 example for old-vs-new numbers):
//!
//! 1. Band-adaptive prefiltering: pick the frequency band with the most
//!    impulsive content (kurtosis of the rectified signal) instead of a
//!    fixed 3 kHz high-pass.
//! 2. Envelope noise-floor subtraction (a spectral-subtraction analogue on
//!    the magnitude envelope): subtract the block median envelope so the
//!    energy fallback detector stops tripping on rumble.
//! 3. Template learning: bootstrap a click template by averaging the
//!    strongest tick segments, then detect by normalized cross-correlation
//!    (matched filtering — the optimal detector under additive noise).
//! 4. Adaptive (coherent) accumulation: every confident tick is blended
//!    into the running template, which is equivalent to coherently summing
//!    all past ticks: 10*log10(n) dB of SNR gain for free.
//! 5. Phase/period tracking (PLL-style): each tick corrects the predicted
//!    grid instead of re-running full acquisition every block.
//! 6. Sub-sample event location by parabolic interpolation of the
//!    correlation peak.
//! 7. Robust periodic refit: the period is re-estimated per block from the
//!    median of same-type tick spacings, ignoring outliers.

use crate::analyze::Analyzer;
use crate::filters::Biquad;
use crate::stats::median;

/// One located tick event with a quality score (the peak correlation
/// coefficient against the template, 0..1).
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

/// Correlation quality threshold for accepting a tick.
const Q_ACCEPT: f64 = 0.35;
/// Quality above which a tick is blended into the running template.
const Q_LEARN: f64 = 0.55;

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
    tick_count: u64,
    /// Running click template: zero-mean, unit-norm.
    template: Vec<f32>,
    template_n: u32,
    band: (f64, f64),
    /// Used only for acquisition (its filtered pipeline finds the period).
    acq: Analyzer,
    /// Blocks pushed since acquisition (for the lock-confirmation check).
    blocks_since_acq: u32,
    /// Ticks confirmed (accepted by the matched filter) since acquisition.
    confirmed: u64,
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
            tick_count: 0,
            template: Vec::new(),
            template_n: 0,
            band: BANDS[0],
            acq: Analyzer::new(sr),
            blocks_since_acq: 0,
            confirmed: 0,
        }
    }

    /// Feed one block of contiguous samples; `start_sample` is the absolute
    /// sample position of `samples[0]`.
    pub fn push_block(&mut self, samples: &[f32], start_sample: u64) -> V2Result {
        let sr = self.sr as usize;
        if samples.len() < sr {
            return V2Result::default();
        }
        // Lock surveillance: an acquisition that produces no confirmed
        // ticks in its first blocks is a false lock (noise); drop it.
        if self.acquired {
            self.blocks_since_acq += 1;
            if self.blocks_since_acq >= 3 && self.confirmed < 4 {
                self.acquired = false;
                self.template.clear();
                self.template_n = 0;
                self.blocks_since_acq = 0;
                self.confirmed = 0;
                return V2Result::default();
            }
        }

        // 1. Band selection: the most impulsive band wins.
        let xs = self.select_band(samples);

        // 2. Envelope + per-block noise-floor subtraction.
        let env = envelope(&xs, sr);
        let mut env_sorted = env.clone();
        let floor = median(&mut env_sorted);
        let exc: Vec<f32> = env.iter().map(|&v| (v - floor).max(0.0)).collect();

        // 3. Acquisition: reuse the v1 machinery once to lock the period;
        // if its peak detector refuses, fall back to a harmonic comb
        // search over the autocorrelation, which is far more noise-tolerant
        // (it integrates period evidence across many multiples).
        if !self.acquired {
            let mut prepared = samples.to_vec();
            let n = prepared.len();
            self.acq.prepare_data(&mut prepared);
            let period = self
                .acq
                .compute_period(&prepared, n)
                .or_else(|| comb_acquisition(&prepared, self.sr as usize));
            if let Some(period) = period {
                if self.seed_template(&exc, &xs, start_sample, period) {
                    self.acquired = true;
                }
            }
            let mut r = V2Result {
                locked: self.acquired,
                period: self.period,
                bph: self.bph(),
                rate_sday: self.rate(),
                events: Vec::new(),
                band_hz: self.band.0,
            };
            r.rate_sday = None;
            return r;
        }

        // 4-6: track the grid with the matched filter.
        let mut events = Vec::new();
        let block_end = start_sample as f64 + samples.len() as f64;
        let hunt = (self.period / 16.0).max(8.0);
        let tlen = self.template.len();
        // Convention: the template is cut with the tick attack 3/4 into
        // the segment; a segment starting at `s` covers ticks at s+A.
        let anchor_off = 0.75 * tlen as f64;
        let nominal = self.period / 2.0;
        while self.next_expected < block_end {
            let expected = self.next_expected;
            // Segment start range so the anchor covers expected +/- hunt.
            let s_lo = expected - hunt - anchor_off;
            let s_hi = expected + hunt - anchor_off;
            if s_hi < 0.0 || s_lo < start_sample as f64 - tlen as f64 {
                // Window entirely before this block: skip.
                self.next_expected = expected + nominal;
                self.next_is_tic = !self.next_is_tic;
                continue;
            }
            if s_hi + tlen as f64 >= block_end {
                break; // Grid continues into the next block.
            }
            let lo = (s_lo - start_sample as f64).max(0.0) as usize;
            let hi = (s_hi - start_sample as f64) as usize;
            if let Some((seg, quality, _mag)) = self.match_filter(&xs, lo, hi) {
                // Sub-sample interpolation of the correlation peak.
                let qa = if seg > 0 { self.corr_at(&xs, seg - 1) } else { 0.0 };
                let qb = self.corr_at(&xs, seg);
                let qc = if seg + 1 + tlen < xs.len() { self.corr_at(&xs, seg + 1) } else { 0.0 };
                let denom = qa - 2.0 * qb + qc;
                let mut delta = 0.0;
                if denom.abs() > 1e-12 {
                    let d = 0.5 * (qa - qc) / denom;
                    if d.abs() < 1.0 {
                        delta = d;
                    }
                }
                let m = start_sample as f64 + seg as f64 + delta + anchor_off;
                let err = m - expected;
                let pred = expected + nominal;
                if err.abs() > hunt {
                    // Wild correction: trust the grid, not the match —
                    // almost certainly a noise peak that slipped through.
                    self.next_expected = pred;
                } else {
                    // 5. PLL update: phase partly, period very gently and
                    // only on small, consistent errors (guardrail against
                    // a bad stretch of noise walking the period away).
                    self.next_expected = pred + 0.4 * err;
                    if err.abs() < hunt / 4.0 {
                        self.period += (0.02 * err).clamp(-1.0, 1.0);
                    }
                    self.tick_count += 1;
                    self.confirmed += 1;
                    // 4. Adaptive template accumulation (coherent gain).
                    if quality >= 0.8 {
                        self.blend_segment(&xs, seg);
                    }
                    events.push(V2Event { t: m, tic: self.next_is_tic, quality });
                }
            } else {
                // Missed tick: advance the grid without correcting.
                self.next_expected = expected + nominal;
            }
            self.next_is_tic = !self.next_is_tic;
        }

        // 7. Robust refit from the median of same-type spacings in block.
        self.refit_period(&events);

        V2Result {
            locked: self.acquired,
            period: self.period,
            bph: self.bph(),
            rate_sday: self.rate(),
            events,
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

    /// Bootstrap the template from the strongest clicks found by the energy
    /// detector, and initialize the tracking grid.
    fn seed_template(
        &mut self,
        exc: &[f32],
        xs: &[f32],
        start: u64,
        period: f64,
    ) -> bool {
        let tmpl_len = (period / 8.0).min(2000.0).max(64.0) as usize;
        // Locate candidate ticks: envelope maxima spaced at least a
        // half-period apart.
        let half = period / 2.0;
        let mut cands: Vec<usize> = Vec::new();
        let mut i = 0usize;
        while i < exc.len() {
            let end = (i + half as usize).min(exc.len());
            let (m, im) = crate::stats::vmax(exc, i, end);
            if m > 0.0 {
                cands.push(im);
            }
            i = end;
        }
        if cands.len() < 4 {
            return false;
        }
        // Take the four strongest, average their segments.
        let mut cands = cands;
        cands.sort_by(|&a, &b| exc[b].partial_cmp(&exc[a]).unwrap());
        let mut tmpl = vec![0f32; tmpl_len];
        let mut used = 0usize;
        for &c in cands.iter().take(4) {
            let s = c.saturating_sub(tmpl_len * 3 / 4);
            if s + tmpl_len > xs.len() {
                continue;
            }
            for (k, v) in xs[s..s + tmpl_len].iter().enumerate() {
                tmpl[k] += v;
            }
            used += 1;
        }
        if used == 0 {
            return false;
        }
        for v in tmpl.iter_mut() {
            *v /= used as f32;
        }
        self.template = normalize(&mut tmpl);
        self.template_n = used as u32;
        self.period = period;
        // Grid starts at the strongest candidate.
        let first = cands[0] as f64 + start as f64;
        self.next_expected = first;
        self.next_is_tic = true;
        self.acquired = true;
        true
    }

    /// Harmonic comb acquisition: score candidate periods by the sum of
    /// autocorrelation peaks at their multiples. Where tg's peak detector
    /// needs one clean peak, the comb can find a period whose 10th harmonic
/// is still above the noise floor.

    /// Normalized cross-correlation of the template against segments
    /// starting at every position in `lo..=hi` (block-relative). Returns
    /// the best segment start, its correlation quality and peak magnitude.
    fn match_filter(&self, xs: &[f32], lo: usize, hi: usize) -> Option<(usize, f64, f64)> {
        let mut best_seg = None;
        let mut best_q = f64::MIN;
        let mut best_mag = 0f64;
        for s in lo..=hi {
            let q = self.corr_at(xs, s);
            if q > best_q {
                best_q = q;
                best_seg = Some(s);
                best_mag = q;
            }
        }
        if best_q < Q_ACCEPT || best_seg.is_none() {
            return None;
        }
        Some((best_seg.unwrap(), best_q, best_mag))
    }

    /// Pearson correlation of the template against the segment at `s`.
    fn corr_at(&self, xs: &[f32], s: usize) -> f64 {
        let t = &self.template;
        let tlen = t.len();
        if s + tlen > xs.len() {
            return f64::MIN;
        }
        let seg = &xs[s..s + tlen];
        let n = tlen as f64;
        let mut dot = 0f64;
        let mut s_sum = 0f64;
        let mut s_sq = 0f64;
        let t_mean = t.iter().map(|&v| v as f64).sum::<f64>() / n;
        for i in 0..tlen {
            let sv = seg[i] as f64;
            dot += (sv) * (t[i] as f64 - t_mean);
            s_sum += sv;
            s_sq += sv * sv;
        }
        let var = s_sq - s_sum * s_sum / n;
        if var <= 1e-12 {
            return f64::MIN;
        }
        dot / var.sqrt()
    }

    /// Blend a confirmed tick's segment into the running template.
    /// `seg_start_rel` is the best-correlating segment start (block-relative,
    /// already cut on the template grid).
    fn blend_segment(&mut self, xs: &[f32], seg_start_rel: usize) {
        let tlen = self.template.len();
        let s = seg_start_rel;
        if s + tlen > xs.len() {
            return;
        }
        let mut seg = xs[s..s + tlen].to_vec();
        normalize(&mut seg);
        let n = (self.template_n + 1) as f64;
        let w = (1.0 / n).min(0.15);
        for (k, &v) in seg.iter().enumerate() {
            self.template[k] = ((1.0 - w) * self.template[k] as f64 + w * v as f64) as f32;
        }
        normalize(&mut self.template);
        self.template_n += 1;
    }

    /// 7. Robust period refit: median of same-type spacings in this block.
    fn refit_period(&mut self, events: &[V2Event]) {
        let mut d = Vec::new();
        for w in events.windows(2) {
            if w[0].tic == w[1].tic {
                d.push((w[1].t - w[0].t).abs());
            }
        }
        if d.len() < 4 {
            return;
        }
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = d[d.len() / 2];
        if p > 0.0 && (p - self.period).abs() / self.period < 0.02 {
            self.period = 0.9 * self.period + 0.1 * p;
        }
    }

    pub fn locked(&self) -> bool {
        self.acquired
    }

    fn bph(&self) -> u32 {
        let detected = 2.0 * 3600.0 * self.sr as f64 / self.period;
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


/// Harmonic comb acquisition: score candidate periods by the sum of
/// autocorrelation values at their multiples, tolerating several dB more
/// noise than a single-peak detector.
fn comb_acquisition(prepared: &[f32], sr: usize) -> Option<f64> {
    let n = prepared.len();
    let an = Analyzer::new(sr as u32);
    let ac = an.autocorrelate(prepared, n);
    // Candidate periods: standard beat rates, 12000..43200 bph.
    let lo = 2.0 * 3600.0 / 43200.0 * sr as f64;
    let hi = 2.0 * 3600.0 / 12000.0 * sr as f64;
    let steps = 800usize;
    let mut best = None;
    let mut best_score = 0f64;
    let mut scores = Vec::new();
    for i in 0..steps {
        let p = lo + (hi - lo) * i as f64 / steps as f64;
        let max_mult = ((n as f64 * 0.6) / p) as usize;
        if max_mult < 4 {
            continue;
        }
        let mut score = 0f64;
        let mut hits = 0usize;
        for k in 1..=max_mult.min(12) {
            let pk = (p * k as f64) as usize;
            if pk + 4 >= n {
                break;
            }
            let (m, _) = crate::stats::vmax(&ac, pk.saturating_sub(4), pk + 4);
            score += m as f64;
            hits += 1;
        }
        if hits == 0 {
            continue;
        }
        // Prefer stronger scores; a mild preference for faster rates
        // (denser combs) avoids octave errors.
        score /= (hits as f64).sqrt();
        scores.push(score);
        if score > best_score {
            best_score = score;
            best = Some(p);
        }
    }
    // Validation gates: (1) in pure noise comb scores are all similar,
    // so the winner must stand above the field; (2) real movements run at
    // standard beat rates (plus a small rate error), so a winning period
    // that does not sit near a standard rate is noise.
    scores.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = scores[scores.len() / 2];
    if best_score < 3.0 * mid.max(1e-9) {
        return None;
    }
    let p = best?;
    let snapped = crate::analyze::snap_bph(14400.0 * sr as f64 / p);
    if (p - 14400.0 * sr as f64 / snapped).abs() / p > 0.015 {
        return None;
    }
    Some(p)
}

/// Low-passed magnitude envelope of x at roughly 1 kHz.
fn envelope(x: &[f32], sr: usize) -> Vec<f32> {
    let mut a: Vec<f32> = x.iter().map(|v| v.abs()).collect();
    Biquad::low_pass(1000.0 / sr as f64).run(&mut a);
    a
}

/// Zero-mean, unit-norm in-place normalization; returns the normalized copy.
fn normalize(v: &mut [f32]) -> Vec<f32> {
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
    v.to_vec()
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
    use crate::signal::SyntheticWatch;

    const SR: u32 = 44100;

    #[test]
    fn locks_and_finds_ticks() {
        let watch = SyntheticWatch {
            bph: 28800,
            rate_sday: 9.0,
            beat_error_ms: 0.5,
            amplitude_deg: 275.0,
            quartz: false,
        };
        let samples = watch.generate(SR, 12.0);
        let mut tr = V2Tracker::new(SR);
        tr.bph_hint = 28800;
        let mut all = Vec::new();
        let block = 4 * SR as usize;
        for (i, chunk) in samples.chunks(block).enumerate() {
            let r = tr.push_block(chunk, (i * block) as u64);
            all.extend(r.events);
        }
        assert!(tr.acquired, "tracker never acquired");
        let rate = tr.rate().expect("rate");
        assert!((rate - 9.0).abs() < 5.0, "rate {rate}");
        let expected = 28800.0 / 3600.0 * (12.0 - 4.0);
        assert!(
            all.len() as f64 > 0.8 * expected,
            "found {} of {expected} ticks",
            all.len()
        );
    }
}
