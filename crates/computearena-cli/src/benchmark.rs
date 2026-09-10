use crate::adapters::{file_sha256, BenchmarkRequest, Runtime};
use crate::config::{
    APPLE_CONDITIONED_PHASES_PER_WORKLOAD, APPLE_TELEMETRY_IDLE_BASELINE_SECONDS,
    BASERT_HARNESS_NAME, CONDITIONING_FALLBACK_WAIT_SECONDS, CONDITIONING_MAXIMUM_WAIT_SECONDS,
    CONDITIONING_MINIMUM_WARMUP_SECONDS, CONDITIONING_STABLE_WINDOW_SECONDS,
    PORTABLE_CONDITIONED_PHASES_PER_WORKLOAD, TELEMETRY_WINDOW_SECONDS,
};
use crate::protocol::{HARNESS_SCHEMA, REPORT_SCHEMA, RUNTIME_NAME, TELEMETRY_SCHEMA};
use crate::reports::b64_encode;
use crate::reports::{
    atomic_write_json, hex, load_or_create_installation_key, sha256_hex, sign_report, Paths,
};
use crate::ui::{finish_activity, prompt, prompt_yes_no, start_activity, TerminalUi};
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

pub(crate) fn print_benchmark_plan(runtime: Runtime, r: &BenchmarkRequest<'_>) -> Result<()> {
    let ui = TerminalUi::detect();
    let prefill = parse_pp(r.pp)?
        .iter()
        .map(|n| format!("PP{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    ui.section("Benchmark plan");
    println!("  {} {}", ui.neutral("Runtime:"), runtime.adapter().name());
    println!("  {} {}", ui.neutral("Model:"), r.model.display());
    println!("  {} {prefill}", ui.neutral("Prefill:"));
    println!("  {} TG{}", ui.neutral("Decode:"), r.tg);
    match runtime {
        Runtime::Basert => {
            println!(
                "  {} {} requested warmup {} + {} recorded {} per throughput workload",
                ui.neutral("Sampling:"),
                r.warmup,
                repetition_label(r.warmup),
                r.reps,
                repetition_label(r.reps)
            );
            println!(
                "            Warmup runs for at least {} before each measured phase",
                format_duration(CONDITIONING_MINIMUM_WARMUP_SECONDS)
            );
            println!(
                "  {} Collected by the BaseRT benchmark harness",
                ui.neutral("Telemetry:")
            );
        }
        Runtime::LlamaCpp => {
            println!(
                "  {} {} + {} recorded {} per throughput workload",
                ui.neutral("Sampling:"),
                if r.warmup == 0 {
                    "No warmup"
                } else {
                    "Runtime-native warmup"
                },
                r.reps,
                repetition_label(r.reps)
            );
            if r.warmup > 0 {
                println!("            llama.cpp controls warmup; --warmup is not a repetition count for this runtime.");
            }
            println!(
                "  {} Independent PP and TG tests; initial context depth 0",
                ui.neutral("Context:")
            );
            println!(
                "  {} Automatic process memory, temperature sensors, and power/device snapshots where available (whole run; 1-second sampling)",
                ui.neutral("Telemetry:")
            );
        }
    }
    println!(
        "  {} Synthetic token sequences; this does not test model accuracy",
        ui.neutral("Input:")
    );
    println!(
        "  {} Signed JSON report saved locally",
        ui.neutral("Output:")
    );
    println!("          Nothing is uploaded automatically");
    println!();
    println!(
        "{} This creates sustained CPU/GPU load and can consume substantial memory.",
        ui.warning("!")
    );
    println!("  The device may become hot during the benchmark.");
    println!("  For comparable results, connect external power, disable power-saving mode,");
    println!("  and close demanding apps.");
    Ok(())
}

/// Resolve once before confirmation and pass these exact paths to execution.
pub(crate) fn identify_benchmark_paths(
    runtime: Runtime,
    override_path: Option<PathBuf>,
    model: &Path,
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
    let executable = fs::canonicalize(runtime.adapter().discover(override_path)?)
        .context("resolving runtime executable path")?;
    let ui = TerminalUi::detect();
    ui.section("Selected binaries");
    println!(
        "  {} {}",
        ui.neutral("ComputeArena:"),
        std::env::current_exe()?.display()
    );
    println!(
        "  {} {}",
        ui.neutral(format!("{}:", runtime.adapter().name())),
        executable.display()
    );
    println!("  {} Compatibility is checked before execution; release provenance is checked at submission.", ui.muted("Note:"));
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
    print_benchmark_plan(
        Runtime::Basert,
        &BenchmarkRequest {
            model,
            pp,
            tg,
            reps,
            warmup,
            cooldown: cooldown_requested,
        },
    )?;

    if !skip_confirmation && !io::stdin().is_terminal() {
        bail!("benchmark confirmation requires a terminal; pass --yes to run non-interactively");
    }

    let timing = benchmark_timing(prefill_tokens.len(), cfg!(target_os = "macos"));
    print_run_profiles(ui, timing);
    let profile = if cooldown_requested || skip_confirmation {
        BenchmarkProfile::from_cooldown(cooldown_requested)
    } else {
        prompt_benchmark_profile()?
    };
    println!(
        "{} {} — {}",
        ui.success("✓"),
        ui.strong(format!("Selected: {}", profile.name())),
        ui.accent_bold(profile.duration(timing))
    );

    if skip_confirmation {
        return Ok(Some(profile.cooldown_enabled()));
    }
    if prompt_yes_no("Start this benchmark?", false)? {
        Ok(Some(profile.cooldown_enabled()))
    } else {
        Ok(None)
    }
}

pub(crate) fn confirm_llama_profile(r: &BenchmarkRequest<'_>, yes: bool) -> Result<Option<bool>> {
    let ui = TerminalUi::detect();
    print_benchmark_plan(Runtime::LlamaCpp, r)?;
    let count = (parse_pp(r.pp)?.len() + 1) as f64;
    let standard = if r.warmup == 0 {
        "Standard — no warmup"
    } else {
        "Standard — native warmup"
    };
    println!();
    println!("{}", ui.brand_bold("Run profile"));
    println!(
        "  {}  {} {}",
        ui.strong("1"),
        ui.strong(standard),
        ui.neutral("(default)")
    );
    println!(
        "     {}",
        ui.accent_bold("No cooldown waits; total runtime depends on your model and device")
    );
    println!();
    println!(
        "  {}  {}",
        ui.strong("2"),
        ui.strong("Thermally controlled")
    );
    println!(
        "     {}",
        ui.accent_bold(format!(
            "Adds approximately {}–{} of cooldown waits",
            format_duration(count * CONDITIONING_STABLE_WINDOW_SECONDS),
            format_duration(count * CONDITIONING_MAXIMUM_WAIT_SECONDS)
        ))
    );
    println!(
        "     {}",
        ui.muted(format!(
            "Without usable die-temperature sensors: about {} of timed rests",
            format_duration(count * CONDITIONING_FALLBACK_WAIT_SECONDS)
        ))
    );
    println!(
        "     {}",
        ui.muted("Runs each PP size and TG separately; reloads the model after each cooldown.")
    );
    println!(
        "     {}",
        ui.muted(
            "Waits precede loading and native warmup, not the measured phase inside llama.cpp."
        )
    );
    println!("  {}",ui.muted("Total time = loading + native warmup + recorded work + the waits above; no calibrated total estimate yet."));
    if !yes && !io::stdin().is_terminal() {
        bail!("benchmark confirmation requires a terminal; pass --yes to run non-interactively");
    }
    let selected = if r.cooldown || yes {
        BenchmarkProfile::from_cooldown(r.cooldown)
    } else {
        prompt_benchmark_profile()?
    };
    println!(
        "{} {}",
        ui.success("✓"),
        ui.strong(format!(
            "Selected: {}",
            if selected.cooldown_enabled() {
                "Thermally controlled"
            } else {
                standard
            }
        ))
    );
    if yes || prompt_yes_no("Start this benchmark?", false)? {
        Ok(Some(selected.cooldown_enabled()))
    } else {
        Ok(None)
    }
}

fn print_run_profiles(ui: TerminalUi, timing: BenchmarkTiming) {
    println!();
    println!("{}", ui.brand_bold("Run profile"));
    println!();
    println!(
        "  {}  {} {}",
        ui.strong("1"),
        ui.strong(BenchmarkProfile::Standard.name()),
        ui.neutral("(default)")
    );
    println!(
        "     {}",
        ui.accent_bold(BenchmarkProfile::Standard.duration(timing))
    );
    println!(
        "     {}",
        ui.muted("Fastest option. Thermal state may affect comparability.")
    );
    println!();
    println!(
        "  {}  {}",
        ui.strong("2"),
        ui.strong(BenchmarkProfile::ThermallyControlled.name())
    );
    println!(
        "     {}",
        ui.accent_bold(BenchmarkProfile::ThermallyControlled.duration(timing))
    );
    println!(
        "     {}",
        ui.muted("Waits for thermal recovery between workloads.")
    );
    println!(
        "     {} {}",
        ui.muted("Without a usable temperature sensor:"),
        ui.accent_bold(format!(
            "about {}",
            format_duration(timing.cooldown_fallback_seconds())
        ))
    );
    println!();
    println!(
        "  {}",
        ui.muted("Times exclude model loading and recorded repetitions.")
    );
}

fn prompt_benchmark_profile() -> Result<BenchmarkProfile> {
    loop {
        let answer = prompt("Select a run profile [1]: ")?;
        match parse_benchmark_profile(&answer) {
            Some(profile) => return Ok(profile),
            None => println!(
                "{} Enter `1` for standard or `2` for thermally controlled.",
                TerminalUi::detect().warning("!")
            ),
        }
    }
}

fn parse_benchmark_profile(answer: &str) -> Option<BenchmarkProfile> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "standard" | "warmup" => Some(BenchmarkProfile::Standard),
        "2" | "thermal" | "cooldown" => Some(BenchmarkProfile::ThermallyControlled),
        _ => None,
    }
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
    let harness = adapter.discover(harness_override)?;
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

pub(crate) fn resolve_harness(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return executable_path(path);
    }
    for variable in ["COMPUTEARENA_BASERT_HARNESS", "BASERT_COMPUTEARENA_HARNESS"] {
        if let Some(path) = std::env::var_os(variable) {
            if path.is_empty() {
                bail!("{variable} is set but empty");
            }
            return executable_path(PathBuf::from(path));
        }
    }
    executable_on_path(BASERT_HARNESS_NAME).with_context(|| {
        format!(
            "{BASERT_HARNESS_NAME} was not found on PATH; add it to PATH or pass --harness /path/to/{BASERT_HARNESS_NAME}"
        )
    })
}

pub(crate) fn executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

pub(crate) fn executable_path(path: PathBuf) -> Result<PathBuf> {
    if path.components().count() == 1 {
        return executable_on_path(path.to_string_lossy().as_ref()).with_context(|| {
            format!(
                "benchmark harness was not found on PATH: {}",
                path.display()
            )
        });
    }
    if !path.is_file() {
        bail!("benchmark harness not found: {}", path.display());
    }
    Ok(path)
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

    #[test]
    fn parses_run_profile_numbers_names_and_default() {
        assert_eq!(
            parse_benchmark_profile(""),
            Some(BenchmarkProfile::Standard)
        );
        assert_eq!(
            parse_benchmark_profile("1"),
            Some(BenchmarkProfile::Standard)
        );
        assert_eq!(
            parse_benchmark_profile("COOLDOWN"),
            Some(BenchmarkProfile::ThermallyControlled)
        );
        assert_eq!(
            parse_benchmark_profile("2"),
            Some(BenchmarkProfile::ThermallyControlled)
        );
        assert_eq!(parse_benchmark_profile("3"), None);
    }
}
