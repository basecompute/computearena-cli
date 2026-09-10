//! Rendering. Every screen shares the same frame: a header naming the session,
//! a body, and a key bar, so nothing moves between screens except the body.
use super::app::{
    App, HubFileRow, HubModelRow, ModelRow, ReportMode, ReportRow, Screen, MENU_ITEMS,
    SETUP_ACTIONS,
};
use super::job::Job;
use crate::benchmark::LOAD_WARNING;
use crate::theme::BASECOMPUTE_THEME;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

fn colour(component: crate::theme::Rgb) -> Color {
    let (red, green, blue) = component.rgb();
    Color::Rgb(red, green, blue)
}

fn brand() -> Color {
    colour(BASECOMPUTE_THEME.brand)
}

fn neutral() -> Color {
    colour(BASECOMPUTE_THEME.neutral)
}

fn danger() -> Color {
    colour(BASECOMPUTE_THEME.danger)
}

fn positive() -> Color {
    colour(BASECOMPUTE_THEME.positive)
}

fn deep_ocean() -> Color {
    colour(BASECOMPUTE_THEME.selection_background)
}

fn accent() -> Color {
    colour(BASECOMPUTE_THEME.accent)
}

fn muted() -> Style {
    Style::default().fg(neutral()).add_modifier(Modifier::DIM)
}

/// Panels are hairlines; the one holding the cursor takes the focus colour,
/// as the design system's focus ring does.
fn panel(title: &str) -> Block<'_> {
    focus_panel(title, true)
}

fn focus_panel(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused { accent() } else { neutral() }))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(brand()).add_modifier(Modifier::BOLD),
        ))
}

/// The masthead is three rows of Deep Ocean carrying a letter-spaced Lime
/// wordmark — the design system's signature pairing — with the session's
/// context on the row beneath it. Short terminals get the one-row version.
const MASTHEAD_ROWS: u16 = 3;

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    let masthead = if frame.area().height >= 24 {
        MASTHEAD_ROWS
    } else {
        1
    };
    let areas = Layout::vertical([
        Constraint::Length(masthead + 1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(frame.area());
    header(frame, areas[0], app);
    body(frame, areas[1], app);
    footer(frame, areas[2], app);
}

/// Letters spaced apart, words spaced further, the way the brand's tracking
/// reads at display size: `C O M P U T E   A R E N A`.
fn wordmark(text: &str) -> String {
    let mut spaced = String::new();
    for character in text.chars() {
        if character.is_uppercase() && !spaced.is_empty() {
            spaced.push_str("  ");
        } else if !spaced.is_empty() {
            spaced.push(' ');
        }
        spaced.extend(character.to_uppercase());
    }
    spaced
}

fn header(frame: &mut Frame, area: Rect, app: &App) {
    let bar_rows = area.height.saturating_sub(1);
    let bar = Rect {
        height: bar_rows,
        ..area
    };
    let context = Rect {
        y: area.y + bar_rows,
        height: 1,
        ..area
    };
    let padding = if bar_rows > 1 { 1 } else { 0 };
    let mut lines = vec![Line::from(""); padding as usize];
    lines.push(Line::from(Span::styled(
        format!("  {}", wordmark("ComputeArena")),
        Style::default()
            .fg(brand())
            .bg(deep_ocean())
            .add_modifier(Modifier::BOLD),
    )));
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(deep_ocean())),
        bar,
    );

    // The runtime is not settled until it has been chosen, so the context row
    // says so rather than naming the default.
    let runtime = if matches!(app.screen(), Screen::Runtime { .. }) {
        Span::styled("choosing a runtime", muted())
    } else {
        Span::styled(app.runtime_label(), Style::default().fg(neutral()))
    };
    let account = match &app.account {
        Some(user) => Span::styled(format!("@{user}"), Style::default().fg(brand())),
        None => Span::styled("not signed in", muted()),
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
            runtime,
            Span::raw("  ·  "),
            account,
        ])),
        context,
    );
}

