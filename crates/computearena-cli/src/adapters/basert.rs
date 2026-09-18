use super::{BenchmarkRequest, RuntimeAdapter, RuntimeOutput};
use crate::protocol::{
    BASERT_ISOLATED_PROTOCOL_SCHEMA, BASERT_SAME_RUN_TELEMETRY_SCHEMA,
    DECODE_INITIAL_CONTEXT_TOKENS, THROUGHPUT_PROTOCOL_ID,
};
use crate::ui::TerminalUi;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) struct BaseRtAdapter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TelemetryMode {
    ExternalWholeProcess,
    NativeSameRun,
}

pub(crate) fn supports_isolated_workloads(descriptor: &Value) -> bool {
    descriptor
        .pointer("/features/isolated_workload_contexts")
        .and_then(Value::as_bool)
        == Some(true)
}

pub(crate) fn supports_headline_capacity(descriptor: &Value) -> Result<bool> {
    let supported = descriptor
        .pointer("/features/headline_context_capacity")
        .and_then(Value::as_bool)
        == Some(true);
    if supported
        && descriptor["capacity_protocol_schema"].as_str()
            != Some(crate::protocol::BASERT_CAPACITY_PROTOCOL_SCHEMA)
    {
        bail!("BaseRT advertises an unsupported headline context protocol");
    }
    Ok(supported)
}

fn telemetry_mode(descriptor: &Value) -> Result<TelemetryMode> {
    if descriptor
        .pointer("/features/same_run_telemetry")
        .and_then(Value::as_bool)
        != Some(true)
    {
        return Ok(TelemetryMode::ExternalWholeProcess);
    }
    let schema = descriptor
        .get("telemetry_schema")
        .and_then(Value::as_str)
        .context("BaseRT advertises same-run telemetry without a telemetry_schema")?;
    if schema != BASERT_SAME_RUN_TELEMETRY_SCHEMA {
        bail!(
            "unsupported BaseRT same-run telemetry schema {schema}; expected {BASERT_SAME_RUN_TELEMETRY_SCHEMA}"
        );
    }
    Ok(TelemetryMode::NativeSameRun)
}

