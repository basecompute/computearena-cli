use super::{BenchmarkRequest, RuntimeAdapter, RuntimeOutput};
use crate::benchmark::{executable_on_path, executable_path, validate_pp};
use crate::ui::TerminalUi;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct LlamaCppAdapter;
const DOWNLOAD: &str = "https://github.com/ggml-org/llama.cpp/releases";

impl RuntimeAdapter for LlamaCppAdapter {
    fn name(&self) -> &'static str {
        "llama-cpp"
    }

    fn discover(&self, path: Option<PathBuf>) -> Result<PathBuf> {
        if let Some(path) = path {
            return executable_path(path);
        }
        executable_on_path("llama-bench").with_context(|| format!(
            "llama.cpp was not found. Download it from {DOWNLOAD} and add its binaries to PATH, or use --runtime-path /path/to/llama-bench."
        ))
    }

    fn probe(&self, executable: &Path) -> Result<Value> {
        let output = Command::new(executable)
            .arg("--help")
            .output()
            .context("checking llama.cpp installation")?;
        let help = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for feature in [
            "--n-prompt",
            "--n-gen",
            "--n-depth",
            "--repetitions",
            "--no-warmup",
            "json",
        ] {
            if !output.status.success() || !help.contains(feature) {
                bail!("This llama.cpp build does not support {feature}. Download a supported build from {DOWNLOAD}.");
            }
        }
        // llama-bench builds do not consistently implement --version. Its
        // structured benchmark rows are the authoritative build identity.
        Ok(
            json!({"adapter": "llama-bench-json/1", "version_source": "benchmark.build_commit",
            "warmup": "runtime_native", "cooldown_supported": true}),
        )
    }

    fn select_model(&self, paths: &crate::reports::Paths) -> Result<Option<PathBuf>> {
        print_model_download_hint();
        crate::recent_gguf::select(paths, validate_model)
    }

    fn confirm(&self, r: &BenchmarkRequest<'_>, yes: bool) -> Result<Option<bool>> {
        validate_model(r.model)?;
        validate_pp(r.pp)?;
        if r.tg == 0 || r.reps == 0 || r.reps > 100 {
            bail!("Use positive token sizes and between 1 and 100 repetitions.");
        }
        crate::benchmark::confirm_llama_profile(r, yes)
    }

    fn execute(&self, executable: &Path, r: &BenchmarkRequest<'_>) -> Result<RuntimeOutput> {
        validate_model(r.model)?;
        let ui = TerminalUi::detect();
        println!("{}", ui.neutral("Telemetry: observing process memory and available device sensors (whole run, 1-second sampling)."));
        let (rows, telemetry) = if r.cooldown {
            let mut cooldown = crate::conditioning::Cooldown::new();
            run_conditioned(executable, r, |label| cooldown.prepare(label))?
        } else {
            run_native(executable, r, r.pp, r.tg)?
        };
        let mut result = normalize(&rows, r)?;
        ui.section("Benchmark results");
        for pp in r.pp.split(',') {
            if let Some(rate) = result.benchmark["metrics"][format!("pp{pp}_t_s")].as_f64() {
                println!("  {}", ui.neutral(format!("PP{pp}: {rate:.2} tok/s")));
            }
        }
        println!(
            "  {}",
            ui.neutral(format!(
                "TG{}: {:.2} tok/s",
                r.tg,
                result.benchmark["metrics"]["decode_t_s"]
                    .as_f64()
                    .unwrap_or(0.0)
            ))
        );
        result.benchmark["protocol"]["telemetry_available"] =
            json!(telemetry.get("observer").is_some() || telemetry.get("workloads").is_some());
        result.benchmark["protocol"]["measurement_observer"] =
            json!("external_whole_process_sampler");
        if let Some(peak) = telemetry
            .pointer("/process_memory/statistics/peak")
            .and_then(Value::as_f64)
        {
            result.benchmark["memory"] = json!({"process_peak_rss_mb":peak,
                "measurement_relation":"concurrent_observer","scope":"whole_runtime_process",
                "unit":"MiB","note":"Observed sampled peak, including loading and warmup; not a kernel high-water mark"});
            println!("{}", ui.neutral(format!("Telemetry: observed peak process memory {peak:.0} MiB (includes loading and warmup).")));
        } else {
            println!("{}", ui.neutral("Telemetry: process memory unavailable; see sensor coverage in the saved report."));
        }
        result.benchmark["telemetry"] = telemetry;
        if r.cooldown {
            result.benchmark["protocol"]["id"] = json!("llama-bench-conditioned-pp-tg/1");
            result.benchmark["protocol"]["cooldown_enabled"] = json!(true);
            result.benchmark["protocol"]["execution_layout"] = json!("one_process_per_workload");
            result.benchmark["protocol"]["conditioning"] =
                result.benchmark["telemetry"]["conditioning"].clone();
            result.benchmark["protocol"]["conditioning_workloads"] =
                result.benchmark["telemetry"]["conditioning_workloads"].clone();
            if result.benchmark.get("memory").is_some() {
                result.benchmark["memory"]["scope"] = json!("all_workload_processes");
            }
        }
        let mut model = super::gguf::inspect(r.model)?;
        model["runtime_description"] = result.model["runtime_description"].clone();
        model["parameters"] = result.model["parameters"].clone();
        result.model = model;
        Ok(result)
    }
}

