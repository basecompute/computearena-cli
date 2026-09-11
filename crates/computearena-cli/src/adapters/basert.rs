use super::{BenchmarkRequest, RuntimeAdapter, RuntimeOutput};
use crate::protocol::BASERT_SAME_RUN_TELEMETRY_SCHEMA;
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
        let mut benchmark: Value =
            serde_json::from_slice(&output.stdout).context("BaseRT returned invalid JSON")?;
        let expected_schema = match mode {
            TelemetryMode::ExternalWholeProcess => None,
            TelemetryMode::NativeSameRun => Some(BASERT_SAME_RUN_TELEMETRY_SCHEMA),
        };
        crate::benchmark::validate_harness_result(&benchmark, expected_schema)?;
        validate_requested_workloads(&benchmark, r)?;
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
}
