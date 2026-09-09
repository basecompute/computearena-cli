use crate::theme::BASECOMPUTE_THEME;
use anyhow::{Context, Result};
use dialoguer::console::Style;
use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::time::Instant;

const RULE: &str = "────────────────────────────────────────────────────────────";

#[derive(Clone, Copy)]
pub(crate) struct TerminalUi {
    color: bool,
}

impl TerminalUi {
    pub(crate) fn detect() -> Self {
        Self {
            color: io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var("TERM").as_deref() != Ok("dumb"),
        }
    }

    fn render(self, style: Style, text: impl Display) -> String {
        style.force_styling(self.color).apply_to(text).to_string()
    }

    pub(crate) fn brand(self, text: impl Display) -> String {
        self.render(BASECOMPUTE_THEME.brand.style(), text)
    }

    pub(crate) fn brand_bold(self, text: impl Display) -> String {
        self.render(BASECOMPUTE_THEME.brand.style().bold(), text)
    }

    pub(crate) fn strong(self, text: impl Display) -> String {
        self.render(Style::new().bold(), text)
    }

    pub(crate) fn accent_bold(self, text: impl Display) -> String {
        self.render(BASECOMPUTE_THEME.accent.style().bold(), text)
    }

    pub(crate) fn muted(self, text: impl Display) -> String {
        self.render(Style::new().dim(), text)
    }

    pub(crate) fn success(self, text: impl Display) -> String {
        self.brand_bold(text)
    }

    pub(crate) fn warning(self, text: impl Display) -> String {
        self.render(Style::new().yellow().bold(), text)
    }

    pub(crate) fn error(self, text: impl Display) -> String {
        self.render(BASECOMPUTE_THEME.danger.style().bold(), text)
    }

    pub(crate) fn neutral(self, text: impl Display) -> String {
        self.render(BASECOMPUTE_THEME.neutral.style(), text)
    }

    pub(crate) fn section(self, title: &str) {
        println!("\n{}", self.muted(RULE));
        println!("{}", self.brand_bold(title));
        println!("{}", self.muted(RULE));
    }
}

pub(crate) fn start_activity(ui: TerminalUi, message: impl Display) -> Instant {
    println!("{} {message}", ui.brand_bold("…"));
    let _ = io::stdout().flush();
    Instant::now()
}

pub(crate) fn finish_activity(ui: TerminalUi, started: Instant, message: impl Display) {
    let elapsed = started.elapsed().as_secs_f64();
    let timing = if elapsed >= 0.1 {
        format!(" ({elapsed:.1}s)")
    } else {
        String::new()
    };
    println!("{} {message}{}", ui.success("✓"), ui.muted(timing));
}

pub(crate) fn prompt(message: &str) -> Result<String> {
    let ui = TerminalUi::detect();
    print!("\n{} {message}", ui.brand_bold("›"));
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input).context("reading input")?;
    Ok(input.trim().to_string())
}

pub(crate) fn prompt_yes_no(message: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        let answer = prompt(&format!("{message} {hint}: "))?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!(
                "{} Enter `y` for yes or `n` for no.",
                TerminalUi::detect().warning("!")
            ),
        }
    }
}
