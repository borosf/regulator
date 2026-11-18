// SPDX-License-Identifier: GPL-2.0
//! regulator-gui: graphical watch timing machine built on the regulator
//! core, using egui for an immediate-mode, cross-platform interface.

#[cfg(feature = "audio")]
mod audio;
mod chart;
mod sheet;

use eframe::egui;
use regulator_core::dsp2::V2Tracker;
use regulator_core::{CalStatus, CalibrationState, Analyzer, SyntheticWatch, TimingResult};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One located tick event on the live trace.
#[derive(Clone, Copy)]
struct TracePoint {
    /// Absolute time in seconds.
    t: f64,
    /// Phase error against the nominal beat grid, in milliseconds.
    phase_ms: f64,
    tic: bool,
}

/// Live calibration feedback for the center panel.
struct CalView {
    points: Vec<(f64, f64)>,
    have: usize,
    need: usize,
    estimate: Option<(f64, f64)>,
}

/// What the worker sends to the UI each iteration.
struct Feed {
    result: Option<TimingResult>,
    trace: Vec<TracePoint>,
    cal: Option<CalView>,
    /// tg-style signal level: 0..4, the cascade step that passed.
    signal: u32,
    /// Whether microphone samples are still arriving (None = not mic).
    mic_live: Option<bool>,
}

const SAMPLE_RATE: u32 = 44100;
const BLOCK_SECONDS: f64 = 4.0;

#[derive(Clone)]
struct Params {
    bph: u32,
    lift_angle: f64,
    noise_suppressor: bool,
    calibrating: bool,
    calibration: f64,
    /// Calibration came from the passive clock estimate, not a quartz run.
    cal_auto: bool,
    cal_progress: String,
    /// Settings changed since the last save.
    dirty: bool,
}

impl Params {
    fn config_path() -> std::path::PathBuf {
        let dir = if let Ok(a) = std::env::var("APPDATA") {
            std::path::PathBuf::from(a).join("regulator")
        } else if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
            std::path::PathBuf::from(x).join("regulator")
        } else if let Ok(h) = std::env::var("HOME") {
            std::path::PathBuf::from(h).join(".config").join("regulator")
        } else {
            std::path::PathBuf::from(".")
        };
        std::fs::create_dir_all(&dir).ok();
        dir.join("config.txt")
    }

    fn load() -> Self {
        let mut p = Params::default();
        if let Ok(text) = std::fs::read_to_string(Self::config_path()) {
            for line in text.lines() {
                let Some((k, v)) = line.split_once('=') else { continue };
                match k.trim() {
                    "bph" => {
                        if let Ok(n) = v.trim().parse() {
                            p.bph = n;
                        }
                    }
                    "lift_angle" => {
                        if let Ok(n) = v.trim().parse() {
                            p.lift_angle = n;
                        }
                    }
                    "calibration" => {
                        if let Ok(n) = v.trim().parse() {
                            p.calibration = n;
                            p.cal_auto = true;
                        }
                    }
                    "noise_suppressor" => p.noise_suppressor = v.trim() == "true",
                    _ => {}
                }
            }
        }
        p
    }

    fn save(&self) {
        let text = format!(
            "bph={}\nlift_angle={:.1}\ncalibration={:.3}\nnoise_suppressor={}\n",
            self.bph,
            self.lift_angle,
            self.calibration,
            self.noise_suppressor
        );
        let _ = std::fs::write(Self::config_path(), text);
    }
}

impl Default for Params {
    fn default() -> Self {
        Params {
            bph: 0,
            lift_angle: 52.0,
            noise_suppressor: true,
            calibrating: false,
            calibration: 0.0,
            cal_auto: false,
            cal_progress: String::new(),
            dirty: false,
        }
    }
}

/// Shared ring buffer of live microphone samples (if capture is available),
/// with a monotonic total so window start times never go backwards.
struct MicBuf {
    data: Vec<f32>,
    total: u64,
    updated: std::time::Instant,
}

impl Default for MicBuf {
    fn default() -> Self {
        MicBuf {
            data: Vec::new(),
            total: 0,
            updated: std::time::Instant::now(),
        }
    }
}

#[cfg(feature = "audio")]
impl MicBuf {
    fn push(&mut self, s: f32, cap: usize) {
        self.data.push(s);
        self.total += 1;
        self.updated = std::time::Instant::now();
        if self.data.len() > cap {
            self.data.drain(..cap / 2);
        }
    }
}

type Ring = Arc<Mutex<MicBuf>>;

struct RegulatorApp {
    result: Option<TimingResult>,
    trace: Vec<TracePoint>,
    cal: Option<CalView>,
    last_export: Option<String>,
    /// tg-style signal level 0..4 (which cascade window passed).
    signal: u32,
    /// True while the readout is frozen (space).
    held: bool,
    /// Whether microphone samples are still arriving (None = not mic).
    mic_live: Option<bool>,
    /// Rate trend for the side panel: (when, s/day) of the last 5 min.
    history: Vec<(std::time::Instant, f64)>,
    params: Arc<Mutex<Params>>,
    rx: mpsc::Receiver<Feed>,
}

