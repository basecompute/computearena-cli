use crate::api::{client as api_client, error_message as api_error_message};
use crate::auth::load_api_session;
use crate::config::SUBMISSION_HTTP_TIMEOUT;
use crate::model_identity::{verify_submission_model, SubmissionModelVerification};
use crate::reports::{
    model_identity_for_report, report_summaries, resolve_report, short_id, verify_report, Paths,
};
use crate::ui::{
    choose_many, finish_activity, print_menu, prompt, prompt_yes_no, rule_with_title,
    start_activity, MenuItem, TerminalUi,
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct PreparedSubmission {
    pub(crate) path: PathBuf,
    value: Value,
    bytes: Vec<u8>,
    model_verification: SubmissionModelVerification,
}

#[derive(Debug)]
pub(crate) struct InvalidSubmission {
    pub(crate) path: PathBuf,
    label: String,
    pub(crate) reason: String,
}

#[derive(Debug, Default)]
pub(crate) struct SubmissionPreflight {
    pub(crate) ready: Vec<PreparedSubmission>,
    pub(crate) invalid: Vec<InvalidSubmission>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubmissionOutcomeKind {
    Submitted,
    Duplicate,
    Rejected,
    NotAttempted,
}

#[derive(Debug)]
struct SubmissionOutcome {
    label: String,
    kind: SubmissionOutcomeKind,
    detail: Option<String>,
}
pub(crate) fn select_reports_for_submission(
    paths: &Paths,
    requested: &[String],
    ui: TerminalUi,
) -> Result<Vec<PathBuf>> {
    if !requested.is_empty() {
        let mut selected = Vec::with_capacity(requested.len());
        let mut seen = HashSet::new();
        for report in requested {
            let path = resolve_report(paths, report)?;
            if seen.insert(path.clone()) {
                selected.push(path);
            }
        }
        return Ok(selected);
    }

    let started = start_activity(ui, "Loading and verifying saved benchmarks…");
    let reports = report_summaries(paths)?;
    finish_activity(
        ui,
        started,
        format!("Found {} saved benchmark(s)", reports.len()),
    );
    if reports.is_empty() {
        println!("No local benchmarks are available yet. Run a benchmark first.");
        return Ok(Vec::new());
    }

    let items: Vec<MenuItem> = reports
        .iter()
        .map(|report| {
            let status = if report["status"].as_str() == Some("valid") {
                ui.success("VALID")
            } else {
                ui.error("INVALID")
            };
            MenuItem::new(format!(
                "{}  [{}]",
                report["model"].as_str().unwrap_or("Unknown model"),
                status
            ))
            .detail(format!(
                "{}  •  report {}",
                report["created_at"].as_str().unwrap_or("Unknown time"),
                report["short_id"].as_str().unwrap_or("unknown")
            ))
        })
        .collect();
    // Valid benchmarks start ticked: submitting everything submittable is the
    // usual intent, so most people only press Enter.
    let preselected: Vec<bool> = reports
        .iter()
        .map(|report| report["status"].as_str() == Some("valid"))
        .collect();

    let indexes = match choose_many(ui, "Benchmarks to submit: ", &items, &preselected)? {
        Some(indexes) => indexes,
        None => prompt_report_numbers(ui, &items)?,
    };
    if indexes.is_empty() {
        println!("No benchmarks selected. Nothing was submitted.");
        return Ok(Vec::new());
    }
    indexes
        .into_iter()
        .map(|index| {
            reports[index]["path"]
                .as_str()
                .map(PathBuf::from)
                .context("saved benchmark has no file path")
        })
        .collect()
}

/// Non-terminal fallback: the numbered list and a comma-separated answer.
fn prompt_report_numbers(ui: TerminalUi, items: &[MenuItem]) -> Result<Vec<usize>> {
    let count = items.len();
    println!("Choose one or more saved benchmarks:\n");
    print_menu(ui, items, None);
    println!("\nEnter numbers separated by commas, `all`, or `0` to go back.");
    loop {
        let input = prompt("Benchmarks to submit: ")?;
        let input = input.trim();
        if matches!(input, "0" | "q" | "quit" | "back") {
            return Ok(Vec::new());
        }
        match parse_report_selection(input, count) {
            Ok(indexes) => return Ok(indexes),
            Err(error) => println!("{} {error}", ui.warning("!")),
        }
    }
}

pub(crate) fn parse_report_selection(input: &str, count: usize) -> Result<Vec<usize>> {
    if input.eq_ignore_ascii_case("all") {
        return Ok((0..count).collect());
    }
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for part in input
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let number: usize = part
            .parse()
            .with_context(|| format!("{part:?} is not a benchmark number"))?;
        let index = number
            .checked_sub(1)
            .filter(|index| *index < count)
            .with_context(|| format!("choose numbers from 1 to {count}"))?;
        if seen.insert(index) {
            selected.push(index);
        }
    }
    if selected.is_empty() {
        bail!("choose at least one benchmark");
    }
    Ok(selected)
}

pub(crate) fn submit_reports(
    paths: &Paths,
    reports: &[PathBuf],
    api_url: &str,
    assume_yes: bool,
    skip_invalid: bool,
) -> Result<()> {
    if reports.is_empty() {
        return Ok(());
    }
    let ui = TerminalUi::detect();
    let checking_started = start_activity(ui, "Checking selected benchmarks…");
    let mut preflight = preflight_submissions(reports);
    finish_activity(
        ui,
        checking_started,
        format!("Checked {} benchmark(s)", reports.len()),
    );
    if !preflight.ready.is_empty() {
        let model_started =
            start_activity(ui, "Verifying exact model artifacts with Hugging Face…");
        verify_submission_models(&mut preflight);
        finish_activity(ui, model_started, "Model artifact check complete");
    }
    print_submission_preflight(ui, &preflight);

    if assume_yes && !skip_invalid && !preflight.invalid.is_empty() {
        bail!(
            "refusing a partial non-interactive submission; review the invalid reports or pass --yes --skip-invalid"
        );
    }
    if preflight.ready.is_empty() {
        bail!("no valid benchmarks were selected; nothing was uploaded");
    }

    let session = load_api_session(paths, api_url)?.with_context(|| format!(
        "Login is required to upload benchmarks. Run `computearena --api-url {api_url} login`, then retry. Offline benchmarks and saved reports are unchanged."
    ))?;

    println!();
    println!(
        "Ready to submit {} benchmark(s) to {api_url} as @{}.",
        preflight.ready.len(),
        session.username
    );
    println!(
        "{}",
        ui.neutral(
            "Only the valid benchmarks listed as ready will be uploaded. Their data will be publicly accessible on ComputeArena."
        )
    );

    if !assume_yes {
        if !io::stdin().is_terminal() {
            bail!("submission confirmation requires a terminal; pass --yes to submit non-interactively");
        }
        if prompt_yes_no("Preview the JSON data before submitting?", true)? {
            print_submission_preview(ui, &preflight.ready)?;
        }
        if !prompt_yes_no(
            &format!(
                "Submit the {} valid benchmark(s) now?",
                preflight.ready.len()
            ),
            false,
        )? {
            println!(
                "{} Nothing was uploaded.",
                ui.neutral("Submission cancelled.")
            );
            return Ok(());
        }
    }

    let endpoint = format!("{api_url}/submissions");
    let client = api_client(SUBMISSION_HTTP_TIMEOUT)?;
    let report_count = preflight.ready.len();
    let mut outcomes = Vec::with_capacity(report_count);
    let mut queue = preflight.ready.into_iter().enumerate();
    while let Some((index, report)) = queue.next() {
        let run_id = report
            .value
            .get("run_id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let label = submission_label(&report.value, &report.path);
        let started = start_activity(
            ui,
            format!(
                "[{}/{}] Submitting report {}…",
                index + 1,
                report_count,
                short_id(&run_id)
            ),
        );
        let request = client
            .post(&endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(&session.access_token)
            .body(report.bytes);
        match request.send() {
            Ok(response) => {
                let status = response.status();
                let body = response.text().unwrap_or_default();
                if status.is_success() {
                    let duplicate = status == reqwest::StatusCode::OK;
                    let submission_id =
                        serde_json::from_str::<Value>(&body).ok().and_then(|value| {
                            value.get("id").and_then(Value::as_str).map(str::to_owned)
                        });
                    finish_activity(
                        ui,
                        started,
                        if duplicate {
                            "Already submitted".to_string()
                        } else {
                            "Benchmark submitted".to_string()
                        },
                    );
                    if let Ok(response) = serde_json::from_str::<Value>(&body) {
                        if let Some(provenance) = response.get("runtime_provenance") {
                            if let Some(message) = provenance.get("message").and_then(Value::as_str)
                            {
                                println!("{}", ui.neutral(message));
                            }
                            if let Some(url) =
                                provenance.get("download_url").and_then(Value::as_str)
                            {
                                println!("{}", ui.neutral(format!("Official downloads: {url}")));
                            }
                        }
                        if let Some(verification) = response.get("model_verification") {
                            let status = verification
                                .get("status")
                                .and_then(Value::as_str)
                                .unwrap_or("unresolved");
                            if let Some(message) =
                                verification.get("message").and_then(Value::as_str)
                            {
                                let marker = if status == "verified" {
                                    ui.success("✓")
                                } else {
                                    ui.warning("!")
                                };
                                println!("{marker} Server model verification: {message}");
                            }
                        }
                    }
                    outcomes.push(SubmissionOutcome {
                        label,
                        kind: if duplicate {
                            SubmissionOutcomeKind::Duplicate
                        } else {
                            SubmissionOutcomeKind::Submitted
                        },
                        detail: submission_id.map(|id| format!("Submission ID: {id}")),
                    });
                } else {
                    let message = api_error_message(&body)
                        .unwrap_or_else(|| format!("server returned HTTP {}", status.as_u16()));
                    eprintln!("{} {message}", ui.error("✗"));
                    outcomes.push(SubmissionOutcome {
                        label,
                        kind: SubmissionOutcomeKind::Rejected,
                        detail: Some(message.clone()),
                    });
                    if should_stop_submission(status) {
                        let reason = format!(
                            "Not attempted after the server returned HTTP {}.",
                            status.as_u16()
                        );
                        outcomes.extend(queue.map(|(_, pending)| SubmissionOutcome {
                            label: submission_label(&pending.value, &pending.path),
                            kind: SubmissionOutcomeKind::NotAttempted,
                            detail: Some(reason.clone()),
                        }));
                        break;
                    }
                }
            }
            Err(error) => {
                eprintln!("{} {error}", ui.error("✗"));
                outcomes.push(SubmissionOutcome {
                    label,
                    kind: SubmissionOutcomeKind::Rejected,
                    detail: Some(error.to_string()),
                });
                outcomes.extend(queue.map(|(_, pending)| SubmissionOutcome {
                    label: submission_label(&pending.value, &pending.path),
                    kind: SubmissionOutcomeKind::NotAttempted,
                    detail: Some(
                        "Not attempted because the connection to ComputeArena failed.".to_string(),
                    ),
                }));
                break;
            }
        }
    }

    print_submission_results(ui, &outcomes);
    let submitted = outcomes
        .iter()
        .filter(|outcome| outcome.kind == SubmissionOutcomeKind::Submitted)
        .count();
    let duplicates = outcomes
        .iter()
        .filter(|outcome| outcome.kind == SubmissionOutcomeKind::Duplicate)
        .count();
    let failures = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.kind,
                SubmissionOutcomeKind::Rejected | SubmissionOutcomeKind::NotAttempted
            )
        })
        .count();
    if failures > 0 {
        bail!(
            "{failures} of {report_count} eligible benchmark(s) were not submitted; successful submissions remain saved"
        );
    }
    println!(
        "{} Submission complete: {submitted} uploaded, {duplicates} already present.",
        ui.success("✓"),
    );
    Ok(())
}

