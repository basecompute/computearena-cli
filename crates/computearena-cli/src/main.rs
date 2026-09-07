mod api;
mod auth;
mod config;
mod protocol;
mod submission;
mod theme;
mod ui;

use auth::{load_api_session, login, logout, resolve_api_url};
#[cfg(test)]
use auth::{remove_api_session, save_api_session, ApiSession};
use config::*;
use protocol::*;
#[cfg(test)]
use submission::{parse_report_selection, preflight_submissions, should_stop_submission};
use submission::{select_reports_for_submission, submit_reports};
use theme::model_selector_theme;
use ui::{finish_activity, prompt, start_activity, TerminalUi};

use anyhow::{bail, Context, Result};
use base_format::BaseReader;
use base_sign::{b64_decode, b64_encode, sign_payload, signing_key_from_bytes, verify_payload};
use clap::{Parser, Subcommand};
use dialoguer::FuzzySelect;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(
    name = "basert-computearena",
    bin_name = "basert computearena",
    version,
    about = "Run, retain, and verify ComputeArena benchmarks"
)]
struct Cli {
    /// Override the local ComputeArena data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Override the benchmark harness executable.
    #[arg(long, global = true)]
    harness: Option<PathBuf>,

    /// Override the ComputeArena API base URL.
    #[arg(long, global = true)]
    api_url: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a BaseRT prefill/decode benchmark and save a signed report.
    Run {
        /// Path to a local `.base` model. Prompted for when omitted.
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
        /// Warmup repetitions (not recorded).
        #[arg(short = 'w', long, default_value_t = DEFAULT_WARMUP_REPETITIONS)]
        warmup: u32,
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

#[derive(Clone, Debug)]
struct Paths {
    root: PathBuf,
    reports: PathBuf,
    secret_key: PathBuf,
    auth: PathBuf,
}

#[derive(Clone, Debug)]
struct InstalledModel {
    path: PathBuf,
    id: String,
    variant: String,
    architecture: String,
    quantization: String,
}

impl Paths {
    fn resolve(override_root: Option<PathBuf>) -> Result<Self> {
        let root = match override_root {
            Some(path) => path,
            None => dirs::data_local_dir()
                .context("could not determine the local data directory")?
                .join("basert")
                .join("computearena"),
        };
        Ok(Self {
            reports: root.join("reports"),
            secret_key: root.join("keys").join("installation.ed25519"),
            auth: root.join("auth.json"),
            root,
        })
    }

    fn prepare(&self) -> Result<()> {
        fs::create_dir_all(&self.reports)
            .with_context(|| format!("creating {}", self.reports.display()))?;
        fs::create_dir_all(
            self.secret_key
                .parent()
                .context("installation key path has no parent")?,
        )
        .with_context(|| format!("creating key directory under {}", self.root.display()))?;
        Ok(())
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
        None => match std::env::var_os("BASERT_COMPUTEARENA_HOME") {
            Some(path) if path.is_empty() => bail!("BASERT_COMPUTEARENA_HOME is set but empty"),
            Some(path) => Some(PathBuf::from(path)),
            None => None,
        },
    };
    let paths = Paths::resolve(data_dir)?;
    let api_url = resolve_api_url(cli.api_url)?;
    match cli.command {
        Some(command) => execute(command, &paths, cli.harness, &api_url),
        None => interactive(&paths, cli.harness, &api_url),
    }
}

fn execute(command: Command, paths: &Paths, harness: Option<PathBuf>, api_url: &str) -> Result<()> {
    match command {
        Command::Run {
            model,
            pp,
            tg,
            reps,
            warmup,
            output,
        } => {
            let model = match model {
                Some(path) => path,
                None => match prompt_model_path()? {
                    Some(path) => path,
                    None => return Ok(()),
                },
            };
            run_benchmark(paths, harness, &model, &pp, tg, reps, warmup, output)?;
            Ok(())
        }
        Command::List { json } => list_reports(paths, json),
        Command::Inspect { report } => {
            let path = resolve_report(paths, &report)?;
            let value = read_report(&path)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Command::Verify { report } => {
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
        Command::Login => login(paths, api_url),
        Command::Logout => logout(paths, api_url),
        Command::Submit {
            reports,
            yes,
            skip_invalid,
        } => {
            let reports = select_reports_for_submission(paths, &reports, TerminalUi::detect())?;
            submit_reports(paths, &reports, api_url, yes, skip_invalid)
        }
    }
}

fn interactive(paths: &Paths, harness: Option<PathBuf>, api_url: &str) -> Result<()> {
    let ui = TerminalUi::detect();
    ui.banner();
    loop {
        println!();
        println!(
            "{}",
            ui.brand("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")
        );
        println!(
            "  {}  {}",
            ui.brand_bold("ComputeArena"),
            ui.muted("computearena.ai")
        );
        println!(
            "{}",
            ui.brand("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")
        );
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
                let Some(model) = prompt_model_path()? else {
                    continue;
                };
                if let Err(error) = run_benchmark(
                    paths,
                    harness.clone(),
                    &model,
                    DEFAULT_PREFILL_TOKENS,
                    DEFAULT_DECODE_TOKENS,
                    DEFAULT_REPETITIONS,
                    DEFAULT_WARMUP_REPETITIONS,
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

#[allow(clippy::too_many_arguments)]
fn run_benchmark(
    paths: &Paths,
    harness_override: Option<PathBuf>,
    model: &Path,
    pp: &str,
    tg: u32,
    reps: u32,
    warmup: u32,
    output: Option<PathBuf>,
) -> Result<PathBuf> {
    if !model.is_file() {
        bail!("model does not exist or is not a file: {}", model.display());
    }
    validate_pp(pp)?;
    if tg == 0 || reps == 0 {
        bail!("--tg and --reps must be greater than zero");
    }

    paths.prepare()?;
    let harness = resolve_harness(harness_override)?;
    let ui = TerminalUi::detect();
    let benchmark_started = start_activity(
        ui,
        format!(
            "Running the BaseRT prefill/decode benchmark with {}…",
            harness.display()
        ),
    );
    let result = ProcessCommand::new(&harness)
        .arg(model)
        .args(["--mode", "text", "-p", pp, "-n"])
        .arg(tg.to_string())
        .args(["-r"])
        .arg(reps.to_string())
        .args(["-w"])
        .arg(warmup.to_string())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("launching benchmark harness {}", harness.display()))?;
    if !result.status.success() {
        bail!("benchmark harness exited with {}", result.status);
    }
    let benchmark: Value = serde_json::from_slice(&result.stdout)
        .context("benchmark harness did not return one valid JSON object")?;
    validate_harness_result(&benchmark)?;
    finish_activity(ui, benchmark_started, "Benchmark measurements complete");

    let finalizing_started = start_activity(ui, "Reading model metadata and signing the report…");
    let key = load_or_create_installation_key(paths)?;
    let public = key.verifying_key();
    let public_bytes = public.to_bytes();
    let key_id = sha256_hex(&public_bytes);
    let run_id = random_id();
    let model_metadata = inspect_model(model)?;

    // Intentionally omit the user's account and local model path: a benchmark
    // can be created offline and attached to an authenticated account later.
    let mut report = json!({
        "schema": REPORT_SCHEMA,
        "run_id": run_id,
        "created_at_unix_ms": unix_ms(),
        "runtime": {
            "name": RUNTIME_NAME,
            "computearena_version": env!("CARGO_PKG_VERSION")
        },
        "installation": {
            "key_id": key_id,
            "public_key": b64_encode(&public_bytes)
        },
        "model": model_metadata,
        "benchmark": benchmark
    });
    sign_report(&mut report, &key)?;

    let path = output.unwrap_or_else(|| paths.reports.join(format!("{run_id}.json")));
    atomic_write_json(&path, &report)?;
    let digest = sha256_hex(&fs::read(&path)?);
    finish_activity(ui, finalizing_started, "Report finalized and signed");
    println!("Saved signed benchmark: {}", path.display());
    println!("Run ID: {run_id}");
    println!("Report SHA-256: {digest}");
    Ok(path)
}

fn inspect_model(path: &Path) -> Result<Value> {
    let file = fs::metadata(path)
        .with_context(|| format!("reading model metadata for {}", path.display()))?;
    let header = BaseReader::read_header(path)
        .with_context(|| format!("reading BaseRT model header from {}", path.display()))?;
    let fallback_name = fallback_model_name(path);
    let mut model = json!({
        "name": fallback_name,
        "file_name": path.file_name().and_then(|name| name.to_str()).unwrap_or("unknown"),
        "size_bytes": file.len(),
        "format_schema": header.schema,
        "architecture": header.arch,
        "quantization": header.quant_scheme,
        "quant_profile": header.quant_profile,
        "target_backend": header.target_backend,
        "source_sha256": header.source.sha256
    });
    if let Some((id, variant)) = model_identity_from_path(path)? {
        model["name"] = json!(id);
        model["id"] = json!(id);
        model["variant"] = json!(variant);
    }
    Ok(model)
}

fn fallback_model_name(path: &Path) -> String {
    let file_name = path.file_name().and_then(|name| name.to_str());
    if file_name == Some("model.base") {
        return path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .or_else(|| path.parent().and_then(Path::file_name))
            .and_then(|name| name.to_str())
            .unwrap_or("Unknown model")
            .to_string();
    }
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Unknown model")
        .to_string()
}

fn validate_pp(pp: &str) -> Result<()> {
    let values: Result<Vec<u32>, _> = pp.split(',').map(str::parse::<u32>).collect();
    let values = values.context("--pp must be a comma-separated list of positive integers")?;
    if values.is_empty() || values.contains(&0) {
        bail!("--pp values must be greater than zero");
    }
    Ok(())
}

fn validate_harness_result(value: &Value) -> Result<()> {
    if let Some(reason) = value.get("skip").and_then(Value::as_str) {
        bail!("benchmark skipped: {reason}");
    }
    if value.get("schema").and_then(Value::as_str) != Some(HARNESS_SCHEMA) {
        bail!("unsupported or missing harness schema (expected {HARNESS_SCHEMA})");
    }
    if value.get("mode").and_then(Value::as_str) != Some("text") {
        bail!("harness returned a non-text benchmark");
    }
    let raw = value
        .get("raw_samples")
        .context("harness result is missing raw_samples")?;
    let prefill = raw
        .get("prefill")
        .and_then(Value::as_object)
        .context("harness result is missing prefill samples")?;
    if prefill.is_empty() {
        bail!("harness returned no prefill sample groups");
    }
    for samples in prefill.values() {
        validate_duration_samples(samples, "tokens")?;
    }
    validate_duration_samples(
        raw.get("decode")
            .context("harness result is missing decode samples")?,
        "generated_tokens",
    )?;
    Ok(())
}

fn validate_duration_samples(value: &Value, token_key: &str) -> Result<()> {
    let samples = value.as_array().context("raw samples must be an array")?;
    if samples.is_empty() {
        bail!("raw sample array is empty");
    }
    for sample in samples {
        if sample.get(token_key).and_then(Value::as_u64).unwrap_or(0) == 0 {
            bail!("raw sample has an invalid {token_key}");
        }
        if sample
            .get("elapsed_ns")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            == 0
        {
            bail!("raw sample has an invalid elapsed_ns");
        }
    }
    Ok(())
}

fn resolve_harness(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return executable_path(path);
    }
    if let Some(path) = std::env::var_os("BASERT_COMPUTEARENA_HARNESS") {
        if path.is_empty() {
            bail!("BASERT_COMPUTEARENA_HARNESS is set but empty");
        }
        return executable_path(PathBuf::from(path));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sibling = parent.join(PRIMARY_HARNESS_NAME);
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
        // Source-tree builds place the Rust binary under
        // tools/base-convert/target/{debug,release} and the C++ harness under
        // the repository's build/. Walk ancestors so invocation does not
        // depend on the caller's current directory.
        for ancestor in exe.ancestors() {
            for name in [PRIMARY_HARNESS_NAME, LEGACY_HARNESS_NAME] {
                let candidate = ancestor.join("build").join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    for development in DEVELOPMENT_HARNESS_PATHS {
        let path = PathBuf::from(development);
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Some(path) = executable_on_path(PRIMARY_HARNESS_NAME) {
        return Ok(path);
    }
    bail!(
        "benchmark harness was not found; from the BaseRT repository root, run:\n  \
         cmake -S . -B build -DCMAKE_BUILD_TYPE=Release\n  \
         cmake --build build --target {LEGACY_HARNESS_NAME}"
    )
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn executable_path(path: PathBuf) -> Result<PathBuf> {
    if path.components().count() == 1 {
        return Ok(path);
    }
    if !path.is_file() {
        bail!("benchmark harness not found: {}", path.display());
    }
    Ok(path)
}

fn load_or_create_installation_key(paths: &Paths) -> Result<SigningKey> {
    if paths.secret_key.is_file() {
        return load_installation_key(&paths.secret_key);
    }
    paths.prepare()?;
    let key = SigningKey::generate(&mut OsRng);
    let parent = paths
        .secret_key
        .parent()
        .context("installation key path has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating installation key under {}", parent.display()))?;
    set_private_permissions(temp.as_file())?;
    temp.write_all(&key.to_bytes())?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(&paths.secret_key) {
        Ok(_) => Ok(key),
        Err(_error) if paths.secret_key.is_file() => load_installation_key(&paths.secret_key),
        Err(error) => Err(error.error)
            .with_context(|| format!("saving installation key to {}", paths.secret_key.display())),
    }
}

fn load_installation_key(path: &Path) -> Result<SigningKey> {
    let bytes =
        fs::read(path).with_context(|| format!("reading installation key {}", path.display()))?;
    signing_key_from_bytes(&bytes)
}

#[cfg(unix)]
fn set_private_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &fs::File) -> Result<()> {
    Ok(())
}

fn sign_report(report: &mut Value, key: &SigningKey) -> Result<()> {
    if report.get("signature").is_some() {
        bail!("refusing to sign a report that already has a signature");
    }
    let payload = signature_payload(report)?;
    let signature = sign_payload(key, &payload);
    report
        .as_object_mut()
        .context("report must be a JSON object")?
        .insert(
            "signature".to_string(),
            json!({
                "algorithm": SIGNATURE_ALGORITHM,
                "canonicalization": SIGNATURE_CANONICALIZATION,
                "value": b64_encode(&signature.to_bytes())
            }),
        );
    Ok(())
}

fn verify_report(report: &Value) -> Result<String> {
    if report.get("schema").and_then(Value::as_str) != Some(REPORT_SCHEMA) {
        bail!("unsupported or missing report schema (expected {REPORT_SCHEMA})");
    }
    let signature_value = report.get("signature").context("report is unsigned")?;
    if signature_value.get("algorithm").and_then(Value::as_str) != Some(SIGNATURE_ALGORITHM) {
        bail!("unsupported signature algorithm");
    }
    if signature_value
        .get("canonicalization")
        .and_then(Value::as_str)
        != Some(SIGNATURE_CANONICALIZATION)
    {
        bail!("unsupported signature canonicalization");
    }
    let signature_bytes = b64_decode(
        signature_value
            .get("value")
            .and_then(Value::as_str)
            .context("signature value is missing")?,
    )?;
    let signature =
        Signature::from_slice(&signature_bytes).context("invalid Ed25519 signature length")?;
    let installation = report
        .get("installation")
        .context("installation identity is missing")?;
    let public_bytes = b64_decode(
        installation
            .get("public_key")
            .and_then(Value::as_str)
            .context("installation public key is missing")?,
    )?;
    let public_array: [u8; 32] = public_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Ed25519 public key length"))?;
    let public = VerifyingKey::from_bytes(&public_array).context("invalid Ed25519 public key")?;
    let expected_key_id = sha256_hex(&public_array);
    let key_id = installation
        .get("key_id")
        .and_then(Value::as_str)
        .context("installation key ID is missing")?;
    if key_id != expected_key_id {
        bail!("installation key ID does not match its public key");
    }
    let mut unsigned = report.clone();
    unsigned
        .as_object_mut()
        .context("report must be a JSON object")?
        .remove("signature");
    let payload = signature_payload(&unsigned)?;
    verify_payload(&public, &payload, &signature).map_err(|_| {
        anyhow::anyhow!(
            "signature verification failed; the report was modified after signing or has an invalid signature"
        )
    })?;
    Ok(key_id.to_string())
}

fn signature_payload(unsigned_report: &Value) -> Result<Vec<u8>> {
    let mut payload = Vec::from(SIGNATURE_DOMAIN);
    write_canonical_json(unsigned_report, &mut payload)?;
    Ok(payload)
}

// ComputeArena JSON v1 supports ordinary JSON values, recursively sorts object
// keys by UTF-8 bytes, preserves array order, and uses serde_json's stable
// string/number encoding. The named protocol keeps this independent from the
// pretty-printed file representation and leaves room for an RFC 8785 migration.
fn write_canonical_json(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(value) => out.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => out.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => out.extend_from_slice(serde_json::to_string(value)?.as_bytes()),
        Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(b']');
        }
        Value::Object(values) => {
            out.push(b'{');
            let mut keys: Vec<&String> = values.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key)?.as_bytes());
                out.push(b':');
                write_canonical_json(&values[*key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    if path.exists() {
        bail!("refusing to overwrite existing report: {}", path.display());
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating report under {}", parent.display()))?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("saving report to {}", path.display()))?;
    Ok(())
}

fn list_reports(paths: &Paths, as_json: bool) -> Result<()> {
    let ui = TerminalUi::detect();
    let reports = if as_json {
        report_summaries(paths)?
    } else {
        let started = start_activity(ui, "Loading and verifying saved benchmarks…");
        let reports = report_summaries(paths)?;
        finish_activity(
            ui,
            started,
            format!("Found {} saved benchmark(s)", reports.len()),
        );
        reports
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(());
    }
    if reports.is_empty() {
        println!("No local benchmarks found in {}", paths.reports.display());
        return Ok(());
    }
    println!(
        "{}",
        ui.brand_bold(format!("Local benchmarks ({})", reports.len()))
    );
    for (index, report) in reports.iter().enumerate() {
        let status = report["status"].as_str().unwrap_or("invalid");
        let status_label = if status == "valid" {
            ui.success("VALID")
        } else {
            ui.error("INVALID")
        };
        println!(
            "\n  {} {}  [{}]",
            ui.brand_bold(format!("{}.", index + 1)),
            report["model"].as_str().unwrap_or("Unknown model"),
            status_label
        );
        println!(
            "     Ran: {}",
            report["created_at"].as_str().unwrap_or("Unknown time")
        );
        println!(
            "     Device: {} ({})",
            report["device"].as_str().unwrap_or("Unknown device"),
            report["backend"].as_str().unwrap_or("unknown backend")
        );
        println!(
            "     Runtime: BaseRT {} | {} | {}",
            report["runtime_version"].as_str().unwrap_or("unknown"),
            report["architecture"]
                .as_str()
                .unwrap_or("unknown architecture"),
            report["quantization"]
                .as_str()
                .unwrap_or("unknown quantization")
        );

        println!("     Throughput:");
        let prefill = report["prefill"].as_array().cloned().unwrap_or_default();
        if prefill.is_empty() {
            println!("       Prefill    Unavailable");
        } else {
            for (sample_index, value) in prefill.iter().enumerate() {
                println!(
                    "       {:<10} {:>5} tokens   {:>10.1} tok/s",
                    if sample_index == 0 { "Prefill" } else { "" },
                    value["tokens"].as_u64().unwrap_or(0),
                    value["tokens_per_second"].as_f64().unwrap_or(0.0)
                );
            }
        }
        if let Some(decode) = report["decode"].as_object() {
            println!(
                "       {:<10} {:>5} tokens   {:>10.1} tok/s",
                "Decode",
                decode.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                decode
                    .get("tokens_per_second")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            );
        } else {
            println!("       Decode     Unavailable");
        }

        let mut telemetry = Vec::new();
        if let Some(memory) = report["peak_memory_mb"].as_f64() {
            telemetry.push(format!("peak memory {memory:.0} MiB"));
        }
        if let Some(temperature) = report["ending_temperature_c"].as_f64() {
            telemetry.push(format!("ending temperature {temperature:.1}°C"));
        }
        if !telemetry.is_empty() {
            println!("     Telemetry: {}", telemetry.join(" | "));
        }
        println!(
            "     Report ID: {}",
            report["short_id"].as_str().unwrap_or("unknown")
        );
        if status != "valid" {
            println!("     Verification: {status}");
        }
    }
    println!("\nStored in: {}", paths.reports.display());
    Ok(())
}

fn report_summaries(paths: &Paths) -> Result<Vec<Value>> {
    if !paths.reports.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = fs::read_dir(&paths.reports)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    let mut reports = Vec::new();
    for path in files {
        match read_report(&path) {
            Ok(value) => {
                let status = match verify_report(&value) {
                    Ok(_) => "valid".to_string(),
                    Err(error) => format!("invalid: {error}"),
                };
                let (model_id, variant) = model_identity_for_report(&value);
                let model = match variant.as_deref() {
                    Some(variant) => format!("{model_id} ({variant})"),
                    None => model_id.clone(),
                };
                let (prefill, decode) = throughput_for_report(&value);
                let created_at_unix_ms = value
                    .get("created_at_unix_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let run_id = value
                    .get("run_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                reports.push(json!({
                    "run_id": run_id,
                    "short_id": short_id(run_id),
                    "model": model,
                    "model_id": model_id,
                    "variant": variant,
                    "architecture": value.pointer("/model/architecture").and_then(Value::as_str),
                    "quantization": value.pointer("/model/quantization").and_then(Value::as_str),
                    "created_at_unix_ms": created_at_unix_ms,
                    "created_at": format_unix_ms(created_at_unix_ms),
                    "device": value.pointer("/benchmark/chip").and_then(Value::as_str),
                    "backend": value.pointer("/benchmark/backend").and_then(Value::as_str),
                    "runtime_version": value.pointer("/benchmark/runtime_version").and_then(Value::as_str)
                        .or_else(|| value.pointer("/runtime/computearena_version").and_then(Value::as_str)),
                    "prefill": prefill,
                    "decode": decode,
                    "peak_memory_mb": value.pointer("/benchmark/memory/process_peak_rss_mb").and_then(Value::as_f64),
                    "ending_temperature_c": value.pointer("/benchmark/thermal/die_end_c").and_then(Value::as_f64),
                    "status": status,
                    "path": path
                }));
            }
            Err(error) => reports.push(json!({
                "run_id": "unknown",
                "short_id": "unknown",
                "model": "unknown",
                "created_at_unix_ms": null,
                "created_at": "Unknown time",
                "status": format!("invalid: {error}"),
                "path": path
            })),
        }
    }
    reports.sort_by(|left, right| {
        right["created_at_unix_ms"]
            .as_u64()
            .cmp(&left["created_at_unix_ms"].as_u64())
    });
    Ok(reports)
}

fn model_identity_for_report(report: &Value) -> (String, Option<String>) {
    if let Some(id) = report
        .pointer("/model/id")
        .or_else(|| report.pointer("/model/name"))
        .and_then(Value::as_str)
    {
        return (
            id.to_string(),
            report
                .pointer("/model/variant")
                .and_then(Value::as_str)
                .map(str::to_string),
        );
    }

    let file_name = report
        .pointer("/model/file_name")
        .and_then(Value::as_str)
        .unwrap_or("Unknown model");
    if file_name != "model.base" {
        return (file_name.to_string(), None);
    }
    let architecture = report
        .pointer("/model/architecture")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let quantization = report
        .pointer("/model/quantization")
        .and_then(Value::as_str)
        .unwrap_or("unknown quantization");
    (
        format!("Legacy {architecture} model ({quantization}; name unavailable)"),
        None,
    )
}

fn throughput_for_report(report: &Value) -> (Vec<Value>, Value) {
    let metrics = report
        .pointer("/benchmark/metrics")
        .and_then(Value::as_object);
    let mut prefill: Vec<(u64, f64)> = metrics
        .into_iter()
        .flat_map(|metrics| metrics.iter())
        .filter_map(|(name, value)| {
            let tokens = name
                .strip_prefix("pp")?
                .strip_suffix("_t_s")?
                .parse()
                .ok()?;
            Some((tokens, value.as_f64()?))
        })
        .collect();
    prefill.sort_by_key(|(tokens, _)| *tokens);
    let prefill = prefill
        .into_iter()
        .map(|(tokens, tokens_per_second)| {
            json!({"tokens": tokens, "tokens_per_second": tokens_per_second})
        })
        .collect();

    let decode = metrics
        .and_then(|metrics| metrics.get("decode_t_s"))
        .and_then(Value::as_f64)
        .map(|tokens_per_second| {
            json!({
                "tokens": report.pointer("/benchmark/params/tg").and_then(Value::as_u64),
                "tokens_per_second": tokens_per_second
            })
        })
        .unwrap_or(Value::Null);
    (prefill, decode)
}

fn short_id(run_id: &str) -> &str {
    run_id.get(..12).unwrap_or(run_id)
}

fn format_unix_ms(milliseconds: u64) -> String {
    if milliseconds == 0 {
        return "Unknown time".to_string();
    }
    let seconds = milliseconds / 1000;
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_date_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

// Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn resolve_report(paths: &Paths, reference: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(reference);
    if direct.is_file() {
        return Ok(direct);
    }
    let exact = paths.reports.join(reference);
    if exact.is_file() {
        return Ok(exact);
    }
    let exact_json = paths.reports.join(format!("{reference}.json"));
    if exact_json.is_file() {
        return Ok(exact_json);
    }
    if !paths.reports.is_dir() {
        bail!("report not found: {reference}");
    }
    let mut matches = Vec::new();
    for entry in fs::read_dir(&paths.reports)? {
        let path = entry?.path();
        if path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.starts_with(reference))
        {
            matches.push(path);
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!("report not found: {reference}"),
        _ => bail!("report prefix is ambiguous: {reference}"),
    }
}

fn read_report(path: &Path) -> Result<Value> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("opening report {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing report {}", path.display()))
}

fn prompt_model_path() -> Result<Option<PathBuf>> {
    let ui = TerminalUi::detect();
    let started = start_activity(ui, "Scanning installed BaseRT model metadata…");
    let installed = discover_installed_models()?;
    finish_activity(
        ui,
        started,
        format!("Found {} compatible model(s)", installed.len()),
    );
    if installed.is_empty() {
        return model_path_from_input(prompt("Model path: ")?).map(Some);
    }

    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        prompt_model_path_interactive(&installed, ui)
    } else {
        prompt_model_path_numbered(&installed, ui)
    }
}

fn prompt_model_path_interactive(
    installed: &[InstalledModel],
    ui: TerminalUi,
) -> Result<Option<PathBuf>> {
    let mut choices = model_choice_labels(installed);
    choices.push("Enter another model path…".to_string());
    println!(
        "{}",
        ui.neutral("Type to filter · ↑/↓ move · Enter select · Esc back")
    );
    io::stdout().flush()?;

    let theme = model_selector_theme();
    let selected = FuzzySelect::with_theme(&theme)
        .with_prompt(format!("Select a model · {} installed", installed.len()))
        .items(&choices)
        .max_length(MODEL_SELECTOR_VISIBLE_ROWS)
        .report(false)
        .interact_opt()
        .context("reading model selection")?;

    let Some(index) = selected else {
        println!("{} Model selection cancelled", ui.neutral("←"));
        return Ok(None);
    };
    let Some(model) = installed.get(index) else {
        return model_path_from_input(prompt("Model path: ")?).map(Some);
    };
    print_selected_model(model, ui);
    Ok(Some(model.path.clone()))
}

fn prompt_model_path_numbered(
    installed: &[InstalledModel],
    ui: TerminalUi,
) -> Result<Option<PathBuf>> {
    println!("Installed BaseRT models:");
    for (index, label) in model_choice_labels(installed).iter().enumerate() {
        println!("  {} {label}", ui.brand_bold(format!("{}.", index + 1)));
    }
    println!("  {} Enter another model path", ui.brand_bold("p."));
    let input = prompt("Choose a model number or enter a path: ")?;
    if matches!(input.to_ascii_lowercase().as_str(), "q" | "quit" | "back") {
        return Ok(None);
    }
    if let Ok(index) = input.parse::<usize>() {
        let model = index
            .checked_sub(1)
            .and_then(|i| installed.get(i))
            .context("model selection is out of range")?;
        print_selected_model(model, ui);
        return Ok(Some(model.path.clone()));
    }
    if input.eq_ignore_ascii_case("p") {
        return model_path_from_input(prompt("Model path: ")?).map(Some);
    }
    model_path_from_input(input).map(Some)
}

fn model_choice_labels(installed: &[InstalledModel]) -> Vec<String> {
    let id_width = installed
        .iter()
        .map(|model| model.id.chars().count())
        .max()
        .unwrap_or_default()
        .min(MODEL_ID_COLUMN_WIDTH);
    let variant_width = installed
        .iter()
        .map(|model| model.variant.chars().count())
        .max()
        .unwrap_or_default()
        .min(MODEL_VARIANT_COLUMN_WIDTH);
    let quantizations: Vec<String> = installed
        .iter()
        .map(|model| display_quantization(&model.quantization))
        .collect();
    let quant_width = quantizations
        .iter()
        .map(|quantization| quantization.chars().count())
        .max()
        .unwrap_or_default()
        .min(MODEL_QUANT_COLUMN_WIDTH);

    installed
        .iter()
        .zip(quantizations)
        .map(|(model, quantization)| {
            format!(
                "{:<id_width$}  {:<variant_width$}  {:<quant_width$}  {}",
                model.id, model.variant, quantization, model.architecture
            )
        })
        .collect()
}

fn display_quantization(value: &str) -> String {
    value
        .strip_prefix("base_q")
        .filter(|bits| !bits.is_empty() && bits.chars().all(|character| character.is_ascii_digit()))
        .map_or_else(|| value.to_string(), |bits| format!("Q{bits}"))
}

fn print_selected_model(model: &InstalledModel, ui: TerminalUi) {
    println!(
        "\n{} {}/{}",
        ui.success("Selected"),
        model.id,
        model.variant
    );
    println!("  Architecture  {}", model.architecture);
    println!(
        "  Quantisation  {}",
        display_quantization(&model.quantization)
    );
    println!("  Path          {}", compact_home_path(&model.path));
}

fn compact_home_path(path: &Path) -> String {
    dirs::home_dir()
        .and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
        .map_or_else(
            || path.display().to_string(),
            |relative| format!("~/{}", relative.display()),
        )
}

fn model_path_from_input(input: String) -> Result<PathBuf> {
    if input.is_empty() {
        bail!("a model path is required");
    }
    Ok(PathBuf::from(input))
}

fn model_cache_root() -> Result<PathBuf> {
    let root = match std::env::var_os("BASERT_MODELS_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        Some(_) => bail!("BASERT_MODELS_DIR is set but empty"),
        None => dirs::cache_dir()
            .context("could not determine the BaseRT model cache")?
            .join("baseRT")
            .join("models"),
    };
    Ok(root)
}

fn model_identity_from_path(path: &Path) -> Result<Option<(String, String)>> {
    let root = model_cache_root()?;
    let Some(parent) = path.parent() else {
        return Ok(None);
    };
    let Ok(relative) = parent.strip_prefix(root) else {
        return Ok(None);
    };
    let mut components: Vec<String> = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str().map(str::to_string))
        .collect();
    if components.len() < 2 {
        return Ok(None);
    }
    let variant = components.pop().unwrap_or_default();
    Ok(Some((components.join("/"), variant)))
}

fn discover_installed_models() -> Result<Vec<InstalledModel>> {
    let root = model_cache_root()?;
    if !root.is_dir() {
        return Ok(Vec::new());
    }

    let mut directories = vec![root.clone()];
    let mut models = Vec::new();
    while let Some(directory) = directories.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) != Some(".src") {
                    directories.push(path);
                }
            } else if file_type.is_file()
                && path.file_name().and_then(|name| name.to_str()) == Some("model.base")
            {
                let header = match BaseReader::read_header(&path) {
                    Ok(header) => header,
                    Err(_) => continue,
                };
                if header.arch == "whisper" {
                    continue;
                }
                let quantization = serde_json::to_value(header.quant_scheme)?
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();
                let Some((id, variant)) = model_identity_from_path(&path)? else {
                    continue;
                };
                models.push(InstalledModel {
                    architecture: header.arch,
                    quantization,
                    variant,
                    id,
                    path,
                });
            }
        }
    }
    models.sort_by(|left, right| (&left.id, &left.variant).cmp(&(&right.id, &right.variant)));
    Ok(models)
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn random_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    hex(&bytes)
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let cli =
            Cli::try_parse_from(["basert-computearena", "submit", "--yes", "report.json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Submit {
                reports,
                yes: true,
                skip_invalid: false,
            }) if reports == ["report.json"]
        ));

        let cli = Cli::try_parse_from([
            "basert-computearena",
            "submit",
            "--yes",
            "--skip-invalid",
            "report.json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Submit {
                skip_invalid: true,
                ..
            })
        ));
        assert!(Cli::try_parse_from([
            "basert-computearena",
            "submit",
            "--skip-invalid",
            "report.json",
        ])
        .is_err());
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
        assert!(validate_pp("128,512").is_ok());
        assert!(validate_pp("128,0").is_err());
        assert!(validate_pp("128,nope").is_err());
    }

    #[test]
    fn harness_samples_must_have_positive_counts_and_durations() {
        let valid = json!({
            "schema": HARNESS_SCHEMA,
            "mode": "text",
            "raw_samples": {
                "prefill": {"128": [{"tokens": 128, "elapsed_ns": 10}]},
                "decode": [{"generated_tokens": 128, "elapsed_ns": 20}]
            }
        });
        assert!(validate_harness_result(&valid).is_ok());

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
