use super::{BenchmarkRequest, RuntimeAdapter, RuntimeOutput};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) struct BaseRtAdapter;

impl RuntimeAdapter for BaseRtAdapter {
    fn name(&self) -> &'static str {
        "basert"
    }

    fn discover(&self, path: Option<PathBuf>) -> Result<PathBuf> {
        crate::benchmark::resolve_harness(path)
    }

    fn probe(&self, executable: &Path) -> Result<Value> {
        crate::benchmark::validate_harness_descriptor(executable)?;
        let output = Command::new(executable)
            .args(["describe", "--json"])
            .output()?;
        serde_json::from_slice(&output.stdout).context("reading BaseRT benchmark capabilities")
    }

    fn select_model(&self, _paths: &crate::reports::Paths) -> Result<Option<PathBuf>> {
        crate::models::prompt_model_path()
    }

    fn confirm(&self, r: &BenchmarkRequest<'_>, yes: bool) -> Result<Option<bool>> {
        crate::benchmark::confirm_benchmark_run(
            r.model, r.pp, r.tg, r.reps, r.warmup, r.cooldown, yes,
        )
    }

    fn execute(&self, executable: &Path, r: &BenchmarkRequest<'_>) -> Result<RuntimeOutput> {
        let mut command = Command::new(executable);
        command
            .arg("run")
            .arg(r.model)
            .args(["--mode", "text", "-p", r.pp, "-n"])
            .arg(r.tg.to_string())
            .arg("-r")
            .arg(r.reps.to_string())
            .arg("-w")
            .arg(r.warmup.to_string())
            .arg("--telemetry");
        if r.cooldown {
            command.arg("--cooldown");
        }
        let output = command
            .stderr(Stdio::inherit())
            .output()
            .context("running BaseRT benchmark")?;
        if !output.status.success() {
            bail!("BaseRT benchmark exited with {}", output.status);
        }
        let benchmark: Value =
            serde_json::from_slice(&output.stdout).context("BaseRT returned invalid JSON")?;
        crate::benchmark::validate_harness_result(&benchmark)?;
        validate_requested_workloads(&benchmark, r)?;
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
