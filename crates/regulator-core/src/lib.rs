// SPDX-License-Identifier: GPL-2.0
//! regulator-core: the signal-processing heart of regulator.
//!
//! A Rust port of the timing algorithm from tg (Copyright 2015 Marcello
//! Mamino, GPLv2, https://github.com/xyzzy42/tg). The pipeline:
//!
//! 1. optionally suppress impulsive noise (windowed energy gating)
//! 2. rectify and low-pass the microphone signal
//! 3. estimate the tick period via FFT-based autocorrelation
//! 4. fold samples at the detected period into an average waveform
//! 5. locate the tic/toc events, then compute rate, beat error and
//!    the balance amplitude from the pulse widths.

pub mod analyze;
pub mod calibrate;
pub mod clock_cal;
pub mod filters;
pub mod signal;
pub mod stats;

pub use analyze::{Analyzer, TimingResult};
pub use calibrate::{CalStatus, CalibrationState, CAL_DATA_SIZE};
pub use clock_cal::ClockCal;
pub use signal::SyntheticWatch;
pub const FILTER_CUTOFF: f64 = 3000.0;
