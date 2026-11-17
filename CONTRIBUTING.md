# Contributing to regulator

Thanks for your interest in improving regulator!

## Getting started

```sh
git clone https://github.com/borosf/regulator.git
cd regulator
cargo test --workspace        # everything should pass before you start
cargo run -p regulator-gui    # demo mode, no hardware needed
```

## Ground rules

- **License**: contributions are accepted under GPLv2, the project's
  license. By submitting a pull request you agree your work is licensed
  accordingly.
- **Scope of the core**: `regulator-core` must stay free of I/O and
  platform dependencies. Audio capture, files and UI belong in the
  frontend crates.
- **Tests**: the DSP core is unit-tested against a synthetic watch
  generator (`crates/regulator-core/src/signal.rs`). If you change the
  analysis, add or adjust a synthetic-signal test that demonstrates the
  behavior.
- **Style**: follow the existing code style; no new dependencies unless
  they're clearly justified.

## Bug reports

Open an issue and include:

- your OS and how you built (release/debug, audio feature),
- the movement's beat rate and lift angle,
- what you expected vs. what happened,
- if possible, a recording of the watch (WAV preferred) — recordings make
  DSP bugs reproducible.

## Pull requests

Keep PRs focused: one change per PR. Describe what changed and why, and
list the commands you ran to verify (`cargo test --workspace`,
`cargo build --no-default-features`).