impl RuntimeAdapter for BaseRtAdapter {
    fn name(&self) -> &'static str {
        "basert"
    }

    fn display_name(&self) -> &'static str {
        "BaseRT"
    }

    fn binary_name(&self) -> &'static str {
        crate::config::BASERT_HARNESS_NAME
    }

    fn environment_overrides(&self) -> &'static [&'static str] {
        &["COMPUTEARENA_BASERT_HARNESS", "BASERT_COMPUTEARENA_HARNESS"]
    }

    fn known_locations(&self) -> Vec<PathBuf> {
        crate::runtimes::basert_install_dir().into_iter().collect()
    }

    fn probe(&self, executable: &Path) -> Result<Value> {
        crate::benchmark::validate_harness_descriptor(executable)?;
        let output = Command::new(executable)
            .args(["describe", "--json"])
            .output()?;
        serde_json::from_slice(&output.stdout).context("reading BaseRT benchmark capabilities")
    }

    fn select_model(&self, paths: &crate::reports::Paths) -> Result<Option<PathBuf>> {
        crate::models::prompt_model_path(paths)
    }

    fn confirm(&self, r: &BenchmarkRequest<'_>, yes: bool) -> Result<Option<bool>> {
        crate::benchmark::confirm_benchmark_run(
            r.model, r.pp, r.tg, r.reps, r.warmup, r.cooldown, yes,
        )
    }

    fn execute(
        &self,
        executable: &Path,
        r: &BenchmarkRequest<'_>,
        descriptor: &Value,
    ) -> Result<RuntimeOutput> {
        let ui = TerminalUi::detect();
        let mode = telemetry_mode(descriptor)?;
        let isolated_workloads = supports_isolated_workloads(descriptor);
        let headline_capacity = supports_headline_capacity(descriptor)?;
        let suite_conditioning = if mode == TelemetryMode::ExternalWholeProcess && r.cooldown {
            let mut cooldown = crate::conditioning::Cooldown::new();
            Some(cooldown.prepare("BaseRT benchmark suite"))
        } else {
            None
        };
        let mut command = Command::new(executable);
        command
            .arg("run")
            .arg(r.model)
            .args(["--mode", "text", "-p", r.pp, "-n"])
            .arg(r.tg.to_string())
            .arg("-r")
            .arg(r.reps.to_string())
            .arg("-w")
            .arg(r.warmup.to_string());
        if headline_capacity {
            command.arg("--headline-first");
        } else if isolated_workloads {
            command.arg("--isolated-workloads");
        }
        let native_environment_before =
            (mode == TelemetryMode::NativeSameRun).then(crate::telemetry::capture_environment);
        let (output, external_telemetry) = match mode {
            TelemetryMode::ExternalWholeProcess => {
                println!("{}", ui.neutral("Telemetry: observing the BaseRT harness process and available device sensors (whole run, 1-second sampling; no telemetry replays)."));
                let (output, telemetry) = crate::telemetry::run_observed(&mut command)
                    .context("running BaseRT benchmark")?;
                (output, Some(telemetry))
            }
            TelemetryMode::NativeSameRun => {
                command.arg("--telemetry");
                if r.cooldown {
                    command.arg("--cooldown");
                }
                let output = command
                    .stderr(Stdio::inherit())
                    .output()
                    .context("running BaseRT benchmark")?;
                (output, None)
            }
        };
        if !output.status.success() {
            bail!("BaseRT benchmark exited with {}", output.status);
        }
        let native_environment = native_environment_before.map(|before| {
            json!({"schema":"computearena-environment/1","measurement_relation":"outside_runtime_execution",
                "before":before,"after":crate::telemetry::capture_environment()})
        });
        let mut benchmark: Value =
            serde_json::from_slice(&output.stdout).context("BaseRT returned invalid JSON")?;
        let expected_schema = match mode {
            TelemetryMode::ExternalWholeProcess => None,
            TelemetryMode::NativeSameRun => Some(BASERT_SAME_RUN_TELEMETRY_SCHEMA),
        };
        crate::benchmark::validate_harness_result(&benchmark, expected_schema)?;
        validate_requested_workloads(&benchmark, r)?;
        normalize_protocol(&mut benchmark, r, isolated_workloads, headline_capacity)?;
        if let Some(environment) = native_environment {
            crate::telemetry::attach_environment(&mut benchmark, environment);
        }
        if let Some(mut telemetry) = external_telemetry {
            let mut policy = if r.cooldown {
                crate::conditioning::before_suite_policy()
            } else {
                json!({"schema":"computearena-conditioning/1","mode":"warmup_only",
                    "cooldown_enabled":false,"scope":"harness_managed_warmup",
                    "note":"No external thermal wait; the harness performs the requested warmup repetitions"})
            };
            if let Some(outcome) = suite_conditioning {
                policy["suite_outcome"] = outcome;
            }
            telemetry["conditioning"] = policy;
            telemetry["performance_run"] = json!({
                "measurement_relation":"same_process",
                "note":"The observer wraps the single harness execution that produced the signed raw samples"
            });
            telemetry["adapter"] = json!({
                "runtime":"basert",
                "mode":"external_whole_process",
                "native_harness_telemetry_requested":false,
                "advertised_harness_telemetry_schema":descriptor.get("telemetry_schema")
            });
            if let Some(peak) = crate::telemetry::attach_whole_process(&mut benchmark, telemetry) {
                println!("{}", ui.neutral(format!("Telemetry: observed peak process memory {peak:.0} MiB (includes loading and warmup).")));
            } else {
                println!("{}", ui.neutral("Telemetry: process memory unavailable; see sensor coverage in the saved report."));
            }
        }
        Ok(RuntimeOutput {
            benchmark,
            model: crate::models::inspect_model(r.model)?,
        })
    }
}

