use crate::adapters::{file_sha256, BenchmarkRequest, Runtime};
use crate::config::{
    APPLE_CONDITIONED_PHASES_PER_WORKLOAD, APPLE_TELEMETRY_IDLE_BASELINE_SECONDS,
    CONDITIONING_FALLBACK_WAIT_SECONDS, CONDITIONING_MAXIMUM_WAIT_SECONDS,
    CONDITIONING_MINIMUM_WARMUP_SECONDS, CONDITIONING_STABLE_WINDOW_SECONDS,
    PORTABLE_CONDITIONED_PHASES_PER_WORKLOAD, TELEMETRY_WINDOW_SECONDS,
};
use crate::models::compact_home_path;
use crate::protocol::{HARNESS_SCHEMA, REPORT_SCHEMA, RUNTIME_NAME, TELEMETRY_SCHEMA};
use crate::reports::b64_encode;
use crate::reports::{
    atomic_write_json, hex, load_or_create_installation_key, sha256_hex, sign_report, Paths,
};
use crate::ui::{
    choose_default, finish_activity, print_fields, start_activity, MenuChoice, MenuItem, TerminalUi,
};
use anyhow::{bail, Context, Result};
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug)]
struct BenchmarkTiming {
    scheduled_seconds: f64,
    cooldown_phases: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BenchmarkProfile {
    Standard,
    ThermallyControlled,
}

impl BenchmarkProfile {
    fn from_cooldown(cooldown: bool) -> Self {
        if cooldown {
            Self::ThermallyControlled
        } else {
            Self::Standard
        }
    }

    fn cooldown_enabled(self) -> bool {
        matches!(self, Self::ThermallyControlled)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Standard => "Standard — warmup only",
            Self::ThermallyControlled => "Thermally controlled",
        }
    }

    fn duration(self, timing: BenchmarkTiming) -> String {
        match self {
            Self::Standard => format!("at least {}", format_duration(timing.scheduled_seconds)),
            Self::ThermallyControlled => format!(
                "{}–{}",
                format_duration(timing.cooldown_minimum_seconds()),
                format_duration(timing.cooldown_maximum_seconds())
            ),
        }
    }
}

impl BenchmarkTiming {
    fn cooldown_minimum_seconds(self) -> f64 {
        self.scheduled_seconds + self.cooldown_phases * CONDITIONING_STABLE_WINDOW_SECONDS
    }

    fn cooldown_fallback_seconds(self) -> f64 {
        self.scheduled_seconds + self.cooldown_phases * CONDITIONING_FALLBACK_WAIT_SECONDS
    }

    fn cooldown_maximum_seconds(self) -> f64 {
        self.scheduled_seconds + self.cooldown_phases * CONDITIONING_MAXIMUM_WAIT_SECONDS
    }
}

fn benchmark_timing(prefill_count: usize, apple: bool) -> BenchmarkTiming {
    let workload_count = (prefill_count + 1) as f64;
    if apple {
        BenchmarkTiming {
            scheduled_seconds: APPLE_TELEMETRY_IDLE_BASELINE_SECONDS
                + workload_count
                    * (2.0 * TELEMETRY_WINDOW_SECONDS
                        + APPLE_CONDITIONED_PHASES_PER_WORKLOAD
                            * CONDITIONING_MINIMUM_WARMUP_SECONDS),
            cooldown_phases: workload_count * APPLE_CONDITIONED_PHASES_PER_WORKLOAD + 1.0,
        }
    } else {
        BenchmarkTiming {
            scheduled_seconds: workload_count
                * (TELEMETRY_WINDOW_SECONDS
                    + PORTABLE_CONDITIONED_PHASES_PER_WORKLOAD
                        * CONDITIONING_MINIMUM_WARMUP_SECONDS),
            cooldown_phases: workload_count * PORTABLE_CONDITIONED_PHASES_PER_WORKLOAD,
        }
    }
}

