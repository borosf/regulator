// SPDX-License-Identifier: GPL-2.0
//! regulator-gui: graphical watch timing machine built on the regulator
//! core, using egui for an immediate-mode, cross-platform interface.

#[cfg(feature = "audio")]
mod audio;

use eframe::egui;
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

/// What the worker sends to the UI each iteration.
struct Feed {
    result: Option<TimingResult>,
    trace: Vec<TracePoint>,
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
    cal_progress: String,
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
            cal_progress: String::new(),
        }
    }
}

/// Shared ring buffer of live microphone samples (if capture is available).
type Ring = Arc<Mutex<Vec<f32>>>;

struct RegulatorApp {
    result: Option<TimingResult>,
    trace: Vec<TracePoint>,
    params: Arc<Mutex<Params>>,
    rx: mpsc::Receiver<Feed>,
}

impl RegulatorApp {
    fn new(ring: Option<Ring>) -> Self {
        let params = Arc::new(Mutex::new(Params::default()));
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
            let mut start = 0usize;
            let mut win_start_s = 0.0f64;
            let mut last_event_s = 0.0f64;
            let mut trace: Vec<TracePoint> = Vec::new();
            let mut ts: u64 = 0;
            let mut last_len = 0usize;
            let mut cal_state = CalibrationState::new();
            let mut cal_cooldown = 0u32;
            loop {
                let mut p = params_arc.lock().unwrap_or_else(|e| e.into_inner());
                if p.calibrating {
                    if let Some(ring) = &ring {
                        let buf = ring.lock().unwrap_or_else(|e| e.into_inner());
                        ts += buf.len().saturating_sub(last_len) as u64;
                        last_len = buf.len();
                        if buf.len() >= block && cal_cooldown == 0 {
                            let cal_an = Analyzer::for_calibration(SAMPLE_RATE);
                            let window_start_ts = ts.saturating_sub(buf.len() as u64);
                            match cal_state.add_block(&cal_an, &buf[buf.len() - block..], window_start_ts) {
                                CalStatus::Done(cal) => {
                                    p.calibration = cal;
                                    p.calibrating = false;
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
                    drop(p);
                    let _ = tx.send(Feed { result: None, trace: trace.clone() });
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                drop(p);
                let p2 = params_arc.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let mut analyzer = Analyzer::new(SAMPLE_RATE);
                analyzer.bph = p2.bph;
                analyzer.lift_angle = p2.lift_angle;
                analyzer.noise_suppressor = p2.noise_suppressor;
                analyzer.calibration = p2.calibration;
                let res = if p2.demo {
                    let end = (start + block).min(demo_samples.len());
                    let r = analyzer.analyze(&demo_samples[start..end]);
                    win_start_s = start as f64 / SAMPLE_RATE as f64;
                    if end == demo_samples.len() {
                        start = 0;
                    } else {
                        start = end;
                    }
                    r
                } else if let Some(ring) = &ring {
                    let buf = ring.lock().unwrap_or_else(|e| e.into_inner());
                    if buf.len() >= block {
                        win_start_s = (buf.len() - block) as f64 / SAMPLE_RATE as f64;
                        analyzer.analyze(&buf[buf.len() - block..])
                    } else {
                        None
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
                });
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        Self {
            result: None,
            trace: Vec::new(),
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
        }

        egui::SidePanel::right("settings")
            .min_width(220.0)
            .show(ctx, |ui| {
                let mut p = self.params.lock().unwrap_or_else(|e| e.into_inner());
                ui.heading("Settings");
                ui.separator();
                ui.label("Beat rate");
                egui::ComboBox::from_id_salt("bph")
                    .selected_text(if p.bph == 0 { "auto".into() } else { p.bph.to_string() })
                    .show_ui(ui, |ui| {
                        for b in [0u32, 18000, 21600, 25200, 28800, 36000] {
                            ui.selectable_value(
                                &mut p.bph,
                                b,
                                if b == 0 { "auto".into() } else { b.to_string() },
                            );
                        }
                    });
                ui.add(
                    egui::Slider::new(&mut p.lift_angle, 40.0..=60.0).text("lift angle (°)"),
                );
                ui.checkbox(&mut p.noise_suppressor, "noise suppressor");
                ui.separator();
                if p.demo {
                    ui.label("source: demo signal");
                } else {
                    ui.label("source: microphone");
                }
                ui.checkbox(&mut p.demo, "demo mode");
                ui.separator();
                if p.calibrating {
                    ui.label(format!("calibrating: {} (quartz watch, 1 tick/s; refits every second, stops when confident)", p.cal_progress));
                    if ui.button("stop calibration").clicked() {
                        p.calibrating = false;
                    }
                } else {
                    if ui.button("calibrate (quartz, exits early)").clicked() {
                        p.calibrating = !p.demo; // calibration needs the mic
                        p.cal_progress.clear();
                    }
                    if p.calibration != 0.0 {
                        ui.label(format!("calibration: {:+.1} s/d", p.calibration));
                    }
                }
                ui.separator();
                ui.small("regulator — GPLv2\na fork of tg");
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                big_metric(
                    ui,
                    "Rate",
                    &self.result.as_ref().and_then(|r| r.rate_sday),
                    "s/day",
                    1,
                );
                big_metric(
                    ui,
                    "Amplitude",
                    &self.result.as_ref().and_then(|r| r.amplitude_deg),
                    "°",
                    0,
                );
                big_metric(
                    ui,
                    "Beat error",
                    &self.result.as_ref().and_then(|r| r.beat_error_ms),
                    "ms",
                    2,
                );
            });
            ui.separator();
            let wave_h = ui.available_size().y * 0.42;
            draw_waveform(ui, &self.result, wave_h);
            ui.separator();
            ui.label("Trace");
            draw_trace(ui, &self.trace);
        });

        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

/// The tick-event trace: each event plotted at its phase error over time.
/// A rate error shows as drifting diagonal bands, beat error as two
/// separated bands, noise as scatter - the classic timing-machine display.
fn draw_trace(ui: &mut egui::Ui, trace: &[TracePoint]) {
    let (resp, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::hover());
    painter.rect_filled(resp.rect, 4.0, egui::Color32::from_gray(16));
    if trace.is_empty() {
        return;
    }
    let rect = resp.rect;
    let t_end = trace.last().map(|p| p.t).unwrap_or(0.0);
    let t_start = (t_end - 60.0).max(0.0);
    // Vertical scale: half the beat period, so tic and toc bands both fit.
    let half_ms = 1000.0 * 3600.0 / 14400.0;
    let to_x = |t: f64| rect.left() + (((t - t_start) / 60.0).clamp(0.0, 1.0) * rect.width() as f64) as f32;
    let to_y = |ms: f64| {
        let k = (ms / half_ms).clamp(-1.0, 1.0) as f32;
        rect.center().y - k * (rect.height() / 2.0 - 3.0)
    };
    // Center line: the nominal beat grid.
    painter.line_segment(
        [rect.left_top() + egui::vec2(0.0, rect.height() / 2.0), rect.right_top() + egui::vec2(0.0, rect.height() / 2.0)],
        egui::Stroke::new(1.0_f32, egui::Color32::from_gray(48)),
    );
    for p in trace.iter().filter(|p| p.t >= t_start) {
        let color = if p.tic {
            egui::Color32::LIGHT_GREEN
        } else {
            egui::Color32::from_rgb(255, 120, 255)
        };
        painter.circle_filled(egui::pos2(to_x(p.t), to_y(p.phase_ms)), 1.5_f32, color);
    }
}

fn big_metric(ui: &mut egui::Ui, name: &str, value: &Option<f64>, unit: &str, decimals: usize) {
    let text = match value {
        Some(v) => format!("{:+.width$}", v, width = decimals),
        None => "---".into(),
    };
    ui.vertical(|ui| {
        ui.heading(egui::RichText::new(text).size(34.0));
        ui.label(format!("{name} ({unit})"));
    });
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
    // Live capture (if built with the audio feature); a relay thread moves
    // samples into a shared ring buffer the worker reads.
    let ring: Option<Ring> = {
        #[cfg(feature = "audio")]
        {
            let (tx, rx) = mpsc::channel::<f32>();
            match audio::spawn_capture(tx) {
                Ok(()) => {
                    let ring: Ring = Arc::new(Mutex::new(Vec::new()));
                    let r = ring.clone();
                    std::thread::spawn(move || {
                        let block = (BLOCK_SECONDS * SAMPLE_RATE as f64) as usize;
                        while let Ok(s) = rx.recv() {
                            let mut buf = r.lock().unwrap_or_else(|e| e.into_inner());
                            buf.push(s);
                            if buf.len() > block * 2 {
                                buf.drain(..block);
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
        Box::new(move |_cc| Ok(Box::new(RegulatorApp::new(ring)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
