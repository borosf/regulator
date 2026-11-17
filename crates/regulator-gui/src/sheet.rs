// SPDX-License-Identifier: GPL-2.0
//! Branded JPEG result sheet: renders the current readout and trace onto a
//! regulator-styled card and writes it next to the SVG/CSV exports.

use crate::{Params, TracePoint};
use image::{Rgb, RgbImage};
use regulator_core::TimingResult;
use std::sync::{Arc, Mutex};

const BG: Rgb<u8> = Rgb([10, 11, 12]);
const CARD: Rgb<u8> = Rgb([16, 18, 20]);
const FG: Rgb<u8> = Rgb([238, 238, 238]);
const MUTED: Rgb<u8> = Rgb([120, 124, 128]);
const GRID: Rgb<u8> = Rgb([40, 42, 46]);
const GRID0: Rgb<u8> = Rgb([70, 74, 78]);
const GREEN: Rgb<u8> = Rgb([140, 230, 140]);
const MAGENTA: Rgb<u8> = Rgb([240, 140, 240]);
const ACCENT: Rgb<u8> = Rgb([235, 200, 90]);
const RED: Rgb<u8> = Rgb([235, 120, 100]);

/// 5x7 bitmap font, one byte per row, bit 4 = leftmost column.
fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '+' => [0x00, 0x04, 0x04, 0x1F, 0x04, 0x04, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        ',' => [0x00, 0x00, 0x00, 0x00, 0x0C, 0x04, 0x08],
        '/' => [0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '\u{00b0}' => [0x06, 0x09, 0x09, 0x06, 0x00, 0x00, 0x00],
        _ => [0; 7],
    }
}

fn text_width(s: &str, scale: u32) -> u32 {
    s.chars().count() as u32 * 6 * scale
}

fn draw_text(img: &mut RgbImage, x: i64, y: i64, s: &str, scale: u32, color: Rgb<u8>) {
    let mut cx = x;
    for c in s.chars() {
        if c != ' ' {
            let g = glyph(c);
            for (row, bits) in g.iter().enumerate() {
                for col in 0..5 {
                    if bits & (1 << (4 - col)) != 0 {
                        for dy in 0..scale as i64 {
                            for dx in 0..scale as i64 {
                                let px = cx + col as i64 * scale as i64 + dx;
                                let py = y + row as i64 * scale as i64 + dy;
                                if px >= 0 && py >= 0 {
                                    img.put_pixel(px as u32, py as u32, color);
                                }
                            }
                        }
                    }
                }
            }
        }
        cx += 6 * scale as i64;
    }
}

fn rect_filled(img: &mut RgbImage, x: u32, y: u32, w: u32, h: u32, color: Rgb<u8>) {
    for yy in y..y.saturating_add(h) {
        for xx in x..x.saturating_add(w) {
            if (xx as usize) < img.width() as usize && (yy as usize) < img.height() as usize {
                img.put_pixel(xx, yy, color);
            }
        }
    }
}

fn hline(img: &mut RgbImage, x0: u32, x1: u32, y: u32, color: Rgb<u8>) {
    for x in x0..=x1 {
        if y < img.height() && x < img.width() {
            img.put_pixel(x, y, color);
        }
    }
}

