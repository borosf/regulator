# regulator

A modern fork of [tg](https://github.com/xyzzy42/tg) — open-source watch
timing software for mechanical movements. regulator captures the ticking of
a watch with a microphone, analyzes the tick train, and reports the rate
error (s/day), balance amplitude, and beat error, with a cleaner interface
and a restructured, testable analysis core.

## Status

Early-stage Rust rewrite. The original C code (GPLv2) is kept at the root of
the repository as the reference implementation and for its build system; see
[docs/original-readme.md](docs/original-readme.md) for the original project's
documentation, including the microphone calibration procedure.

## Layout

- `crates/regulator-core` — pure DSP: noise suppression, FFT-based period
  detection, waveform folding, rate/amplitude/beat-error computation. No I/O,
  fully unit-tested against a synthetic watch signal generator.
- `crates/regulator-tui` — the terminal interface (ratatui), with live
  microphone capture (cpal) and a `--demo` mode driven by a synthetic watch.
- `crates/regulator-gui` — the graphical interface (egui): big readouts,
  waveform plot, settings panel; same demo/microphone sources.

## Build & run

```sh
cargo run -p regulator-tui -- --demo  # synthetic watch, no hardware needed
cargo run -p regulator-tui            # live microphone (needs ALSA/CoreAudio/WASAPI)
cargo run -p regulator-gui            # graphical interface (demo mode by default)
```

Useful keys: `←/→` beat rate preset (auto, 18000…36000 bph), `+/-` lift
angle, `n` noise suppressor, `q` quit.

To build on a system without an audio stack:

```sh
cargo build --no-default-features
```

## Improvements over tg

- Partial results: rate, beat error and amplitude are reported independently;
  a missing measurement is shown blank rather than failing the whole readout.
- Sub-sample period refinement (parabolic interpolation of the autocorrelation
  peak) for finer rate resolution than tg's integer-sample estimate.
- Robust amplitude estimation: the impulse duration is measured directly as
  the region above 20% of the tic/toc peak, instead of tg's escalating
  threshold heuristic.
- Cached FFT plans and reused scratch buffers: plans are built once per
  block length instead of per call.

## Portability

The core crate has no platform dependencies (pure Rust, no C). Both
frontends use cpal for capture, which works on Linux (ALSA), macOS
(CoreAudio) and Windows (WASAPI); without the `audio` feature they build
anywhere Rust does.

## License

GPLv2, like the original project. regulator is a derivative work of tg,
Copyright 2015 Marcello Mamino and contributors.