pub(crate) fn preflight_submissions(reports: &[PathBuf]) -> SubmissionPreflight {
    let mut preflight = SubmissionPreflight::default();
    for path in reports {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                preflight.invalid.push(InvalidSubmission {
                    path: path.clone(),
                    label: submission_file_label(path),
                    reason: format!("Could not read the report: {error}"),
                });
                continue;
            }
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                preflight.invalid.push(InvalidSubmission {
                    path: path.clone(),
                    label: submission_file_label(path),
                    reason: format!("Invalid JSON: {error}"),
                });
                continue;
            }
        };
        if let Err(error) = verify_report(&value) {
            preflight.invalid.push(InvalidSubmission {
                path: path.clone(),
                label: submission_label(&value, path),
                reason: error.to_string(),
            });
            continue;
        }
        preflight.ready.push(PreparedSubmission {
            path: path.clone(),
            value,
            bytes,
            model_verification: SubmissionModelVerification::Unresolved("not checked".to_string()),
        });
    }
    preflight
}

fn verify_submission_models(preflight: &mut SubmissionPreflight) {
    let candidates = std::mem::take(&mut preflight.ready);
    for mut report in candidates {
        let verification = verify_submission_model(&report.value);
        if let SubmissionModelVerification::Mismatch(reason) = verification {
            preflight.invalid.push(InvalidSubmission {
                label: submission_label(&report.value, &report.path),
                path: report.path,
                reason: format!("Model provenance mismatch: {reason}"),
            });
        } else {
            report.model_verification = verification;
            preflight.ready.push(report);
        }
    }
}

