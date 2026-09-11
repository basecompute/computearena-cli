//! Browse and pull BaseRT's public model catalogue through the installed
//! `basert` command. BaseRT remains responsible for backend-aware artifact
//! selection, split downloads, conversion, and its `hub.json` provenance.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub(crate) struct RemoteModel {
    pub(crate) id: String,
    pub(crate) variant: String,
    pub(crate) architecture: String,
    pub(crate) size_bytes: Option<u64>,
    pub(crate) pull_target: String,
}

pub(crate) fn locate_cli(harness: Option<&Path>) -> Result<PathBuf> {
    let sibling = harness
        .and_then(Path::parent)
        .map(|directory| directory.join("basert"));
    sibling
        .filter(|candidate| candidate.is_file())
        .or_else(|| crate::runtimes::executable_on_path("basert"))
        .or_else(|| {
            crate::runtimes::basert_install_dir()
                .map(|directory| directory.join("basert"))
                .filter(|candidate| candidate.is_file())
        })
        .context("the BaseRT model tool was not found beside basert-benchmark-harness or on PATH")
}

fn list(cli: &Path, remote: bool) -> Result<Vec<Value>> {
    let mut command = Command::new(cli);
    command.arg("list");
    if remote {
        command.arg("--remote");
    }
    let output = command
        .arg("--json")
        .output()
        .with_context(|| format!("running {} list", cli.display()))?;
    if !output.status.success() {
        bail!(
            "BaseRT could not list {}models: {}",
            if remote { "remote " } else { "installed " },
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("BaseRT returned an invalid model list")
}

pub(crate) fn available(harness: Option<&Path>) -> Result<Vec<RemoteModel>> {
    let cli = locate_cli(harness)?;
    Ok(normalize_available(list(&cli, true)?))
}

fn normalize_available(rows: Vec<Value>) -> Vec<RemoteModel> {
    let mut models = BTreeMap::new();
    let mut installed = BTreeSet::new();
    for row in rows {
        let Some(model) = remote_model(&row) else {
            continue;
        };
        // `basert pull --target` chooses a backend-compatible artifact. Several
        // catalogue rows can therefore represent one actionable choice; show
        // the choice once instead of promising a specific internal variant.
        let key = (model.id.to_ascii_lowercase(), model.pull_target.clone());
        if row.get("installed").and_then(Value::as_bool) == Some(true) {
            installed.insert(key);
            continue;
        }
        match models.entry(key) {
            Entry::Vacant(entry) => {
                entry.insert(model);
            }
            Entry::Occupied(mut entry) => {
                // Multiple backend variants can have different sizes. The
                // generic pull target cannot promise which one BaseRT picks.
                if entry.get().size_bytes != model.size_bytes {
                    entry.get_mut().size_bytes = None;
                }
            }
        }
    }
    models.retain(|key, _| !installed.contains(key));
    let mut models: Vec<_> = models.into_values().collect();
    models.sort_by(|left, right| {
        left.id
            .to_ascii_lowercase()
            .cmp(&right.id.to_ascii_lowercase())
            .then_with(|| left.pull_target.cmp(&right.pull_target))
    });
    models
}

fn remote_model(row: &Value) -> Option<RemoteModel> {
    let variant = row.get("variant")?.as_str()?.to_string();
    let pull_target = target_for(&variant).ok()?;
    let architecture = row.get("arch").and_then(Value::as_str).unwrap_or("unknown");
    if architecture == "whisper" {
        return None;
    }
    Some(RemoteModel {
        id: row.get("id")?.as_str()?.to_string(),
        variant,
        architecture: architecture.to_string(),
        size_bytes: row.get("size_bytes").and_then(Value::as_u64),
        pull_target,
    })
}

pub(crate) fn display_pull_target(target: &str) -> String {
    let scheme = target.strip_prefix("base-").unwrap_or(target);
    format!("BaseRT {}", scheme.to_ascii_uppercase())
}

fn target_for(variant: &str) -> Result<String> {
    let lower = variant.to_ascii_lowercase();
    for bits in ["2", "3", "4", "5", "6", "8"] {
        if lower
            .as_bytes()
            .windows(bits.len() + 1)
            .any(|window| window == format!("q{bits}").as_bytes())
        {
            return Ok(format!("base-q{bits}"));
        }
    }
    for target in ["bf16", "mxfp4", "nvfp4"] {
        if lower.contains(target) {
            return Ok(target.to_string());
        }
    }
    bail!("BaseRT cannot select the {variant} variant through `basert pull`")
}

fn same_quantization(left: &str, right: &str) -> bool {
    target_for(left).ok() == target_for(right).ok()
}

pub(crate) fn pull(cli: &Path, model: &RemoteModel) -> Result<PathBuf> {
    println!(
        "Downloading {} ({}) with BaseRT…",
        model.id, model.pull_target
    );
    let status = Command::new(cli)
        .arg("pull")
        .arg(&model.id)
        .args(["--target", &model.pull_target])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("running {} pull", cli.display()))?;
    if !status.success() {
        bail!("BaseRT model download exited with {status}");
    }
    let rows = list(cli, false)?;
    rows.into_iter()
        .find(|row| {
            row.get("id").and_then(Value::as_str) == Some(model.id.as_str())
                && row
                    .get("variant")
                    .and_then(Value::as_str)
                    .is_some_and(|variant| same_quantization(variant, &model.variant))
        })
        .and_then(|row| row.get("path").and_then(Value::as_str).map(PathBuf::from))
        .context("BaseRT completed the pull but did not list the installed model")
}

/// Record the provenance of a model `basert pull` just installed. The pull
/// itself is done; failing to match the file on the Hub only means the report
/// will identify the bytes by hash instead of by an exact file.
pub(crate) fn record_download(root: &Path, model: &Path) -> Result<()> {
    let artifact_sha256 = crate::adapters::file_sha256(model)?;
    match resolve_artifact(root, model, &artifact_sha256) {
        Ok(file) => println!("Matched the installed model to {file} on Hugging Face."),
        Err(error) => println!(
            "Could not match the installed model to a Hugging Face file ({error:#}); its reports will identify it by hash."
        ),
    }
    Ok(())
}

/// Bind an installed BaseRT model to the exact Hub file whose published
/// SHA-256 matches it, and save a receipt. BaseRT's `hub.json` names the
/// repository and usually a mutable ref, never the file, so the file name is
/// recovered from the repository listing rather than guessed from the local
/// path. Returns the matched `repository/path`.
pub(crate) fn resolve_artifact(root: &Path, model: &Path, artifact_sha256: &str) -> Result<String> {
    let sidecar_path = model
        .parent()
        .context("BaseRT model has no variant directory")?
        .join("hub.json");
    let sidecar: Value = serde_json::from_slice(
        &std::fs::read(&sidecar_path)
            .with_context(|| format!("reading {}", sidecar_path.display()))?,
    )
    .with_context(|| format!("parsing {}", sidecar_path.display()))?;
    let repository = sidecar
        .get("hf_repo")
        .and_then(Value::as_str)
        .context("BaseRT model provenance omitted its Hugging Face repository")?;
    if let Some(expected) = sidecar.get("base_sha256").and_then(Value::as_str) {
        if !expected.eq_ignore_ascii_case(artifact_sha256) {
            bail!(
                "the installed file does not match the SHA-256 recorded in {}",
                sidecar_path.display()
            );
        }
    }
    let revision = sidecar
        .get("revision")
        .and_then(Value::as_str)
        .unwrap_or("main");
    let file = crate::huggingface::find_file_by_sha256(repository, revision, artifact_sha256)?;
    let canonical = sidecar
        .get("source_repo")
        .and_then(Value::as_str)
        .or(file.canonical_repository.as_deref());
    crate::model_identity::record_huggingface_download(
        root,
        repository,
        &file.revision,
        &file.path,
        artifact_sha256,
        file.sha256.as_deref(),
        canonical,
        "basert_pull",
    )?;
    Ok(format!("{repository}/{}", file.path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_catalog_variants_to_pull_targets() {
        assert_eq!(target_for("default-q4").unwrap(), "base-q4");
        assert_eq!(target_for("cuda-q4mix").unwrap(), "base-q4");
        assert_eq!(target_for("default-q8").unwrap(), "base-q8");
        assert_eq!(target_for("bf16").unwrap(), "bf16");
        assert!(target_for("experimental").is_err());
    }

    #[test]
    fn catalogue_collapses_variants_that_share_one_pull_target() {
        let rows = vec![
            serde_json::json!({
                "id": "Qwen/Qwen3-4B",
                "variant": "default-q4",
                "arch": "qwen",
                "quant": "base_q4",
                "size_bytes": 100,
                "installed": false
            }),
            serde_json::json!({
                "id": "Qwen/Qwen3-4B",
                "variant": "metal-q4mix",
                "arch": "qwen",
                "quant": "base_q4",
                "size_bytes": 101,
                "installed": false
            }),
            serde_json::json!({
                "id": "Qwen/Qwen3-4B",
                "variant": "default-q8",
                "arch": "qwen",
                "quant": "base_q8",
                "size_bytes": 200,
                "installed": false
            }),
        ];
        let models = normalize_available(rows);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].pull_target, "base-q4");
        assert_eq!(models[1].pull_target, "base-q8");
    }

    #[test]
    fn catalogue_excludes_an_installed_target_and_unsupported_whisper_models() {
        let rows = vec![
            serde_json::json!({
                "id": "basecompute/Qwen3-4B",
                "variant": "default-q4",
                "arch": "qwen",
                "installed": true
            }),
            serde_json::json!({
                "id": "basecompute/Qwen3-4B",
                "variant": "metal-q4mix",
                "arch": "qwen",
                "installed": false
            }),
            serde_json::json!({
                "id": "basecompute/whisper-base",
                "variant": "default-q8",
                "arch": "whisper",
                "installed": false
            }),
        ];
        assert!(normalize_available(rows).is_empty());
    }
}
