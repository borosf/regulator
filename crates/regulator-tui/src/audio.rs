// SPDX-License-Identifier: GPL-2.0
//! Live audio capture via cpal. Compiled only with the `audio` feature.

use anyhow::{Context, Result};
use std::sync::mpsc::Sender;

/// Spawn the default input device at 44.1 kHz mono and stream f32 samples
/// to `tx`.
pub fn spawn_capture(tx: Sender<f32>) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no audio input device found")?;
    let supported = device
        .build_input_stream_config()
        .or_else(|_| Ok::<cpal::SupportedStreamConfig, anyhow::Error>({
            device.default_input_config()
        })?)?;
    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(44100),
        buffer_size: supported.buffer_size.clone(),
    };
    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _| {
            for &s in data {
                let _ = tx.send(s);
            }
        },
        |err| eprintln!("audio error: {err}"),
        None,
    )?;
    stream.play()?;
    std::mem::forget(stream);
    Ok(())
}
