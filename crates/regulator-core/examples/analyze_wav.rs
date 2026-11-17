// SPDX-License-Identifier: GPL-2.0
//! Analyze a WAV recording of a watch and print results per block.
//! Usage: cargo run -p regulator-core --example analyze_wav -- file.wav [bph]

use regulator_core::Analyzer;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: analyze_wav <file.wav> [bph] [lift_angle]");
        std::process::exit(1);
    }
    let bph: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let lift: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(52.0);
    let mut reader = hound::WavReader::open(&args[1]).expect("cannot open wav");
    let sr = reader.spec().sample_rate;
    let samples: Vec<f32> = reader
        .samples::<i16>()
        .map(|s| s.map(|v| v as f32 / i16::MAX as f32).unwrap_or(0.0))
        .collect();
    println!(
        "file: {}  rate: {sr}  samples: {}  ({:.1} s)  bph: {bph}  lift: {lift}",
        args[1],
        samples.len(),
        samples.len() as f64 / sr as f64
    );
    let mut analyzer = Analyzer::new(sr);
    analyzer.bph = bph;
    analyzer.lift_angle = lift;
    let block = (4.0 * sr as f64) as usize;
    let mut t = 0.0;
    for (i, chunk) in samples.chunks(block).enumerate() {
        if chunk.len() < sr as usize {
            break;
        }
        match analyzer.analyze(chunk) {
            Some(r) => println!(
                "t={t:6.1}s  rate={:+8.1} s/d  amp={:>5}  be={:+6.2} ms  period={:.1} bph={}",
                r.rate_sday.unwrap_or(f64::NAN),
                r.amplitude_deg.map(|a| format!("{a:.0}")).unwrap_or("---".into()),
                r.beat_error_ms.unwrap_or(f64::NAN),
                r.period,
                r.bph,
            ),
            None => println!("t={t:6.1}s  no signal",),
        }
        t = (i + 1) as f64 * 4.0;
    }
}