fn footer(frame: &mut Frame, area: Rect, app: &App) {
    let keys = match app.screen() {
        Screen::Models { .. } => "↑/↓ move · type to filter · Enter select · Esc back",
        Screen::PathEntry { .. } => "type a path · Enter confirm · Esc back",
        Screen::Plan { details, .. } if details.is_some() => "Esc close details",
        Screen::Plan { .. } => "↑/↓ move · Enter start · Esc cancel",
        Screen::Reports { mode, .. } if *mode == ReportMode::Submit => {
            "↑/↓ move · Space tick · a all · Enter preview · Esc back"
        }
        Screen::Reports { .. } => "↑/↓ move · Enter verify · Esc back",
        Screen::Preview { .. } => "↑/↓ scroll · Enter submit · Esc back",
        Screen::Running => "↑/↓ scroll · Enter/Esc back when finished · Ctrl+C quit",
        Screen::Loading { .. } => "working…",
        Screen::HubSearch { .. } => "type a search · Enter search · Esc back",
        Screen::HubModels { .. } => "↑/↓ move · Enter list files · Esc back",
        Screen::HubFiles { .. } => "↑/↓ move · Enter download · Esc back",
        _ => "↑/↓ move · Enter select · Esc back · Ctrl+C quit",
    };
    let status = if app.status.is_empty() {
        Line::from("")
    } else {
        Line::from(Span::styled(
            app.status.clone(),
            Style::default().fg(accent()),
        ))
    };
    frame.render_widget(
        Paragraph::new(vec![status, Line::from(Span::styled(keys, muted()))]),
        area,
    );
}

fn body(frame: &mut Frame, area: Rect, app: &mut App) {
    match app.screen() {
        Screen::Runtime { cursor } => runtime_screen(frame, area, *cursor),
        Screen::Setup {
            problem,
            instructions,
            cursor,
        } => setup_screen(frame, area, problem, instructions, *cursor),
        Screen::Menu { cursor } => menu_screen(frame, area, *cursor),
        Screen::Models {
            rows,
            filter,
            cursor,
        } => models_screen(frame, area, rows, filter, *cursor),
        Screen::PathEntry { input, error } => path_screen(frame, area, input, error.as_deref()),
        Screen::Plan {
            rows,
            options,
            cursor,
            details,
            ..
        } => {
            plan_screen(frame, area, rows, options, *cursor);
            if let Some(details) = details {
                details_overlay(frame, area, details);
            }
        }
        Screen::Running => job_screen(frame, area, app.job.as_ref()),
        Screen::Reports {
            rows,
            marks,
            cursor,
            mode,
        } => reports_screen(frame, area, rows, marks, *cursor, *mode),
        Screen::Preview { lines, scroll, .. } => preview_screen(frame, area, lines, *scroll),
        Screen::Info { title, lines } => info_screen(frame, area, title, lines),
        Screen::Loading { message } => loading_screen(frame, area, message),
        Screen::HubSearch { input } => search_screen(frame, area, input),
        Screen::HubModels { rows, cursor } => hub_models_screen(frame, area, rows, *cursor),
        Screen::HubFiles {
            repository,
            rows,
            cursor,
        } => hub_files_screen(frame, area, repository, rows, *cursor),
        Screen::Account { cursor } => account_screen(frame, area, app.account.as_deref(), *cursor),
    }
}

