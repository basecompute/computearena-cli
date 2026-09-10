use crate::config::MENU_VISIBLE_ROWS;
use crate::theme::{selector_theme, BASECOMPUTE_THEME};
use anyhow::{Context, Result};
use dialoguer::console::Style;
use dialoguer::{MultiSelect, Select};
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

/// Ask for one option. On a terminal this is an arrow-key list: nothing to
/// type, Enter selects, Esc leaves. Everywhere else it falls back to the
/// numbered prompt so scripts and pipes keep working.
pub(crate) fn choose(
    ui: TerminalUi,
    label: &str,
    items: &[MenuItem],
    escape: Option<&str>,
) -> Result<MenuChoice> {
    choose_default(ui, label, items, escape, 0)
}

/// `choose` with the cursor pre-placed on the option most people want.
pub(crate) fn choose_default(
    ui: TerminalUi,
    label: &str,
    items: &[MenuItem],
    escape: Option<&str>,
    default: usize,
) -> Result<MenuChoice> {
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        return select_interactive(ui, label, items, escape, default);
    }
    choose_numbered(ui, label, items, escape)
}

fn select_interactive(
    ui: TerminalUi,
    label: &str,
    items: &[MenuItem],
    escape: Option<&str>,
    default: usize,
) -> Result<MenuChoice> {
    let mut choices: Vec<String> = items
        .iter()
        .map(|item| match &item.detail {
            Some(detail) => format!("{}  {}", item.label, ui.muted(detail)),
            None => item.label.clone(),
        })
        .collect();
    if let Some(escape) = escape {
        choices.push(ui.neutral(format!("← {escape}")));
    }
    println!("{}", ui.muted("↑/↓ move · Enter select · Esc back"));
    io::stdout().flush()?;

    let selected = Select::with_theme(&selector_theme())
        .with_prompt(menu_prompt(label))
        .items(&choices)
        .default(default.min(choices.len().saturating_sub(1)))
        .max_length(MENU_VISIBLE_ROWS)
        .report(false)
        .interact_opt()
        .context("reading a menu selection")?;

    match selected {
        Some(index) if index < items.len() => Ok(MenuChoice::Item(index)),
        _ => Ok(MenuChoice::Escape),
    }
}

/// Menu labels are written as prompts (`"Choose an option: "`); the list
/// selector adds its own separator.
fn menu_prompt(label: &str) -> String {
    label.trim_end().trim_end_matches([':', '?']).to_string()
}

/// Print a numbered menu and read a choice, repeating until the answer is one
/// of the listed options.
fn choose_numbered(
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

/// Pick several options at once. On a terminal this is a checklist — Space
/// toggles, Enter confirms, Esc leaves — with `preselected` already ticked.
/// Elsewhere it falls back to reading a comma-separated list of numbers.
pub(crate) fn choose_many(
    ui: TerminalUi,
    label: &str,
    items: &[MenuItem],
    preselected: &[bool],
) -> Result<Option<Vec<usize>>> {
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        return Ok(None);
    }
    let choices: Vec<String> = items
        .iter()
        .map(|item| match &item.detail {
            Some(detail) => format!("{}  {}", item.label, ui.muted(detail)),
            None => item.label.clone(),
        })
        .collect();
    println!(
        "{}",
        ui.muted("↑/↓ move · Space toggle · Enter confirm · Esc back")
    );
    io::stdout().flush()?;

    let selected = MultiSelect::with_theme(&selector_theme())
        .with_prompt(menu_prompt(label))
        .items(&choices)
        .defaults(preselected)
        .max_length(MENU_VISIBLE_ROWS)
        .report(false)
        .interact_opt()
        .context("reading a menu selection")?;
    Ok(Some(selected.unwrap_or_default()))
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
