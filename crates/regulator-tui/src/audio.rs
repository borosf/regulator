// SPDX-License-Identifier: GPL-2.0
//! Live audio capture via cpal. Compiled only with the `audio` feature.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::mpsc::Sender;

const TARGET_RATE: f64 = 44100.0;

/// Naive linear resampler state: converts the device's sample rate to the
/// 44.1 kHz mono stream the analyzer expects.
struct Resampler {
    step: f64,
    frac: f64,
}

impl Resampler {
    fn new(input_rate: f64) -> Self {
        Resampler {
            step: input_rate / TARGET_RATE,
            frac: 0.0,
        }
    }

    fn push(&mut self, mono: &[f32], tx: &Sender<f32>) {
        while self.frac < mono.len() as f64 {
            let x = self.frac as usize;
            let next = mono[(x + 1).min(mono.len() - 1)];
            let t = (self.frac - x as f64) as f32;
            let s = mono[x] * (1.0 - t) + next * t;
            let _ = tx.send(s);
            self.frac += self.step;
        }
        self.frac -= mono.len() as f64;
    }
}

/// Spawn the default input device and stream mono 44.1 kHz f32 samples to
/// `tx`.
pub fn spawn_capture(tx: Sender<f32>) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no audio input device found")?;
    let supported = device
        .default_input_config()
        .context("no supported input configuration")?;
    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.into();
    let mut resampler = Resampler::new(config.sample_rate.0 as f64);
    let tx_f32 = tx.clone();

    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config,
            move |data: &[f32], _| {
                let mono = mix_mono(data, channels);
                resampler.push(&mono, &tx_f32);
            },
            err_cb,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config,
            move |data: &[i16], _| {
                let mono = mix_mono(
                    &data.iter().map(|&v| v as f32 / i16::MAX as f32).collect::<Vec<_>>(),
                    channels,
                );
                resampler.push(&mono, &tx_f32);
            },
            err_cb,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_input_stream(
            &config,
            move |data: &[u16], _| {
                let mono = mix_mono(
                    &data
                        .iter()
                        .map(|&v| (v as f32 - u16::MAX as f32 / 2.0) / (u16::MAX as f32 / 2.0))
                        .collect::<Vec<_>>(),
                    channels,
                );
                resampler.push(&mono, &tx_f32);
            },
            err_cb,
            None,
        )?,
        other => anyhow::bail!("unsupported sample format: {other:?}"),
    };
    stream.play()?;
    std::mem::forget(stream);
    Ok(())
}

fn err_cb(err: cpal::StreamError) {
    eprintln!("audio error: {err}");
}

fn mix_mono(data: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return data.to_vec();
    }
    data.chunks(channels)
        .map(|c| c.iter().sum::<f32>() / c.len() as f32)
        .collect()
}