fn print_submission_preflight(ui: TerminalUi, preflight: &SubmissionPreflight) {
    println!();
    println!("{}", ui.brand_bold("Submission check complete"));
    println!(
        "  {:<22} {}",
        "Ready to submit",
        ui.success(preflight.ready.len())
    );

    let mut verified = 0;
    let mut unresolved = 0;
    let mut unavailable = 0;
    for report in &preflight.ready {
        match report.model_verification {
            SubmissionModelVerification::Verified(_) => verified += 1,
            SubmissionModelVerification::Unresolved(_) => unresolved += 1,
            SubmissionModelVerification::Unavailable(_) => unavailable += 1,
            SubmissionModelVerification::Mismatch(_) => unreachable!("mismatches are invalid"),
        }
    }
    println!("\n{}", ui.brand_bold("Model identity"));
    println!("  {:<22} {}", "Verified artifacts", ui.success(verified));
    println!("  {:<22} {}", "Unresolved", ui.neutral(unresolved));
    println!("  {:<22} {}", "Check unavailable", ui.warning(unavailable));
    for report in &preflight.ready {
        let detail = match &report.model_verification {
            SubmissionModelVerification::Verified(message) => {
                Some((ui.success("✓"), message.as_str()))
            }
            SubmissionModelVerification::Unresolved(message) => {
                Some((ui.neutral("—"), message.as_str()))
            }
            SubmissionModelVerification::Unavailable(message) => {
                Some((ui.warning("!"), message.as_str()))
            }
            SubmissionModelVerification::Mismatch(_) => None,
        };
        if let Some((marker, message)) = detail {
            println!(
                "  {marker} {}",
                submission_label(&report.value, &report.path)
            );
            println!("    {}", ui.neutral(message));
        }
    }
    println!(
        "  {:<22} {}  {}",
        "Invalid reports",
        if preflight.invalid.is_empty() {
            ui.neutral(0)
        } else {
            ui.error(preflight.invalid.len())
        },
        ui.neutral("— will not be uploaded")
    );

    if !preflight.invalid.is_empty() {
        println!("\n{} Invalid reports:", ui.warning("!"));
        for invalid in &preflight.invalid {
            println!("  {} {}", ui.error("✗"), invalid.label);
            println!("    {}", ui.neutral(&invalid.reason));
            println!("    {}", ui.muted(invalid.path.display()));
        }
    }
    println!();
    println!("{}", ui.neutral(crate::reports::SIGNATURE_SCOPE_NOTICE));
}

