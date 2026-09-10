//! Screen state and key handling for the full-screen session.
use super::job::{Job, JobKind};
use crate::adapters::{BenchmarkRequest, Runtime};
use crate::auth::{load_api_session, login, logout};
use crate::benchmark::{
    benchmark_details, plan_rows, profile_options, run_benchmark, RunProfileOption,
};
use crate::config::{
    DEFAULT_DECODE_TOKENS, DEFAULT_PREFILL_TOKENS, DEFAULT_REPETITIONS, DEFAULT_WARMUP_REPETITIONS,
};
use crate::models::{installed_models, model_choice_labels};
use crate::reports::{list_reports, read_report, report_summaries, verify_report, Paths};
use crate::runtimes::{self, compact_path, manual_instructions};
use crate::submission::submit_reports;
use anyhow::{Context, Result};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelSource {
    Local,
    Hub,
}

pub(crate) struct ModelRow {
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) path: Option<PathBuf>,
    pub(crate) source: ModelSource,
}

pub(crate) struct HubModelRow {
    pub(crate) id: String,
    pub(crate) detail: String,
}

pub(crate) struct HubFileRow {
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) file: crate::huggingface::HubFile,
}

pub(crate) struct ReportRow {
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) path: PathBuf,
    pub(crate) valid: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportMode {
    Verify,
    Submit,
}

pub(crate) enum Screen {
    Loading {
        message: String,
    },
    Runtime {
        cursor: usize,
    },
    Setup {
        problem: String,
        instructions: Vec<String>,
        cursor: usize,
    },
    Menu {
        cursor: usize,
    },
    Models {
        rows: Vec<ModelRow>,
        filter: String,
        cursor: usize,
    },
    PathEntry {
        input: String,
        error: Option<String>,
    },
    /// Text entry for a Hugging Face search.
    HubSearch {
        input: String,
    },
    HubModels {
        rows: Vec<HubModelRow>,
        cursor: usize,
    },
    HubFiles {
        repository: String,
        rows: Vec<HubFileRow>,
        cursor: usize,
    },
    Account {
        cursor: usize,
    },
    Plan {
        model: PathBuf,
        rows: Vec<(&'static str, String)>,
        options: Vec<RunProfileOption>,
        cursor: usize,
        details: Option<Vec<(&'static str, String)>>,
    },
    Running,
    Reports {
        rows: Vec<ReportRow>,
        marks: Vec<bool>,
        cursor: usize,
        mode: ReportMode,
    },
    Preview {
        lines: Vec<String>,
        scroll: usize,
        reports: Vec<PathBuf>,
    },
    Info {
        title: String,
        lines: Vec<String>,
    },
}

pub(crate) const MENU_ITEMS: [(&str, &str); 7] = [
    ("Run benchmarks", "Pick a model, pick a profile, start"),
    ("Submit previous benchmarks", "Upload signed reports"),
    ("List local benchmarks", "Everything saved on this machine"),
    ("Verify a local benchmark", "Check one report's signature"),
    ("Account", "See who is signed in"),
    ("Switch runtime", "Benchmark with the other runtime"),
    ("Exit", "Leave ComputeArena"),
];

pub(crate) const SETUP_ACTIONS: [(&str, &str); 3] = [
    ("Install with ComputeArena", "Download the prebuilt release"),
    ("Check again", "After installing it yourself"),
    (
        "Continue without it",
        "Submitting, listing and verifying still work",
    ),
];

/// Scanning the model cache or verifying saved reports reads many files, so
/// both happen on a worker thread and the screen says what it is waiting for.
enum Loaded {
    Models(Vec<ModelRow>),
    Reports(Vec<ReportRow>, ReportMode),
    HubModels(Vec<HubModelRow>),
    HubFiles(String, Vec<HubFileRow>),
}

struct Pending {
    receiver: std::sync::mpsc::Receiver<Result<Loaded, String>>,
}

impl Pending {
    fn spawn<F>(work: F) -> Self
    where
        F: FnOnce() -> Result<Loaded> + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(work().map_err(|error| format!("{error:#}")));
        });
        Self { receiver }
    }
}

pub(crate) struct App {
    pub(crate) paths: Paths,
    pub(crate) api_url: String,
    pub(crate) harness_override: Option<PathBuf>,
    pub(crate) runtime: Runtime,
    pub(crate) executable: Option<PathBuf>,
    pub(crate) account: Option<String>,
    pub(crate) screens: Vec<Screen>,
    pub(crate) job: Option<Job>,
    pending: Option<Pending>,
    /// Where the running download will land, so its plan can open when it
    /// finishes.
    downloaded: Option<PathBuf>,
    pub(crate) status: String,
    pub(crate) should_quit: bool,
    /// Where the benchmark job leaves the path of the report it saved, so
    /// leaving that job can offer to submit it.
    completed_report: Arc<Mutex<Option<PathBuf>>>,
    /// A freshly saved report waiting for the person to sign in before it is
    /// offered for submission.
    pub(crate) pending_submission: Option<PathBuf>,
}

