// SPDX-License-Identifier: GPL-2.0
use regulator_core::signal::SyntheticWatch;
use regulator_core::Analyzer;

fn main() {
    let sr = 44100u32;
    let watch = SyntheticWatch {
        bph: 28800,
        rate_sday: 12.0,
        beat_error_ms: 0.0,
        amplitude_deg: 270.0,
    };
    let samples = watch.generate(sr, 4.0);
    let an = Analyzer::new(sr);
    match an.analyze(&samples) {
        Some(r) => println!("ok: {r:?}"),
        None => println!("analysis failed"),
    }
}