fn submission_file_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

fn submission_label(report: &Value, path: &Path) -> String {
    let (model, variant) = model_identity_for_report(report);
    let model = match variant {
        Some(variant) => format!("{model} ({variant})"),
        None => model,
    };
    let run_id = report
        .get("run_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if model == "Unknown model" && run_id == "unknown" {
        submission_file_label(path)
    } else {
        format!("{model} · report {}", short_id(run_id))
    }
}

pub(crate) fn should_stop_submission(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::REQUEST_TIMEOUT
            | reqwest::StatusCode::TOO_MANY_REQUESTS
    ) || status.is_server_error()
}

fn print_submission_results(ui: TerminalUi, outcomes: &[SubmissionOutcome]) {
    ui.section("Submission results");
    for outcome in outcomes {
        let (marker, status) = match outcome.kind {
            SubmissionOutcomeKind::Submitted => (ui.success("✓"), "Submitted"),
            SubmissionOutcomeKind::Duplicate => (ui.neutral("="), "Already submitted"),
            SubmissionOutcomeKind::Rejected => (ui.error("✗"), "Rejected"),
            SubmissionOutcomeKind::NotAttempted => (ui.warning("—"), "Not attempted"),
        };
        println!("  {marker} {} — {status}", outcome.label);
        if let Some(detail) = &outcome.detail {
            println!("    {}", ui.neutral(detail));
        }
    }
}

fn print_submission_preview(ui: TerminalUi, reports: &[PreparedSubmission]) -> Result<()> {
    println!();
    println!(
        "{}",
        ui.neutral(rule_with_title(
            "SUBMISSION PREVIEW · NOT YET UPLOADED",
            '─'
        ))
    );
    for (index, report) in reports.iter().enumerate() {
        println!();
        println!(
            "{}",
            ui.neutral(format!("Report {}/{}", index + 1, reports.len()))
        );
        println!(
            "{} {}",
            ui.muted("Local source (not submitted):"),
            ui.muted(report.path.display())
        );
        println!(
            "{}",
            ui.neutral(serde_json::to_string_pretty(&report.value)?)
        );
    }
    println!("{}", ui.neutral(rule_with_title("END PREVIEW", '─')));
    println!(
        "\n{}",
        ui.neutral(
            "The JSON fields above will be publicly accessible on ComputeArena. Local file paths are not sent."
        )
    );
    Ok(())
}