fn run_native(
    executable: &Path,
    r: &BenchmarkRequest<'_>,
    pp: &str,
    tg: u32,
) -> Result<(Value, Value)> {
    let mut command = Command::new(executable);
    command
        .arg("-m")
        .arg(r.model)
        .args(["-p", pp, "-n"])
        .arg(tg.to_string())
        .args(["-d", "0", "-r"])
        .arg(r.reps.to_string())
        .args(["-o", "json"]);
    if r.warmup == 0 {
        command.arg("--no-warmup");
    }
    let (output, telemetry) =
        crate::telemetry::run_observed(&mut command).context("running llama.cpp benchmark")?;
    if !output.status.success() {
        bail!(
            "llama.cpp exited with {}. See its output above; no report was signed.",
            output.status
        );
    }
    let rows = serde_json::from_slice(&output.stdout)
        .context("llama.cpp did not return benchmark JSON")?;
    Ok((rows, telemetry))
}

fn run_conditioned(
    executable: &Path,
    r: &BenchmarkRequest<'_>,
    mut prepare: impl FnMut(&str) -> Value,
) -> Result<(Value, Value)> {
    let schedule: Vec<(u32, u32)> =
        r.pp.split(',')
            .map(|v| v.parse::<u32>().map(|pp| (pp, 0)))
            .chain(std::iter::once(Ok((0, r.tg))))
            .collect::<std::result::Result<_, _>>()?;
    let mut rows = Vec::new();
    let mut observations = Map::new();
    let mut conditioning = Map::new();
    let mut peak: Option<f64> = None;
    let ui = TerminalUi::detect();
    for (index, (pp, tg)) in schedule.iter().enumerate() {
        let label = if *pp > 0 {
            format!("pp{pp}")
        } else {
            format!("tg{tg}")
        };
        println!(
            "{}",
            ui.strong(format!("[{}/{}] {label}", index + 1, schedule.len()))
        );
        conditioning.insert(label.clone(), prepare(&label));
        println!(
            "{}",
            ui.neutral(format!(
                "{label} — loading model, then {} and {} recorded repetitions",
                if r.warmup == 0 {
                    "no warmup"
                } else {
                    "native warmup"
                },
                r.reps
            ))
        );
        let (native, telemetry) = run_native(executable, r, &pp.to_string(), *tg)?;
        let native = native
            .as_array()
            .context("llama.cpp benchmark output must be an array")?;
        if native.len() != 1
            || native[0]["n_prompt"].as_u64() != Some(u64::from(*pp))
            || native[0]["n_gen"].as_u64() != Some(u64::from(*tg))
        {
            bail!("llama.cpp returned an unexpected workload for {label}; no report was signed");
        }
        rows.extend(native.iter().cloned());
        if let Some(value) = telemetry
            .pointer("/process_memory/statistics/peak")
            .and_then(Value::as_f64)
        {
            peak = Some(peak.map_or(value, |p| p.max(value)));
        }
        observations.insert(label, telemetry);
    }
    Ok((
        Value::Array(rows),
        json!({"schema":"computearena-telemetry/1","coverage":"basic",
        "scope":"separate_workload_processes","measurement_relation":"concurrent_observer",
        "conditioning":crate::conditioning::policy(),"conditioning_workloads":conditioning,
        "workloads":observations,"process_memory":{"metric":"resident_set_size","unit":"MiB",
            "statistics":{"available":peak.is_some(),"peak":peak},
            "note":"Maximum observed peak across separate process windows including loading and warmup"}}),
    ))
}

fn print_model_download_hint() {
    let ui = TerminalUi::detect();
    println!();
    println!("{}", ui.neutral("Need a GGUF model for llama.cpp?"));
    for (label, instruction) in [
        ("Browse", "https://huggingface.co/models?library=gguf"),
        (
            "Download",
            "hf download <repo-id> <filename.gguf> --local-dir ./models",
        ),
        (
            "CLI setup",
            "https://huggingface.co/docs/huggingface_hub/guides/cli",
        ),
    ] {
        println!(
            "  {}  {}",
            ui.neutral(format!("{label:<9}")),
            ui.accent_bold(instruction)
        );
    }
    println!("{}", ui.muted("Or download a .gguf file from the repository's Files and versions tab in your browser."));
    println!("{}", ui.muted("Choose a model and quantization that fit your device's memory, then enter the downloaded file's local path below."));
}

