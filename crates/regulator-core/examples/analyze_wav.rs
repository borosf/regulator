// SPDX-License-Identifier: GPL-2.0
//! Analyze a recording of a watch (WAV, MP3, M4A/AAC) and print results
//! per block.
//! Usage: cargo run -p regulator-core --example analyze_wav -- file.mp3 [bph] [lift]

use regulator_core::Analyzer;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: analyze_wav <file> [bph] [lift_angle]");
        std::process::exit(1);
    }
    let bph: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let lift: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(52.0);
    let (sr, samples) = load_audio(&args[1]);
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
                "t={t:6.1}s  rate={:+8.1} s/d  amp={:>5}  be={:+6.2} ms  period={:.1} bph={}  events={}",
                r.rate_sday.unwrap_or(f64::NAN),
                r.amplitude_deg
                    .map(|a| format!("{a:.0}"))
                    .unwrap_or("---".into()),
                r.beat_error_ms.unwrap_or(f64::NAN),
                r.period,
                r.bph,
                r.events.len(),
            ),
            None => println!("t={t:6.1}s  no signal"),
        }
        t = (i + 1) as f64 * 4.0;
    }
}

/// Load any supported audio file as mono f32 samples via symphonia.
fn load_audio(path: &str) -> (u32, Vec<f32>) {
    let file = std::fs::File::open(path).expect("cannot open file");
    let mss = symphonia::core::io::MediaSourceStream::new(
        Box::new(file),
        Default::default(),
    );
    let hint = symphonia::core::probe::Hint::new();
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &Default::default(), &Default::default())
        .expect("unsupported format");
    let mut format = probed.format;
    let track = format
        .tracks()
        .first()
        .expect("no track")
        .clone();
    let sr = track.codec_params.sample_rate.expect("no sample rate");
    let channels = track
        .codec_params
        .channels
        .map(|c| c.count())
        .unwrap_or(1);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &Default::default())
        .expect("unsupported codec");

    let mut samples = Vec::new();
    loop {
        match format.next_packet() {
            Ok(packet) => {
                if packet.track_id() != track.id {
                    continue;
                }
                match decoder.decode(&packet) {
                    Ok(audio) => {
                        let mut sb = symphonia::core::audio::SampleBuffer::<f32>::new(
                            audio.frames() as u64,
                            *audio.spec(),
                        );
                        sb.copy_interleaved_ref(audio);
                        for frame in sb.samples().chunks(channels) {
                            samples.push(frame.iter().sum::<f32>() / channels as f32);
                        }
                    }
                    Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                    Err(_) => break,
                }
            }
            Err(_) => break,
        }
    }
    (sr, samples)
}