/// One description of a planned run, shared by the printed plan and the
/// full-screen interface so the two can never drift apart.
pub(crate) fn plan_rows(
    runtime: Runtime,
    r: &BenchmarkRequest<'_>,
) -> Result<Vec<(&'static str, String)>> {
    let prefill_tokens = parse_pp(r.pp)?;
    let prefill = prefill_tokens
        .iter()
        .map(|n| format!("PP{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(vec![
        ("Runtime", runtime.adapter().display_name().to_string()),
        ("Model", compact_home_path(r.model)),
        ("Workloads", format!("{prefill} + TG{}", r.tg)),
        ("Sampling", sampling_summary(runtime, r)),
        ("Estimated", estimate_summary(runtime, prefill_tokens.len())),
        (
            "Output",
            "Signed JSON report saved locally\nNothing is uploaded automatically".to_string(),
        ),
    ])
}

pub(crate) const LOAD_WARNING: [&str; 2] = [
    "This creates sustained CPU/GPU load and the device may get hot. For comparable",
    "results use external power, turn off power saving, and close demanding apps.",
];

/// The plan people read before starting: what will run, on what, and where it
/// lands. Everything that is background rather than decision-making lives in
/// `benchmark_details`, one keypress away from the start menu.
pub(crate) fn print_benchmark_plan(runtime: Runtime, r: &BenchmarkRequest<'_>) -> Result<()> {
    let ui = TerminalUi::detect();
    ui.section("Benchmark plan");
    print_fields(ui, &plan_rows(runtime, r)?);
    println!();
    println!("{} {}", ui.warning("!"), LOAD_WARNING[0]);
    println!("  {}", LOAD_WARNING[1]);
    Ok(())
}

fn sampling_summary(runtime: Runtime, r: &BenchmarkRequest<'_>) -> String {
    match runtime {
        Runtime::Basert => format!(
            "{} warmup {} + {} recorded {} per workload",
            r.warmup,
            repetition_label(r.warmup),
            r.reps,
            repetition_label(r.reps)
        ),
        Runtime::LlamaCpp => format!(
            "{} + {} recorded {} per workload",
            if r.warmup == 0 {
                "No warmup"
            } else {
                "Runtime-native warmup"
            },
            r.reps,
            repetition_label(r.reps)
        ),
    }
}

/// Both run profiles, side by side, so the durations are on the plan itself and
/// not only inside the start menu.
fn estimate_summary(runtime: Runtime, prefill_count: usize) -> String {
    match runtime {
        Runtime::Basert => {
            let timing = benchmark_timing(prefill_count, cfg!(target_os = "macos"));
            format!(
                "Standard {} · thermally controlled {} (about {} without sensors)\nExcludes model loading and recorded repetitions",
                BenchmarkProfile::Standard.duration(timing),
                BenchmarkProfile::ThermallyControlled.duration(timing),
                format_duration(timing.cooldown_fallback_seconds())
            )
        }
        Runtime::LlamaCpp => {
            let count = (prefill_count + 1) as f64;
            format!(
                "Standard: no cooldown waits · thermally controlled adds {}–{} of waits\n(about {} without sensors); llama.cpp times the measured phase itself",
                format_duration(count * CONDITIONING_STABLE_WINDOW_SECONDS),
                format_duration(count * CONDITIONING_MAXIMUM_WAIT_SECONDS),
                format_duration(count * CONDITIONING_FALLBACK_WAIT_SECONDS)
            )
        }
    }
}

/// The long-form explanation, shown on request from the start menu.
pub(crate) fn benchmark_details(
    runtime: Runtime,
    r: &BenchmarkRequest<'_>,
) -> Vec<(&'static str, String)> {
    let mut rows = vec![(
        "Input",
        "Synthetic token sequences; this does not test model accuracy".to_string(),
    )];
    match runtime {
        Runtime::Basert => {
            rows.push((
                "Warmup",
                format!(
                    "Runs for at least {} before each measured phase",
                    format_duration(CONDITIONING_MINIMUM_WARMUP_SECONDS)
                ),
            ));
            rows.push((
                "Telemetry",
                "Collected by the BaseRT benchmark harness".to_string(),
            ));
        }
        Runtime::LlamaCpp => {
            if r.warmup > 0 {
                rows.push((
                    "Warmup",
                    "llama.cpp controls warmup; --warmup is not a repetition count here"
                        .to_string(),
                ));
            }
            rows.push((
                "Context",
                "Independent PP and TG tests; initial context depth 0".to_string(),
            ));
            rows.push((
                "Telemetry",
                "Process memory, temperature, and power/device snapshots where available\n(whole run; 1-second sampling)".to_string(),
            ));
        }
    }
    rows.push((
        "Report",
        "Signed locally; submission is a separate, explicit step".to_string(),
    ));
    rows
}