impl RegulatorApp {
    fn new(ring: Option<Ring>) -> Self {
        let params = Arc::new(Mutex::new(Params::load()));
        let (tx, rx) = mpsc::channel::<Feed>();
        let params_arc = params.clone();
        std::thread::spawn(move || {
            let watch = SyntheticWatch {
                bph: 28800,
                rate_sday: 9.0,
                beat_error_ms: 0.6,
                amplitude_deg: 275.0,
                quartz: false,
            };
            let demo_samples = watch.generate(SAMPLE_RATE, 120.0);
            let block = (BLOCK_SECONDS * SAMPLE_RATE as f64) as usize;
            
            let mut win_start_s = 0.0f64;
            let mut last_event_s = 0.0f64;
            let mut trace: Vec<TracePoint> = Vec::new();
            // tg's window cascade: 2, 4, 8, 16 s trailing windows,
            // analyzed from the last good step, climbing while
            // sigma < period/10000, falling back on failure. The longest
            // passing window's result is the measurement (tg's
            // compute_update).
            const CASCADE_S: [f64; 4] = [2.0, 4.0, 8.0, 16.0];
            let mut step: usize = 0;
            // Demo playhead (slides 1 s per cycle; loops).
            let mut demo_head: usize = 0;
            let mut demo_epoch: usize = 0;
            // Next-generation detection chain state.
            let mut tracker = V2Tracker::new(SAMPLE_RATE);
            let mut fed_until: u64 = 0;
            let mut last_v2 = None;
            let mut cal_state = CalibrationState::new();
            let mut cal_cooldown = 0u32;
            let mut cal_fed_until: u64 = 0;
            let mut analyzer = Analyzer::new(SAMPLE_RATE);
            loop {
                let mut p = params_arc.lock().unwrap_or_else(|e| e.into_inner());
                if p.calibrating {
                    if let Some(ring) = &ring {
                        let buf = ring.lock().unwrap_or_else(|e| e.into_inner());
                        // Feed calibration only non-overlapping windows:
                        // overlapping windows produce correlated (time,
                        // phase) samples, which inflate the fit's
                        // confidence and let it exit on a bias.
                        let start_u64 = buf.total.saturating_sub(block as u64);
                        if buf.data.len() >= block && start_u64 >= cal_fed_until {
                            let cal_an = Analyzer::for_calibration(SAMPLE_RATE);
                            let w = buf.data[buf.data.len() - block..].to_vec();
                            let window_start_ts = start_u64;
                            cal_fed_until = start_u64 + w.len() as u64;
                            match cal_state.add_block(&cal_an, &w, window_start_ts) {
                                CalStatus::Done(cal) => {
                                    // The quartz fit reports the raw error of
                                    // a perfect watch; tg's convention (which
                                    // the analyzer applies) needs its negation.
                                    p.calibration = -cal;
                                    p.calibrating = false;
                                    p.cal_auto = true;
                                    p.dirty = true;
                                    p.cal_progress = format!("{:+.1} s/d", -cal);
                                }
                                CalStatus::Failed => {
                                    p.calibrating = false;
                                    p.cal_progress = "failed".into();
                                }
                                CalStatus::Collecting { have, need } => {
                                    p.cal_progress = format!("{have}/{need} s");
                                }
                                CalStatus::Missed => {}
                            }
                            cal_cooldown = 5;
                        }
                    }
                    if cal_cooldown > 0 {
                        cal_cooldown -= 1;
                    }
                    let view = CalView {
                        points: cal_state.points().to_vec(),
                        have: cal_state.progress().0,
                        need: cal_state.progress().1,
                        estimate: cal_state.estimate(),
                    };
                    drop(p);
                    let _ = tx.send(Feed {
                        result: None,
                        trace: Vec::new(),
                        cal: Some(view),
                        signal: 0,
                        mic_live: None,
                    });
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                if p.dirty {
                    p.save();
                    p.dirty = false;
                }
                drop(p);
                let p2 = params_arc.lock().unwrap_or_else(|e| e.into_inner()).clone();
                analyzer.bph = p2.bph;
                analyzer.lift_angle = p2.lift_angle;
                analyzer.noise_suppressor = p2.noise_suppressor;
                analyzer.calibration = p2.calibration;
                // The demo signal drives the app only when no microphone
                // capture is available, so the interface stays usable on any
                // machine without a hardware toggle.
                // Trailing tail of the live source (up to the largest
                // cascade window), with its absolute sample position.
                let src_tail: Option<(u64, Vec<f32>)> = if ring.is_none() {
                    // Demo: last min(played-this-pass, 16) seconds ending
                    // at the playhead.
                    let take = demo_epoch.min(16 * SAMPLE_RATE as usize);
                    let begin = demo_head - take;
                    // demo_head lives in absolute-consumed space; map into
                    // the looping buffer only within this pass.
                    let pass_pos = demo_epoch;
                    let w = if begin == 0 && pass_pos == 0 {
                        Vec::new()
                    } else {
                        let lo = pass_pos.saturating_sub(take);
                        demo_samples[lo..pass_pos].to_vec()
                    };
                    demo_head += SAMPLE_RATE as usize; // slide 1 s
                    demo_epoch += SAMPLE_RATE as usize;
                    if demo_head % demo_samples.len() < SAMPLE_RATE as usize {
                        demo_epoch = SAMPLE_RATE as usize; // looped
                    }
                    if w.is_empty() {
                        None
                    } else {
                        Some(((demo_head - w.len()) as u64, w))
                    }
                } else if let Some(ring) = &ring {
                    let buf = ring.lock().unwrap_or_else(|e| e.into_inner());
                    if buf.data.len() >= 2 * SAMPLE_RATE as usize {
                        let take = buf.data.len().min(16 * SAMPLE_RATE as usize);
                        Some((
                            buf.total - take as u64,
                            buf.data[buf.data.len() - take..].to_vec(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                };

                // tg's cascade: analyze from the last good step, climb on
                // a tight sigma, fall back on failure; the longest passing
                // window is the measurement.
                // One step per cycle (cost is spread like tg's
                // per-interval computations): a passing step promotes
                // the cascade to the next-larger window for the next
                // cycle; a failure demotes it. The result shown is the
                // step analyzed this cycle.
                let mut res: Option<(u64, TimingResult)> = None;
                if let Some((abs_start, tail)) = &src_tail {
                    let mut s_step = step.min(3);
                    if tail.len() < (CASCADE_S[s_step] * SAMPLE_RATE as f64) as usize
                        && s_step > 0
                    {
                        s_step -= 1;
                    }
                    let need = (CASCADE_S[s_step] * SAMPLE_RATE as f64) as usize;
                    if tail.len() >= need {
                        let w = &tail[tail.len() - need..];
                        let ws = *abs_start + (tail.len() - need) as u64;
                        let r = analyzer.analyze(w).filter(|r| r.sigma < r.period / 10000.0);
                        match r {
                            Some(r) => {
                                res = Some((ws, r));
                                // Promote for the next cycle.
                            }
                            None => {
                                if s_step == 0 {
                                    res = None;
                                } else {
                                    // Demote; the failing longer window's
                                    // result is not used (tg keeps the
                                    // last good one for the readout).
                                    let prev = step.min(s_step.saturating_sub(1));
                                    let need_prev =
                                        (CASCADE_S[prev] * SAMPLE_RATE as f64) as usize;
                                    if tail.len() >= need_prev {
                                        let w = &tail[tail.len() - need_prev..];
                                        let ws = *abs_start + (tail.len() - need_prev) as u64;
                                        res = analyzer
                                            .analyze(w)
                                            .filter(|r| r.sigma < r.period / 10000.0)
                                            .map(|r| (ws, r));
                                    }
                                    s_step = prev;
                                }
                            }
                        }
                    }
                    if res.is_some() && s_step < 3 {
                        s_step += 1;
                    }
                    step = s_step;
                }
                if let Some((ws, _r)) = &res {
                    win_start_s = *ws as f64 / SAMPLE_RATE as f64;
                }

                // v2 detection chain: fed non-overlapping 4-second
                // windows from the same tail. When it locks, its
                // template-matched events drive the trace, and when the
                // cascade cannot lock at all (faint signal), its rate
                // becomes the readout.
                tracker.bph_hint = p2.bph;
                tracker.calibration = p2.calibration;
                if let Some((abs_start, tail)) = &src_tail {
                    if tail.len() >= 4 * SAMPLE_RATE as usize {
                        let ws = abs_start + tail.len() as u64 - 4 * SAMPLE_RATE as u64;
                        if ws >= fed_until {
                            let w = tail[tail.len() - 4 * SAMPLE_RATE as usize..].to_vec();
                            let r2 = tracker.push_block(&w, ws);
                            fed_until = ws + w.len() as u64;
                            last_v2 = Some(r2);
                        }
                    }
                }
                let use_v2_events = tracker.locked();
                // The measurement of record is tg's cascade result; the
                // v2 tracker's session estimate stands in only when the
                // cascade cannot lock at all.
                let combined: Option<TimingResult> = match &res {
                    Some((_, r)) => Some(r.clone()),
                    None => match &last_v2 {
                        Some(r2) if r2.locked => Some(TimingResult {
                            period: r2.period,
                            rate_sday: tracker.rate().or(r2.rate_sday),
                            beat_error_ms: None,
                            amplitude_deg: None,
                            sigma: 0.0,
                            waveform: Vec::new(),
                            bph: r2.bph,
                            tic: None,
                            toc: None,
                            events: Vec::new(),
                        }),
                        _ => None,
                    },
                };
                if !use_v2_events {
                    if let Some((_, r)) = &res {
                        let p0 = 7200.0 * SAMPLE_RATE as f64 / r.bph as f64;
                        let quarter = (2.0 * 3600.0 / r.bph as f64) / 4.0;
                        for (ev, is_tic) in &r.events {
                            let t = win_start_s + ev / SAMPLE_RATE as f64;
                            if t <= last_event_s + quarter {
                                continue;
                            }
                            last_event_s = t;
                            let mut e = (ev % p0) / p0 - 0.5;
                            if e < -0.5 {
                                e += 1.0;
                            }
                            trace.push(TracePoint {
                                t,
                                phase_ms: e * (2.0 * 3600.0 / r.bph as f64) * 1000.0,
                                tic: *is_tic,
                            });
                        }
                    }
                }
                if use_v2_events {
                    if let Some(r2) = &last_v2 {
                        let period_s = r2.period / SAMPLE_RATE as f64;
                        // tg's event deadband: a quarter period, so
                        // the same tick is never added twice with a
                        // slightly different measured time when windows
                        // overlap.
                        let quarter = period_s / 4.0;
                        for ev in &r2.events {
                            let t = ev.t / SAMPLE_RATE as f64;
                            if t <= last_event_s + quarter {
                                continue;
                            }
                            last_event_s = t;
                            let mut e = (ev.t % r2.period) / r2.period - 0.5;
                            if e < -0.5 {
                                e += 1.0;
                            }
                            trace.push(TracePoint {
                                t,
                                phase_ms: e * period_s * 1000.0,
                                tic: ev.tic,
                            });
                        }
                    }
                }
                while let Some(first) = trace.first() {
                    if first.t < win_start_s - 120.0 {
                        trace.remove(0);
                    } else {
                        break;
                    }
                }
                                // tg's signal semantics: step+1 for a passing window,
                // 0 when nothing passed.
                let sig = if res.is_some() {
                    step as u32 + 1
                } else if tracker.locked() {
                    1
                } else {
                    0
                };
                let mic_live = if ring.is_some() {
                    ring.as_ref().map(|r| {
                        r.lock()
                            .map(|b| b.updated.elapsed().as_secs_f64() < 2.0)
                            .unwrap_or(false)
                    })
                } else {
                    None
                };
                let _ = tx.send(Feed {
                    result: combined,
                    trace: trace.clone(),
                    cal: None,
                    signal: sig.min(4),
                    mic_live,
                });
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        Self {
            result: None,
            trace: Vec::new(),
            cal: None,
            last_export: None,
            signal: 0,
            held: false,
            mic_live: None,
            history: Vec::new(),
            params,
            rx,
        }
    }
}

impl eframe::App for RegulatorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keyboard: space = hold/resume, e = export. Controls that free
        // the watchmaker's hands while a measurement settles.
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.held = !self.held;
        }
        let mut want_export = false;
        if ctx.input(|i| i.key_pressed(egui::Key::E)) {
            want_export = true;
        }
        while let Ok(feed) = self.rx.try_recv() {
            if self.held {
                // Frozen: drain worker output without applying it, so the
                // readout and trace keep exactly what was held.
                continue;
            }
            self.result = feed.result;
            self.trace = feed.trace;
            self.signal = feed.signal;
            self.mic_live = feed.mic_live;
            if feed.cal.is_some() {
                self.cal = feed.cal;
            } else {
                self.cal = None;
            }
            if let Some(r) = &self.result {
                if let Some(v) = r.rate_sday {
                    let now = std::time::Instant::now();
                    self.history.retain(|(t, _)| now.duration_since(*t).as_secs_f64() < 300.0);
                    self.history.push((now, v));
                }
            }
        }
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(12, 13, 15);
        visuals.window_fill = egui::Color32::from_rgb(12, 13, 15);
        visuals.extreme_bg_color = egui::Color32::from_rgb(8, 9, 10);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(18, 20, 22);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(22, 24, 27);
        visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(4);
        visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(4);
        visuals.widgets.active.corner_radius = egui::CornerRadius::same(4);
        visuals.selection.bg_fill = egui::Color32::from_rgb(40, 60, 48);
        ctx.set_visuals(visuals);

        // Top: the primary readout. One number the watchmaker stares at,
        // everything else is secondary.
        egui::TopBottomPanel::top("readout").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let rate = self.result.as_ref().and_then(|r| r.rate_sday);
                let text = match rate {
                    Some(v) => format!("{v:+.1}"),
                    None => "---".into(),
                };
                let color = match rate {
                    Some(v) if v.abs() < 5.0 => egui::Color32::from_rgb(120, 220, 120),
                    Some(v) if v.abs() < 15.0 => egui::Color32::from_rgb(235, 200, 90),
                    Some(_) => egui::Color32::from_rgb(235, 120, 100),
                    None => egui::Color32::GRAY,
                };
                ui.label(
                    egui::RichText::new(text)
                        .monospace()
                        .size(52.0)
                        .strong()
                        .color(color),
                );
                ui.label(egui::RichText::new("s/d").monospace().size(16.0).color(egui::Color32::GRAY));
                // tg-style signal dots: filled per the cascade window that
                // passed; one glance answers "is this reading solid?".
                ui.add_space(10.0);
                let (dot_rect, _) = ui.allocate_exact_size(
                    egui::vec2(52.0, 12.0),
                    egui::Sense::hover(),
                );
                let painter = ui.painter_at(dot_rect);
                for i in 0..4 {
                    let filled = (i as u32) < self.signal;
                    let color = if !filled {
                        egui::Color32::from_gray(50)
                    } else if self.signal == 4 {
                        egui::Color32::WHITE
                    } else {
                        egui::Color32::from_rgb(235, 200, 90)
                    };
                    let cx = dot_rect.left() + 7.0 + i as f32 * 13.0;
                    let cy = dot_rect.center().y;
                    painter.circle_filled(egui::pos2(cx, cy), 5.0, color);
                }
                if self.signal > 0 {
                    let win_s = [2u32, 4, 8, 16][(self.signal - 1) as usize];
                    ui.label(
                        egui::RichText::new(format!("w {win_s}s"))
                            .monospace()
                            .size(12.0)
                            .color(egui::Color32::from_gray(110)),
                    );
                }
                if self.held {
                    ui.label(
                        egui::RichText::new("HELD")
                            .monospace()
                            .strong()
                            .size(14.0)
                            .color(egui::Color32::from_rgb(235, 200, 90)),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let p = self.params.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if ui.button("export").clicked() || want_export {
                        self.last_export = chart::export_chart(&self.trace, &self.result, &self.params);
                    }
                    if let Some(path) = &self.last_export {
                        ui.label(
                            egui::RichText::new(format!("saved {path}"))
                                .monospace()
                                .size(12.0)
                                .color(egui::Color32::from_rgb(120, 220, 120)),
                        );
                    }
                    let mode = match self.mic_live {
                        Some(false) => "mic: no input".to_string(),
                        Some(true) => "mic".to_string(),
                        None => "demo".to_string(),
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  {}",
                            mode,
                            match p.bph {
                                0 => match self.result.as_ref().map(|r| r.bph) {
                                    Some(b) => format!("auto (detected {b} bph)"),
                                    None => "auto".to_string(),
                                },
                                b => format!("{b} bph"),
                            }
                        ))
                        .monospace()
                        .size(13.0)
                        .color(egui::Color32::GRAY),
                    );
                });
            });
            ui.add_space(2.0);
        });