fn validate_model(path: &Path) -> Result<()> {
    let mut file = File::open(path).with_context(|| format!("opening model {}", path.display()))?;
    let mut magic = [0; 4];
    file.read_exact(&mut magic)
        .context("model file is too small")?;
    if &magic != b"GGUF" {
        bail!("llama.cpp needs a GGUF model file.");
    }
    Ok(())
}

pub(crate) fn normalize(value: &Value, r: &BenchmarkRequest<'_>) -> Result<RuntimeOutput> {
    let rows = value
        .as_array()
        .context("llama.cpp benchmark output must be an array")?;
    let first = rows.first().context("llama.cpp returned no measurements")?;
    let commit = first["build_commit"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("llama.cpp output is missing its build commit")?;
    let build = first["build_number"]
        .as_u64()
        .context("llama.cpp output is missing its build number")?;
    let expected: BTreeSet<u64> =
        r.pp.split(',')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
    if expected.len() != r.pp.split(',').count() {
        bail!("duplicate prefill sizes are not supported");
    }
    let mut prefill = Map::new();
    let mut decode = None;
    let mut metrics = Map::new();
    let mut settings = Map::new();
    // Persist effective configuration, never local model paths or raw stderr.
    const SETTINGS: &[&str] = &[
        "n_batch",
        "n_ubatch",
        "n_threads",
        "type_k",
        "type_v",
        "n_gpu_layers",
        "n_cpu_moe",
        "split_mode",
        "main_gpu",
        "no_kv_offload",
        "flash_attn",
        "devices",
        "tensor_split",
        "embeddings",
        "backends",
    ];
    for field in SETTINGS {
        settings.insert((*field).into(), first[*field].clone());
    }
    for row in rows {
        for field in SETTINGS.iter().copied().chain([
            "build_commit",
            "build_number",
            "model_type",
            "model_filename",
            "cpu_info",
            "gpu_info",
        ]) {
            if row[field] != first[field] {
                bail!("llama.cpp changed {field} between workloads");
            }
        }
        if row["n_depth"].as_u64() != Some(0) {
            bail!("llama.cpp returned an unexpected context depth");
        }
        let pp = row["n_prompt"].as_u64().context("missing n_prompt")?;
        let tg = row["n_gen"].as_u64().context("missing n_gen")?;
        let is_pp = pp > 0 && tg == 0;
        if !(is_pp && expected.contains(&pp) || pp == 0 && tg == u64::from(r.tg)) {
            bail!("llama.cpp returned an unexpected workload PP{pp}/TG{tg}");
        }
        let durations = row["samples_ns"]
            .as_array()
            .context("llama.cpp omitted per-repetition samples_ns; install a supported build")?;
        if durations.len() != r.reps as usize {
            bail!("llama.cpp returned an unexpected repetition count");
        }
        let count = if is_pp { pp } else { tg };
        let mut samples = Vec::new();
        let mut rates = Vec::new();
        for duration in durations {
            let ns = duration
                .as_u64()
                .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991)
                .context("llama.cpp returned an invalid sample duration")?;
            rates.push(count as f64 * 1e9 / ns as f64);
            samples.push(if is_pp {
                json!({"tokens": count, "elapsed_ns": ns})
            } else {
                json!({"generated_tokens": count, "elapsed_ns": ns})
            });
        }
        let mean = rates.iter().sum::<f64>() / rates.len() as f64;
        let metric = if is_pp {
            format!("pp{pp}_t_s")
        } else {
            "decode_t_s".into()
        };
        metrics.insert(metric, json!(mean));
        if is_pp {
            if prefill.insert(pp.to_string(), json!(samples)).is_some() {
                bail!("duplicate prefill workload");
            }
        } else if decode.replace(json!(samples)).is_some() {
            bail!("duplicate decode workload");
        }
    }
    if prefill.len() != expected.len() || decode.is_none() {
        bail!("llama.cpp did not complete every requested workload");
    }
    let cpu_only = first["n_gpu_layers"].as_i64() == Some(0);
    let backend = if cpu_only {
        "CPU"
    } else {
        first["backends"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("missing llama.cpp backend identity")?
    };
    let chip = if cpu_only || backend == "CPU" {
        first["cpu_info"].as_str()
    } else {
        first["gpu_info"].as_str()
    }
    .filter(|s| !s.is_empty())
    .context("llama.cpp did not identify the benchmark device")?;
    let model_type = first["model_type"]
        .as_str()
        .context("missing llama.cpp model description")?;
    Ok(RuntimeOutput {
        model: json!({
            "name": r.model.file_stem().and_then(|s| s.to_str()).unwrap_or(model_type),
            "file_name": r.model.file_name().and_then(|s| s.to_str()),
            "format": "gguf",
            "runtime_description": model_type,
            "quantization": model_type,
            "size_bytes": first["model_size"],
            "parameters": first["model_n_params"]
        }),
        benchmark: json!({
            "schema": "computearena-measurements/1", "mode": "text",
            "runtime_version": format!("b{build} ({commit})"), "chip": chip, "backend": backend,
            "params": {"pp": r.pp, "tg": r.tg, "reps": r.reps, "decode_context_tokens": 0},
            "metrics": metrics, "raw_samples": {"prefill": prefill, "decode": decode},
            "protocol": {"id": "llama-bench-independent-pp-tg/1", "initial_context_tokens": 0,
                "tokenization_timed": false, "sampling_timed": false, "timing_source": "runtime_samples_ns",
                "warmup": if r.warmup == 0 { "disabled" } else { "runtime_native" },
                "cooldown_enabled": false, "telemetry_available": false},
            "runtime_configuration": settings
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn cooled_execution_waits_before_each_isolated_workload_and_keeps_raw_samples() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("llama-bench");
        std::fs::write(&executable, r#"#!/bin/sh
pp=0
tg=0
while [ "$#" -gt 0 ]; do
case "$1" in
-p) shift; pp="$1";;
-n) shift; tg="$1";;
esac
shift
done
printf '[{"build_commit":"abc123","build_number":123,"model_type":"Qwen Q4","model_filename":"test.gguf","n_prompt":%s,"n_gen":%s,"n_depth":0,"n_gpu_layers":0,"cpu_info":"Test CPU","gpu_info":"","backends":"CPU","samples_ns":[100000000,200000000]}]' "$pp" "$tg"
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut request = request();
        request.cooldown = true;
        let mut order = Vec::new();
        let (rows,telemetry)=run_conditioned(&executable,&request,|label|{
            order.push(label.to_owned());
            json!({"method":"timed_fallback","target_reached":false,"timed_out":false,"waited_s":30.0,"sample_count":1})
        }).unwrap();
        assert_eq!(order, vec!["pp128", "pp512", "tg128"]);
        let normalized = normalize(&rows, &request).unwrap();
        assert_eq!(normalized.benchmark["metrics"]["pp512_t_s"], 3840.0);
        assert_eq!(telemetry["scope"], "separate_workload_processes");
        assert_eq!(
            telemetry["conditioning_workloads"]
                .as_object()
                .unwrap()
                .len(),
            3
        );
        assert!(telemetry["workloads"]["tg128"].get("observer").is_some());
    }

    fn request() -> BenchmarkRequest<'static> {
        BenchmarkRequest {
            model: Path::new("Qwen3-4B.gguf"),
            pp: "128,512",
            tg: 128,
            reps: 2,
            warmup: 3,
            cooldown: false,
        }
    }
    fn rows() -> Value {
        let base = json!({"build_commit":"abc123","build_number":123,"model_type":"qwen2 4B Q4_K - Medium",
            "model_filename":"/private/models/Qwen3-4B.gguf","cpu_info":"CPU","gpu_info":"Apple M5 Pro","backends":"Metal",
            "n_depth":0,"n_gpu_layers":99,"n_batch":2048,"n_ubatch":512,"n_threads":8,
            "model_size":123,"model_n_params":4000000000u64});
        Value::Array(
            [(128, 0), (512, 0), (0, 128)]
                .into_iter()
                .map(|(p, g)| {
                    let mut row = base.clone();
                    row["n_prompt"] = json!(p);
                    row["n_gen"] = json!(g);
                    row["samples_ns"] = json!([100000000, 200000000]);
                    row
                })
                .collect(),
        )
    }
    #[test]
    fn native_samples_are_normalized_without_local_paths() {
        let result = normalize(&rows(), &request()).unwrap();
        assert_eq!(result.benchmark["metrics"]["pp128_t_s"], json!(960.0));
        assert_eq!(
            result.benchmark["params"]["decode_context_tokens"],
            json!(0)
        );
        assert!(!result.benchmark.to_string().contains("/private/models"));
        assert!(result.benchmark.get("telemetry").is_none());
    }
    #[test]
    fn rejects_incomplete_inconsistent_or_invalid_measurements() {
        let mut value = rows();
        value.as_array_mut().unwrap().pop();
        assert!(normalize(&value, &request()).is_err());
        let mut value = rows();
        value[0]["samples_ns"] = json!([0, 100]);
        assert!(normalize(&value, &request()).is_err());
        let mut value = rows();
        value[1]["n_depth"] = json!(512);
        assert!(normalize(&value, &request()).is_err());
        let mut value = rows();
        value[1]["build_commit"] = json!("different");
        assert!(normalize(&value, &request()).is_err());
        let mut value = rows();
        value[2]["samples_ns"] = json!([100]);
        assert!(normalize(&value, &request()).is_err());
    }
}
