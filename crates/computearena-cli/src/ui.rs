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
    if io::stdin().read_line(&mut input).context("reading input")? == 0 {
        anyhow::bail!("input closed; nothing else will be run or submitted");
    }
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

/// One line of a numbered menu. Aliases answer to the option in addition to
/// its number, so `basert` still selects BaseRT and `q` still exits.
pub(crate) struct MenuItem {
    pub(crate) label: String,
    pub(crate) detail: Option<String>,
    pub(crate) aliases: &'static [&'static str],
}

impl MenuItem {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
            aliases: &[],
        }
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn aliases(mut self, aliases: &'static [&'static str]) -> Self {
        self.aliases = aliases;
        self
    }
}

pub(crate) enum MenuChoice {
    Item(usize),
    Escape,
}

const ESCAPE_WORDS: [&str; 5] = ["0", "q", "quit", "exit", "back"];

/// Every selection in the CLI is a numbered list, one option per line, in the
/// same style: a brand-coloured number, the label, and an optional muted
/// detail line. `escape` is offered as option 0 and also answers to q, quit,
/// exit, and back.
pub(crate) fn print_menu(ui: TerminalUi, items: &[MenuItem], escape: Option<&str>) {
    for (index, item) in items.iter().enumerate() {
        println!(
            "  {} {}",
            ui.brand_bold(format!("{}.", index + 1)),
            item.label
        );
        if let Some(detail) = &item.detail {
            println!("     {}", ui.muted(detail));
        }
    }
    if let Some(label) = escape {
        println!("  {} {label}", ui.brand_bold("0."));
    }
}

/// Print a numbered menu and read a choice, repeating until the answer is one
/// of the listed options.
pub(crate) fn choose(
    ui: TerminalUi,
    label: &str,
    items: &[MenuItem],
    escape: Option<&str>,
) -> Result<MenuChoice> {
    print_menu(ui, items, escape);
    loop {
        let answer = prompt(label)?;
        let answer = answer.trim();
        if escape.is_some() && ESCAPE_WORDS.contains(&answer.to_ascii_lowercase().as_str()) {
            return Ok(MenuChoice::Escape);
        }
        if let Ok(number) = answer.parse::<usize>() {
            if (1..=items.len()).contains(&number) {
                return Ok(MenuChoice::Item(number - 1));
            }
        }
        if let Some(index) = items.iter().position(|item| {
            item.aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(answer))
        }) {
            return Ok(MenuChoice::Item(index));
        }
        println!(
            "{} Choose a number from 1 to {}{}.",
            ui.warning("!"),
            items.len(),
            match escape {
                Some(label) => format!(", or 0 to {}", label.to_ascii_lowercase()),
                None => String::new(),
            }
        );
    }
}

/// Aligned `label  value` rows for plans and summaries.
pub(crate) fn print_fields(ui: TerminalUi, rows: &[(&str, String)]) {
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    for (label, value) in rows {
        let mut lines = value.lines();
        println!(
            "  {}  {}",
            ui.neutral(format!("{label:<width$}")),
            lines.next().unwrap_or_default()
        );
        for line in lines {
            println!("  {}  {line}", " ".repeat(width));
        }
    }
}