        // Bottom: the secondary data strip.
        egui::TopBottomPanel::bottom("strip").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.style_mut().override_font_id = Some(egui::FontId::monospace(15.0));
                let p = self.params.lock().unwrap_or_else(|e| e.into_inner()).clone();
                strip_value(ui, "amp", &self.result.as_ref().and_then(|r| r.amplitude_deg).map(|v| format!("{v:.0}°")));
                strip_value(ui, "be", &self.result.as_ref().and_then(|r| r.beat_error_ms).map(|v| format!("{v:+.2} ms")));
                strip_value(ui, "lift", &Some(format!("{:.0}°", p.lift_angle)));
                strip_value(ui, "noise", &Some(if p.noise_suppressor { "on".to_string() } else { "off".to_string() }));
                if p.calibration != 0.0 {
                    let tag = if p.cal_auto { " auto" } else { "" };
                    strip_value(ui, "cal", &Some(format!("{:+.1} s/d{tag}", p.calibration)));
                }
                if p.calibrating {
                    ui.label(
                        egui::RichText::new(format!("calibrating {}", p.cal_progress))
                            .monospace()
                            .size(13.0)
                            .color(egui::Color32::from_rgb(235, 200, 90)),
                    );
                }
            });
            ui.add_space(4.0);
        });

        // Right: controls, compact.
        egui::SidePanel::right("settings")
            .min_width(190.0)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.style_mut().override_font_id = Some(egui::FontId::monospace(13.0));
                let mut p = self.params.lock().unwrap_or_else(|e| e.into_inner());
                let before = (p.bph, p.lift_angle, p.calibration, p.noise_suppressor);
                ui.label(egui::RichText::new("BEAT RATE").small().weak());
                egui::ComboBox::from_id_salt("bph")
                    .selected_text(if p.bph == 0 { "auto".into() } else { p.bph.to_string() })
                    .width(ui.available_width() - 12.0)
                    .show_ui(ui, |ui| {
                        for b in [0u32, 12000, 14400, 18000, 19800, 21600, 25200, 28800, 36000, 43200] {
                            ui.selectable_value(
                                &mut p.bph,
                                b,
                                if b == 0 { "auto".into() } else { b.to_string() },
                            );
                        }
                    });
                ui.add_space(6.0);
                ui.label(egui::RichText::new("LIFT ANGLE").small().weak());
                ui.add(
                    egui::DragValue::new(&mut p.lift_angle)
                        .range(30.0..=70.0)
                        .speed(0.5)
                        .suffix("°"),
                );
                ui.add_space(6.0);
                ui.checkbox(&mut p.noise_suppressor, "noise suppressor");
                ui.add_space(6.0);
                ui.label(egui::RichText::new("CALIBRATION s/d").small().weak());
                ui.add(
                    egui::DragValue::new(&mut p.calibration)
                        .speed(0.1)
                        .range(-100.0..=100.0),
                );
                ui.add_space(10.0);
                if p.calibrating {
                    if ui.button("stop calibration").clicked() {
                        p.calibrating = false;
                    }
                } else if ui.button("calibrate (quartz)").clicked() {
                    p.calibrating = true;
                    p.cal_progress.clear();
                }
                ui.add_space(10.0);
                let after = (p.bph, p.lift_angle, p.calibration, p.noise_suppressor);
                if after != before {
                    p.dirty = true;
                }
                ui.separator();
                ui.add_space(6.0);
                ui.label(egui::RichText::new("RATE TREND (5 MIN)").small().weak());
                draw_trend(ui, &self.history, 110.0);
                ui.add_space(6.0);
                ui.checkbox(&mut self.held, "hold (space)");
            });

        // Center: the strip chart on top, tg's tick scope views beneath —
        // live calibration feedback takes over while calibrating.
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::from_rgb(10, 11, 12)))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                if let Some(cal) = &self.cal {
                    chart::draw_cal_view(ui, cal);
                } else {
                    let avail = ui.available_size();
                    let scopes = self
                        .result
                        .as_ref()
                        .and_then(|r| r.tic.map(|tic| (r, tic)))
                        .filter(|(r, _)| r.waveform.iter().cloned().fold(0f32, f32::max) > 0.0);
                    let scope_h = match scopes {
                        Some((r, _)) if r.toc.is_some() => 300.0,
                        Some(_) => 150.0,
                        None => 0.0,
                    };
                    if avail.y > scope_h + 80.0 {
                        if self.trace.is_empty() {
                            draw_listening(ui, self.mic_live, avail.y - scope_h);
                        } else {
                            draw_trace(ui, &self.trace, avail.y - scope_h);
                        }
                        if let Some((r, tic)) = scopes {
                            let lift = self
                                .params
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .lift_angle;
                            let h = 144.0;
                            draw_scope(
                                ui,
                                &r.waveform,
                                tic,
                                "TIC",
                                egui::Color32::WHITE,
                                r.period,
                                lift,
                                h,
                            );
                            ui.add_space(6.0);
                            if let Some(toc) = r.toc {
                                draw_scope(
                                    ui,
                                    &r.waveform,
                                    toc,
                                    "TOC",
                                    egui::Color32::from_rgb(235, 200, 90),
                                    r.period,
                                    lift,
                                    h,
                                );
                            }
                        }
                    } else if self.trace.is_empty() {
                        draw_listening(ui, self.mic_live, avail.y);
                    } else {
                        draw_trace(ui, &self.trace, avail.y);
                    }
                }
            });

        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

