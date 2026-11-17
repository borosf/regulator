// SPDX-License-Identifier: GPL-2.0
//! Calibration feedback view and chart export.

use crate::{CalView, Params, TracePoint};
use eframe::egui;
use std::sync::{Arc, Mutex};

/// Live calibration feedback: the collected (time, phase) samples, progress,
/// and the current estimate with its standard error.
pub fn draw_cal_view(ui: &mut egui::Ui, cal: &CalView) {
    ui.vertical(|ui| {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("CALIBRATION")
                .monospace()
                .size(18.0)
                .strong(),
        );
        ui.label(
            egui::RichText::new("place a quartz watch (1 tick/s) near the microphone")
                .monospace()
                .size(12.0)
                .color(egui::Color32::GRAY),
        );
        ui.label(
            egui::RichText::new(format!("samples {}/{}", cal.have, cal.need))
                .monospace()
                .size(13.0),
        );
        if let Some((est, err)) = cal.estimate {
            ui.label(
                egui::RichText::new(format!(
                    "estimate {est:+.2} ± {err:.2} s/d (done under ±0.10)"
                ))
                .monospace()
                .size(13.0)
                .color(egui::Color32::from_rgb(235, 200, 90)),
            );
        }
        if cal.have == 0 {
            ui.label(
                egui::RichText::new("waiting for ticks…")
                    .monospace()
                    .size(13.0)
                    .color(egui::Color32::from_gray(120)),
            );
        }
        ui.add_space(4.0);
    });
    let (resp, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::hover());
    let rect = resp.rect;
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(10, 11, 12));
    if cal.points.is_empty() {
        return;
    }
    let t_max = cal.points.last().map(|p| p.0).unwrap_or(1.0).max(1.0);
    let to_x = |t: f64| rect.left() + (t / t_max * (rect.width() - 12.0) as f64) as f32 + 6.0;
    let to_y = |ph: f64| {
        let k = ph.clamp(0.0, 1.0) as f32;
        rect.bottom() - k * (rect.height() - 12.0) - 6.0
    };
    for y in [to_y(0.0), to_y(1.0)] {
        painter.line_segment(
            [egui::pos2(rect.left() + 6.0, y), egui::pos2(rect.right() - 6.0, y)],
            egui::Stroke::new(1.0_f32, egui::Color32::from_gray(40)),
        );
    }
    for &(t, ph) in &cal.points {
        painter.circle_filled(
            egui::pos2(to_x(t), to_y(ph)),
            2.0_f32,
            egui::Color32::from_rgb(140, 230, 140),
        );
    }
}

/// Export the current strip chart as an SVG file plus the raw points as CSV.
/// Returns the base path written.
pub fn export_chart(
    trace: &[TracePoint],
    result: &Option<crate::TimingResult>,
    params: &Arc<Mutex<Params>>,
) -> Option<String> {
    use std::fmt::Write as _;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let secs = now.as_secs();
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    let base = format!("regulator-chart-{d}-{h:02}{m:02}");
    let p = params.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let fmt_or = |v: Option<f64>, spec: &dyn Fn(f64) -> String| {
        v.map(spec).unwrap_or_else(|| "---".into())
    };
    let (rate, amp, be) = match result {
        Some(r) => (
            fmt_or(r.rate_sday, &|v| format!("{v:+.1}")),
            fmt_or(r.amplitude_deg, &|v| format!("{v:.0}")),
            fmt_or(r.beat_error_ms, &|v| format!("{v:+.2}")),
        ),
        None => ("---".into(), "---".into(), "---".into()),
    };
    let bph = match p.bph {
        0 => "auto".to_string(),
        b => b.to_string(),
    };

    let mut csv = String::from("time_s,phase_ms,tic\n");
    for t in trace {
        let _ = writeln!(csv, "{:.2},{:.3},{}", t.t, t.phase_ms, t.tic);
    }
    std::fs::write(format!("{base}.csv"), csv).ok()?;

    const W: f64 = 800.0;
    const H: f64 = 600.0;
    const WINDOW_MS: f64 = 25.0;
    let top = 60.0;
    let chart_h = H - top - 10.0;
    let cy = top + chart_h / 2.0;
    let half_h = chart_h / 2.0;
    let wrap = |ms: f64| {
        ((ms + WINDOW_MS) % (2.0 * WINDOW_MS) + 2.0 * WINDOW_MS) % (2.0 * WINDOW_MS) - WINDOW_MS
    };
    let mut svg = String::new();
    let _ = writeln!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{W}\" height=\"{H}\">"
    );
    let _ = writeln!(svg, "<rect width=\"{W}\" height=\"{H}\" fill=\"#0a0b0c\"/>");
    let _ = writeln!(
        svg,
        "<text x=\"12\" y=\"28\" fill=\"#eee\" font-family=\"monospace\" font-size=\"20\">rate {rate} s/d</text>"
    );
    let _ = writeln!(
        svg,
        "<text x=\"12\" y=\"48\" fill=\"#888\" font-family=\"monospace\" font-size=\"13\">amp {amp} deg   be {be} ms   bph {bph}   lift {:.0} deg</text>",
        p.lift_angle
    );
    for ms in [-20.0, -10.0, 0.0, 10.0, 20.0] {
        let y = cy - wrap(ms) / WINDOW_MS * half_h;
        let color = if ms == 0.0 { "#3a3a3a" } else { "#1c1c1c" };
        let _ = writeln!(
            svg,
            "<line x1=\"12\" y1=\"{y:.1}\" x2=\"{}\" y2=\"{y:.1}\" stroke=\"{color}\"/>",
            W - 12.0
        );
    }
    let t_end = trace.last().map(|t| t.t).unwrap_or(0.0);
    for t in trace.iter().filter(|t| t_end - t.t <= 60.0) {
        let x = W - 12.0 - (t_end - t.t) / 60.0 * (W - 24.0);
        let y = cy - wrap(t.phase_ms) / WINDOW_MS * half_h;
        let color = if t.tic { "#8ce68c" } else { "#f08cf0" };
        let _ = writeln!(
            svg,
            "<line x1=\"{x:.1}\" y1=\"{y:.1}\" x2=\"{x:.1}\" y2=\"{y:.1}\" stroke=\"{color}\" stroke-width=\"1.6\"/>"
        );
    }
    let _ = writeln!(svg, "</svg>");
    std::fs::write(format!("{base}.svg"), svg).ok()?;
    crate::sheet::export_jpeg(&base, trace, result, params)?;
    Some(base)
}
