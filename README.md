# regulator

A modern, free-software watch timing machine — a Rust rewrite of
[tg](https://github.com/xyzzy42/tg), the open-source mechanical-watch
timing program. Put a watch on a microphone and regulator measures the
rate error, balance amplitude, and beat error, live, with a clean
instrument-style interface.

![screenshot](docs/screenshot.jpg)

## Features

- **Live measurement** of rate (s/day), amplitude (degrees) and beat
  error (ms), with each value reported independently — a measurement that
  fails is shown blank instead of breaking the whole readout.
- **Auto beat-rate detection** with snapping to standard rates
  (12000–43200 bph, including 36000 and 43200 high-beat movements).
- **Strip chart trace**: time runs left to right, phase error moves up
  and down, tic and toc in different colors — the classic timegrapher
  view.
- **Sound-card calibration** against a quartz watch ticking once per
  second, with live progress feedback and an early exit once the estimate
  converges.
- **Export**: chart as SVG, raw trace as CSV, and a branded JPEG result
  sheet with all readouts, ready to attach to a service record.
- **Demo mode**: a synthetic watch drives every feature with no hardware.

## Build & run

```sh
cargo run -p regulator-gui   # graphical interface (demo mode by default)
```

Uncheck "demo signal" in the side panel to analyze the microphone. On
Linux you may need `libasound2-dev`; on Windows and macOS it just works.

To build on a system without an audio stack:

```sh
cargo build --no-default-features
cargo test
```

## Using it

1. Rest the watch on a microphone (a piezo contact mic or the watch
   directly on a laptop mic both work).
2. Pick the beat rate, or leave it on auto.
3. Set the lift angle for the movement (52° is a safe default; check the
   movement's spec).
4. Read the rate. The trace confirms it: a rising drift means fast, a
   falling drift means slow, two horizontal bands mean beat error.
5. To calibrate the sound card, click "calibrate (quartz)" and place a
   quartz watch (exactly 1 tick/s) on the microphone for a minute or two.
   The correction persists between runs.

## Layout

- `crates/regulator-core` — pure DSP: filters, FFT-based period
  detection, waveform folding, rate/amplitude/beat-error computation,
  calibration. No I/O, fully unit-tested against a synthetic watch.
- `crates/regulator-gui` — the graphical interface (egui). **This is the
  main frontend.**
- `crates/regulator-tui` — the terminal interface (ratatui). **Experimental:**
  it lags behind the GUI in features and polish and may be removed or
  reworked.

## Improvements over tg

- Partial results: rate, beat error and amplitude are reported
  independently; a missing measurement is blank, not fatal.
- Sub-sample period refinement (parabolic interpolation of the
  autocorrelation peak) for finer rate resolution.
- Cached FFT plans and reused scratch buffers.
- Faster calibration: the quartz fit exits early once the estimate
  converges instead of always collecting the full 900-second window.
- Modern cross-platform GUI with a result-sheet export.

The original C code (GPLv2) is kept at the root of the repository as the
reference implementation; see [docs/original-readme.md](docs/original-readme.md)
for the original project's documentation.

## Known limitations

- The passive clock estimate (sound card vs. local timer) is
  informational only: many machines' timers drift by tens of s/day, so
  only the quartz calibration is applied to measurements.
- Tick events are located by peak search around expected positions, not
  template matching; very noisy recordings may drop events.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Bug reports and feature requests
are welcome via the issue tracker.

## License

GPLv2, like the original project. regulator is a derivative work of tg,
Copyright 2015 Marcello Mamino and contributors.