fn print_benchmark_details(runtime: Runtime, r: &BenchmarkRequest<'_>) {
    let ui = TerminalUi::detect();
    ui.section("What this runs");
    print_fields(ui, &benchmark_details(runtime, r));
    println!();
}

/// Resolve once before confirmation and pass these exact paths to execution.
pub(crate) fn identify_benchmark_paths(
    runtime: Runtime,
    override_path: Option<PathBuf>,
    model: &Path,
    paths: &Paths,
) -> Result<(PathBuf, PathBuf)> {
    let expand = |path: &Path| -> Result<PathBuf> {
        if let Some(rest) = path.to_str().and_then(|p| p.strip_prefix("~/")) {
            Ok(dirs::home_dir()
                .context("cannot locate home directory")?
                .join(rest))
        } else {
            Ok(path.to_path_buf())
        }
    };
    let model = fs::canonicalize(expand(model)?).context("resolving model path")?;
    let override_path = override_path.map(|p| expand(&p)).transpose()?;
    let executable = fs::canonicalize(crate::runtimes::locate(runtime, override_path, paths)?.path)
        .context("resolving runtime executable path")?;
    let ui = TerminalUi::detect();
    println!();
    print_fields(
        ui,
        &[
            (
                "computearena",
                std::env::current_exe()?.display().to_string(),
            ),
            (runtime.adapter().name(), executable.display().to_string()),
        ],
    );
    Ok((executable, model))
}

pub(crate) fn confirm_benchmark_run(
    model: &Path,
    pp: &str,
    tg: u32,
    reps: u32,
    warmup: u32,
    cooldown_requested: bool,
    skip_confirmation: bool,
) -> Result<Option<bool>> {
    if !model.is_file() {
        bail!("model does not exist or is not a file: {}", model.display());
    }
    let prefill_tokens = parse_pp(pp)?;
    if tg == 0 || reps == 0 {
        bail!("--tg and --reps must be greater than zero");
    }

    let ui = TerminalUi::detect();
    let request = BenchmarkRequest {
        model,
        pp,
        tg,
        reps,
        warmup,
        cooldown: cooldown_requested,
    };
    print_benchmark_plan(Runtime::Basert, &request)?;

    if !skip_confirmation && !io::stdin().is_terminal() {
        bail!("benchmark confirmation requires a terminal; pass --yes to run non-interactively");
    }

    let timing = benchmark_timing(prefill_tokens.len(), cfg!(target_os = "macos"));
    if skip_confirmation {
        let profile = BenchmarkProfile::from_cooldown(cooldown_requested);
        announce_profile(ui, profile.name(), &profile.duration(timing));
        return Ok(Some(profile.cooldown_enabled()));
    }

    let options = profile_options(Runtime::Basert, &request)?;
    let Some(index) = choose_run_profile(
        ui,
        Runtime::Basert,
        &request,
        &options,
        usize::from(cooldown_requested),
    )?
    else {
        return Ok(None);
    };
    let selected = &options[index];
    announce_profile(ui, &selected.label, &selected.duration);
    Ok(Some(selected.cooldown_enabled()))
}

pub(crate) fn confirm_llama_profile(r: &BenchmarkRequest<'_>, yes: bool) -> Result<Option<bool>> {
    let ui = TerminalUi::detect();
    print_benchmark_plan(Runtime::LlamaCpp, r)?;
    parse_pp(r.pp)?;
    let standard = if r.warmup == 0 {
        "Standard — no warmup"
    } else {
        "Standard — native warmup"
    };
    if !yes && !io::stdin().is_terminal() {
        bail!("benchmark confirmation requires a terminal; pass --yes to run non-interactively");
    }
    if yes {
        let profile = BenchmarkProfile::from_cooldown(r.cooldown);
        announce_profile(
            ui,
            if profile.cooldown_enabled() {
                "Thermally controlled"
            } else {
                standard
            },
            "",
        );
        return Ok(Some(profile.cooldown_enabled()));
    }

    let options = profile_options(Runtime::LlamaCpp, r)?;
    let Some(index) =
        choose_run_profile(ui, Runtime::LlamaCpp, r, &options, usize::from(r.cooldown))?
    else {
        return Ok(None);
    };
    let selected = &options[index];
    println!(
        "{} {}",
        ui.success("✓"),
        ui.strong(format!("Selected: {}", selected.label))
    );
    Ok(Some(selected.cooldown_enabled()))
}

