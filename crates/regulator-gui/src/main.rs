// SPDX-License-Identifier: GPL-2.0
//! regulator-gui: graphical watch timing machine built on the regulator
//! core, using egui for an immediate-mode, cross-platform interface.

#[cfg(feature = "audio")]
mod audio;
mod chart;
mod sheet;

use eframe::egui;
use regulator_core::{CalStatus, CalibrationState, ClockCal, Analyzer, SyntheticWatch, TimingResult};
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
}

const SAMPLE_RATE: u32 = 44100;
const BLOCK_SECONDS: f64 = 4.0;

#[derive(Clone)]
struct Params {
    bph: u32,
    lift_angle: f64,
    noise_suppressor: bool,
    demo: bool,
    calibrating: bool,
    calibration: f64,
    /// Calibration came from the passive clock estimate, not a quartz run.
    cal_auto: bool,
    /// Live clock-calibration estimate and uncertainty while converging.
    cal_live: Option<(f64, f64)>,
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
            demo: true,
            calibrating: false,
            calibration: 0.0,
            cal_auto: false,
            cal_live: None,
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
}

#[cfg(feature = "audio")]
impl MicBuf {
    fn push(&mut self, s: f32, cap: usize) {
        self.data.push(s);
        self.total += 1;
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
    params: Arc<Mutex<Params>>,
    rx: mpsc::Receiver<Feed>,
}

impl RegulatorApp {
    fn new(ring: Option<Ring>, clock_cal: Arc<Mutex<ClockCal>>) -> Self {
        let params = Arc::new(Mutex::new(Params::load()));
        let (tx, rx) = mpsc::channel::<Feed>();
        let params_arc = params.clone();
        let clock_cal = clock_cal;
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
            let mut start = 0usize;
            let mut win_start_s = 0.0f64;
            let mut demo_total_s = 0.0f64;
            let mut last_event_s = 0.0f64;
            let mut trace: Vec<TracePoint> = Vec::new();
            let mut ts: u64 = 0;
            let mut last_len = 0usize;
            let mut cal_state = CalibrationState::new();
            let mut cal_cooldown = 0u32;
            let mut analyzer = Analyzer::new(SAMPLE_RATE);
            loop {
                let mut p = params_arc.lock().unwrap_or_else(|e| e.into_inner());
                if p.calibrating {
                    if let Some(ring) = &ring {
                        let buf = ring.lock().unwrap_or_else(|e| e.into_inner());
                        ts += buf.total.saturating_sub(last_len as u64) as u64;
                        last_len = buf.total as usize;
                        if buf.data.len() >= block && cal_cooldown == 0 {
                            let cal_an = Analyzer::for_calibration(SAMPLE_RATE);
                            let window_start_ts = ts.saturating_sub(buf.data.len() as u64);
                            match cal_state.add_block(&cal_an, &buf.data[buf.data.len() - block..], window_start_ts) {
                                CalStatus::Done(cal) => {
                                    p.calibration = cal;
                                    p.calibrating = false;
                                    p.cal_auto = true;
                                    p.dirty = true;
                                    p.cal_progress = format!("{cal:+.1} s/d");
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
                    let _ = tx.send(Feed { result: None, trace: Vec::new(), cal: Some(view) });
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                // Passive clock calibration: apply once the sample-count
                // regression has converged (unless a quartz calibration
                // has set the value manually).
                let cc_est = clock_cal.lock().unwrap_or_else(|e| e.into_inner()).estimate();
                if let Some((err, unc)) = cc_est {
                    // `p` is already held here; re-locking params would
                    // deadlock (std Mutex is not reentrant). The clock
                    // estimate is informational only: it measures the
                    // sound card against the local timer, whose own
                    // drift is unknown (tens of s/day on some hardware),
                    // so it is never auto-applied. The quartz calibration
                    // is the authoritative correction.
                    p.cal_live = Some((err, unc));
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
                let res = if p2.demo {
                    let end = (start + block).min(demo_samples.len());
                    let r = analyzer.analyze(&demo_samples[start..end]);
                    win_start_s = demo_total_s;
                    demo_total_s += (end - start) as f64 / SAMPLE_RATE as f64;
                    if end == demo_samples.len() {
                        start = 0;
                    } else {
                        start = end;
                    }
                    r
                } else if let Some(ring) = &ring {
                    // Copy the window out under the lock (a fraction of a
                    // millisecond), then analyze without holding it so the
                    // capture relay never stalls.
                    let window = {
                        let buf = ring.lock().unwrap_or_else(|e| e.into_inner());
                        if buf.data.len() >= block {
                            Some((buf.total, buf.data[buf.data.len() - block..].to_vec()))
                        } else {
                            None
                        }
                    };
                    match window {
                        Some((total, w)) => {
                            win_start_s = (total - block as u64) as f64 / SAMPLE_RATE as f64;
                            analyzer.analyze(&w)
                        }
                        None => None,
                    }
                } else {
                    None
                };
                if let Some(r) = &res {
                    let p0 = 7200.0 * SAMPLE_RATE as f64 / r.bph as f64;
                    for (ev, is_tic) in &r.events {
                        let t = win_start_s + ev / SAMPLE_RATE as f64;
                        if t <= last_event_s {
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
                    while let Some(first) = trace.first() {
                        if first.t < win_start_s - 120.0 {
                            trace.remove(0);
                        } else {
                            break;
                        }
                    }
                }
                let _ = tx.send(Feed {
                    result: res,
                    trace: trace.clone(),
                    cal: None,
                });
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        Self {
            result: None,
            trace: Vec::new(),
            cal: None,
            last_export: None,
            params,
            rx,
        }
    }
}

impl eframe::App for RegulatorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(feed) = self.rx.try_recv() {
            self.result = feed.result;
            self.trace = feed.trace;
            if feed.cal.is_some() {
                self.cal = feed.cal;
            } else {
                self.cal = None;
            }
        }
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(14, 16, 18);
        visuals.window_fill = egui::Color32::from_rgb(14, 16, 18);
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let p = self.params.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    if ui.button("export").clicked() {
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
                    ui.label(
                        egui::RichText::new(format!(
                            "{}  {}",
                            if p.demo { "demo" } else { "mic" },
                            match p.bph {
                                0 => "auto".to_string(),
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
                } else if let Some((err, unc)) = p.cal_live {
                    ui.label(
                        egui::RichText::new(format!("clock {err:+.1} ±{unc:.1}"))
                            .monospace()
                            .size(14.0)
                            .color(egui::Color32::from_gray(140)),
                    );
                    ui.add_space(12.0);
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
                let before = (p.bph, p.lift_angle, p.calibration, p.noise_suppressor, p.demo);
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
                ui.checkbox(&mut p.demo, "demo signal");
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
                    p.demo = false;
                    p.calibrating = true;
                    p.cal_progress.clear();
                }
                ui.add_space(10.0);
                let after = (p.bph, p.lift_angle, p.calibration, p.noise_suppressor, p.demo);
                if after != before {
                    p.dirty = true;
                }
                ui.separator();
                ui.add_space(6.0);
                ui.label(egui::RichText::new("WAVEFORM").small().weak());
                draw_waveform(ui, &self.result, 90.0);
            });

        // Center: the strip chart, or live calibration feedback while
        // calibrating.
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(egui::Color32::from_rgb(10, 11, 12)))
            .show(ctx, |ui| {
                if let Some(cal) = &self.cal {
                    chart::draw_cal_view(ui, cal);
                } else {
                    draw_trace(ui, &self.trace);
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
fn draw_trace(ui: &mut egui::Ui, trace: &[TracePoint]) {
    let (resp, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(10, 11, 12));
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
        painter.line_segment(
            [egui::pos2(x, y - 1.5), egui::pos2(x, y + 1.5)],
            egui::Stroke::new(1.6_f32, color),
        );
    }
}

fn draw_waveform(ui: &mut egui::Ui, result: &Option<TimingResult>, height: f32) {
    let size = egui::vec2(ui.available_size().x, height);
    let (resp, painter) = ui.allocate_painter(size, egui::Sense::hover());
    painter.rect_filled(resp.rect, 4.0, egui::Color32::from_gray(16));
    let Some(r) = result else { return };
    let wf = &r.waveform;
    if wf.is_empty() {
        return;
    }
    let max = wf.iter().cloned().fold(0f32, f32::max).max(1e-9);
    let rect = resp.rect;
    let n = wf.len();
    let mid = rect.center().y;
    let to_screen = |i: usize, v: f32| {
        egui::pos2(
            rect.left() + (i as f32 / n as f32) * rect.width(),
            mid - (v / max) * (rect.height() / 2.0 - 4.0),
        )
    };
    let points: Vec<egui::Pos2> = wf.iter().enumerate().map(|(i, &v)| to_screen(i, v)).collect();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(1.5_f32, egui::Color32::LIGHT_GREEN),
    ));
    if let Some(tic) = r.tic {
        let x = to_screen(tic, 0.0).x;
        painter.line_segment(
            [egui::pos2(x, rect.top() + 2.0), egui::pos2(x, rect.bottom() - 2.0)],
            egui::Stroke::new(1.0_f32, egui::Color32::YELLOW),
        );
    }
}

fn main() -> anyhow::Result<()> {
    let clock_cal: Arc<Mutex<ClockCal>> = Arc::new(Mutex::new(ClockCal::new(SAMPLE_RATE)));

    // Live capture (if built with the audio feature); a relay thread moves
    // samples into a shared ring buffer the worker reads, feeding the
    // clock calibration as it goes.
    let ring: Option<Ring> = {
        #[cfg(feature = "audio")]
        {
            let (tx, rx) = mpsc::channel::<f32>();
            match audio::spawn_capture(tx) {
                Ok(()) => {
                    let ring: Ring = Arc::new(Mutex::new(MicBuf { data: Vec::new(), total: 0 }));
                    let r = ring.clone();
                    let cc = clock_cal.clone();
                    std::thread::spawn(move || {
                        let cap = (BLOCK_SECONDS * SAMPLE_RATE as f64) as usize * 2;
                        let mut batch = 0usize;
                        while let Ok(s) = rx.recv() {
                            r.lock().unwrap_or_else(|e| e.into_inner()).push(s, cap);
                            batch += 1;
                            if batch >= 2048 {
                                cc.lock().unwrap_or_else(|e| e.into_inner()).add(batch);
                                batch = 0;
                            }
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
        Box::new(move |_cc| Ok(Box::new(RegulatorApp::new(ring, clock_cal)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
