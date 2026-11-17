// SPDX-License-Identifier: GPL-2.0
//! Biquad filters, ported from tg's algo.c.

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    pub a0: f64,
    pub a1: f64,
    pub a2: f64,
    pub b1: f64,
    pub b2: f64,
}

impl Biquad {
    /// Low-pass filter; `freq` is normalized to the sample rate (cutoff/sr).
    pub fn low_pass(freq: f64) -> Self {
        let k = (std::f64::consts::PI * freq).tan();
        let norm = 1.0 / (1.0 + k * std::f64::consts::SQRT_2 + k * k);
        let a0 = k * k * norm;
        Biquad {
            a0,
            a1: 2.0 * a0,
            a2: a0,
            b1: 2.0 * (k * k - 1.0) * norm,
            b2: (1.0 - k * std::f64::consts::SQRT_2 + k * k) * norm,
        }
    }

    /// High-pass filter (Q = 1/sqrt(2)); `freq` normalized to the sample
    /// rate. tg's default chain high-passes at FILTER_CUTOFF before
    /// analysis, which removes mains hum and rumble.
    pub fn high_pass(freq: f64) -> Self {
        let k = (std::f64::consts::PI * freq).tan();
        let norm = 1.0 / (1.0 + k * std::f64::consts::SQRT_2 + k * k);
        let a0 = norm;
        Biquad {
            a0,
            a1: -2.0 * a0,
            a2: a0,
            b1: 2.0 * (k * k - 1.0) * norm,
            b2: (1.0 - k * std::f64::consts::SQRT_2 + k * k) * norm,
        }
    }

    /// In-place Direct Form 1 filtering, matching tg's run_filter().
    pub fn run(&self, buff: &mut [f32]) {
        let mut z1 = 0.0;
        let mut z2 = 0.0;
        for v in buff.iter_mut() {
            let input = *v as f64;
            let out = input * self.a0 + z1;
            z1 = input * self.a1 + z2 - self.b1 * out;
            z2 = input * self.a2 - self.b2 * out;
            *v = out as f32;
        }
    }
}
