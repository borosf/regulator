# dsp2 draft: experiment notes

Rough next-generation detection chain in `crates/regulator-core/src/dsp2.rs`,
benchmarked old-vs-new in `examples/bench_dsp2.rs` (3 beat rates on parallel
threads, white noise sweep; 12 s synthetic watch, 7.5 s/d rate error, v2
given the same bph hint v1 gets).

## Results (rate error in s/d; event_ratio = ticks found / expected)

| bph | chain | n=0.02 | n=0.06 | n=0.15 | n=0.4 | n=1.0 | n=2.5 |
|---|---|---|---|---|---|---|---|
| 18000 | v1 rate | 1.04 | 1.04 | 1.01 | 0.01 | — | — |
| 18000 | v2 rate | 0.80 | 0.80 | 0.80 | 1.97 | — | — |
| 28800 | v1 rate | 1.91 | 1.91 | 1.93 | 3.63 | — | — |
| 28800 | v2 rate | **0.38** | **0.39** | 2.62 | 8.01 | — | — |
| 36000 | v1 rate | 2.33 | 2.33 | 2.32 | 2.30 | — | — |
| 36000 | v2 rate | 2.26 | 2.27 | 2.25 | 1.64 | — | — |

Neither chain acquires at noise ≥ 1.0. v2 detection ratio after
acquisition: 0.95–0.975. v2 costs ~0.2–0.27 s per 12 s signal (fine:
analysis budget is one 4 s block per 4 s wall time).

## Verdict

**Wins**
- Rate precision in the typical regime: up to 5× better (28800: 0.38 vs
  1.91 s/d) thanks to matched-filter event location + sub-sample
  interpolation + the tick-to-tick PLL. Better than v1 in 9 of 12
  lockable configurations.
- Every event carries a quality score (correlation with the learned
  template) — v1 has nothing like it.
- Robust per-block median refit keeps the period from walking (early
  draft runs had 50+ s/d runaways before the guardrails).

**Where it lost**
- 28800 at noise 0.4: grid loss mid-track (event ratio 0), rate 8 s/d.
  The PLL trust-region plus template EMA is not enough; next step is a
  proper innovation gate (Mahalanobis-style test on phase error against
  its own rolling variance) rather than fixed hunts.
- Acquisition is still v1's estimator with a comb fallback — the real
  "faint tick" mechanism (coherent multi-tick pre-sum detection) is only
  half-present: the template EMA gives 10·log10(n) dB gain for *tracks*,
  but acquisition at noise ≥ 1.0 needs the pre-sum up front. That is the
  highest-value next experiment.

**Draft lessons already banked**
- A matched filter bootstrapped from a template carved out of pure noise
  happily "confirms" its own ghost — naive correlation thresholds are not
  enough. Fixed with: comb-vs-field gate, standard-rate proximity gate,
  and lock-surveillance (no confirmations in 3 blocks → unlock).
- Real movements live on standard beat rates; using that as an
  acquisition prior is legitimate and removes an entire class of
  garbage locks.

Wire into the GUI only after the grid-loss case is fixed.
