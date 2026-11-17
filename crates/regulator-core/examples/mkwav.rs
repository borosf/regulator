fn main() {
    use regulator_core::SyntheticWatch;
    let w = SyntheticWatch { bph: 18000, rate_sday: 33.0, beat_error_ms: 1.8, amplitude_deg: 272.0, quartz: false };
    let samples = w.generate(44100, 12.0);
    let spec = hound::WavSpec { channels: 1, sample_rate: 44100, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create("/tmp/watch.wav", spec).unwrap();
    for s in samples { w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).unwrap(); }
}