/// A startable run profile: what to call it, how long it is expected to take,
/// and what the wait buys. Built once so the printed menu and the full-screen
/// interface offer exactly the same choices.
pub(crate) struct RunProfileOption {
    profile: BenchmarkProfile,
    pub(crate) label: String,
    pub(crate) duration: String,
    pub(crate) detail: String,
}

impl RunProfileOption {
    pub(crate) fn cooldown_enabled(&self) -> bool {
        self.profile.cooldown_enabled()
    }
}

pub(crate) fn profile_options(
    runtime: Runtime,
    r: &BenchmarkRequest<'_>,
) -> Result<Vec<RunProfileOption>> {
    let prefill_count = parse_pp(r.pp)?.len();
    Ok(match runtime {
        Runtime::Basert => {
            let timing = benchmark_timing(prefill_count, cfg!(target_os = "macos"));
            vec![
                RunProfileOption {
                    profile: BenchmarkProfile::Standard,
                    label: "Standard".to_string(),
                    duration: BenchmarkProfile::Standard.duration(timing),
                    detail: "Fastest; thermal state may affect comparability".to_string(),
                },
                RunProfileOption {
                    profile: BenchmarkProfile::ThermallyControlled,
                    label: "Thermally controlled".to_string(),
                    duration: BenchmarkProfile::ThermallyControlled.duration(timing),
                    detail: "Waits for thermal recovery between workloads".to_string(),
                },
            ]
        }
        Runtime::LlamaCpp => {
            let count = (prefill_count + 1) as f64;
            vec![
                RunProfileOption {
                    profile: BenchmarkProfile::Standard,
                    label: if r.warmup == 0 {
                        "Standard — no warmup".to_string()
                    } else {
                        "Standard — native warmup".to_string()
                    },
                    duration: "no cooldown waits".to_string(),
                    detail: "Total runtime depends on your model and device".to_string(),
                },
                RunProfileOption {
                    profile: BenchmarkProfile::ThermallyControlled,
                    label: "Thermally controlled".to_string(),
                    duration: format!(
                        "adds {}–{} of waits",
                        format_duration(count * CONDITIONING_STABLE_WINDOW_SECONDS),
                        format_duration(count * CONDITIONING_MAXIMUM_WAIT_SECONDS)
                    ),
                    detail:
                        "Runs each workload separately, reloading the model after each cooldown"
                            .to_string(),
                },
            ]
        }
    })
}

/// One prompt decides everything: which profile, and whether to start at all.
/// Picking a profile *is* the confirmation, so nothing stands between the plan
/// and the run.
fn choose_run_profile(
    ui: TerminalUi,
    runtime: Runtime,
    request: &BenchmarkRequest<'_>,
    options: &[RunProfileOption],
    default: usize,
) -> Result<Option<usize>> {
    loop {
        let mut items: Vec<MenuItem> = options
            .iter()
            .map(|option| {
                MenuItem::new(format!("Start — {} · {}", option.label, option.duration))
                    .detail(option.detail.clone())
            })
            .collect();
        items.push(
            MenuItem::new("What does this run?")
                .detail("Sampling, telemetry, and what the report contains"),
        );
        match choose_default(ui, "Start benchmark: ", &items, Some("Cancel"), default)? {
            MenuChoice::Item(index) if index < options.len() => return Ok(Some(index)),
            MenuChoice::Item(_) => print_benchmark_details(runtime, request),
            MenuChoice::Escape => return Ok(None),
        }
    }
}

