use crate::config::{
    APPLE_TELEMETRY_IDLE_BASELINE_SECONDS, APPLE_TELEMETRY_SECONDS_PER_WORKLOAD,
    DEVELOPMENT_HARNESS_PATHS, LEGACY_HARNESS_NAME, MEMORY_TELEMETRY_SECONDS_PER_WORKLOAD,
    PRIMARY_HARNESS_NAME,
};
use crate::models::{compact_home_path, inspect_model};
use crate::protocol::{HARNESS_SCHEMA, REPORT_SCHEMA, RUNTIME_NAME, TELEMETRY_SCHEMA};
use crate::reports::{
    atomic_write_json, hex, load_or_create_installation_key, sha256_hex, sign_report, Paths,
};
use crate::ui::{finish_activity, prompt_yes_no, start_activity, TerminalUi};
use anyhow::{bail, Context, Result};
use base_sign::b64_encode;
use rand_core::{OsRng, RngCore};
use serde_json::{json, Value};
use std::fs;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn confirm_benchmark_run(
    model: &Path,
    pp: &str,
    tg: u32,
    reps: u32,
    warmup: u32,
    skip_confirmation: bool,
) -> Result<bool> {
    if !model.is_file() {
        bail!("model does not exist or is not a file: {}", model.display());
    }
    let prefill_tokens = parse_pp(pp)?;
    if tg == 0 || reps == 0 {
        bail!("--tg and --reps must be greater than zero");
    }

    let ui = TerminalUi::detect();
    ui.section("Benchmark plan");
    println!("  {} {}", ui.neutral("Model:"), compact_home_path(model));
    println!(
        "  {} {}",
        ui.neutral("Prefill:"),
        prefill_tokens
            .iter()
            .map(|tokens| format!("PP{tokens}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  {} TG{tg}", ui.neutral("Decode:"));
    println!(
        "  {} {warmup} warmup {} + {reps} recorded {} per throughput workload",
        ui.neutral("Sampling:"),
        repetition_label(warmup),
        repetition_label(reps)
    );
    println!(
        "  {} Synthetic token sequences; this does not test model accuracy",
        ui.neutral("Input:")
    );

    if cfg!(target_os = "macos") {
        let telemetry_seconds = APPLE_TELEMETRY_IDLE_BASELINE_SECONDS
            + APPLE_TELEMETRY_SECONDS_PER_WORKLOAD * (prefill_tokens.len() + 1) as f64;
        println!(
            "  {} Separate energy and combined memory/temperature replays add about {} or more",
            ui.neutral("Telemetry:"),
            format_duration(telemetry_seconds)
        );
    } else {
        let telemetry_seconds =
            MEMORY_TELEMETRY_SECONDS_PER_WORKLOAD * (prefill_tokens.len() + 1) as f64;
        println!(
            "  {} Separate process-memory replays add about {} or more; NVIDIA/ROCm sensors remain basic",
            ui.neutral("Telemetry:"),
            format_duration(telemetry_seconds)
        );
    }
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

    if skip_confirmation {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        bail!("benchmark confirmation requires a terminal; pass --yes to run non-interactively");
    }
    prompt_yes_no("Start this benchmark?", false)
}

fn format_duration(seconds: f64) -> String {
    let seconds = seconds.ceil() as u64;
    let minutes = seconds / 60;
    let remaining = seconds % 60;
    if minutes == 0 {
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
            "Running the BaseRT benchmark and hardware telemetry with {}…",
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
        .arg("--telemetry")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_preflight_telemetry_duration() {
        let workload_count = parse_pp("128,256,512,1024,2048,4096,8192,16384")
            .unwrap()
            .len()
            + 1;
        let seconds = APPLE_TELEMETRY_IDLE_BASELINE_SECONDS
            + APPLE_TELEMETRY_SECONDS_PER_WORKLOAD * workload_count as f64;
        assert_eq!(format_duration(seconds), "1m 32s");
        assert_eq!(
            format_duration(MEMORY_TELEMETRY_SECONDS_PER_WORKLOAD * workload_count as f64),
            "45s"
        );
    }
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
            "benchmark harness omitted supported telemetry (expected {TELEMETRY_SCHEMA}); rebuild basert-harness"
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
