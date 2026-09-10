//! The full-screen interactive session.
//!
//! Only the interactive session is drawn this way. Every command with
//! arguments, and any session whose input or output is not a terminal, keeps
//! the printed flow in `main`, so scripts and CI see exactly what they did
//! before.
mod app;
mod draw;
mod job;

use crate::adapters::Runtime;
use crate::reports::Paths;
use anyhow::{Context, Result};
use app::App;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::CrosstermBackend;
use ratatui::Terminal;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// Long jobs redirect the process's own output, so the interface draws through
/// a duplicate of the terminal taken before any of that happens.
type Screen = Terminal<CrosstermBackend<std::fs::File>>;

/// Lines moved per wheel notch.
const SCROLL_LINES: isize = 3;

pub(crate) fn session(
    runtime: Runtime,
    choose_runtime: bool,
    paths: &Paths,
    harness: Option<PathBuf>,
    api_url: &str,
) -> Result<()> {
    let mut app = App::new(
        runtime,
        choose_runtime,
        paths.clone(),
        harness,
        api_url.to_string(),
    )?;
    let mut terminal = start().context("opening the full-screen interface")?;
    let outcome = run(&mut terminal, &mut app);
    stop(&mut terminal);
    outcome
}

/// Put the terminal back before anything else prints a panic, and record it
/// where it can be read: while a job is running the process's own stderr is
/// captured, so an unhandled panic would otherwise vanish.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        if let Ok(mut file) = job::tty() {
            let _ = ratatui::crossterm::execute!(file, DisableMouseCapture, LeaveAlternateScreen);
        }
        previous(info);
    }));
}

fn start() -> Result<Screen> {
    install_panic_hook();
    let mut file = job::tty()?;
    enable_raw_mode()?;
    // Mouse capture is what delivers wheel events; the terminal's own text
    // selection still works with Shift held.
    execute!(file, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(file))?)
}

fn stop(terminal: &mut Screen) {
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal.show_cursor();
    let _ = terminal.backend_mut().flush();
}

fn run(terminal: &mut Screen, app: &mut App) -> Result<()> {
    // A short poll keeps the elapsed time and spinner moving while a job runs
    // without spinning the CPU when nothing is happening.
    let idle = Duration::from_millis(100);
    loop {
        terminal.draw(|frame| draw::draw(frame, app))?;
        if event::poll(idle)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key)?,
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll(-SCROLL_LINES),
                    MouseEventKind::ScrollDown => app.scroll(SCROLL_LINES),
                    _ => {}
                },
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        app.tick();
        if app.should_quit {
            return Ok(());
        }
    }
}
