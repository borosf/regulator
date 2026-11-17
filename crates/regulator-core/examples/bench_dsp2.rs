// SPDX-License-Identifier: GPL-2.0
//! Old-vs-new chain benchmark. Runs each beat rate on its own thread.
//! Usage: cargo run --release --example bench_dsp2
//! CSV: chain,bph,noise,locked,rate_err,event_ratio,seconds

use regulator_core::analyze::Analyzer;
use regulator_core::dsp2::V2Tracker;
use regulator_core::signal::SyntheticWatch;
use std::sync::mpsc;

const SR: u32 = 44100;
const DUR: f64 = 12.0;

fn main() {
    println!("chain,bph,noise,locked,rate_err,event_ratio,seconds");
    let (tx, rx) = mpsc::channel::<String>();
    for bph in [18000u32, 28800, 36000] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in run_bph(bph) {
                tx.send(line).unwrap();
            }
        });
    }
    drop(tx);
    let mut rows: Vec<String> = rx.iter().collect();
    rows.sort_by_key(|l| {
        let f: Vec<f64> = l.split(',').skip(1).take(2).map(|x| x.parse().unwrap_or(0.0)).collect();
        (f[0] as i64, (f[1] * 100.0) as i64, l.contains("v2"))
    });
    for r in rows {
        println!("{r}");
    }
}

fn run_bph(bph: u32) -> Vec<String> {
    let watch = SyntheticWatch {
        bph,
        rate_sday: 7.5,
        beat_error_ms: 0.5,
        amplitude_deg: 275.0,
        quartz: false,
    };
    let clean = watch.generate_clean(SR, DUR);
    let expected = bph as f64 / 3600.0 * DUR;
    let expected_after_acq = bph as f64 / 3600.0 * (DUR - 4.0);
    let block = 4 * SR as usize;
    let mut out = Vec::new();

    for noise in [0.02, 0.06, 0.15, 0.4, 1.0, 2.5] {
        let mut samples = clean.clone();
        add_noise(&mut samples, noise);

        // v1: whole signal in one shot (single source of truth for lock).
        let t0 = std::time::Instant::now();
        let mut an = Analyzer::new(SR);
        an.bph = bph;
        let v1 = an.analyze(&samples);
        let v1_time = t0.elapsed().as_secs_f64();
        match &v1 {
            Some(r) => out.push(format!(
                "v1,{bph},{noise},1,{:.2},{:.3},{:.2}",
                (r.rate_sday.unwrap() - 7.5).abs(),
                r.events.len() as f64 / expected,
                v1_time
            )),
            None => out.push(format!("v1,{bph},{noise},0,,")),
        }

        // v2: tracker fed 4s blocks.
        let t0 = std::time::Instant::now();
        let mut tr = V2Tracker::new(SR);
        tr.bph_hint = bph;
        let mut found = 0usize;
        for (i, chunk) in samples.chunks(block).enumerate() {
            let r = tr.push_block(chunk, (i * block) as u64);
            found += r.events.len();
        }
        let v2_time = t0.elapsed().as_secs_f64();
        if tr.locked() {
            let rate = tr.rate().unwrap();
            out.push(format!(
                "v2,{bph},{noise},1,{:.2},{:.3},{:.2}",
                (rate - 7.5).abs(),
                found as f64 / expected_after_acq,
                v2_time
            ));
        } else {
            out.push(format!("v2,{bph},{noise},0,,"));
        }
    }
    out
}

fn add_noise(samples: &mut [f32], amp: f64) {
    let mut rng = 0xfeed_faceu64;
    for v in samples.iter_mut() {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = ((rng >> 11) as i64 % 2000) as f64 / 1000.0 - 1.0;
        *v += (r * amp) as f32;
    }
}
