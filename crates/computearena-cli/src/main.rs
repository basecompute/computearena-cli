mod adapters;
mod api;
use adapters::{BenchmarkRequest, Runtime};
mod auth;
mod benchmark;
mod conditioning;
mod config;
mod models;
mod protocol;
mod reports;
mod submission;
mod telemetry;
mod theme;
mod ui;

use auth::{load_api_session, login, logout, resolve_api_url};
#[cfg(test)]
use auth::{remove_api_session, save_api_session, ApiSession};
use benchmark::run_benchmark;
#[cfg(test)]
use benchmark::{validate_harness_result, validate_pp};
use config::*;
#[cfg(test)]
use models::{
    compact_home_path, display_quantization, fallback_model_name, model_choice_labels,
    InstalledModel,
};
#[cfg(test)]
use protocol::*;
#[cfg(test)]
use reports::{
    b64_encode, format_unix_ms, peak_memory_for_report, sha256_hex, sign_report,
    throughput_for_report, write_canonical_json,
};
use reports::{
    list_reports, model_identity_for_report, read_report, report_summaries, resolve_report,
    short_id, verify_report, Paths,
};
#[cfg(test)]
use submission::{parse_report_selection, preflight_submissions, should_stop_submission};
use submission::{select_reports_for_submission, submit_reports};
use ui::{finish_activity, prompt, start_activity, TerminalUi};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
#[cfg(test)]
use ed25519_dalek::SigningKey;
#[cfg(test)]
use rand_core::OsRng;
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
#[cfg(test)]
use std::fs;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "computearena",
    bin_name = "computearena",
    version,
    about = "Run, retain, and verify ComputeArena benchmarks",
    after_help = "Select a runtime: computearena basert or computearena llama-cpp"
)]
struct Cli {
    /// Override the local ComputeArena data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Path to the runtime benchmark executable.
    #[arg(long = "runtime-path", visible_alias = "harness", global = true)]
    harness: Option<PathBuf>,

    /// Override the ComputeArena API base URL.
    #[arg(long, global = true)]
    api_url: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Open the BaseRT session or run a BaseRT action.
    Basert {
        #[command(subcommand)]
        command: Option<Action>,
    },
    /// Open the llama.cpp session or run a llama.cpp action.
    #[command(name = "llama-cpp", alias = "llamacpp")]
    LlamaCpp {
        #[command(subcommand)]
        command: Option<Action>,
    },
    #[command(flatten)]
    Action(Action),
}

