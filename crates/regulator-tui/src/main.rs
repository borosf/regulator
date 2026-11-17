// SPDX-License-Identifier: GPL-2.0
//! regulator: a watch timing machine for the terminal.

mod ui;
#[cfg(feature = "audio")]
mod audio;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use regulator_core::{Analyzer, SyntheticWatch, TimingResult};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use ui::Settings;

const SAMPLE_RATE: u32 = 44100;
const BLOCK_SECONDS: f64 = 4.0;

fn main() -> Result<()> {
    let demo = std::env::args().any(|a| a == "--demo");
    let settings = Arc::new(Mutex::new(Settings::default()));
    settings.lock().unwrap().source = if demo { "demo" } else { "mic" }.into();

    let (result_tx, result_rx) = mpsc::channel::<Option<TimingResult>>();

    if demo {
        spawn_demo_worker(result_tx, settings.clone());
    } else {
        #[cfg(feature = "audio")]
        {
            let (sample_tx, sample_rx) = mpsc::channel::<f32>();
            audio::spawn_capture(sample_tx)?;
            spawn_live_worker(sample_rx, result_tx, settings.clone());
        }
        #[cfg(not(feature = "audio"))]
        {
            anyhow::bail!("built without the `audio` feature; run with --demo");
        }
    }

    run_tui(result_rx, settings)
}

fn build_analyzer(settings: &Settings) -> Analyzer {
    let mut an = Analyzer::new(SAMPLE_RATE);
    an.bph = settings.bph;
    an.lift_angle = settings.lift_angle;
    an.noise_suppressor = settings.noise_suppressor;
    an
}

fn analyze_block(samples: &[f32], settings: &Settings) -> Option<TimingResult> {
    build_analyzer(settings).analyze(samples)
}

fn spawn_demo_worker(tx: mpsc::Sender<Option<TimingResult>>, settings: Arc<Mutex<Settings>>) {
    std::thread::spawn(move || {
        let watch = SyntheticWatch {
            bph: 28800,
            rate_sday: 9.0,
            beat_error_ms: 0.6,
            amplitude_deg: 275.0,
        };
        let samples = watch.generate(SAMPLE_RATE, 60.0);
        let mut start = 0usize;
        let block = (BLOCK_SECONDS * SAMPLE_RATE as f64) as usize;
        loop {
            let end = (start + block).min(samples.len());
            let res = analyze_block(&samples[start..end], &settings.lock().unwrap().clone());
            let _ = tx.send(res);
            if end == samples.len() {
                start = 0;
            } else {
                start = end;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

#[cfg(feature = "audio")]
fn spawn_live_worker(
    rx: mpsc::Receiver<f32>,
    tx: mpsc::Sender<Option<TimingResult>>,
    settings: Arc<Mutex<Settings>>,
) {
    std::thread::spawn(move || {
        let block = (BLOCK_SECONDS * SAMPLE_RATE as f64) as usize;
        let mut buf: Vec<f32> = Vec::with_capacity(block * 2);
        loop {
            while let Ok(s) = rx.try_recv() {
                buf.push(s);
                if buf.len() > block * 2 {
                    buf.drain(..block);
                }
            }
            if buf.len() >= block {
                let res = analyze_block(&buf[buf.len() - block..], &settings.lock().unwrap().clone());
                let _ = tx.send(res);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

fn run_tui(
    rx: mpsc::Receiver<Option<TimingResult>>,
    settings: Arc<Mutex<Settings>>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let mut result: Option<TimingResult> = None;
    let mut last_draw = Instant::now();

    loop {
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let mut s = settings.lock().unwrap();
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Left | KeyCode::Right => {
                            const BPH: [u32; 6] = [0, 18000, 21600, 25200, 28800, 36000];
                            let idx = BPH.iter().position(|&b| b == s.bph).unwrap_or(0);
                            let next = if key.code == KeyCode::Right {
                                (idx + 1) % BPH.len()
                            } else {
                                (idx + BPH.len() - 1) % BPH.len()
                            };
                            s.bph = BPH[next];
                        }
                        KeyCode::Char('+') | KeyCode::Char('=') => s.lift_angle += 1.0,
                        KeyCode::Char('-') => s.lift_angle -= 1.0,
                        KeyCode::Char('n') => s.noise_suppressor = !s.noise_suppressor,
                        _ => {}
                    }
                }
            }
        }
        while let Ok(r) = rx.try_recv() {
            result = r;
        }
        if last_draw.elapsed() > Duration::from_millis(100) {
            let s = settings.lock().unwrap().clone();
            terminal.draw(|f| ui::draw(f, &result, &s))?;
            last_draw = Instant::now();
        }
    }

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}
