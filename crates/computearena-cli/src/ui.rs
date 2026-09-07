use crate::config::{BASECOMPUTE_DISCORD, BASECOMPUTE_WEBSITE};
use crate::theme::BASECOMPUTE_THEME;
use anyhow::{Context, Result};
use dialoguer::console::Style;
use std::fmt::Display;
use std::io::{self, IsTerminal, Write};
use std::time::Instant;

// BaseRT supplies the branded launcher build. A future standalone repository
// can replace this one compile-time asset without changing CLI behavior.
const BASERT_BANNER_HEADER: &str = include_str!("../../../../tools/basert_banner.h");
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

    pub(crate) fn banner(self) {
        println!();
        for (line, style) in shared_banner_art()
            .into_iter()
            .zip(shared_banner_gradient())
        {
            println!("  {}", self.render(style.bold(), line));
        }
        println!(
            "\n  {} {} {}\n",
            self.render(BASECOMPUTE_THEME.brand.style().dim(), BASECOMPUTE_WEBSITE),
            self.muted("·"),
            self.render(BASECOMPUTE_THEME.brand.style().dim(), BASECOMPUTE_DISCORD)
        );
    }
}

fn shared_banner_array(marker: &str) -> Vec<&'static str> {
    let block = BASERT_BANNER_HEADER
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing {marker:?} in tools/basert_banner.h"))
        .1
        .split_once("};")
        .unwrap_or_else(|| panic!("unterminated {marker:?} in tools/basert_banner.h"))
        .0;
    let mut values = Vec::new();
    let mut rest = block;
    while let Some((_, after_opening_quote)) = rest.split_once('"') {
        let Some((value, after_closing_quote)) = after_opening_quote.split_once('"') else {
            break;
        };
        values.push(value);
        rest = after_closing_quote;
    }
    values
}

fn shared_banner_art() -> Vec<&'static str> {
    shared_banner_array("static const char *art[8] = {")
}

fn shared_banner_gradient() -> Vec<Style> {
    shared_banner_array("static const char *grad[8] = {")
        .into_iter()
        .map(banner_gradient_style)
        .collect()
}

fn banner_gradient_style(value: &str) -> Style {
    let values: Vec<u8> = value
        .strip_prefix("\\x1b[")
        .and_then(|value| value.strip_suffix('m'))
        .unwrap_or_else(|| panic!("invalid gradient entry in tools/basert_banner.h"))
        .split(';')
        .map(|component| {
            component
                .parse()
                .unwrap_or_else(|_| panic!("invalid gradient entry in tools/basert_banner.h"))
        })
        .collect();
    let [38, 2, red, green, blue] = values.as_slice() else {
        panic!("unsupported gradient entry in tools/basert_banner.h");
    };
    Style::new().true_color(*red, *green, *blue)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_basert_banner_header_is_parseable() {
        let art = shared_banner_art();
        let gradient = shared_banner_gradient();
        assert_eq!(art.len(), 8);
        assert_eq!(gradient.len(), art.len());
    }
}
