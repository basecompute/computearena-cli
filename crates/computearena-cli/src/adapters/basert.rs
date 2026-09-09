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

    fn select_model(&self) -> Result<Option<PathBuf>> {
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
        Ok(RuntimeOutput {
            benchmark,
            model: crate::models::inspect_model(r.model)?,
        })
    }
}