/// One list item: a title line and a dim detail line beneath it.
fn item<'a>(title: impl Into<String>, detail: impl Into<String>, selected: bool) -> ListItem<'a> {
    let marker = if selected { "› " } else { "  " };
    let title_style = if selected {
        Style::default()
            .fg(brand())
            .bg(deep_ocean())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let detail = detail.into();
    let mut lines = vec![Line::from(vec![
        Span::styled(marker, Style::default().fg(brand())),
        Span::styled(title.into(), title_style),
    ])];
    if !detail.is_empty() {
        lines.push(Line::from(Span::styled(format!("    {detail}"), muted())));
    }
    ListItem::new(lines)
}

fn render_list(frame: &mut Frame, area: Rect, title: &str, items: Vec<ListItem>, cursor: usize) {
    let mut state = ListState::default();
    state.select(Some(cursor));
    frame.render_stateful_widget(List::new(items).block(panel(title)), area, &mut state);
}

fn runtime_screen(frame: &mut Frame, area: Rect, cursor: usize) {
    let items = vec![
        item(
            "BaseRT",
            "basert-benchmark-harness · .base models",
            cursor == 0,
        ),
        item("llama.cpp", "llama-bench · .gguf models", cursor == 1),
    ];
    render_list(frame, area, "Choose a runtime", items, cursor);
}

fn setup_screen(
    frame: &mut Frame,
    area: Rect,
    problem: &str,
    instructions: &[String],
    cursor: usize,
) {
    let areas = Layout::vertical([Constraint::Min(5), Constraint::Length(8)]).split(area);
    let mut lines = vec![
        Line::from(Span::styled(
            problem.lines().next().unwrap_or_default().to_string(),
            Style::default().fg(danger()),
        )),
        Line::from(""),
    ];
    lines.extend(
        instructions
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(neutral())))),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(focus_panel("Runtime not ready", false)),
        areas[0],
    );
    let items = SETUP_ACTIONS
        .iter()
        .enumerate()
        .map(|(index, (label, detail))| item(*label, *detail, index == cursor))
        .collect();
    render_list(frame, areas[1], "What next", items, cursor);
}

fn menu_screen(frame: &mut Frame, area: Rect, cursor: usize) {
    let items = MENU_ITEMS
        .iter()
        .enumerate()
        .map(|(index, (label, detail))| item(*label, *detail, index == cursor))
        .collect();
    render_list(frame, area, "ComputeArena", items, cursor);
}

fn models_screen(frame: &mut Frame, area: Rect, rows: &[ModelRow], filter: &str, cursor: usize) {
    let visible = App::visible_models(rows, filter);
    let items: Vec<ListItem> = visible
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let row = &rows[*index];
            item(row.label.clone(), row.detail.clone(), position == cursor)
        })
        .collect();
    let title = if filter.is_empty() {
        format!(
            "Choose a model · {} available",
            rows.len().saturating_sub(1)
        )
    } else {
        format!("Choose a model · filter: {filter}")
    };
    render_list(frame, area, &title, items, cursor);
}

fn path_screen(frame: &mut Frame, area: Rect, input: &str, error: Option<&str>) {
    let mut lines = vec![
        Line::from(Span::styled(
            "Type the model's path. ~/ works.",
            Style::default().fg(neutral()),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("› ", Style::default().fg(brand())),
            Span::styled(input.to_string(), Style::default().fg(brand())),
            Span::styled("▏", Style::default().fg(brand())),
        ]),
    ];
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            error.to_string(),
            Style::default().fg(danger()),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel("Model path")),
        area,
    );
}

fn plan_screen(
    frame: &mut Frame,
    area: Rect,
    rows: &[(&'static str, String)],
    options: &[crate::benchmark::RunProfileOption],
    cursor: usize,
) {
    // Two borders, a blank line, and the two-line load warning.
    let plan_height = rows
        .iter()
        .map(|(_, value)| value.lines().count() as u16)
        .sum::<u16>()
        + 5;
    let areas = Layout::vertical([Constraint::Length(plan_height), Constraint::Min(5)]).split(area);
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for (label, value) in rows {
        let mut parts = value.lines();
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<width$}  "), Style::default().fg(neutral())),
            Span::raw(parts.next().unwrap_or_default().to_string()),
        ]));
        for part in parts {
            lines.push(Line::from(format!("{}  {part}", " ".repeat(width))));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("! ", Style::default().fg(danger())),
        Span::raw(LOAD_WARNING[0]),
    ]));
    lines.push(Line::from(format!("  {}", LOAD_WARNING[1])));
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(focus_panel("Benchmark plan", false)),
        areas[0],
    );

    let mut items: Vec<ListItem> = options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            item(
                format!("Start — {} · {}", option.label, option.duration),
                option.detail.clone(),
                index == cursor,
            )
        })
        .collect();
    items.push(item(
        "What does this run?",
        "Sampling, telemetry, and what the report contains",
        cursor == options.len(),
    ));
    render_list(frame, areas[1], "Start benchmark", items, cursor);
}