impl App {
    pub(crate) fn new(
        runtime: Runtime,
        choose_runtime: bool,
        paths: Paths,
        harness_override: Option<PathBuf>,
        api_url: String,
    ) -> Result<Self> {
        let mut app = Self {
            paths,
            api_url,
            harness_override,
            runtime,
            executable: None,
            account: None,
            screens: Vec::new(),
            job: None,
            pending: None,
            downloaded: None,
            status: String::new(),
            should_quit: false,
            completed_report: Arc::new(Mutex::new(None)),
            pending_submission: None,
        };
        app.refresh_account();
        // The same rule as the printed session: only ask which runtime to use
        // when the answer is not already obvious.
        let installed = app.installed_runtimes();
        if choose_runtime {
            // Ask only when the answer is not already obvious: one runtime
            // installed, or the one this installation used last. The header
            // names it and the menu can switch.
            let settled = match installed[..] {
                [only] => Some(only),
                _ => last_runtime(&app.paths).filter(|runtime| installed.contains(runtime)),
            };
            match settled {
                Some(runtime) => {
                    app.runtime = runtime;
                    app.enter_runtime()?;
                }
                None => app.screens.push(Screen::Runtime { cursor: 0 }),
            }
        } else {
            app.enter_runtime()?;
        }
        Ok(app)
    }

    fn installed_runtimes(&self) -> Vec<Runtime> {
        [Runtime::Basert, Runtime::LlamaCpp]
            .into_iter()
            .filter(|runtime| {
                runtimes::locate(*runtime, self.harness_override.clone(), &self.paths).is_ok()
            })
            .collect()
    }

    fn refresh_account(&mut self) {
        self.account = load_api_session(&self.paths, &self.api_url)
            .ok()
            .flatten()
            .map(|session| session.username);
    }