fn strip_value(ui: &mut egui::Ui, name: &str, value: &Option<String>) {
    let v = value.clone().unwrap_or_else(|| "---".into());
    ui.label(egui::RichText::new(name).size(11.0).color(egui::Color32::from_gray(110)));
    ui.label(egui::RichText::new(v).size(15.0).strong());
    ui.add_space(12.0);
}

/// The strip chart: time runs left to right, phase error moves up and
/// down (tg's orientation). Rate error shows as a rising or falling drift,
/// beat error splits tic/toc into two horizontal bands, noise widens the
/// scatter.
fn draw_trace(ui: &mut egui::Ui, trace: &[TracePoint], height: f32) {
    let size = egui::vec2(ui.available_size().x, height);
    let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(15, 16, 18));
    // Phase-error window: +/- 25 ms, ticks wrap into it.
    const WINDOW_MS: f64 = 25.0;
    const SPAN_S: f64 = 60.0;
    let cy = rect.center().y;
    let half_h = rect.height() / 2.0 - 8.0;
    let to_y = |ms: f64| {
        let wrapped = ((ms + WINDOW_MS) % (2.0 * WINDOW_MS) + 2.0 * WINDOW_MS) % (2.0 * WINDOW_MS) - WINDOW_MS;
        cy - (wrapped / WINDOW_MS * half_h as f64) as f32
    };
    // Grid: center line plus 10 ms steps, horizontal like tg's display.
    for ms in [-20.0, -10.0, 0.0, 10.0, 20.0] {
        let y = to_y(ms);
        let color = if ms == 0.0 {
            egui::Color32::from_gray(52)
        } else {
            egui::Color32::from_gray(28)
        };
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(1.0_f32, color),
        );
    }
    let t_end = trace.last().map(|q| q.t).unwrap_or(0.0);
    let w = rect.width();
    for p in trace {
        let age = t_end - p.t;
        if age > SPAN_S {
            continue;
        }
        let x = rect.right() - (age / SPAN_S * w as f64) as f32;
        let y = to_y(p.phase_ms);
        let color = if p.tic {
            egui::Color32::from_rgb(140, 230, 140)
        } else {
            egui::Color32::from_rgb(240, 140, 240)
        };
        painter.circle_filled(egui::pos2(x, y), 2.0, color);
    }
}

