// SPDX-License-Identifier: GPL-2.0
//! regulator-gui: graphical watch timing machine built on the regulator
//! core, using egui for an immediate-mode, cross-platform interface.

#[cfg(feature = "audio")]
mod audio;

use eframe::egui;
use regulator_core::{Analyzer, SyntheticWatch, TimingResult};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SAMPLE_RATE: u32 = 44100;
const BLOCK_SECONDS: f64 = 4.0;

#[derive(Clone)]
struct Params {
    bph: u32,
    lift_angle: f64,
    noise_suppressor: bool,
    demo: bool,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            bph: 0,
            lift_angle: 52.0,
            noise_suppressor: true,
            demo: true,
        }
    }
}

/// Shared ring buffer of live microphone samples (if capture is available).
type Ring = Arc<Mutex<Vec<f32>>>;

struct RegulatorApp {
    result: Option<TimingResult>,
    params: Arc<Mutex<Params>>,
    rx: mpsc::Receiver<Option<TimingResult>>,
}

impl RegulatorApp {
    fn new(ring: Option<Ring>) -> Self {
        let params = Arc::new(Mutex::new(Params::default()));
        let (tx, rx) = mpsc::channel::<Option<TimingResult>>();
        let p = params.clone();
        std::thread::spawn(move || {
            let watch = SyntheticWatch {
                bph: 28800,
                rate_sday: 9.0,
                beat_error_ms: 0.6,
                amplitude_deg: 275.0,
            };
            let demo_samples = watch.generate(SAMPLE_RATE, 120.0);
            let block = (BLOCK_SECONDS * SAMPLE_RATE as f64) as usize;
            let mut start = 0usize;
            loop {
                let p = p.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let mut analyzer = Analyzer::new(SAMPLE_RATE);
                analyzer.bph = p.bph;
                analyzer.lift_angle = p.lift_angle;
                analyzer.noise_suppressor = p.noise_suppressor;
                let res = if p.demo {
                    let end = (start + block).min(demo_samples.len());
                    let r = analyzer.analyze(&demo_samples[start..end]);
                    if end == demo_samples.len() {
                        start = 0;
                    } else {
                        start = end;
                    }
                    r
                } else if let Some(ring) = &ring {
                    let buf = ring.lock().unwrap_or_else(|e| e.into_inner());
                    if buf.len() >= block {
                        analyzer.analyze(&buf[buf.len() - block..])
                    } else {
                        None
                    }
                } else {
                    None
                };
                let _ = tx.send(res);
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        Self {
            result: None,
            params,
            rx,
        }
    }
}

impl eframe::App for RegulatorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(r) = self.rx.try_recv() {
            self.result = r;
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
            draw_waveform(ui, &self.result);
        });

        ctx.request_repaint_after(Duration::from_millis(200));
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

fn draw_waveform(ui: &mut egui::Ui, result: &Option<TimingResult>) {
    let (resp, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::hover());
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