fn normalize_protocol(
    benchmark: &mut Value,
    request: &BenchmarkRequest<'_>,
    isolated_workloads: bool,
    headline_capacity: bool,
) -> Result<()> {
    let runtime_protocol = benchmark.get("protocol").cloned().unwrap_or(Value::Null);
    benchmark["params"]["decode_context_tokens"] = json!(DECODE_INITIAL_CONTEXT_TOKENS);
    if headline_capacity {
        return normalize_headline_protocol(benchmark, request, runtime_protocol);
    }
    if !isolated_workloads {
        benchmark["protocol"] = json!({
            "id":"computearena-throughput-legacy/1",
            "comparable":false,
            "reason":"The installed BaseRT harness does not advertise isolated workload contexts",
            "context_isolation":"shared_sweep_context",
            "decode":{"initial_context_tokens":DECODE_INITIAL_CONTEXT_TOKENS},
            "runtime_protocol":runtime_protocol
        });
        return Ok(());
    }

    if runtime_protocol["schema"].as_str() != Some(BASERT_ISOLATED_PROTOCOL_SCHEMA)
        || runtime_protocol["context_isolation"].as_str() != Some("per_workload")
        || runtime_protocol["context_capacity_policy"].as_str() != Some("minimum_required")
        || runtime_protocol["model_load_in_timing"].as_bool() != Some(false)
    {
        bail!("BaseRT advertised isolated contexts but returned incompatible protocol metadata");
    }
    for prompt in request.pp.split(',') {
        let tokens = prompt.parse::<u64>()?;
        let workload = &runtime_protocol["prefill"][prompt];
        if workload["initial_context_tokens"].as_u64() != Some(0)
            || workload["context_capacity_tokens"].as_u64() != Some(tokens)
        {
            bail!("BaseRT returned incompatible PP{tokens} context metadata");
        }
    }
    if runtime_protocol["decode"]["initial_context_tokens"].as_u64()
        != Some(DECODE_INITIAL_CONTEXT_TOKENS)
        || runtime_protocol["decode"]["context_capacity_tokens"].as_u64()
            != Some(u64::from(request.tg) + DECODE_INITIAL_CONTEXT_TOKENS)
    {
        bail!(
            "BaseRT returned incompatible TG{} context metadata",
            request.tg
        );
    }
    benchmark["protocol"] = json!({
        "id":THROUGHPUT_PROTOCOL_ID,
        "comparable":true,
        "context_isolation":"per_workload",
        "context_capacity_policy":"minimum_required",
        "model_load_in_timing":false,
        "prefill":{"initial_context_tokens":0},
        "decode":{"initial_context_tokens":DECODE_INITIAL_CONTEXT_TOKENS},
        "tokenization_timed":false,
        "sampling_timed":false,
        "runtime_protocol":runtime_protocol
    });
    Ok(())
}