/// Rate trend: the last 5 minutes of s/day readings on an auto-ranged
/// thin plot, so settling vs. drifting is obvious at a glance.
fn draw_trend(ui: &mut egui::Ui, history: &[(std::time::Instant, f64)], height: f32) {
    let size = egui::vec2(ui.available_size().x, height);
    let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 2.0, egui::Color32::from_rgb(16, 18, 20));
    if history.len() < 2 {
        return;
    }
    let now = std::time::Instant::now();
    let span = 300.0_f64;
    let (lo, hi) = {
        let mut lo = f64::MAX;
        let mut hi = f64::MIN;
        for (_, v) in history {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
        if hi - lo < 1.0 {
            let mid = (hi + lo) / 2.0;
            (mid - 0.5, mid + 0.5)
        } else {
            (lo, hi)
        }
    };
    let y_of = |v: f64| {
        rect.bottom() - 4.0 - ((v - lo) / (hi - lo) * (rect.height() - 8.0) as f64) as f32
    };
    if lo <= 0.0 && 0.0 <= hi {
        let y = y_of(0.0);
        painter.line_segment(
            [egui::pos2(rect.left() + 4.0, y), egui::pos2(rect.right() - 4.0, y)],
            egui::Stroke::new(1.0_f32, egui::Color32::from_gray(45)),
        );
    }
    let to_px = |t: f64, v: f64| egui::pos2(
        rect.left() + 4.0 + (t / span * (rect.width() - 8.0) as f64) as f32,
        y_of(v),
    );
    let pts: Vec<egui::Pos2> = history
        .iter()
        .map(|(when, v)| (now.duration_since(*when).as_secs_f64(), *v))
        .filter(|(t, _)| *t < span)
        .map(|(t, v)| to_px(t, v))
        .collect();
    if pts.len() >= 2 {
        painter.add(egui::Shape::line(
            pts,
            egui::Stroke::new(1.5_f32, egui::Color32::from_rgb(140, 220, 180)),
        ));
    }
    painter.text(
        egui::pos2(rect.left() + 6.0, rect.top() + 4.0),
        egui::Align2::LEFT_TOP,
        format!("{:+.1} … {:+.1}", lo, hi),
        egui::FontId::monospace(10.0),
        egui::Color32::from_gray(110),
    );
}