#[derive(Subcommand, Debug)]
enum Action {
    /// Run a prefill/decode benchmark and save a signed report.
    Run {
        /// Local model file (.base for BaseRT, .gguf for llama.cpp).
        model: Option<PathBuf>,
        /// Comma-separated prefill token counts.
        #[arg(long, default_value = DEFAULT_PREFILL_TOKENS)]
        pp: String,
        /// Decode token count per repetition.
        #[arg(long, default_value_t = DEFAULT_DECODE_TOKENS)]
        tg: u32,
        /// Recorded repetitions.
        #[arg(short = 'r', long, default_value_t = DEFAULT_REPETITIONS)]
        reps: u32,
        /// BaseRT warmup repetitions; llama.cpp uses native warmup when greater than zero.
        #[arg(short = 'w', long, default_value_t = DEFAULT_WARMUP_REPETITIONS)]
        warmup: u32,
        /// Enable adaptive thermal cooldowns before measured phases (can take substantially longer).
        #[arg(long)]
        cooldown: bool,
        /// Run without prompts (warmup-only unless --cooldown is also passed).
        #[arg(short = 'y', long)]
        yes: bool,
        /// Write to this path instead of the local report directory.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// List reports available on this installation.
    List {
        /// Emit a machine-readable JSON array.
        #[arg(long)]
        json: bool,
    },
    /// Print one report. Accepts a path, run ID, or unique run-ID prefix.
    Inspect { report: String },
    /// Verify one report's Ed25519 signature.
    Verify { report: String },
    /// Log in through a browser and connect this installation.
    Login,
    /// Revoke and remove the session for the selected API URL.
    Logout,
    /// Submit one or more saved reports. Prompts for reports when omitted.
    Submit {
        reports: Vec<String>,
        /// Submit without the preview and confirmation prompts.
        #[arg(short = 'y', long)]
        yes: bool,
        /// With --yes, exclude invalid reports instead of refusing a partial submission.
        #[arg(long, requires = "yes")]
        skip_invalid: bool,
    },
}

fn select_runtime(command: Option<Command>) -> (Runtime, Option<Action>) {
    match command {
        Some(Command::Basert { command }) => (Runtime::Basert, command),
        Some(Command::LlamaCpp { command }) => (Runtime::LlamaCpp, command),
        Some(Command::Action(command)) => (Runtime::Basert, Some(command)),
        None => (Runtime::Basert, None),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = match cli.data_dir {
        Some(path) => Some(path),
        None => match std::env::var_os("COMPUTEARENA_HOME")
            .or_else(|| std::env::var_os("BASERT_COMPUTEARENA_HOME"))
        {
            Some(path) if path.is_empty() => bail!("COMPUTEARENA_HOME is set but empty"),
            Some(path) => Some(PathBuf::from(path)),
            None => None,
        },
    };
    let paths = Paths::resolve(data_dir)?;
    let api_url = resolve_api_url(cli.api_url)?;
    let choose_runtime = cli.command.is_none();
    let (mut runtime, command) = select_runtime(cli.command);
    if choose_runtime {
        println!("Choose a runtime: 1. BaseRT  2. llama.cpp  0. Exit");
        loop {
            match prompt("Runtime: ")?.as_str() {
                "1" | "basert" => {
                    runtime = Runtime::Basert;
                    break;
                }
                "2" | "llama-cpp" | "llamacpp" => {
                    runtime = Runtime::LlamaCpp;
                    break;
                }
                "0" | "" => return Ok(()),
                _ => println!("Choose 1, 2, or 0."),
            }
        }
    }
    match command {
        Some(command) => execute(runtime, command, &paths, cli.harness, &api_url),
        None => interactive(runtime, &paths, cli.harness, &api_url),
    }
}

fn execute(
    runtime: Runtime,
    command: Action,
    paths: &Paths,
    harness: Option<PathBuf>,
    api_url: &str,
) -> Result<()> {
    match command {
        Action::Run {
            model,
            pp,
            tg,
            reps,
            warmup,
            cooldown,
            yes,
            output,
        } => {
            let model = match model {
                Some(path) => path,
                None => match runtime.adapter().select_model()? {
                    Some(path) => path,
                    None => return Ok(()),
                },
            };
            let (harness, model) = benchmark::identify_benchmark_paths(runtime, harness, &model)?;
            let Some(cooldown_enabled) = runtime.adapter().confirm(
                &BenchmarkRequest {
                    model: &model,
                    pp: &pp,
                    tg,
                    reps,
                    warmup,
                    cooldown,
                },
                yes,
            )?
            else {
                println!("Benchmark cancelled. Nothing was run.");
                return Ok(());
            };
            run_benchmark(
                runtime,
                paths,
                Some(harness),
                &model,
                &pp,
                tg,
                reps,
                warmup,
                cooldown_enabled,
                output,
            )?;
            Ok(())
        }
        Action::List { json } => list_reports(paths, json),
        Action::Inspect { report } => {
            let path = resolve_report(paths, &report)?;
            let value = read_report(&path)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Action::Verify { report } => {
            let ui = TerminalUi::detect();
            let started = start_activity(ui, "Reading and verifying the saved benchmark…");
            let path = resolve_report(paths, &report)?;
            let value = read_report(&path)?;
            let key_id = verify_report(&value)?;
            finish_activity(ui, started, "Signature is valid");
            println!("Report: {}", path.display());
            println!("Installation key: {key_id}");
            Ok(())
        }
        Action::Login => login(paths, api_url),
        Action::Logout => logout(paths, api_url),
        Action::Submit {
            reports,
            yes,
            skip_invalid,
        } => {
            let reports = select_reports_for_submission(paths, &reports, TerminalUi::detect())?;
            submit_reports(paths, &reports, api_url, yes, skip_invalid)
        }
    }
}

fn interactive(
    runtime: Runtime,
    paths: &Paths,
    harness: Option<PathBuf>,
    api_url: &str,
) -> Result<()> {
    let ui = TerminalUi::detect();

    loop {
        println!();
        println!(
            "{}",
            ui.brand("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")
        );
        println!(
            "  {}  {}",
            ui.brand_bold("ComputeArena"),
            ui.muted(format!(
                "{} · {}",
                COMPUTEARENA_WEBSITE, COMPUTEARENA_DISCORD
            ))
        );
        println!(
            "{}",
            ui.brand("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")
        );
        println!("  Runtime: {}", runtime.adapter().name());
        let session = load_api_session(paths, api_url)?;
        if let Some(session) = &session {
            println!(
                "  {} Log out ({})",
                ui.brand_bold("1."),
                ui.brand(format!("@{}", session.username))
            );
        } else {
            println!("  {} Log in", ui.brand_bold("1."));
        }
        println!("  {} Run benchmarks", ui.brand_bold("2."));
        println!("  {} Submit previous benchmarks", ui.brand_bold("3."));
        println!("  {} List local benchmarks", ui.brand_bold("4."));
        println!("  {} Verify a local benchmark", ui.brand_bold("5."));
        println!("  {} Exit", ui.brand_bold("6."));
        let choice = prompt("Choose an option: ")?;
        match choice.trim() {
            "1" => {
                if session.is_some() {
                    ui.section("Log out");
                    if let Err(error) = logout(paths, api_url) {
                        eprintln!("{} {error:#}", ui.error("Logout failed:"));
                    }
                } else {
                    ui.section("Log in");
                    if let Err(error) = login(paths, api_url) {
                        eprintln!("{} {error:#}", ui.error("Login failed:"));
                    }
                }
            }
            "2" => {
                ui.section("Run a benchmark");
                let model = match runtime.adapter().select_model() {
                    Ok(Some(model)) => model,
                    Ok(None) => continue,
                    Err(error) => {
                        eprintln!("{} {error:#}", ui.error("Could not select model:"));
                        continue;
                    }
                };
                let (selected_harness, model) =
                    match benchmark::identify_benchmark_paths(runtime, harness.clone(), &model) {
                        Ok(selected) => selected,
                        Err(error) => {
                            eprintln!("{} {error:#}", ui.error("Could not select runtime:"));
                            continue;
                        }
                    };
                let confirmation = runtime.adapter().confirm(
                    &BenchmarkRequest {
                        model: &model,
                        pp: DEFAULT_PREFILL_TOKENS,
                        tg: DEFAULT_DECODE_TOKENS,
                        reps: DEFAULT_REPETITIONS,
                        warmup: DEFAULT_WARMUP_REPETITIONS,
                        cooldown: false,
                    },
                    false,
                );
                let cooldown_enabled = match confirmation {
                    Ok(Some(enabled)) => enabled,
                    Ok(None) => {
                        println!("Benchmark cancelled. Nothing was run.");
                        continue;
                    }
                    Err(error) => {
                        eprintln!("{} {error:#}", ui.error("Could not prepare benchmark:"));
                        continue;
                    }
                };
                if let Err(error) = run_benchmark(
                    runtime,
                    paths,
                    Some(selected_harness),
                    &model,
                    DEFAULT_PREFILL_TOKENS,
                    DEFAULT_DECODE_TOKENS,
                    DEFAULT_REPETITIONS,
                    DEFAULT_WARMUP_REPETITIONS,
                    cooldown_enabled,
                    None,
                ) {
                    eprintln!("{} {error:#}", ui.error("Benchmark failed:"));
                }
            }
            "3" => {
                ui.section("Submit previous benchmarks");
                match select_reports_for_submission(paths, &[], ui) {
                    Ok(reports) if !reports.is_empty() => {
                        if let Err(error) = submit_reports(paths, &reports, api_url, false, false) {
                            eprintln!("{} {error:#}", ui.error("Submission failed:"));
                        }
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("{} {error:#}", ui.error("Could not select reports:")),
                }
            }
            "4" => {
                ui.section("Local benchmarks");
                list_reports(paths, false)?;
            }
            "5" => {
                ui.section("Verify a benchmark");
                if let Some(path) = prompt_report_choice(paths, ui)? {
                    let started = start_activity(ui, "Reading and verifying the benchmark…");
                    match read_report(&path)
                        .and_then(|value| verify_report(&value).map(|key| (value, key)))
                    {
                        Ok((value, key)) => {
                            let (model, variant) = model_identity_for_report(&value);
                            let model = variant
                                .map(|variant| format!("{model} ({variant})"))
                                .unwrap_or(model);
                            finish_activity(ui, started, "Signature is valid");
                            println!("  Model: {model}");
                            println!(
                                "  Report ID: {}",
                                value
                                    .get("run_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown")
                            );
                            println!("  Installation key: {}", short_id(&key));
                            println!("  File: {}", path.display());
                        }
                        Err(error) => {
                            eprintln!("\n{} {error:#}", ui.error("✗ Verification failed:"));
                        }
                    }
                }
            }
            "0" | "6" | "q" | "quit" | "exit" => return Ok(()),
            _ => println!("{} Choose a number from 1 to 6.", ui.warning("!")),
        }
    }
}

fn prompt_report_choice(paths: &Paths, ui: TerminalUi) -> Result<Option<PathBuf>> {
    let started = start_activity(ui, "Loading and verifying saved benchmarks…");
    let reports = report_summaries(paths)?;
    finish_activity(
        ui,
        started,
        format!("Found {} saved benchmark(s)", reports.len()),
    );
    if reports.is_empty() {
        println!("No local benchmarks are available yet. Run a benchmark first.");
        return Ok(None);
    }

    println!("Choose a saved benchmark:\n");
    for (index, report) in reports.iter().enumerate() {
        let status = if report["status"].as_str() == Some("valid") {
            ui.success("VALID")
        } else {
            ui.error("INVALID")
        };
        println!(
            "  {} {}  [{}]",
            ui.brand_bold(format!("{}.", index + 1)),
            report["model"].as_str().unwrap_or("Unknown model"),
            status
        );
        println!(
            "     {}  •  report {}",
            report["created_at"].as_str().unwrap_or("Unknown time"),
            report["short_id"].as_str().unwrap_or("unknown")
        );
    }
    println!("\n  {} Back", ui.brand_bold("0."));

    loop {
        let input = prompt("Choose a benchmark: ")?;
        let input = input.trim();
        if matches!(input, "0" | "q" | "quit" | "back") {
            return Ok(None);
        }
        if let Ok(number) = input.parse::<usize>() {
            if let Some(report) = number.checked_sub(1).and_then(|index| reports.get(index)) {
                let path = report["path"]
                    .as_str()
                    .context("saved benchmark has no file path")?;
                return Ok(Some(PathBuf::from(path)));
            }
            println!(
                "{} Choose a number from 1 to {}, or 0 to go back.",
                ui.warning("!"),
                reports.len()
            );
            continue;
        }

        // Advanced users can still paste a full path, run ID, or ID prefix.
        match resolve_report(paths, input) {
            Ok(path) => return Ok(Some(path)),
            Err(_) => println!(
                "{} Choose a listed number, or paste a valid report ID or path.",
                ui.warning("!")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_selection_is_explicit_and_cannot_be_nested() {
        let cli = Cli::try_parse_from([
            "computearena",
            "basert",
            "--harness",
            "/tmp/harness",
            "list",
        ])
        .unwrap();
        assert_eq!(cli.harness, Some(PathBuf::from("/tmp/harness")));
        assert_eq!(select_runtime(cli.command).0, Runtime::Basert);
        let cli = Cli::try_parse_from(["computearena", "llama-cpp", "list"]).unwrap();
        assert_eq!(select_runtime(cli.command).0, Runtime::LlamaCpp);
        assert!(Cli::try_parse_from(["computearena", "basert", "llama-cpp"]).is_err());
        assert!(
            Cli::try_parse_from(["computearena", "basert", "run", "llama-cpp", "model.gguf"])
                .is_err()
        );
    }

    #[test]
    fn reads_base_model_metadata_without_linking_basert() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("model.base");
        let header = serde_json::to_vec(&json!({
            "schema": 7,
            "arch": "qwen",
            "quant_scheme": "base_q4",
            "quant_profile": "default-q4",
            "target_backend": "metal",
            "source": {"sha256": "abc123"}
        }))
        .unwrap();
        let mut contents = b"BASE".to_vec();
        contents.extend_from_slice(&1_u32.to_le_bytes());
        contents.extend_from_slice(&(header.len() as u64).to_le_bytes());
        contents.extend_from_slice(&header);
        fs::write(&path, contents).unwrap();

        let model = models::inspect_model(&path).unwrap();
        assert_eq!(model["format_schema"], 7);
        assert_eq!(model["architecture"], "qwen");
        assert_eq!(model["quantization"], "base_q4");
        assert_eq!(model["source_sha256"], "abc123");
    }

    fn sample_report() -> Value {
        json!({
            "schema": REPORT_SCHEMA,
            "run_id": "test-run",
            "created_at_unix_ms": 1,
            "runtime": {"name": RUNTIME_NAME},
            "installation": {},
            "model": {"file_name": "test.base", "size_bytes": 1},
            "benchmark": {
                "schema": HARNESS_SCHEMA,
                "mode": "text",
                "raw_samples": {"prefill": {"128": []}, "decode": []}
            }
        })
    }

    fn signed_sample_report() -> Value {
        let key = SigningKey::generate(&mut OsRng);
        let mut report = sample_report();
        let public = key.verifying_key().to_bytes();
        report["installation"] = json!({
            "key_id": sha256_hex(&public),
            "public_key": b64_encode(&public)
        });
        sign_report(&mut report, &key).unwrap();
        report
    }
    #[test]
    fn canonical_json_sorts_nested_object_keys() {
        let value = json!({"z": 1, "a": {"y": true, "b": [2, 1]}});
        let mut output = Vec::new();
        write_canonical_json(&value, &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            r#"{"a":{"b":[2,1],"y":true},"z":1}"#
        );
    }

    #[test]
    fn report_selection_supports_multiple_values_and_all() {
        assert_eq!(parse_report_selection("3, 1,3", 4).unwrap(), vec![2, 0]);
        assert_eq!(parse_report_selection("all", 3).unwrap(), vec![0, 1, 2]);
        assert!(parse_report_selection("0", 3).is_err());
        assert!(parse_report_selection("4", 3).is_err());
    }

    #[test]
    fn submit_yes_flag_allows_non_interactive_submission() {
        let cli = Cli::try_parse_from(["computearena", "submit", "--yes", "report.json"]).unwrap();
        assert!(matches!(
            select_runtime(cli.command).1,
            Some(Action::Submit {
                reports,
                yes: true,
                skip_invalid: false,
            }) if reports == ["report.json"]
        ));

        let cli = Cli::try_parse_from([
            "computearena",
            "submit",
            "--yes",
            "--skip-invalid",
            "report.json",
        ])
        .unwrap();
        assert!(matches!(
            select_runtime(cli.command).1,
            Some(Action::Submit {
                skip_invalid: true,
                ..
            })
        ));
        assert!(
            Cli::try_parse_from(["computearena", "submit", "--skip-invalid", "report.json",])
                .is_err()
        );
    }

    #[test]
    fn submission_preflight_separates_invalid_reports() {
        let temporary = tempfile::tempdir().unwrap();
        let valid_path = temporary.path().join("valid.json");
        let tampered_path = temporary.path().join("tampered.json");
        let malformed_path = temporary.path().join("malformed.json");
        let valid = signed_sample_report();
        let mut tampered = valid.clone();
        tampered["model"]["size_bytes"] = json!(2);
        fs::write(&valid_path, serde_json::to_vec(&valid).unwrap()).unwrap();
        fs::write(&tampered_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        fs::write(&malformed_path, b"{not-json").unwrap();

        let preflight = preflight_submissions(&[
            valid_path.clone(),
            tampered_path.clone(),
            malformed_path.clone(),
        ]);

        assert_eq!(preflight.ready.len(), 1);
        assert_eq!(preflight.ready[0].path, valid_path);
        assert_eq!(preflight.invalid.len(), 2);
        assert_eq!(preflight.invalid[0].path, tampered_path);
        assert!(preflight.invalid[0]
            .reason
            .contains("signature verification failed"));
        assert_eq!(preflight.invalid[1].path, malformed_path);
        assert!(preflight.invalid[1].reason.starts_with("Invalid JSON:"));
    }

    #[test]
    fn api_url_is_normalized_and_validated() {
        assert_eq!(
            resolve_api_url(Some("http://127.0.0.1:8080/api/v1/".to_string())).unwrap(),
            "http://127.0.0.1:8080/api/v1"
        );
        assert!(resolve_api_url(Some("file:///tmp/server".to_string())).is_err());
    }

    #[test]
    fn login_sessions_are_isolated_by_api_url() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(temporary.path().to_path_buf())).unwrap();
        let local = ApiSession {
            access_token: "ca_cli_local-test-token".to_string(),
            username: "local-user".to_string(),
            expires_at: "2027-01-01T00:00:00.000Z".to_string(),
        };
        let production = ApiSession {
            access_token: "ca_cli_production-test-token".to_string(),
            username: "production-user".to_string(),
            expires_at: "2027-01-01T00:00:00.000Z".to_string(),
        };

        save_api_session(&paths, "http://localhost:3000/api/v1", &local).unwrap();
        save_api_session(&paths, "https://computearena.ai/api/v1", &production).unwrap();
        assert_eq!(
            load_api_session(&paths, "http://localhost:3000/api/v1")
                .unwrap()
                .unwrap()
                .username,
            "local-user"
        );
        assert_eq!(
            load_api_session(&paths, "https://computearena.ai/api/v1")
                .unwrap()
                .unwrap()
                .username,
            "production-user"
        );

        remove_api_session(&paths, "http://localhost:3000/api/v1").unwrap();
        assert!(load_api_session(&paths, "http://localhost:3000/api/v1")
            .unwrap()
            .is_none());
        assert!(load_api_session(&paths, "https://computearena.ai/api/v1")
            .unwrap()
            .is_some());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&paths.auth).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn signed_report_verifies_and_tampering_fails() {
        let mut report = signed_sample_report();
        assert!(verify_report(&report).is_ok());

        report["model"]["size_bytes"] = json!(2);
        assert_eq!(
            verify_report(&report).unwrap_err().to_string(),
            "signature verification failed; the report was modified after signing or has an invalid signature"
        );
    }

    #[test]
    fn submission_stops_only_for_systemic_http_errors() {
        assert!(!should_stop_submission(reqwest::StatusCode::BAD_REQUEST));
        assert!(!should_stop_submission(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        ));
        assert!(should_stop_submission(reqwest::StatusCode::UNAUTHORIZED));
        assert!(should_stop_submission(reqwest::StatusCode::FORBIDDEN));
        assert!(should_stop_submission(reqwest::StatusCode::REQUEST_TIMEOUT));
        assert!(should_stop_submission(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(should_stop_submission(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn invalid_prompt_sweep_is_rejected() {
        assert_eq!(
            DEFAULT_PREFILL_TOKENS,
            "128,256,512,1024,2048,4096,8192,16384"
        );
        assert!(validate_pp(DEFAULT_PREFILL_TOKENS).is_ok());
        assert!(validate_pp("128,0").is_err());
        assert!(validate_pp("128,nope").is_err());
    }

    #[test]
    fn run_flags_support_non_interactive_cooldown_execution() {
        let cli = Cli::try_parse_from(["computearena", "run", "model.base", "--cooldown", "--yes"])
            .unwrap();
        assert!(matches!(
            select_runtime(cli.command).1,
            Some(Action::Run {
                cooldown: true,
                yes: true,
                ..
            })
        ));
    }

    #[test]
    fn harness_samples_must_have_positive_counts_and_durations() {
        let valid = json!({
            "schema": HARNESS_SCHEMA,
            "mode": "text",
            "telemetry": {"schema": TELEMETRY_SCHEMA},
            "raw_samples": {
                "prefill": {"128": [{"tokens": 128, "elapsed_ns": 10}]},
                "decode": [{"generated_tokens": 128, "elapsed_ns": 20}]
            }
        });
        assert!(validate_harness_result(&valid).is_ok());

        let mut missing_telemetry = valid.clone();
        missing_telemetry["telemetry"] = Value::Null;
        assert!(validate_harness_result(&missing_telemetry).is_err());

        let mut invalid = valid;
        invalid["raw_samples"]["decode"][0]["elapsed_ns"] = json!(0);
        assert!(validate_harness_result(&invalid).is_err());
    }

    #[test]
    fn report_summary_formats_time_and_throughput() {
        assert_eq!(format_unix_ms(1_704_067_200_000), "2024-01-01 00:00:00 UTC");
        assert_eq!(short_id("6709c4b05bebaff47fbb"), "6709c4b05beb");

        let report = json!({
            "benchmark": {
                "params": {"tg": 128},
                "metrics": {
                    "decode_t_s": 91.25,
                    "pp512_t_s": 1234.5,
                    "pp128_t_s": 800.0,
                    "pp128_stddev": 2.0
                }
            }
        });
        let (prefill, decode) = throughput_for_report(&report);
        assert_eq!(prefill[0]["tokens"], 128);
        assert_eq!(prefill[1]["tokens"], 512);
        assert_eq!(decode["tokens"], 128);
        assert_eq!(decode["tokens_per_second"], 91.25);
    }

    #[test]
    fn report_summary_prefers_observed_memory_replay_peak_and_supports_legacy_reports() {
        let report = json!({
            "benchmark": {
                "memory": {"process_lifetime_peak_rss_mb": 900.0},
                "telemetry": {
                    "memory_replay": {
                        "workloads": {
                            "prefill": {
                                "128": {"process_peak_mb": 850.0},
                                "512": {"process_peak_mb": 920.0}
                            },
                            "decode": {"process_peak_mb": 910.0}
                        }
                    }
                }
            }
        });
        assert_eq!(peak_memory_for_report(&report), Some(920.0));

        let legacy = json!({"benchmark": {"memory": {"process_peak_rss_mb": 700.0}}});
        assert_eq!(peak_memory_for_report(&legacy), Some(700.0));
    }

    #[test]
    fn report_model_identity_uses_signed_model_fields() {
        let report = json!({
            "model": {
                "name": "basecompute/Qwen3-4B-Instruct-2507",
                "id": "basecompute/Qwen3-4B-Instruct-2507",
                "variant": "default-q8"
            }
        });
        assert_eq!(
            model_identity_for_report(&report),
            (
                "basecompute/Qwen3-4B-Instruct-2507".to_string(),
                Some("default-q8".to_string())
            )
        );

        assert_eq!(
            fallback_model_name(Path::new("/models/Qwen3-4B/default-q4/model.base")),
            "Qwen3-4B"
        );
        assert_eq!(
            fallback_model_name(Path::new("/models/custom-model.base")),
            "custom-model"
        );
    }

    #[test]
    fn model_selector_labels_are_compact_and_searchable() {
        let models = vec![
            InstalledModel {
                path: PathBuf::from("/models/qwen/model.base"),
                id: "Qwen/Qwen3-4B".to_string(),
                variant: "default-q4".to_string(),
                architecture: "qwen".to_string(),
                quantization: "base_q4".to_string(),
            },
            InstalledModel {
                path: PathBuf::from("/models/gemma/model.base"),
                id: "basecompute/gemma-4-E2B-it".to_string(),
                variant: "default-q8".to_string(),
                architecture: "gemma4".to_string(),
                quantization: "base_q8".to_string(),
            },
        ];
        let labels = model_choice_labels(&models);
        assert_eq!(labels.len(), 2);
        assert!(labels[0].contains("Qwen/Qwen3-4B"));
        assert!(labels[0].contains("default-q4"));
        assert!(labels[0].contains("Q4"));
        assert!(labels[0].contains("qwen"));
        assert!(!labels.iter().any(|label| label.contains("/models/")));
        assert_eq!(display_quantization("base_q8"), "Q8");
        assert_eq!(display_quantization("custom-fp8"), "custom-fp8");
    }

    #[test]
    fn selected_model_paths_abbreviate_the_home_directory() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            compact_home_path(&home.join("models/model.base")),
            "~/models/model.base"
        );
        assert_eq!(
            compact_home_path(Path::new("/var/models/model.base")),
            "/var/models/model.base"
        );
    }
}
