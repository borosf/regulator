// SPDX-License-Identifier: GPL-2.0
//! The ratatui interface: a clean three-zone layout.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use regulator_core::TimingResult;

/// UI-level state shown in the status bar.
#[derive(Clone)]
pub struct Settings {
    pub bph: u32,
    pub lift_angle: f64,
    pub noise_suppressor: bool,
    pub source: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bph: 0,
            lift_angle: 52.0,
            noise_suppressor: true,
            source: String::new(),
        }
    }
}

pub fn draw(f: &mut ratatui::Frame, res: &Option<TimingResult>, s: &Settings) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_readout(f, chunks[0], res);
    draw_waveform(f, chunks[1], res);
    draw_status(f, chunks[2], s);
}

fn draw_readout(f: &mut ratatui::Frame, area: Rect, res: &Option<TimingResult>) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 3); 3])
        .split(area);

    let (rate, amp, be) = match res {
        Some(r) => (
            (
                format!("{:+.1}", r.rate_sday),
                " s/day",
                rate_color(r.rate_sday),
            ),
            (
                match r.amplitude_deg {
                    Some(a) => format!("{a:.0}"),
                    None => "---".into(),
                },
                " deg",
                Color::Cyan,
            ),
            (format!("{:+.2}", r.beat_error_ms), " ms", Color::Magenta),
        ),
        None => (
            ("---".into(), " s/day", Color::DarkGray),
            ("---".into(), " deg", Color::DarkGray),
            ("---".into(), " ms", Color::DarkGray),
        ),
    };
    for (col, (title, (val, unit, color))) in ["Rate", "Amplitude", "Beat error"]
        .into_iter()
        .zip([rate, amp, be])
        .enumerate()
    {
        let block = Block::default().borders(Borders::ALL).title(format!(" {title} "));
        f.render_widget(block, cols[col]);
        let a = cols[col].inner(Margin::new(2, 1));
        let p = Paragraph::new(Line::from(vec![
            Span::styled(val, Style::default().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled(unit, Style::default().fg(Color::Gray)),
        ]))
        .alignment(Alignment::Center);
        f.render_widget(p, a);
    }
}

fn rate_color(rate: f64) -> Color {
    if rate.abs() < 5.0 {
        Color::Green
    } else if rate.abs() < 15.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

fn draw_waveform(f: &mut ratatui::Frame, area: Rect, res: &Option<TimingResult>) {
    let block = Block::default().borders(Borders::ALL).title(" Waveform ");
    f.render_widget(block, area);
    let inner = area.inner(Margin::new(1, 1));
    if inner.width < 2 || inner.height < 2 {
        return;
    }
    let Some(r) = res else { return };
    let wf = &r.waveform;
    if wf.is_empty() {
        return;
    }
    let max = wf.iter().cloned().fold(0f32, f32::max).max(1e-9);
    let mid = inner.height as f32 / 2.0;
    for x in 0..inner.width {
        let i = x as usize * wf.len() / inner.width as usize;
        let v = wf[i.min(wf.len() - 1)] / max;
        let h = ((v.abs() * mid).ceil() as u16).clamp(1, inner.height);
        let y = if v >= 0.0 {
            mid as u16 - h
        } else {
            mid as u16
        };
        let style = Style::default().fg(if v >= 0.0 { Color::Green } else { Color::Red });
        for dy in 0..h {
            let py = (inner.y + y + dy).min(inner.y + inner.height - 1);
            f.buffer_mut()[(inner.x + x, py)].set_char('█').set_style(style);
        }
    }
}

fn draw_status(f: &mut ratatui::Frame, area: Rect, s: &Settings) {
    let bph = if s.bph == 0 { "auto".into() } else { s.bph.to_string() };
    let line = Line::from(vec![
        Span::styled(format!(" {} ", s.source), Style::default().bg(Color::DarkGray)),
        Span::raw(format!(
            " bph:{bph}  lift:{:.0}°  noise:{}  ",
            s.lift_angle,
            if s.noise_suppressor { "on" } else { "off" }
        )),
        Span::styled(
            "←→ bph  +/- lift  n noise  q quit",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}