fn normalize_headline_protocol(
    benchmark: &mut Value,
    request: &BenchmarkRequest<'_>,
    runtime: Value,
) -> Result<()> {
    use crate::protocol::{
        headline_order, headline_prefill, BASERT_CAPACITY_PROTOCOL_SCHEMA,
        HEADLINE_CONTEXT_CAPACITY, HEADLINE_PROTOCOL_ID,
    };
    let headline = headline_prefill(request.pp).parse::<u64>()?;
    let capacity = (headline + u64::from(request.tg)).max(HEADLINE_CONTEXT_CAPACITY);
    if runtime["schema"] != BASERT_CAPACITY_PROTOCOL_SCHEMA
        || runtime["profile"] != "basert-bench-capacity/1"
        || runtime["context_isolation"] != "headline_then_per_prefill"
        || runtime["context_capacity_policy"] != "basert_bench_default"
        || runtime["model_load_in_timing"] != false
        || runtime["execution_layout"] != "headline_then_prefill_processes"
        || runtime["execution_order"] != json!(headline_order(request.pp, request.tg))
    {
        bail!("BaseRT returned incompatible headline capacity/order metadata");
    }
    // Each prefill runs at its own capacity, so the flat `params.ctx` names
    // the largest one the sweep used (BaseRT 0.2.5): 16384 for the default
    // sweep, not the headline's 4096.
    let mut sweep_capacity = capacity;
    for prompt in request.pp.split(',') {
        let tokens = prompt.parse::<u64>()?;
        let expected = if tokens == headline {
            capacity
        } else {
            tokens.max(HEADLINE_CONTEXT_CAPACITY)
        };
        sweep_capacity = sweep_capacity.max(expected);
        if runtime["prefill"][prompt]["initial_context_tokens"] != 0
            || runtime["prefill"][prompt]["context_capacity_tokens"].as_u64() != Some(expected)
        {
            bail!("BaseRT returned incompatible PP{tokens} capacity metadata");
        }
    }
    if runtime["decode"]["initial_context_tokens"] != DECODE_INITIAL_CONTEXT_TOKENS
        || runtime["decode"]["context_capacity_tokens"] != capacity
        || runtime["decode"]["seed_prefill_in_timing"] != false
        || runtime["measurement"]["timed_repetitions"] != request.reps
        || runtime["measurement"]["requested_warmup_repetitions"] != request.warmup
        || benchmark["params"]["ctx"] != sweep_capacity
    {
        bail!("BaseRT returned incompatible headline measurement metadata");
    }
    benchmark["protocol"] = json!({
        "id":HEADLINE_PROTOCOL_ID, "comparable":true,
        "profile":"headline-first-capacity/1", "context_isolation":"headline_then_per_prefill",
        "context_capacity_policy":"basert_bench_default", "model_load_in_timing":false,
        "prefill":{"initial_context_tokens":0},
        "decode":{"initial_context_tokens":DECODE_INITIAL_CONTEXT_TOKENS,"context_capacity_tokens":capacity},
        "execution_order":runtime["execution_order"], "execution_layout":runtime["execution_layout"],
        "measurement":runtime["measurement"], "cooldown_enabled":request.cooldown,
        "tokenization_timed":false, "sampling_timed":false,
        "runtime_protocol":runtime
    });
    Ok(())
}