    /// Resolve the runtime's executable, then open either the menu or the
    /// screen explaining how to obtain it.
    fn enter_runtime(&mut self) -> Result<()> {
        let adapter = self.runtime.adapter();
        match runtimes::locate(self.runtime, self.harness_override.clone(), &self.paths)
            .and_then(|located| adapter.probe(&located.path).map(|_| located.path))
        {
            Ok(path) => {
                self.executable = Some(path);
                self.screens.push(Screen::Menu { cursor: 0 });
            }
            Err(error) => {
                self.executable = None;
                self.screens.push(Screen::Setup {
                    problem: format!("{error:#}"),
                    instructions: manual_instructions(self.runtime),
                    cursor: 0,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn runtime_label(&self) -> String {
        match &self.executable {
            Some(path) => format!("{} · {}", self.runtime.adapter().name(), compact_path(path)),
            None => format!("{} · not set up", self.runtime.adapter().name()),
        }
    }

    pub(crate) fn screen(&self) -> &Screen {
        self.screens.last().expect("a screen is always open")
    }

    fn screen_mut(&mut self) -> &mut Screen {
        self.screens.last_mut().expect("a screen is always open")
    }

    fn back(&mut self) {
        if self.screens.len() > 1 {
            self.screens.pop();
        } else {
            self.should_quit = true;
        }
    }

    fn replace(&mut self, screen: Screen) {
        self.screens.pop();
        self.screens.push(screen);
    }

    pub(crate) fn tick(&mut self) -> bool {
        let mut changed = false;
        if let Some(pending) = self.pending.as_ref() {
            match pending.receiver.try_recv() {
                Ok(loaded) => {
                    self.pending = None;
                    self.finish_loading(loaded);
                    changed = true;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.finish_loading(Err("the worker stopped unexpectedly".to_string()));
                    changed = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // The moment a job finishes, the status line says so and names the key
        // that moves on: the log pane alone did not make it obvious that the
        // interface was waiting for Enter.
        let finished_now = match self.job.as_mut() {
            Some(job) => {
                let was_finished = job.finished();
                changed |= job.poll();
                (!was_finished && job.finished())
                    .then_some((job.kind, matches!(job.outcome, Some(Ok(_)))))
            }
            None => None,
        };
        if let Some((kind, succeeded)) = finished_now {
            self.status = finished_status(kind, succeeded);
        }
        changed
    }

    fn benchmark_request<'a>(&self, model: &'a PathBuf) -> BenchmarkRequest<'a> {
        BenchmarkRequest {
            model,
            pp: DEFAULT_PREFILL_TOKENS,
            tg: DEFAULT_DECODE_TOKENS,
            reps: DEFAULT_REPETITIONS,
            warmup: DEFAULT_WARMUP_REPETITIONS,
            cooldown: false,
        }
    }

    // ---- transitions -----------------------------------------------------

    fn open_models(&mut self) -> Result<()> {
        if self.executable.is_none() {
            self.status = "Set up the runtime first".to_string();
            return Ok(());
        }
        let runtime = self.runtime;
        let paths = self.paths.clone();
        self.screens.push(Screen::Loading {
            message: match runtime {
                Runtime::Basert => "Scanning installed BaseRT models…".to_string(),
                Runtime::LlamaCpp => "Reading recent GGUF files…".to_string(),
            },
        });
        self.pending = Some(Pending::spawn(move || {
            Ok(Loaded::Models(model_rows(runtime, &paths)?))
        }));
        Ok(())
    }

    fn open_plan(&mut self, model: PathBuf) -> Result<()> {
        let model = crate::benchmark::identify_benchmark_paths(
            self.runtime,
            self.harness_override.clone(),
            &model,
            &self.paths,
        )
        .map(|(_, model)| model)?;
        let request = self.benchmark_request(&model);
        let rows = plan_rows(self.runtime, &request)?;
        let options = profile_options(self.runtime, &request)?;
        self.screens.push(Screen::Plan {
            model,
            rows,
            options,
            cursor: 0,
            details: None,
        });
        Ok(())
    }

    fn start_benchmark(&mut self, model: PathBuf, cooldown: bool) {
        let paths = self.paths.clone();
        let runtime = self.runtime;
        let harness = self.executable.clone();
        let completed = self.completed_report.clone();
        *completed.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.job = Some(Job::spawn(
            JobKind::Benchmark,
            format!("Benchmarking {}", crate::models::display_name(&model)),
            move || {
                let path = run_benchmark(
                    runtime,
                    &paths,
                    harness,
                    &model,
                    DEFAULT_PREFILL_TOKENS,
                    DEFAULT_DECODE_TOKENS,
                    DEFAULT_REPETITIONS,
                    DEFAULT_WARMUP_REPETITIONS,
                    cooldown,
                    None,
                )?;
                *completed.lock().unwrap_or_else(PoisonError::into_inner) = Some(path.clone());
                // The full path is in the log; the heading stays one line.
                Ok(format!(
                    "Saved {}",
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string())
                ))
            },
        ));
        self.screens.push(Screen::Running);
    }

    fn open_reports(&mut self, mode: ReportMode) -> Result<()> {
        let paths = self.paths.clone();
        self.screens.push(Screen::Loading {
            message: "Loading and verifying saved benchmarks…".to_string(),
        });
        self.pending = Some(Pending::spawn(move || {
            Ok(Loaded::Reports(report_rows(&paths)?, mode))
        }));
        Ok(())
    }

    /// A loader finished: swap the waiting screen for what it produced.
    fn finish_loading(&mut self, loaded: Result<Loaded, String>) {
        self.screens.pop();
        match loaded {
            Ok(Loaded::Models(rows)) => self.screens.push(Screen::Models {
                rows,
                filter: String::new(),
                cursor: 0,
            }),
            Ok(Loaded::HubModels(rows)) => {
                if rows.is_empty() {
                    self.status = "No GGUF models matched that search".to_string();
                    return;
                }
                self.screens.push(Screen::HubModels { rows, cursor: 0 });
            }
            Ok(Loaded::HubFiles(repository, rows)) => {
                self.screens.push(Screen::HubFiles {
                    repository,
                    rows,
                    cursor: 0,
                });
            }
            Ok(Loaded::Reports(rows, mode)) => {
                if rows.is_empty() {
                    self.screens.push(Screen::Info {
                        title: "No saved benchmarks".to_string(),
                        lines: vec![
                            "Nothing has been benchmarked on this machine yet.".to_string(),
                            String::new(),
                            "Run a benchmark first; reports are saved locally and signed."
                                .to_string(),
                        ],
                    });
                    return;
                }
                // Submitting starts with every valid report ticked, the usual
                // intent.
                let marks = rows
                    .iter()
                    .map(|row| mode == ReportMode::Submit && row.valid)
                    .collect();
                self.screens.push(Screen::Reports {
                    rows,
                    marks,
                    cursor: 0,
                    mode,
                });
            }
            Err(error) => self.status = format!("Failed: {error}"),
        }
    }

    fn open_preview(&mut self, reports: Vec<PathBuf>) -> Result<()> {
        let mut lines = Vec::new();
        for (index, path) in reports.iter().enumerate() {
            let value = read_report(path)?;
            lines.push(format!("Report {}/{}", index + 1, reports.len()));
            lines.push(format!("Local source (not submitted): {}", path.display()));
            lines.extend(
                serde_json::to_string_pretty(&value)?
                    .lines()
                    .map(str::to_string),
            );
            lines.push(String::new());
        }
        lines.push(
            "The fields above will be publicly accessible on ComputeArena. Local file paths are not sent."
                .to_string(),
        );
        self.screens.push(Screen::Preview {
            lines,
            scroll: 0,
            reports,
        });
        Ok(())
    }

    fn start_submission(&mut self, reports: Vec<PathBuf>) {
        let paths = self.paths.clone();
        let api_url = self.api_url.clone();
        let count = reports.len();
        self.job = Some(Job::spawn(
            JobKind::Submit,
            format!("Submitting {count} benchmark(s)"),
            move || {
                submit_reports(&paths, &reports, &api_url, true, false)?;
                Ok(format!("Submitted {count} benchmark(s)"))
            },
        ));
        self.screens.push(Screen::Running);
    }

    /// Fetch a model from the Hub, then go straight to its plan: downloading
    /// one is only ever a step towards benchmarking it.
    fn start_download(&mut self, repository: String, file: crate::huggingface::HubFile) {
        let root = self.paths.root.clone();
        let paths = self.paths.clone();
        let name = file
            .path
            .rsplit('/')
            .next()
            .unwrap_or(&file.path)
            .to_string();
        self.downloaded = Some(crate::huggingface::download_path(
            &root,
            &repository,
            &file.path,
        ));
        self.job = Some(Job::spawn(
            JobKind::Download,
            format!("Downloading {name} from {repository}"),
            move || {
                let path = crate::huggingface::download(&root, &repository, &file)?;
                crate::recent_gguf::remember(&paths, &path)?;
                Ok(format!("Downloaded {name}"))
            },
        ));
        self.screens.push(Screen::Running);
    }

    fn start_verify(&mut self, report: PathBuf) {
        self.job = Some(Job::spawn(
            JobKind::Verify,
            format!(
                "Verifying {}",
                report
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| report.display().to_string())
            ),
            move || {
                let value = read_report(&report)?;
                let key = verify_report(&value)?;
                println!("Report: {}", report.display());
                println!("Installation key: {key}");
                let run_id = value
                    .get("run_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                println!("Run ID: {run_id}");
                Ok("Signature is valid".to_string())
            },
        ));
        self.screens.push(Screen::Running);
    }

    fn start_list(&mut self) {
        let paths = self.paths.clone();
        self.job = Some(Job::spawn(JobKind::List, "Local benchmarks", move || {
            list_reports(&paths, false)?;
            Ok("Listed local benchmarks".to_string())
        }));
        self.screens.push(Screen::Running);
    }

    fn start_account(&mut self) {
        let paths = self.paths.clone();
        let api_url = self.api_url.clone();
        let job = if self.account.is_some() {
            Job::spawn(JobKind::Logout, "Logging out", move || {
                logout(&paths, &api_url)?;
                Ok("Logged out".to_string())
            })
        } else {
            Job::spawn(JobKind::Login, "Logging in", move || {
                login(&paths, &api_url)?;
                Ok("Logged in".to_string())
            })
        };
        self.job = Some(job);
        self.screens.push(Screen::Running);
    }

    fn start_install(&mut self) {
        let paths = self.paths.clone();
        let runtime = self.runtime;
        self.job = Some(Job::spawn(
            JobKind::Install,
            format!("Installing {}", runtime.adapter().display_name()),
            move || {
                let ui = crate::ui::TerminalUi::detect();
                crate::runtimes::install(runtime, &paths, ui, true, None)?;
                Ok(format!("{} installed", runtime.adapter().display_name()))
            },
        ));
        self.screens.push(Screen::Running);
    }

    /// Leaving a finished job applies whatever it changed.
    fn leave_job(&mut self) -> Result<()> {
        let Some(job) = self.job.take() else {
            self.back();
            return Ok(());
        };
        let kind = job.kind;
        let saved = job.outcome.clone();
        self.back();
        match &saved {
            Some(Ok(summary)) => self.status = summary.clone(),
            Some(Err(error)) => self.status = format!("Failed: {error}"),
            None => {}
        }
        match kind {
            JobKind::Install => {
                self.screens.clear();
                self.enter_runtime()?;
            }
            JobKind::Login | JobKind::Logout => {
                self.refresh_account();
                // Signing in from the offer made after a benchmark continues
                // straight to that benchmark's preview.
                if self.account.is_some() {
                    if let Some(report) = self.pending_submission.take() {
                        if matches!(self.screen(), Screen::Account { .. }) {
                            self.back();
                        }
                        self.offer_submission(report)?;
                    }
                }
            }
            // Finishing a benchmark or a submission is the end of that
            // errand, so both unwind to the menu rather than to the picker
            // that started them. A saved benchmark is then offered for
            // submission right away.
            JobKind::Benchmark => {
                while matches!(
                    self.screen(),
                    Screen::Models { .. } | Screen::PathEntry { .. }
                ) {
                    self.back();
                }
                let report = self
                    .completed_report
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let (Some(Ok(_)), Some(report)) = (&saved, report) {
                    self.offer_submission(report)?;
                }
            }
            JobKind::Submit => {
                while matches!(
                    self.screen(),
                    Screen::Preview { .. } | Screen::Reports { .. }
                ) {
                    self.back();
                }
            }
            // A downloaded model is only ever a step towards benchmarking it,
            // so its plan opens straight away.
            JobKind::Download => {
                while matches!(
                    self.screen(),
                    Screen::HubFiles { .. } | Screen::HubModels { .. }
                ) {
                    self.back();
                }
                if let (Some(Ok(_)), Some(path)) = (&saved, self.downloaded.take()) {
                    self.open_plan(path)?;
                }
            }
            JobKind::Verify | JobKind::List => {}
        }
        Ok(())
    }

    /// A benchmark was just saved: signed in, its preview opens with Enter
    /// ready to submit; signed out, the account screen opens first and the
    /// report waits until the sign-in completes.
    fn offer_submission(&mut self, report: PathBuf) -> Result<()> {
        if self.account.is_some() {
            self.status =
                "Benchmark saved · Enter submits it after the preview, Esc keeps it local"
                    .to_string();
            self.open_preview(vec![report])
        } else {
            self.pending_submission = Some(report);
            self.status =
                "Benchmark saved · sign in to submit it now, or Esc to keep it local".to_string();
            self.screens.push(Screen::Account { cursor: 0 });
            Ok(())
        }
    }

    /// Declining the offer: the report stays where every other saved report
    /// lives, and the status says how to submit it later.
    fn keep_local(&mut self) {
        self.pending_submission = None;
        self.status =
            "Kept locally · submit it any time from Submit previous benchmarks".to_string();
    }

    /// Whether the screen on top was opened by the offer made after a
    /// benchmark, rather than from the menu's own submission flow.
    fn offered_preview(&self) -> bool {
        matches!(self.screen(), Screen::Preview { .. })
            && matches!(self.screens.iter().rev().nth(1), Some(Screen::Menu { .. }))
    }

    // ---- key handling ----------------------------------------------------

    /// A wheel notch, or any other coarse scroll.
    pub(crate) fn scroll(&mut self, delta: isize) {
        self.move_cursor(delta);
    }

    pub(crate) fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) -> Result<()> {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return Ok(());
        }
        self.status.clear();
        match key.code {
            KeyCode::Char(character) => self.on_char(character)?,
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Home => self.move_to(0),
            KeyCode::End => self.move_to(usize::MAX),
            KeyCode::Enter => self.activate()?,
            KeyCode::Backspace => self.backspace(),
            KeyCode::Esc => self.escape()?,
            _ => {}
        }
        Ok(())
    }

    fn on_char(&mut self, character: char) -> Result<()> {
        // Typing filters and path entry take precedence over shortcuts.
        match self.screen_mut() {
            Screen::Models { filter, cursor, .. } => {
                filter.push(character);
                *cursor = 0;
                return Ok(());
            }
            Screen::PathEntry { input, error } => {
                input.push(character);
                *error = None;
                return Ok(());
            }
            Screen::HubSearch { input } => {
                input.push(character);
                return Ok(());
            }
            _ => {}
        }
        match character {
            'j' => self.move_cursor(1),
            'k' => self.move_cursor(-1),
            'q' => self.escape()?,
            ' ' => self.toggle_mark(),
            'a' => self.mark_all(),
            _ => {}
        }
        Ok(())
    }

    fn backspace(&mut self) {
        match self.screen_mut() {
            Screen::Models { filter, cursor, .. } => {
                filter.pop();
                *cursor = 0;
            }
            Screen::PathEntry { input, error } => {
                input.pop();
                *error = None;
            }
            Screen::HubSearch { input } => {
                input.pop();
            }
            _ => {}
        }
    }

    fn escape(&mut self) -> Result<()> {
        if matches!(self.screen(), Screen::Running) {
            let finished = self.job.as_ref().is_none_or(Job::finished);
            if finished {
                return self.leave_job();
            }
            self.status = "Still running — Ctrl+C quits ComputeArena".to_string();
            return Ok(());
        }
        if let Screen::Plan { details, .. } = self.screen_mut() {
            if details.is_some() {
                *details = None;
                return Ok(());
            }
        }
        if (matches!(self.screen(), Screen::Account { .. }) && self.pending_submission.is_some())
            || self.offered_preview()
        {
            self.keep_local();
        }
        self.back();
        Ok(())
    }

    pub(crate) fn visible_models(rows: &[ModelRow], filter: &str) -> Vec<usize> {
        let needle = filter.to_ascii_lowercase();
        rows.iter()
            .enumerate()
            .filter(|(_, row)| {
                needle.is_empty()
                    || row.path.is_none()
                    || row.label.to_ascii_lowercase().contains(&needle)
                    || row.detail.to_ascii_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn list_length(&self) -> usize {
        match self.screen() {
            Screen::Runtime { .. } => 2,
            Screen::Setup { .. } => SETUP_ACTIONS.len(),
            Screen::Menu { .. } => MENU_ITEMS.len(),
            Screen::Models { rows, filter, .. } => Self::visible_models(rows, filter).len(),
            Screen::Plan { options, .. } => options.len() + 1,
            Screen::Reports { rows, .. } => rows.len(),
            Screen::HubModels { rows, .. } => rows.len(),
            Screen::HubFiles { rows, .. } => rows.len(),
            Screen::Account { .. } => 2,
            Screen::Preview { lines, .. } => lines.len(),
            Screen::PathEntry { .. } | Screen::Running | Screen::Info { .. } => 0,
            Screen::HubSearch { .. } | Screen::Loading { .. } => 0,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let length = self.list_length();
        if length == 0 {
            if matches!(self.screen(), Screen::Running) {
                if let Some(job) = self.job.as_mut() {
                    job.scroll_by(delta);
                }
            }
            return;
        }
        let cursor = match self.screen_mut() {
            Screen::Runtime { cursor }
            | Screen::Setup { cursor, .. }
            | Screen::Menu { cursor }
            | Screen::Models { cursor, .. }
            | Screen::Plan { cursor, .. }
            | Screen::Reports { cursor, .. }
            | Screen::HubModels { cursor, .. }
            | Screen::HubFiles { cursor, .. }
            | Screen::Account { cursor } => cursor,
            Screen::Preview { scroll, .. } => scroll,
            _ => return,
        };
        *cursor = cursor
            .saturating_add_signed(delta)
            .min(length.saturating_sub(1));
    }

    fn move_to(&mut self, position: usize) {
        let length = self.list_length();
        match self.screen_mut() {
            Screen::Runtime { cursor }
            | Screen::Setup { cursor, .. }
            | Screen::Menu { cursor }
            | Screen::Models { cursor, .. }
            | Screen::Plan { cursor, .. }
            | Screen::Reports { cursor, .. }
            | Screen::HubModels { cursor, .. }
            | Screen::HubFiles { cursor, .. }
            | Screen::Account { cursor } => *cursor = position.min(length.saturating_sub(1)),
            Screen::Preview { scroll, .. } => *scroll = position.min(length.saturating_sub(1)),
            _ => {}
        }
    }

    fn toggle_mark(&mut self) {
        if let Screen::Reports {
            rows,
            marks,
            cursor,
            mode,
        } = self.screen_mut()
        {
            if *mode == ReportMode::Submit && rows[*cursor].valid {
                marks[*cursor] = !marks[*cursor];
            }
        }
    }

    fn mark_all(&mut self) {
        if let Screen::Reports {
            rows, marks, mode, ..
        } = self.screen_mut()
        {
            if *mode == ReportMode::Submit {
                let target = !marks.iter().all(|marked| *marked);
                for (mark, row) in marks.iter_mut().zip(rows.iter()) {
                    *mark = target && row.valid;
                }
            }
        }
    }

    fn activate(&mut self) -> Result<()> {
        match self.screen() {
            Screen::Runtime { cursor } => {
                self.runtime = if *cursor == 0 {
                    Runtime::Basert
                } else {
                    Runtime::LlamaCpp
                };
                remember_runtime(&self.paths, self.runtime);
                // Switching mid-session replaces the whole stack: the menu
                // below belonged to the previous runtime.
                self.screens.clear();
                self.enter_runtime()?;
            }
            Screen::Setup { cursor, .. } => match cursor {
                0 => self.start_install(),
                1 => {
                    self.screens.pop();
                    self.enter_runtime()?;
                }
                _ => self.replace(Screen::Menu { cursor: 0 }),
            },
            Screen::Menu { cursor } => match cursor {
                0 => self.open_models()?,
                1 => self.open_reports(ReportMode::Submit)?,
                2 => self.start_list(),
                3 => self.open_reports(ReportMode::Verify)?,
                // Signing out is a decision, not a side effect of opening the
                // account screen.
                4 => self.screens.push(Screen::Account { cursor: 0 }),
                5 => self.screens.push(Screen::Runtime { cursor: 0 }),
                _ => self.should_quit = true,
            },
            Screen::Account { cursor } => {
                if *cursor == 0 {
                    self.start_account();
                } else {
                    if self.pending_submission.is_some() {
                        self.keep_local();
                    }
                    self.back();
                }
            }
            Screen::HubSearch { input } => {
                let query = input.trim().to_string();
                if query.is_empty() {
                    self.status = "Type something to search for".to_string();
                    return Ok(());
                }
                self.screens.pop();
                self.screens.push(Screen::Loading {
                    message: format!("Searching Hugging Face for \"{query}\"…"),
                });
                self.pending = Some(Pending::spawn(move || {
                    Ok(Loaded::HubModels(
                        crate::huggingface::search(&query)?
                            .into_iter()
                            .map(|model| HubModelRow {
                                detail: format!(
                                    "{} downloads · {} likes",
                                    model.downloads, model.likes
                                ),
                                id: model.id,
                            })
                            .collect(),
                    ))
                }));
            }
            Screen::HubModels { rows, cursor } => {
                let repository = rows[*cursor].id.clone();
                self.screens.push(Screen::Loading {
                    message: format!("Listing GGUF files in {repository}…"),
                });
                self.pending = Some(Pending::spawn(move || {
                    let files = crate::huggingface::gguf_files(&repository)?
                        .into_iter()
                        .map(|file| HubFileRow {
                            label: file.path.clone(),
                            detail: crate::huggingface::format_size(file.size),
                            file,
                        })
                        .collect();
                    Ok(Loaded::HubFiles(repository, files))
                }));
            }
            Screen::HubFiles {
                repository,
                rows,
                cursor,
            } => {
                let repository = repository.clone();
                let file = crate::huggingface::HubFile {
                    path: rows[*cursor].file.path.clone(),
                    size: rows[*cursor].file.size,
                };
                self.start_download(repository, file);
            }
            Screen::Models {
                rows,
                filter,
                cursor,
            } => {
                let visible = Self::visible_models(rows, filter);
                let Some(index) = visible.get(*cursor) else {
                    return Ok(());
                };
                match (rows[*index].path.clone(), rows[*index].source) {
                    (Some(path), _) => self.open_plan(path)?,
                    (None, ModelSource::Hub) => self.screens.push(Screen::HubSearch {
                        input: String::new(),
                    }),
                    (None, ModelSource::Local) => self.screens.push(Screen::PathEntry {
                        input: String::new(),
                        error: None,
                    }),
                }
            }
            Screen::PathEntry { input, .. } => {
                let path = expand_path(input);
                match self.validate_model(&path) {
                    Ok(()) => {
                        self.screens.pop();
                        self.open_plan(path)?;
                    }
                    Err(error) => {
                        if let Screen::PathEntry { error: slot, .. } = self.screen_mut() {
                            *slot = Some(format!("{error:#}"));
                        }
                    }
                }
            }
            Screen::Plan {
                model,
                options,
                cursor,
                ..
            } => {
                if *cursor < options.len() {
                    let cooldown = options[*cursor].cooldown_enabled();
                    let model = model.clone();
                    self.screens.pop();
                    self.start_benchmark(model, cooldown);
                } else {
                    let model = model.clone();
                    let request = self.benchmark_request(&model);
                    let rows = benchmark_details(self.runtime, &request);
                    if let Screen::Plan { details, .. } = self.screen_mut() {
                        *details = Some(rows);
                    }
                }
            }
            Screen::Reports {
                rows,
                marks,
                cursor,
                mode,
            } => match mode {
                ReportMode::Verify => {
                    let path = rows[*cursor].path.clone();
                    self.start_verify(path);
                }
                ReportMode::Submit => {
                    let selected: Vec<PathBuf> = rows
                        .iter()
                        .zip(marks)
                        .filter(|(_, marked)| **marked)
                        .map(|(row, _)| row.path.clone())
                        .collect();
                    if selected.is_empty() {
                        self.status = "Select at least one valid benchmark with Space".to_string();
                    } else {
                        self.open_preview(selected)?;
                    }
                }
            },
            Screen::Preview { reports, .. } => {
                let reports = reports.clone();
                self.start_submission(reports);
            }
            Screen::Running => {
                if self.job.as_ref().is_none_or(Job::finished) {
                    self.leave_job()?;
                }
            }
            Screen::Info { .. } => self.back(),
            // Nothing to activate while a list is still loading.
            Screen::Loading { .. } => {}
        }
        Ok(())
    }

    fn validate_model(&self, path: &std::path::Path) -> Result<()> {
        if !path.is_file() {
            anyhow::bail!("no file at {}", path.display());
        }
        match self.runtime {
            Runtime::Basert => crate::models::inspect_model(path).map(|_| ()),
            Runtime::LlamaCpp => crate::adapters::llama_cpp::validate_model(path),
        }
        .context("that file is not usable with this runtime")
    }
}

const LAST_RUNTIME_FILE: &str = "last-runtime";

/// Which runtime this installation used last. Repeat visits skip the chooser
/// and land on the menu; the header names the runtime, and the menu can switch.
/// The status line shown from the moment a job finishes until the person
/// leaves it: what ended, and the key that moves on.
fn finished_status(kind: JobKind, succeeded: bool) -> String {
    let what = match kind {
        JobKind::Benchmark => "Benchmark",
        JobKind::Submit => "Submission",
        JobKind::Download => "Download",
        JobKind::Install => "Install",
        JobKind::Login => "Sign-in",
        JobKind::Logout => "Sign-out",
        JobKind::Verify => "Verification",
        JobKind::List => "Listing",
    };
    if succeeded {
        format!("{what} complete · press Enter to continue")
    } else {
        format!("{what} failed · press Enter to go back")
    }
}

pub(crate) fn last_runtime(paths: &Paths) -> Option<Runtime> {
    match std::fs::read_to_string(paths.root.join(LAST_RUNTIME_FILE))
        .ok()?
        .trim()
    {
        "basert" => Some(Runtime::Basert),
        "llama-cpp" => Some(Runtime::LlamaCpp),
        _ => None,
    }
}

fn remember_runtime(paths: &Paths, runtime: Runtime) {
    let _ = std::fs::create_dir_all(&paths.root);
    let _ = std::fs::write(paths.root.join(LAST_RUNTIME_FILE), runtime.adapter().name());
}

fn expand_path(input: &str) -> PathBuf {
    let trimmed = input.trim();
    match trimmed.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(trimmed)),
        None => PathBuf::from(trimmed),
    }
}

/// The model list, read off disk. Runs on a worker thread: scanning the BaseRT
/// cache opens every installed model's header.
fn model_rows(runtime: Runtime, paths: &Paths) -> Result<Vec<ModelRow>> {
    let mut rows: Vec<ModelRow> = match runtime {
        Runtime::Basert => {
            let installed = installed_models()?;
            let labels = model_choice_labels(&installed);
            installed
                .iter()
                .zip(labels)
                .map(|(model, label)| ModelRow {
                    label,
                    detail: crate::models::compact_home_path(&model.path),
                    path: Some(model.path.clone()),
                    source: ModelSource::Local,
                })
                .collect()
        }
        Runtime::LlamaCpp => crate::recent_gguf::recent(paths)?
            .into_iter()
            .map(|path| ModelRow {
                label: crate::models::display_name(&path),
                detail: crate::models::compact_home_path(&path),
                path: Some(path),
                source: ModelSource::Local,
            })
            .collect(),
    };
    if runtime == Runtime::LlamaCpp {
        // Most people have no GGUF on disk yet, so the Hub is offered before
        // the path prompt rather than after it.
        rows.push(ModelRow {
            label: "Search Hugging Face for a GGUF…".to_string(),
            detail: "Download a model to benchmark".to_string(),
            path: None,
            source: ModelSource::Hub,
        });
    }
    rows.push(ModelRow {
        label: match runtime {
            Runtime::Basert => "Enter another model path…".to_string(),
            Runtime::LlamaCpp => "Enter a GGUF path…".to_string(),
        },
        detail: "Type an absolute or ~/ path".to_string(),
        path: None,
        source: ModelSource::Local,
    });
    Ok(rows)
}

/// Saved reports with their signatures checked, which reads and verifies each
/// one.
fn report_rows(paths: &Paths) -> Result<Vec<ReportRow>> {
    Ok(report_summaries(paths)?
        .iter()
        .map(|report| ReportRow {
            label: report["model"]
                .as_str()
                .unwrap_or("Unknown model")
                .to_string(),
            detail: format!(
                "{}  ·  report {}",
                report["created_at"].as_str().unwrap_or("Unknown time"),
                report["short_id"].as_str().unwrap_or("unknown")
            ),
            path: PathBuf::from(report["path"].as_str().unwrap_or_default()),
            valid: report["status"].as_str() == Some("valid"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{save_api_session, ApiSession};
    use std::fs;

    fn app(dir: &std::path::Path, account: Option<&str>) -> App {
        let paths = Paths::resolve(Some(dir.join("data"))).unwrap();
        paths.prepare().unwrap();
        App {
            paths,
            api_url: "http://127.0.0.1:1/api/v1".to_string(),
            harness_override: None,
            runtime: Runtime::LlamaCpp,
            executable: None,
            account: account.map(str::to_string),
            screens: vec![Screen::Menu { cursor: 0 }],
            job: None,
            pending: None,
            downloaded: None,
            status: String::new(),
            should_quit: false,
            completed_report: Arc::new(Mutex::new(None)),
            pending_submission: None,
        }
    }

    fn saved_report(app: &App) -> PathBuf {
        let path = app.paths.reports.join("run.json");
        fs::write(&path, br#"{"run_id":"abc123"}"#).unwrap();
        *app.completed_report.lock().unwrap() = Some(path.clone());
        path
    }

    fn finished(kind: JobKind) -> Job {
        let mut job = Job::detached(Vec::new(), 4);
        job.kind = kind;
        job.outcome = Some(Ok("done".to_string()));
        job
    }

    #[test]
    fn a_saved_benchmark_opens_its_preview_when_signed_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path(), Some("isu"));
        let report = saved_report(&app);
        app.screens.push(Screen::Models {
            rows: Vec::new(),
            filter: String::new(),
            cursor: 0,
        });
        app.screens.push(Screen::Running);
        app.job = Some(finished(JobKind::Benchmark));

        // Enter on the finished job.
        app.activate().unwrap();
        assert!(matches!(
            app.screen(),
            Screen::Preview { reports, .. } if *reports == vec![report.clone()]
        ));
        // The picker that started the run is gone: Esc lands on the menu.
        assert!(matches!(
            app.screens.iter().rev().nth(1),
            Some(Screen::Menu { .. })
        ));
        assert!(app.status.contains("Enter submits"), "{}", app.status);

        app.escape().unwrap();
        assert!(matches!(app.screen(), Screen::Menu { .. }));
        assert!(app.status.starts_with("Kept locally"), "{}", app.status);
        assert!(report.exists());
    }

    #[test]
    fn a_saved_benchmark_asks_to_sign_in_first_and_continues_after_login() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path(), None);
        let report = saved_report(&app);
        app.screens.push(Screen::Running);
        app.job = Some(finished(JobKind::Benchmark));

        app.activate().unwrap();
        assert!(matches!(app.screen(), Screen::Account { .. }));
        assert_eq!(app.pending_submission.as_ref(), Some(&report));
        assert!(app.status.contains("sign in"), "{}", app.status);

        // "Log in" spawns a job that ends with a saved session; simulate both.
        save_api_session(
            &app.paths,
            &app.api_url,
            &ApiSession {
                access_token: "token".to_string(),
                username: "isu".to_string(),
                expires_at: "2999-01-01T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        app.screens.push(Screen::Running);
        app.job = Some(finished(JobKind::Login));
        app.activate().unwrap();

        assert_eq!(app.account.as_deref(), Some("isu"));
        assert!(app.pending_submission.is_none());
        assert!(matches!(
            app.screen(),
            Screen::Preview { reports, .. } if *reports == vec![report.clone()]
        ));
        assert!(matches!(
            app.screens.iter().rev().nth(1),
            Some(Screen::Menu { .. })
        ));
    }

    #[test]
    fn declining_to_sign_in_keeps_the_report_local() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path(), None);
        let report = saved_report(&app);
        app.screens.push(Screen::Running);
        app.job = Some(finished(JobKind::Benchmark));
        app.activate().unwrap();

        app.escape().unwrap();
        assert!(matches!(app.screen(), Screen::Menu { .. }));
        assert!(app.pending_submission.is_none());
        assert!(app.status.starts_with("Kept locally"), "{}", app.status);
        assert!(report.exists());
    }

    #[test]
    fn a_failed_benchmark_offers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = app(dir.path(), Some("isu"));
        app.screens.push(Screen::Running);
        let mut job = finished(JobKind::Benchmark);
        job.outcome = Some(Err("harness exited with status 1".to_string()));
        app.job = Some(job);
        app.activate().unwrap();
        assert!(matches!(app.screen(), Screen::Menu { .. }));
        assert!(app.status.starts_with("Failed:"), "{}", app.status);
    }

    #[test]
    fn a_finished_job_names_the_key_that_moves_on() {
        assert_eq!(
            finished_status(JobKind::Benchmark, true),
            "Benchmark complete · press Enter to continue"
        );
        assert_eq!(
            finished_status(JobKind::Submit, false),
            "Submission failed · press Enter to go back"
        );
    }
}