fn vline(img: &mut RgbImage, x: u32, y0: u32, y1: u32, color: Rgb<u8>) {
    for y in y0..=y1 {
        if x < img.width() && y < img.height() {
            img.put_pixel(x, y, color);
        }
    }
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

const W: u32 = 1200;
const H: u32 = 800;
const WINDOW_MS: f64 = 25.0;
const SPAN_S: f64 = 60.0;

/// Render the result sheet and write `<base>.jpg`. Returns the path on
/// success.
pub fn export_jpeg(
    base: &str,
    trace: &[TracePoint],
    result: &Option<TimingResult>,
    params: &Arc<Mutex<Params>>,
) -> Option<String> {
    let p = params.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut img = RgbImage::from_pixel(W, H, BG);

    // Frame: a slightly raised card inset in the dark page.
    let m = 24u32;
    rect_filled(&mut img, m, m, W - 2 * m, H - 2 * m, CARD);
    vline(&mut img, m, m, H - m, GRID0);
    vline(&mut img, W - m, m, H - m, GRID0);
    hline(&mut img, m, W - m, m, GRID0);
    hline(&mut img, m, W - m, H - m, GRID0);

    // Header.
    draw_text(&mut img, 56, 52, "REGULATOR", 4, FG);
    draw_text(&mut img, 56, 92, "WATCH TIMING RESULT SHEET", 1, MUTED);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let secs = now.as_secs();
    let (y, mo, d) = civil_from_days((secs / 86400) as i64);
    let stamp = format!(
        "{y:04}/{mo:02}/{d:02} {:02}:{:02} UTC",
        secs % 86400 / 3600,
        secs % 3600 / 60
    );
    let sw = text_width(&stamp, 1);
    draw_text(&mut img, (W - 56 - sw) as i64, 60, &stamp, 1, MUTED);
    hline(&mut img, m + 1, W - m - 1, 118, GRID);

    // Readouts.
    let fmt = |v: Option<f64>, spec: &dyn Fn(f64) -> String| {
        v.map(spec).unwrap_or_else(|| "---".into())
    };
    let (rate, amp, be) = match result {
        Some(r) => (
            fmt(r.rate_sday, &|v: f64| format!("{v:+.1}")),
            fmt(r.amplitude_deg, &|v: f64| format!("{v:.0} DEG")),
            fmt(r.beat_error_ms, &|v: f64| format!("{v:+.2} MS")),
        ),
        None => ("---".into(), "---".into(), "---".into()),
    };
    let rate_color = match result.as_ref().and_then(|r| r.rate_sday) {
        Some(v) if v.abs() < 5.0 => GREEN,
        Some(v) if v.abs() < 15.0 => ACCENT,
        Some(_) => RED,
        None => MUTED,
    };
    draw_text(&mut img, 56, 150, "RATE S/DAY", 1, MUTED);
    draw_text(&mut img, 56, 172, &rate, 8, rate_color);
    let col_x = [520, 760, 900, 1020];
    let labels = ["AMPLITUDE", "BEAT ERROR", "LIFT ANGLE", "BPH"];
    let bph_s = match p.bph {
        0 => "AUTO".to_string(),
        b => b.to_string(),
    };
    let values = [
        amp,
        be,
        format!("{:.0} DEG", p.lift_angle),
        bph_s,
    ];
    for (i, &x) in col_x.iter().enumerate() {
        draw_text(&mut img, x as i64, 150, labels[i], 1, MUTED);
        draw_text(&mut img, x as i64, 168, &values[i], 2, FG);
    }
    if p.calibration != 0.0 {
        let tag = if p.cal_auto { " AUTO" } else { "" };
        draw_text(
            &mut img,
            56,
            252,
            &format!("CALIBRATION {:+.1} S/D{}", p.calibration, tag),
            1,
            MUTED,
        );
    }
    hline(&mut img, m + 1, W - m - 1, 286, GRID);

    // Chart area: horizontal time axis, vertical phase error.
    let cx0 = 56u32;
    let cx1 = W - 56;
    let cy0 = 310u32;
    let cy1 = H - 90;
    let cy = (cy0 + cy1) / 2;
    let half_h = (cy1 - cy0) as f64 / 2.0;
    let wrap = |ms: f64| {
        ((ms + WINDOW_MS) % (2.0 * WINDOW_MS) + 2.0 * WINDOW_MS) % (2.0 * WINDOW_MS) - WINDOW_MS
    };
    let to_y = |ms: f64| cy as f64 - wrap(ms) / WINDOW_MS * half_h;
    for ms in [-20.0, -10.0, 0.0, 10.0, 20.0] {
        let y = to_y(ms) as u32;
        let color = if ms == 0.0 { GRID0 } else { GRID };
        hline(&mut img, cx0, cx1, y, color);
        let lab = format!("{ms:+.0}");
        draw_text(&mut img, cx0 as i64 - 8, y as i64 + 3, &lab, 1, MUTED);
    }
    for frac in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let x = cx0 as f64 + frac * (cx1 - cx0) as f64;
        vline(&mut img, x as u32, cy0, cy1, GRID);
        let lab = format!("-{:.0}S", (1.0 - frac) * SPAN_S);
        draw_text(&mut img, x as i64 - 6, cy1 as i64 + 8, &lab, 1, MUTED);
    }
    let t_end = trace.last().map(|t| t.t).unwrap_or(0.0);
    for t in trace.iter().filter(|t| t_end - t.t <= SPAN_S) {
        let x = cx1 as f64 - (t_end - t.t) / SPAN_S * (cx1 - cx0) as f64;
        let y = to_y(t.phase_ms);
        let color = if t.tic { GREEN } else { MAGENTA };
        for dy in -1i64..=1 {
            let py = y as i64 + dy;
            if py >= 0 && (py as u32) < H {
                img.put_pixel(x as u32, py as u32, color);
            }
        }
    }

    // Footer.
    hline(&mut img, m + 1, W - m - 1, H - 56, GRID);
    draw_text(&mut img, 56, (H - 44) as i64, "REGULATOR - GPL-2.0 - GITHUB.COM/BOROSF/REGULATOR", 1, MUTED);
    draw_text(&mut img, (W - 56 - text_width("TIC GREEN / TOC MAGENTA", 1)) as i64, (H - 44) as i64, "TIC GREEN / TOC MAGENTA", 1, MUTED);

    let path = format!("{base}.jpg");
    img.save_with_format(&path, image::ImageFormat::Jpeg).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_result_sheet() {
        let trace: Vec<TracePoint> = (0..600)
            .map(|i| TracePoint {
                t: i as f64 * 0.1,
                phase_ms: (i as f64 * 0.05).sin() * 3.0,
                tic: i % 2 == 0,
            })
            .collect();
        let params = Arc::new(Mutex::new(Params::default()));
        let dir = std::env::temp_dir().join("regulator-sheet-test");
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("sheet").to_string_lossy().into_owned();
        let path = export_jpeg(&base, &trace, &None, &params).expect("export");
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() > 10_000, "jpeg too small: {}", meta.len());
        std::fs::remove_file(&path).unwrap();
    }
}