fn details_overlay(frame: &mut Frame, area: Rect, rows: &[(&'static str, String)]) {
    let height = (rows
        .iter()
        .map(|(_, value)| value.lines().count() as u16)
        .sum::<u16>()
        + 2)
    .min(area.height);
    let overlay = Rect {
        x: area.x + 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: area.width.saturating_sub(4),
        height,
    };
    let width = rows.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    for (label, value) in rows {
        let mut parts = value.lines();
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<width$}  "), Style::default().fg(neutral())),
            Span::raw(parts.next().unwrap_or_default().to_string()),
        ]));
        for part in parts {
            lines.push(Line::from(format!("{}  {part}", " ".repeat(width))));
        }
    }
    frame.render_widget(Clear, overlay);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(panel("What this runs")),
        overlay,
    );
}

fn reports_screen(
    frame: &mut Frame,
    area: Rect,
    rows: &[ReportRow],
    marks: &[bool],
    cursor: usize,
    mode: ReportMode,
) {
    let items: Vec<ListItem> = rows
        .iter()
        .zip(marks)
        .enumerate()
        .map(|(index, (row, marked))| {
            let tick = match (mode, marked, row.valid) {
                (ReportMode::Submit, true, _) => "[x] ",
                (ReportMode::Submit, false, true) => "[ ] ",
                (ReportMode::Submit, false, false) => "[-] ",
                (ReportMode::Verify, _, _) => "",
            };
            let status = if row.valid { "VALID" } else { "INVALID" };
            item(
                format!("{tick}{}  [{status}]", row.label),
                row.detail.clone(),
                index == cursor,
            )
        })
        .collect();
    let title = match mode {
        ReportMode::Submit => "Choose benchmarks to submit",
        ReportMode::Verify => "Choose a benchmark to verify",
    };
    render_list(frame, area, title, items, cursor);
}

fn preview_screen(frame: &mut Frame, area: Rect, lines: &[String], scroll: usize) {
    let text: Vec<Line> = lines
        .iter()
        .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(neutral()))))
        .collect();
    frame.render_widget(
        Paragraph::new(text)
            .scroll((scroll as u16, 0))
            .block(panel("Submission preview · not yet uploaded")),
        area,
    );
}

fn info_screen(frame: &mut Frame, area: Rect, title: &str, lines: &[String]) {
    let text: Vec<Line> = lines.iter().map(|line| Line::from(line.clone())).collect();
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(title)),
        area,
    );
}

/// Reading models or verifying reports takes long enough to need saying so.
fn loading_screen(frame: &mut Frame, area: Rect, message: &str) {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or_default();
    let spinner = SPINNER[(elapsed / 120) as usize % SPINNER.len()];
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{spinner} "), Style::default().fg(brand())),
            Span::styled(message.to_string(), Style::default().fg(neutral())),
        ]))
        .block(focus_panel("Working", false)),
        area,
    );
}

fn search_screen(frame: &mut Frame, area: Rect, input: &str) {
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Search the Hugging Face Hub for models published with GGUF files.",
                Style::default().fg(neutral()),
            )),
            Line::from(Span::styled(
                "Gated repositories need HF_TOKEN set in your environment.",
                muted(),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled("› ", Style::default().fg(brand())),
                Span::styled(input.to_string(), Style::default().fg(brand())),
                Span::styled("▏", Style::default().fg(brand())),
            ]),
        ])
        .wrap(Wrap { trim: false })
        .block(panel("Find a model")),
        area,
    );
}

fn hub_models_screen(frame: &mut Frame, area: Rect, rows: &[HubModelRow], cursor: usize) {
    let items = rows
        .iter()
        .enumerate()
        .map(|(index, row)| item(row.id.clone(), row.detail.clone(), index == cursor))
        .collect();
    render_list(frame, area, "Hugging Face · GGUF models", items, cursor);
}