/// A compatible harness must also have completed the work the user requested.
/// Keep raw samples unchanged; reject partial runs instead of signing them.
fn validate_requested_workloads(value: &Value, r: &BenchmarkRequest<'_>) -> Result<()> {
    use std::collections::BTreeSet;
    let expected: BTreeSet<u64> =
        r.pp.split(',')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
    let prefill = value["raw_samples"]["prefill"]
        .as_object()
        .context("missing prefill samples")?;
    if expected.len() != r.pp.split(',').count() || prefill.len() != expected.len() {
        bail!("BaseRT did not complete exactly the requested prefill workloads");
    }
    for field in ["tg", "reps"] {
        let requested = if field == "tg" { r.tg } else { r.reps };
        if let Some(actual) = value["params"].get(field) {
            if actual.as_u64() != Some(u64::from(requested)) {
                bail!("BaseRT returned an unexpected {field} parameter");
            }
        }
    }
    let check_samples = |samples: &Value, key: &str, tokens: u64| -> Result<()> {
        let samples = samples.as_array().context("missing samples")?;
        if samples.len() != r.reps as usize {
            bail!("BaseRT returned an unexpected repetition count");
        }
        for sample in samples {
            if sample[key].as_u64() != Some(tokens) {
                bail!("BaseRT returned an unexpected {key} count");
            }
            if !matches!(
                sample["elapsed_ns"].as_u64(),
                Some(1..=9_007_199_254_740_991)
            ) {
                bail!("BaseRT returned an invalid sample duration");
            }
        }
        Ok(())
    };
    for tokens in expected {
        let samples = prefill
            .get(&tokens.to_string())
            .context("BaseRT omitted a requested prefill workload")?;
        check_samples(samples, "tokens", tokens)?;
    }
    check_samples(
        &value["raw_samples"]["decode"],
        "generated_tokens",
        u64::from(r.tg),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_capacity_is_capability_gated_and_does_not_change_warmup() {
        assert!(!supports_headline_capacity(&json!({})).unwrap());
        assert!(supports_headline_capacity(
            &json!({"features":{"headline_context_capacity":true}})
        )
        .is_err());
        assert!(
            supports_headline_capacity(&json!({"features":{"headline_context_capacity":true},
            "capacity_protocol_schema":crate::protocol::BASERT_CAPACITY_PROTOCOL_SCHEMA}))
            .unwrap()
        );
        let request = BenchmarkRequest {
            model: Path::new("test.base"),
            pp: "128,512",
            tg: 128,
            reps: 3,
            warmup: 0,
            cooldown: false,
            runtime_args: &[],
        };
        let original = json!({"params":{"ctx":4096},"protocol":{
            "schema":"basert-throughput-protocol/2","profile":"basert-bench-capacity/1",
            "context_isolation":"headline_then_per_prefill","context_capacity_policy":"basert_bench_default",
            "model_load_in_timing":false,"execution_layout":"headline_then_prefill_processes",
            "execution_order":["pp512","tg128","pp128"],
            "prefill":{"128":{"initial_context_tokens":0,"context_capacity_tokens":4096},
                       "512":{"initial_context_tokens":0,"context_capacity_tokens":4096}},
            "decode":{"initial_context_tokens":1,"context_capacity_tokens":4096,"seed_prefill_in_timing":false},
            "measurement":{"timed_repetitions":3,"requested_warmup_repetitions":0}
        }});
        let mut report = original.clone();
        normalize_protocol(&mut report, &request, true, true).unwrap();
        assert_eq!(
            report["protocol"]["id"],
            crate::protocol::HEADLINE_PROTOCOL_ID
        );
        assert_eq!(
            report["protocol"]["measurement"]["requested_warmup_repetitions"],
            0
        );
        assert_eq!(report["protocol"]["runtime_protocol"], original["protocol"]);
        for (pointer, value) in [
            ("/protocol/decode/initial_context_tokens", json!(4096)),
            ("/protocol/decode/context_capacity_tokens", json!(129)),
            ("/protocol/prefill/128/context_capacity_tokens", json!(128)),
            ("/protocol/measurement/timed_repetitions", json!(5)),
            (
                "/protocol/measurement/requested_warmup_repetitions",
                json!(12),
            ),
            (
                "/protocol/execution_order",
                json!(["pp128", "pp512", "tg128"]),
            ),
        ] {
            let mut invalid = original.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(
                normalize_protocol(&mut invalid, &request, true, true).is_err(),
                "{pointer}"
            );
        }
    }

    /// What the BaseRT 0.2.5 harness returns for the default sweep: every
    /// prefill at its own capacity, and `params.ctx` naming the largest.
    fn default_sweep_report(ctx: u64) -> Value {
        let pp = crate::config::DEFAULT_PREFILL_TOKENS;
        let prefill: serde_json::Map<String, Value> = pp
            .split(',')
            .map(|size| {
                let tokens: u64 = size.parse().unwrap();
                let capacity = if size == "512" {
                    4096
                } else {
                    tokens.max(4096)
                };
                (
                    size.to_string(),
                    json!({"initial_context_tokens":0,"context_capacity_tokens":capacity}),
                )
            })
            .collect();
        json!({"params":{"ctx":ctx},"protocol":{
            "schema":"basert-throughput-protocol/2","profile":"basert-bench-capacity/1",
            "context_isolation":"headline_then_per_prefill","context_capacity_policy":"basert_bench_default",
            "model_load_in_timing":false,"execution_layout":"headline_then_prefill_processes",
            "execution_order":crate::protocol::headline_order(pp, 128),
            "prefill":prefill,
            "decode":{"initial_context_tokens":1,"context_capacity_tokens":4096,"seed_prefill_in_timing":false},
            "measurement":{"timed_repetitions":3,"requested_warmup_repetitions":3}
        }})
    }

    #[test]
    fn the_default_sweep_reports_its_largest_capacity_as_ctx() {
        let request = BenchmarkRequest {
            model: Path::new("test.base"),
            pp: crate::config::DEFAULT_PREFILL_TOKENS,
            tg: 128,
            reps: 3,
            warmup: 3,
            cooldown: false,
        };
        // BaseRT 0.2.5 publishes the 16384 the PP16384 workload ran at.
        let mut report = default_sweep_report(16384);
        normalize_protocol(&mut report, &request, true, true).unwrap();
        assert_eq!(
            report["protocol"]["id"],
            crate::protocol::HEADLINE_PROTOCOL_ID
        );
        assert_eq!(report["params"]["ctx"], 16384);
        // The decode capacity is still the headline's.
        assert_eq!(
            report["protocol"]["decode"]["context_capacity_tokens"],
            4096
        );

        // Pre-release harnesses that published the headline's capacity
        // instead misdescribe the sweep and are not signed.
        let error = normalize_protocol(&mut default_sweep_report(4096), &request, true, true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("incompatible headline measurement metadata"),
            "{error}"
        );
    }

    #[test]
    fn replay_based_harnesses_use_external_observation() {
        let descriptor = json!({"telemetry_schema":"basert-telemetry/3",
            "features":{"telemetry":true,"same_run_telemetry":false}});
        assert_eq!(
            telemetry_mode(&descriptor).unwrap(),
            TelemetryMode::ExternalWholeProcess
        );
    }

    #[test]
    fn future_same_run_harnesses_select_native_telemetry_by_capability() {
        let descriptor = json!({"telemetry_schema":BASERT_SAME_RUN_TELEMETRY_SCHEMA,
            "features":{"telemetry":true,"same_run_telemetry":true}});
        assert_eq!(
            telemetry_mode(&descriptor).unwrap(),
            TelemetryMode::NativeSameRun
        );
    }

    #[test]
    fn unknown_native_telemetry_versions_fail_closed() {
        let descriptor = json!({"telemetry_schema":"basert-telemetry/999",
            "features":{"same_run_telemetry":true}});
        assert!(telemetry_mode(&descriptor).is_err());
    }
    #[test]
    fn isolated_harness_metadata_normalizes_to_the_shared_protocol() {
        let request = BenchmarkRequest {
            model: Path::new("model.base"),
            pp: "128,512",
            tg: 128,
            reps: 3,
            warmup: 3,
            cooldown: false,
            runtime_args: &[],
        };
        let mut benchmark = json!({
            "params":{},
            "protocol":{
                "schema":BASERT_ISOLATED_PROTOCOL_SCHEMA,
                "context_isolation":"per_workload",
                "context_capacity_policy":"minimum_required",
                "model_load_in_timing":false,
                "prefill":{
                    "128":{"initial_context_tokens":0,"context_capacity_tokens":128},
                    "512":{"initial_context_tokens":0,"context_capacity_tokens":512}
                },
                "decode":{"initial_context_tokens":1,"context_capacity_tokens":129}
            }
        });
        normalize_protocol(&mut benchmark, &request, true, false).unwrap();
        assert_eq!(benchmark["protocol"]["id"], THROUGHPUT_PROTOCOL_ID);
        assert_eq!(benchmark["protocol"]["comparable"], true);
        assert_eq!(benchmark["params"]["decode_context_tokens"], 1);
        assert_eq!(
            benchmark["protocol"]["runtime_protocol"]["decode"]["context_capacity_tokens"],
            129
        );
    }
}