/// Empty-state hint shown before any tick has been found.
fn draw_listening(ui: &mut egui::Ui, mic_live: Option<bool>, height: f32) {
    let size = egui::vec2(ui.available_size().x, height);
    let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(15, 16, 18));
    let msg = match mic_live {
        Some(false) => "mic: no input — check the device",
        _ => "listening… rest the watch on the microphone",
    };
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        msg,
        egui::FontId::monospace(16.0),
        egui::Color32::from_gray(90),
    );
}

/// tg's tick scope view: the folded acoustic waveform centered on the tick,
/// -25 ms .. +10 ms, with a 1 ms time graticule below the axis and an
/// amplitude-degree graticule above it (the rotation time the balance needs
/// to sweep the lift angle). The envelope is mirrored, stroked and filled.
fn draw_scope(
    ui: &mut egui::Ui,
    wf: &[f32],
    center: usize,
    label: &str,
    color: egui::Color32,
    period: f64,
    lift: f64,
    height: f32,
) {
    let size = egui::vec2(ui.available_size().x, height);
    let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 4.0, egui::Color32::from_rgb(15, 16, 18));
    let max = wf.iter().cloned().fold(0f32, f32::max);
    if wf.is_empty() || max <= 0.0 {
        return;
    }
    const NEG_MS: f64 = 25.0;
    const POS_MS: f64 = 10.0;
    let sr = crate::SAMPLE_RATE as f64;
    let period_s = period / sr;
    let w = rect.width();
    let cy = rect.center().y;
    let x_of = |t_ms: f64| rect.left() + ((t_ms + NEG_MS) / (NEG_MS + POS_MS) * w as f64) as f32;

    // Time graticule: 1 ms ticks under the axis, red + numbered every 5 ms.
    let minor = egui::Color32::from_rgb(45, 85, 60);
    let major = egui::Color32::from_rgb(150, 70, 70);
    for i_ms in -(NEG_MS as i32)..(POS_MS as i32) {
        let x = x_of(i_ms as f64);
        let col = if i_ms % 5 == 0 { major } else { minor };
        painter.line_segment(
            [egui::pos2(x, cy), egui::pos2(x, rect.bottom() - 2.0)],
            egui::Stroke::new(1.0_f32, col),
        );
        if i_ms % 5 == 0 && i_ms != 0 {
            painter.text(
                egui::pos2(x + 2.0, rect.bottom() - 3.0),
                egui::Align2::LEFT_BOTTOM,
                format!("{i_ms}"),
                egui::FontId::monospace(9.0),
                egui::Color32::from_gray(120),
            );
        }
    }
    painter.text(
        egui::pos2(rect.right() - 4.0, rect.bottom() - 3.0),
        egui::Align2::RIGHT_BOTTOM,
        "ms",
        egui::FontId::monospace(9.0),
        egui::Color32::from_gray(120),
    );

    // Amplitude graticule: how long the balance takes to sweep 10°..350°,
    // ms mapped to the left of the tick; red + numbered every 50°.
    for amp in (10..360usize).step_by(10) {
        if (amp as f64 + amp as f64) < lift {
            continue;
        }
        let t_ms = period_s * (lift / (2.0 * amp as f64)).asin() / std::f64::consts::PI * 1000.0;
        if t_ms > NEG_MS {
            continue;
        }
        let x = x_of(-t_ms);
        let col = if amp % 50 == 0 { major } else { minor };
        painter.line_segment(
            [egui::pos2(x, rect.top() + 2.0), egui::pos2(x, cy)],
            egui::Stroke::new(1.0_f32, col),
        );
        if amp % 50 == 0 {
            painter.text(
                egui::pos2(x + 2.0, rect.top() + 3.0),
                egui::Align2::LEFT_TOP,
                format!("{amp}"),
                egui::FontId::monospace(9.0),
                egui::Color32::from_gray(120),
            );
        }
    }
    painter.text(
        egui::pos2(rect.right() - 4.0, rect.top() + 3.0),
        egui::Align2::RIGHT_TOP,
        "deg",
        egui::FontId::monospace(9.0),
        egui::Color32::from_gray(120),
    );

    // Mirrored envelope of the folded waveform, stroked and filled (tg's
    // draw_graph: peaks reach 40% of the chart height from the axis).
    let a = center as f64 - NEG_MS / 1000.0 * sr;
    let b = center as f64 + POS_MS / 1000.0 * sr;
    let n = wf.len() as f64;
    let mut pts: Vec<egui::Pos2> = Vec::with_capacity(w as usize * 2);
    for i in 0..w as u32 {
        let t = a + (b - a) * (i as f64 / w as f64);
        let j = t.rem_euclid(n) as usize % n as usize;
        let k = (wf[j] / max) as f64 * 0.4 * rect.height() as f64;
        pts.push(egui::pos2(rect.left() + i as f32, cy - k as f32));
    }
    for i in (0..w as u32).rev() {
        let t = a + (b - a) * (i as f64 / w as f64);
        let j = t.rem_euclid(n) as usize % n as usize;
        let k = (wf[j] / max) as f64 * 0.4 * rect.height() as f64;
        pts.push(egui::pos2(rect.left() + i as f32, cy + k as f32));
    }
    let fill = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 48);
    painter.add(egui::Shape::Path(egui::epaint::PathShape {
        points: pts,
        closed: true,
        fill,
        stroke: egui::Stroke::new(1.5_f32, color).into(),
    }));
    painter.text(
        egui::pos2(rect.left() + 6.0, rect.top() + 3.0),
        egui::Align2::LEFT_TOP,
        label,
        egui::FontId::monospace(9.0),
        color,
    );
}

fn main() -> anyhow::Result<()> {
    // Live capture (if built with the audio feature); a relay thread moves
    // samples into a shared ring buffer the worker reads.
    let ring: Option<Ring> = {
        #[cfg(feature = "audio")]
        {
            let (tx, rx) = mpsc::channel::<f32>();
            match audio::spawn_capture(tx) {
                Ok(()) => {
                    let ring: Ring = Arc::new(Mutex::new(MicBuf::default()));
                    let r = ring.clone();
                    std::thread::spawn(move || {
                        let cap = (BLOCK_SECONDS * SAMPLE_RATE as f64) as usize * 8;
                        while let Ok(s) = rx.recv() {
                            r.lock().unwrap_or_else(|e| e.into_inner()).push(s, cap);
                        }
                    });
                    Some(ring)
                }
                Err(e) => {
                    eprintln!("audio capture unavailable: {e}; using demo mode");
                    None
                }
            }
        }
        #[cfg(not(feature = "audio"))]
        {
            None
        }
    };

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 520.0])
            .with_title("regulator"),
        ..Default::default()
    };
    eframe::run_native(
        "regulator",
        native,
        Box::new(move |_cc| Ok(Box::new(RegulatorApp::new(ring)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