fn announce_profile(ui: TerminalUi, label: &str, duration: &str) {
    if duration.is_empty() {
        println!(
            "{} {}",
            ui.success("✓"),
            ui.strong(format!("Selected: {label}"))
        );
        return;
    }
    println!(
        "{} {} — {}",
        ui.success("✓"),
        ui.strong(format!("Selected: {label}")),
        ui.accent_bold(duration)
    );
}

fn format_duration(seconds: f64) -> String {
    let seconds = seconds.ceil() as u64;
    let hours = seconds / 3_600;
    let minutes = seconds / 60;
    let remaining = seconds % 60;
    if hours > 0 {
        let mut duration = format!("{hours}h");
        if minutes % 60 > 0 {
            duration.push_str(&format!(" {}m", minutes % 60));
        }
        if remaining > 0 {
            duration.push_str(&format!(" {remaining}s"));
        }
        duration
    } else if minutes == 0 {
        format!("{remaining}s")
    } else {
        format!("{minutes}m {remaining}s")
    }
}

fn repetition_label(count: u32) -> &'static str {
    if count == 1 {
        "repetition"
    } else {
        "repetitions"
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_benchmark(
    runtime: Runtime,
    paths: &Paths,
    harness_override: Option<PathBuf>,
    model: &Path,
    pp: &str,
    tg: u32,
    reps: u32,
    warmup: u32,
    cooldown_enabled: bool,
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
    let adapter = runtime.adapter();
    let harness = crate::runtimes::locate(runtime, harness_override, paths)?.path;
    let ui = TerminalUi::detect();
    let checking_started = start_activity(
        ui,
        format!("Checking runtime compatibility with {}…", harness.display()),
    );
    let binary_sha256 = file_sha256(&harness)?;
    let descriptor = adapter.probe(&harness)?;
    finish_activity(ui, checking_started, "Benchmark harness is compatible");
    let hashing_started =
        start_activity(ui, "Hashing the model artifact (outside benchmark timing)…");
    let model_sha256 = file_sha256(model)?;
    finish_activity(ui, hashing_started, "Model artifact fingerprint ready");
    let benchmark_started = start_activity(
        ui,
        format!("Running the benchmark with {}…", harness.display()),
    );
    let result = adapter.execute(
        &harness,
        &BenchmarkRequest {
            model,
            pp,
            tg,
            reps,
            warmup,
            cooldown: cooldown_enabled,
        },
    )?;
    if file_sha256(&harness)? != binary_sha256 {
        bail!("The runtime executable changed during the benchmark. Run it again with a stable installation.");
    }
    let mut benchmark = result.benchmark;
    // Shared identity resolution for every runtime, before signing.
    crate::adapters::chip::finalize(&mut benchmark);
    finish_activity(ui, benchmark_started, "Benchmark measurements complete");

    let finalizing_started = start_activity(ui, "Verifying model artifact and signing the report…");
    if file_sha256(model)? != model_sha256 {
        bail!("The model file changed during the benchmark. No report was signed; run again with a stable model file.");
    }
    let key = load_or_create_installation_key(paths)?;
    let public = key.verifying_key();
    let public_bytes = public.to_bytes();
    let key_id = sha256_hex(&public_bytes);
    let run_id = random_id();
    let mut model_metadata = result.model;
    model_metadata["upstream_id"] = Value::Null;
    model_metadata["upstream_id_source"] = json!("unresolved");
    model_metadata["identity_verification"] = json!("unverified");
    model_metadata["artifact_sha256"] = json!(model_sha256);

    // Intentionally omit the user's account and local model path: a benchmark
    // can be created offline and attached to an authenticated account later.
    let mut report = json!({
        "schema": REPORT_SCHEMA,
        "run_id": run_id,
        "created_at_unix_ms": unix_ms(),
        "runtime": {
            "name": adapter.name(),
            "computearena_version": env!("CARGO_PKG_VERSION"),
            "binary": {
                "sha256": binary_sha256,
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "version": benchmark.get("runtime_version"),
                "descriptor": descriptor
            }
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
    if runtime == Runtime::LlamaCpp {
        if let Err(error) = crate::recent_gguf::remember(paths, model) {
            eprintln!(
                "{} Report saved, but could not update recent GGUF files: {error:#}",
                ui.warning("!")
            );
        }
    }
    Ok(path)
}

pub(crate) fn validate_pp(pp: &str) -> Result<()> {
    parse_pp(pp).map(|_| ())
}

fn parse_pp(pp: &str) -> Result<Vec<u32>> {
    let values: Result<Vec<u32>, _> = pp.split(',').map(str::parse::<u32>).collect();
    let values = values.context("--pp must be a comma-separated list of positive integers")?;
    if values.is_empty() || values.contains(&0) {
        bail!("--pp values must be greater than zero");
    }
    Ok(values)
}

pub(crate) fn validate_harness_result(value: &Value) -> Result<()> {
    if let Some(reason) = value.get("skip").and_then(Value::as_str) {
        bail!("benchmark skipped: {reason}");
    }
    if value.get("schema").and_then(Value::as_str) != Some(HARNESS_SCHEMA) {
        bail!("unsupported or missing harness schema (expected {HARNESS_SCHEMA})");
    }
    if value.get("mode").and_then(Value::as_str) != Some("text") {
        bail!("harness returned a non-text benchmark");
    }
    if value.pointer("/telemetry/schema").and_then(Value::as_str) != Some(TELEMETRY_SCHEMA) {
        bail!(
            "benchmark harness omitted supported telemetry (expected {TELEMETRY_SCHEMA}); rebuild basert-benchmark-harness"
        );
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

pub(crate) fn validate_harness_descriptor(path: &Path) -> Result<()> {
    let output = ProcessCommand::new(path)
        .args(["describe", "--json"])
        .output()
        .with_context(|| format!("describing benchmark harness {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "{} is not a compatible BaseRT benchmark harness",
            path.display()
        );
    }
    let descriptor: Value = serde_json::from_slice(&output.stdout)
        .context("benchmark harness descriptor is not valid JSON")?;
    if descriptor.get("schema").and_then(Value::as_str)
        != Some("basert-benchmark-harness-descriptor/1")
        || descriptor.pointer("/runtime/name").and_then(Value::as_str) != Some(RUNTIME_NAME)
        || descriptor.get("result_schema").and_then(Value::as_str) != Some(HARNESS_SCHEMA)
    {
        bail!(
            "{} does not advertise a compatible BaseRT benchmark protocol",
            path.display()
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_preflight_profile_duration_estimates() {
        let prefill_count = parse_pp("128,256,512,1024,2048,4096,8192,16384")
            .unwrap()
            .len();

        let apple = benchmark_timing(prefill_count, true);
        assert_eq!(format_duration(apple.scheduled_seconds), "2m 53s");
        assert_eq!(format_duration(apple.cooldown_minimum_seconds()), "7m 33s");
        assert_eq!(
            format_duration(apple.cooldown_fallback_seconds()),
            "16m 53s"
        );
        assert_eq!(
            format_duration(apple.cooldown_maximum_seconds()),
            "1h 26m 53s"
        );

        let portable = benchmark_timing(prefill_count, false);
        assert_eq!(format_duration(portable.scheduled_seconds), "1m 39s");
        assert_eq!(
            format_duration(portable.cooldown_minimum_seconds()),
            "4m 39s"
        );
        assert_eq!(
            format_duration(portable.cooldown_fallback_seconds()),
            "10m 39s"
        );
        assert_eq!(
            format_duration(portable.cooldown_maximum_seconds()),
            "55m 39s"
        );
    }

    #[test]
    fn benchmark_profiles_describe_their_timing_and_cooldown_behavior() {
        let timing = benchmark_timing(8, true);

        assert_eq!(
            BenchmarkProfile::Standard.duration(timing),
            "at least 2m 53s"
        );
        assert!(!BenchmarkProfile::Standard.cooldown_enabled());
        assert_eq!(
            BenchmarkProfile::ThermallyControlled.duration(timing),
            "7m 33s–1h 26m 53s"
        );
        assert!(BenchmarkProfile::ThermallyControlled.cooldown_enabled());
    }

    #[test]
    fn formats_hour_scale_durations_for_readability() {
        assert_eq!(format_duration(3_600.0), "1h");
        assert_eq!(format_duration(3_660.0), "1h 1m");
        assert_eq!(format_duration(5_213.0), "1h 26m 53s");
    }
}
