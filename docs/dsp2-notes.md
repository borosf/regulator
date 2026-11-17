# dsp2: the next-generation detection chain

`crates/regulator-core/src/dsp2.rs`. Benchmark: `cargo run --release --example
bench_dsp2` (v1 = the tg-ported analyzer, v2 = this chain; both get the
same bph hint; 12 s synthetic watch, +7.5 s/d, white-noise sweep).

## Final results (rate error in s/d; ratio = events found / expected)

| bph | chain | n=0.02 | n=0.06 | n=0.15 | n=0.4 | n=1.0 | n=2.5 |
|---|---|---|---|---|---|---|---|
| 18000 | v1 | 1.04 | 1.04 | 1.01 | 0.01 | — | — |
| 18000 | v2 | 1.27 | 1.20 | 1.29 | 836 | 836* | 836* |
| 28800 | v1 | 1.91 | 1.91 | 1.93 | 3.63 | — | — |
| 28800 | v2 | 1.87 | 2.00 | 8.53 | 16.17 | 74* | 74* |
| 36000 | v1 | 2.33 | 2.33 | 2.32 | 2.30 | — | — |
| 36000 | v2 | 1.95 | 1.95 | 2.14 | 5.89 | 170* | 844* |

`—` = no lock. `*` = v1 cannot lock there at all; v2 acquires and tracks
(64–76 % event detection) — the faint-tick capability, where the
measured rate itself is information-limited to tens/hundreds of s/d over
12 s and converges with listening time.

## What the chain does

1. **Band-adaptive prefiltering**: band with the most impulsive content
   (rectified kurtosis) instead of a fixed 3 kHz high-pass.
2. **Fold-search acquisition** (the faint-tick mechanism): candidate
   periods near the bph hint (or every standard rate in auto) are each
   evaluated by *folding* the block — coherently summing every tick,
   a 10·log₁₀(K) dB SNR gain. The winning fold simultaneously produces
   the click template, its anchor and the grid phase. Winner selection
   is two-statistic (fold coherence pre-filter, whitened phase span as
   tie-break, sub-sample parabolic refinement).
3. **Matched-filter tracking** against the learned template with a
   *whitened* acceptance threshold (the peak must stand out from the
   lag-to-lag correlation statistics, not merely exceed a constant) and
   **interior-only acceptance**: a match near the hunt-window edge is a
   truncated argmax — statistically biased — and only ever corrects
   phase, never the statistics. Edge matches still steer the phase, or
   errors would grow until every match is rejected.
4. **PLL phase tracking** (phase only) plus a **per-type linear
   regression refit** of event position against tick index over
   gap-free runs — the slope is the period, its standard error shrinks
   to hundredths of a sample per block — accumulated across blocks
   with inverse-variance pooling and SE-gated adoption so scattered
   (high-noise) estimates can never random-walk the period.
5. **Adaptive template accumulation** (each highly correlated tick
   blends into the running template — continued coherent gain),
   lock surveillance (false locks that never confirm are dropped) and
   soft re-lock on sustained loss (re-fold instead of untracking).

## Hard-won lessons (for whoever breaches from here)

- A matched filter bootstrapped from a template carved out of pure
  noise self-confirms — naive correlation thresholds are not a lock.
  Whitened statistics (peak vs its own field) everywhere.
- A truncated argmax at a search-window edge is *biased toward the
  center*; with a feedback loop into the period estimate it runs the
  period away to a fixed-point plateau (~1 % off — hundreds of s/d).
  That was the source of the recurring "rate 500-1000 s/d" plateaus.
  Never let statistically biased measurements into the estimator.
- The per-tick period adaptation walked even at low noise: a tiny
  systematic template-blend shift accumulates. The statistically-aware
  regression refit is the only period estimator; the PLL handles phase.
- At high noise, single-event positions scatter by hundreds of samples:
  per-event spacing statistics are then useless, and the regression
  slope with honest standard errors is the right estimator (it also
  cancels constant phase offsets such as beat asymmetry).
- Fold coherence discriminates periods but is biased toward shorter
  windows; the whitened phase span is unbiased but latches onto
  structural artifacts with few fold segments. Use both, in that order.

## Known weak points

- 18000 bph at high noise: still hits a residual plateau (the one
  regression corner vs v1, which locks 18000/0.4 cleanly). Suspect the
  detected-beat-error/tic-toc interplay at long periods; unfixed.
- Mid-noise band (0.15–0.4 at 28800/36000): v2's rate is a few-to-16
  s/d worse than v1's whole-signal FFT estimate. The GUI keeps v1 as
  the readout engine partially for this reason.