fn hub_files_screen(
    frame: &mut Frame,
    area: Rect,
    repository: &str,
    rows: &[HubFileRow],
    cursor: usize,
) {
    let items = rows
        .iter()
        .enumerate()
        .map(|(index, row)| item(row.label.clone(), row.detail.clone(), index == cursor))
        .collect();
    render_list(
        frame,
        area,
        &format!("{repository} · choose a quantization"),
        items,
        cursor,
    );
}

fn account_screen(frame: &mut Frame, area: Rect, account: Option<&str>, cursor: usize) {
    let areas = Layout::vertical([Constraint::Length(4), Constraint::Min(3)]).split(area);
    let lines = match account {
        Some(user) => vec![
            Line::from(vec![
                Span::styled("Signed in as ", Style::default().fg(neutral())),
                Span::styled(
                    format!("@{user}"),
                    Style::default().fg(brand()).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                "Submitted benchmarks are published under this account.",
                muted(),
            )),
        ],
        None => vec![
            Line::from(Span::styled(
                "Not signed in.",
                Style::default().fg(neutral()),
            )),
            Line::from(Span::styled(
                "Benchmarks still run and are saved locally; signing in is only needed to submit.",
                muted(),
            )),
        ],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(focus_panel("Account", false)),
        areas[0],
    );
    let action = match account {
        Some(_) => item("Log out", "Revoke this installation's session", cursor == 0),
        None => item("Log in", "Opens your browser to connect", cursor == 0),
    };
    render_list(
        frame,
        areas[1],
        "What next",
        vec![action, item("Back", "", cursor == 1)],
        cursor,
    );
}

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

fn job_screen(frame: &mut Frame, area: Rect, job: Option<&Job>) {
    let Some(job) = job else {
        return;
    };
    let elapsed = job.started.elapsed();
    let heading = match &job.outcome {
        None => {
            let frame_index = (elapsed.as_millis() / 120) as usize % SPINNER.len();
            Line::from(vec![
                Span::styled(
                    format!("{} ", SPINNER[frame_index]),
                    Style::default().fg(brand()),
                ),
                Span::raw(job.title.clone()),
                Span::styled(
                    format!("  {}s", elapsed.as_secs()),
                    Style::default().fg(neutral()),
                ),
            ])
        }
        Some(Ok(summary)) => Line::from(vec![
            Span::styled("✓ ", Style::default().fg(positive())),
            Span::styled(
                summary.clone(),
                Style::default().fg(positive()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}s", elapsed.as_secs()),
                Style::default().fg(neutral()),
            ),
        ]),
        Some(Err(error)) => Line::from(vec![
            Span::styled("✗ ", Style::default().fg(danger())),
            Span::styled(error.clone(), Style::default().fg(danger())),
        ]),
    };
    let areas = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).split(area);
    // Deliberately unwrapped: a heading longer than the window should be cut
    // off, not pushed onto a line this row has no room for.
    frame.render_widget(Paragraph::new(heading), areas[0]);

    // The log pane holds whatever the underlying command printed, following the
    // newest line unless the reader has scrolled back.
    let visible = areas[1].height.saturating_sub(2) as usize;
    let bottom = job
        .scroll
        .map_or(job.log.len(), |scroll| (scroll + 1).min(job.log.len()));
    let start = bottom.saturating_sub(visible);
    let lines: Vec<Line> = job.log[start..bottom]
        .iter()
        .map(|line| {
            let style = if line.starts_with('✓') {
                Style::default().fg(positive())
            } else if line.starts_with('✗') || line.starts_with("error") {
                Style::default().fg(danger())
            } else if line.starts_with('!') || line.starts_with('…') {
                Style::default().fg(brand())
            } else {
                Style::default().fg(neutral())
            };
            Line::from(Span::styled(line.clone(), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(panel("Output")), areas[1]);
}
