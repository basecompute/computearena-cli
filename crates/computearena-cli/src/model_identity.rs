//! Runtime-neutral model identity and local acquisition provenance.
//!
//! A model class (the upstream Hugging Face repository), a converted artifact
//! repository, and the exact local bytes are separate identities. Reports keep
//! all three so the server can group equivalent model families without
//! pretending that unlike quantizations are the same artifact.

use crate::adapters::Runtime;
use crate::reports::{atomic_write_json, Paths};
use serde_json::{json, Value};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) const IDENTITY_SCHEMA: &str = "computearena-model/1";
const RECEIPT_SCHEMA: &str = "computearena-model-provenance/1";

#[derive(Clone, Debug)]
pub(crate) enum SubmissionModelVerification {
    Verified(String),
    Unresolved(String),
    Unavailable(String),
    Mismatch(String),
}

/// Advisory client-side check used immediately before submission. The server
/// repeats this independently because a public CLI is not a trust boundary.
pub(crate) fn verify_submission_model(report: &Value) -> SubmissionModelVerification {
    if report
        .pointer("/model/identity_schema")
        .and_then(Value::as_str)
        != Some(IDENTITY_SCHEMA)
    {
        return SubmissionModelVerification::Unresolved(
            "legacy report; no runtime-neutral model identity".to_string(),
        );
    }
    let artifact = match report.pointer("/model/artifact") {
        Some(Value::Object(artifact)) => artifact,
        _ => {
            return SubmissionModelVerification::Unresolved(
                "model artifact identity is incomplete".to_string(),
            )
        }
    };
    let sha256 = match artifact.get("sha256").and_then(Value::as_str) {
        Some(value) if value.len() == 64 => value,
        _ => {
            return SubmissionModelVerification::Unresolved(
                "model artifact SHA-256 is unavailable".to_string(),
            )
        }
    };
    let tuple = (
        artifact.get("provider").and_then(Value::as_str),
        artifact.get("repo_id").and_then(Value::as_str),
        artifact.get("revision").and_then(Value::as_str),
        artifact.get("path").and_then(Value::as_str),
    );
    let (Some("huggingface"), Some(repository), Some(revision), Some(path)) = tuple else {
        return SubmissionModelVerification::Unresolved(
            "exact Hugging Face repository, revision, and path are unavailable".to_string(),
        );
    };

    let identity = match crate::huggingface::artifact_identity(repository, revision, path) {
        Ok(identity) => identity,
        Err(error) => {
            return SubmissionModelVerification::Unavailable(format!(
                "could not contact Hugging Face: {error}"
            ))
        }
    };
    let Some(published_sha256) = identity.file.sha256.as_deref() else {
        return SubmissionModelVerification::Unresolved(
            "Hugging Face does not publish a SHA-256 for this artifact".to_string(),
        );
    };
    if !published_sha256.eq_ignore_ascii_case(sha256) {
        return SubmissionModelVerification::Mismatch(
            "artifact SHA-256 does not match the claimed Hugging Face file".to_string(),
        );
    }

    let claimed_canonical = report
        .pointer("/model/canonical/repo_id")
        .and_then(Value::as_str);
    if let (Some(claimed), Some(published)) = (
        claimed_canonical,
        identity.file.canonical_repository.as_deref(),
    ) {
        if !claimed.eq_ignore_ascii_case(published) {
            return SubmissionModelVerification::Mismatch(
                "canonical model does not match the Hugging Face repository metadata".to_string(),
            );
        }
    }
    SubmissionModelVerification::Verified(match identity.file.canonical_repository {
        Some(canonical) => format!("artifact hash and model family verified as {canonical}"),
        None => "artifact hash verified; model family remains unresolved".to_string(),
    })
}

fn receipts_dir(root: &Path) -> PathBuf {
    root.join("model-provenance")
}

fn receipt_path(root: &Path, sha256: &str) -> PathBuf {
    receipts_dir(root).join(format!("{sha256}.json"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_huggingface_download(
    root: &Path,
    repository: &str,
    revision: &str,
    file: &str,
    artifact_sha256: &str,
    expected_sha256: Option<&str>,
    canonical_repository: Option<&str>,
    method: &str,
) -> anyhow::Result<()> {
    let verification =
        if expected_sha256.is_some_and(|expected| expected.eq_ignore_ascii_case(artifact_sha256)) {
            "sha256"
        } else {
            "immutable_revision"
        };
    let receipt = json!({
        "schema": RECEIPT_SCHEMA,
        "artifact": {
            "provider": "huggingface",
            "repo_id": repository,
            "revision": revision,
            "path": file,
            "sha256": artifact_sha256
        },
        "canonical": canonical_repository.map(|repo_id| json!({
            "provider": "huggingface",
            "repo_id": repo_id,
            "revision": Value::Null,
            "verification": "publisher_asserted"
        })),
        "provenance": {
            "method": method,
            "artifact_verification": verification,
            "identity_source": if canonical_repository.is_some() {
                "huggingface_base_model"
            } else {
                "unresolved"
            }
        }
    });
    let path = receipt_path(root, artifact_sha256);
    if path.is_file() {
        let existing: Value = serde_json::from_slice(&fs::read(&path)?)?;
        if existing.pointer("/artifact/sha256").and_then(Value::as_str) == Some(artifact_sha256) {
            return Ok(());
        }
        anyhow::bail!(
            "model provenance for {artifact_sha256} conflicts with {}",
            path.display()
        );
    }
    atomic_write_json(&path, &receipt)
}

/// Bind manually acquired bytes to one exact Hugging Face file. The caller
/// has already compared the local SHA-256 with the Hub's LFS object ID, so the
/// receipt is safe to reuse even if the local file is later renamed.
pub(crate) fn identify_huggingface_file(
    root: &Path,
    model: &Path,
    source_url: &str,
) -> anyhow::Result<Value> {
    if !model.is_file() {
        anyhow::bail!("model file does not exist: {}", model.display());
    }
    let artifact_sha256 = crate::adapters::file_sha256(model)?;
    let source = crate::huggingface::file_identity(source_url)?;
    let expected = source.file.sha256.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "Hugging Face did not publish a SHA-256 for {}; the local file cannot be verified",
            source.file.path
        )
    })?;
    if !expected.eq_ignore_ascii_case(&artifact_sha256) {
        anyhow::bail!(
            "the local file does not match {}/{} at revision {}: expected {}, got {}",
            source.repository,
            source.file.path,
            source.file.revision,
            expected,
            artifact_sha256
        );
    }
    record_huggingface_download(
        root,
        &source.repository,
        &source.file.revision,
        &source.file.path,
        &artifact_sha256,
        Some(expected),
        source.file.canonical_repository.as_deref(),
        "manual_hash_match",
    )?;
    Ok(json!({
        "model": model,
        "sha256": artifact_sha256,
        "artifact_repository": source.repository,
        "artifact_revision": source.file.revision,
        "artifact_path": source.file.path,
        "canonical_repository": source.file.canonical_repository,
        "verification": "sha256"
    }))
}

fn read_receipt(paths: &Paths, artifact_sha256: &str) -> Option<Value> {
    let bytes = fs::read(receipt_path(&paths.root, artifact_sha256)).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    (value.get("schema").and_then(Value::as_str) == Some(RECEIPT_SCHEMA)
        && value.pointer("/artifact/sha256").and_then(Value::as_str) == Some(artifact_sha256))
    .then_some(value)
}

/// Recover the repository, immutable snapshot, and file name from the standard
/// Hugging Face cache layout. The file itself is still hashed independently.
fn huggingface_cache_artifact(path: &Path, artifact_sha256: &str) -> Option<Value> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let components: Vec<_> = absolute.components().collect();
    let model_index = components.iter().position(|component| match component {
        Component::Normal(value) => value
            .to_str()
            .is_some_and(|value| value.starts_with("models--")),
        _ => false,
    })?;
    let encoded = match components.get(model_index)? {
        Component::Normal(value) => value.to_str()?.strip_prefix("models--")?,
        _ => return None,
    };
    let (owner, model) = encoded.split_once("--")?;
    let snapshots = match components.get(model_index + 1)? {
        Component::Normal(value) => value.to_str()?,
        _ => return None,
    };
    if snapshots != "snapshots" {
        return None;
    }
    let revision = match components.get(model_index + 2)? {
        Component::Normal(value) => value.to_str()?,
        _ => return None,
    };
    if revision.len() < 7 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let file = components[(model_index + 3)..]
        .iter()
        .fold(PathBuf::new(), |path, component| {
            path.join(component.as_os_str())
        });
    let cache_blob_matches = fs::canonicalize(&absolute)
        .ok()
        .and_then(|resolved| resolved.file_name().map(|name| name.to_owned()))
        .and_then(|name| name.to_str().map(str::to_string))
        .is_some_and(|name| name.eq_ignore_ascii_case(artifact_sha256));
    Some(json!({
        "provider": "huggingface",
        "repo_id": format!("{owner}/{model}"),
        "revision": revision,
        "path": file.to_string_lossy(),
        "sha256": artifact_sha256,
        "cache_blob_sha256_verified": cache_blob_matches
    }))
}

fn huggingface_repo_id(url: &str) -> Option<String> {
    let value = url
        .trim()
        .strip_prefix("https://huggingface.co/")?
        .trim_matches('/');
    let mut parts = value.split('/');
    let owner = parts.next()?;
    let model = parts.next()?;
    if owner.is_empty() || model.is_empty() {
        return None;
    }
    Some(format!("{owner}/{model}"))
}

fn base_sidecar(path: &Path, artifact_sha256: &str) -> Option<Value> {
    let bytes = fs::read(path.parent()?.join("hub.json")).ok()?;
    let sidecar: Value = serde_json::from_slice(&bytes).ok()?;
    if sidecar
        .get("base_sha256")
        .and_then(Value::as_str)
        .is_some_and(|sha| !sha.eq_ignore_ascii_case(artifact_sha256))
    {
        return None;
    }
    let repository = sidecar.get("hf_repo").and_then(Value::as_str)?;
    let revision = sidecar
        .get("revision")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let canonical = sidecar.get("source_repo").and_then(Value::as_str);
    Some(json!({
        "artifact": {
            "provider": "huggingface",
            "repo_id": repository,
            "revision": revision,
            "path": path.file_name().and_then(|name| name.to_str()),
            "sha256": artifact_sha256
        },
        "canonical": canonical.map(|repo_id| json!({
            "provider": "huggingface",
            "repo_id": repo_id,
            "revision": Value::Null,
            "verification": "runtime_provenance"
        })),
        "provenance": {
            "method": "basert_hub_sidecar",
            "artifact_verification": if sidecar.get("base_sha256").is_some() {
                "sha256"
            } else {
                "runtime_record"
            },
            "identity_source": if canonical.is_some() {
                "basert_source_repo"
            } else {
                "unresolved"
            }
        }
    }))
}

fn embedded_gguf_identity(model: &Value, artifact_sha256: &str) -> Option<Value> {
    let artifact_repository = model
        .get("repo_url")
        .and_then(Value::as_str)
        .and_then(huggingface_repo_id);
    let canonical = model
        .get("source_repo_url")
        .and_then(Value::as_str)
        .and_then(huggingface_repo_id)
        .or_else(|| {
            model
                .get("base_model_repo_url")
                .and_then(Value::as_str)
                .and_then(huggingface_repo_id)
        });
    if artifact_repository.is_none() && canonical.is_none() {
        return None;
    }
    Some(json!({
        "artifact": {
            "provider": artifact_repository.as_ref().map(|_| "huggingface"),
            "repo_id": artifact_repository,
            "revision": Value::Null,
            "path": model.get("file_name"),
            "sha256": artifact_sha256
        },
        "canonical": canonical.as_ref().map(|repo_id| json!({
            "provider": "huggingface",
            "repo_id": repo_id,
            "revision": Value::Null,
            "verification": "embedded_metadata"
        })),
        "provenance": {
            "method": "embedded_gguf_metadata",
            "artifact_verification": "local_sha256",
            "identity_source": if canonical.is_some() {
                "gguf_source_metadata"
            } else {
                "unresolved"
            }
        }
    }))
}

fn fallback_evidence(runtime: Runtime, path: &Path, model: &Value, artifact_sha256: &str) -> Value {
    if runtime == Runtime::Basert {
        if let Some(evidence) = base_sidecar(path, artifact_sha256) {
            return evidence;
        }
    }
    if runtime == Runtime::LlamaCpp {
        let cache = huggingface_cache_artifact(path, artifact_sha256);
        let embedded = embedded_gguf_identity(model, artifact_sha256);
        if let Some(artifact) = cache {
            let canonical = embedded
                .as_ref()
                .and_then(|value| value.get("canonical"))
                .cloned()
                .unwrap_or(Value::Null);
            let identity_source = if canonical.is_null() {
                "unresolved"
            } else {
                "gguf_source_metadata"
            };
            return json!({
                "artifact": artifact,
                "canonical": canonical,
                "provenance": {
                    "method": "huggingface_cache",
                    "artifact_verification": if artifact["cache_blob_sha256_verified"] == true {
                        "sha256"
                    } else {
                        "local_sha256"
                    },
                    "identity_source": identity_source
                }
            });
        }
        if let Some(evidence) = embedded {
            return evidence;
        }
    }
    json!({
        "artifact": {
            "provider": Value::Null,
            "repo_id": Value::Null,
            "revision": Value::Null,
            "path": Value::Null,
            "sha256": artifact_sha256
        },
        "canonical": Value::Null,
        "provenance": {
            "method": "local_file",
            "artifact_verification": "local_sha256",
            "identity_source": "unresolved"
        }
    })
}

pub(crate) fn finalize(
    runtime: Runtime,
    paths: &Paths,
    model_path: &Path,
    mut model: Value,
    artifact_sha256: &str,
) -> Value {
    let evidence = read_receipt(paths, artifact_sha256)
        .unwrap_or_else(|| fallback_evidence(runtime, model_path, &model, artifact_sha256));
    let artifact = evidence
        .get("artifact")
        .cloned()
        .unwrap_or_else(|| json!({"sha256": artifact_sha256}));
    let canonical = evidence.get("canonical").cloned().unwrap_or(Value::Null);
    let provenance = evidence
        .get("provenance")
        .cloned()
        .unwrap_or_else(|| json!({"method":"local_file"}));
    let scheme = model
        .get("quantization")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let format = model
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or(match runtime {
            Runtime::Basert => "base",
            Runtime::LlamaCpp => "gguf",
        });
    let mut artifact = artifact;
    artifact["format"] = json!(format);
    artifact["quantization"] = json!({
        "namespace": match runtime {
            Runtime::Basert => "basert",
            Runtime::LlamaCpp => "gguf",
        },
        "scheme": scheme
    });

    let upstream_id = canonical.get("repo_id").cloned().unwrap_or(Value::Null);
    let identity_source = provenance
        .get("identity_source")
        .cloned()
        .unwrap_or_else(|| json!("unresolved"));
    let identity_verification = canonical
        .get("verification")
        .cloned()
        .unwrap_or_else(|| json!("unverified"));
    model["identity_schema"] = json!(IDENTITY_SCHEMA);
    model["canonical"] = canonical;
    model["artifact"] = artifact;
    model["provenance"] = provenance;
    // Retain the released fields while the server learns the richer schema.
    model["upstream_id"] = upstream_id;
    model["upstream_id_source"] = identity_source;
    model["identity_verification"] = identity_verification;
    model["artifact_sha256"] = json!(artifact_sha256);
    model
}

pub(crate) fn report_identity_notice(model: &Value) -> String {
    let canonical = model.pointer("/canonical/repo_id").and_then(Value::as_str);
    let exact_artifact = [
        "/artifact/repo_id",
        "/artifact/revision",
        "/artifact/path",
        "/artifact/sha256",
    ]
    .into_iter()
    .all(|pointer| model.pointer(pointer).and_then(Value::as_str).is_some());

    match (exact_artifact, canonical) {
        (true, Some(canonical)) => format!(
            "Model identity recorded as {}; the server will independently verify the exact artifact when submitted.",
            canonical
        ),
        (true, None) => "The exact model artifact was recorded for server verification, but its canonical model family is unresolved.".to_string(),
        (false, _) => "Model identity is unresolved. The signed report remains submittable and will be labelled unverified; use computearena identify to bind manually acquired bytes to an exact Hugging Face file.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_huggingface_snapshot_identity() {
        let path = Path::new(
            "/cache/hub/models--bartowski--Qwen-GGUF/snapshots/0123456789abcdef/model.gguf",
        );
        let artifact = huggingface_cache_artifact(path, "aa").unwrap();
        assert_eq!(artifact["repo_id"], "bartowski/Qwen-GGUF");
        assert_eq!(artifact["revision"], "0123456789abcdef");
        assert_eq!(artifact["path"], "model.gguf");
    }

    #[test]
    fn extracts_huggingface_repository_from_urls() {
        assert_eq!(
            huggingface_repo_id("https://huggingface.co/Qwen/Qwen3-4B/tree/main"),
            Some("Qwen/Qwen3-4B".to_string())
        );
        assert_eq!(huggingface_repo_id("https://example.com/a/b"), None);
    }

    #[test]
    fn download_receipt_populates_canonical_artifact_and_compatibility_fields() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(temporary.path().to_path_buf())).unwrap();
        let sha256 = "a".repeat(64);
        record_huggingface_download(
            &paths.root,
            "bartowski/Qwen3-4B-GGUF",
            "0123456789abcdef",
            "Qwen3-4B-Q4_K_M.gguf",
            &sha256,
            Some(&sha256),
            Some("Qwen/Qwen3-4B"),
            "computearena_download",
        )
        .unwrap();
        let model = finalize(
            Runtime::LlamaCpp,
            &paths,
            Path::new("/tmp/renamed.gguf"),
            json!({"name": "Qwen", "format": "gguf", "quantization": "Q4_K_M"}),
            &sha256,
        );

        assert_eq!(model["identity_schema"], IDENTITY_SCHEMA);
        assert_eq!(model["canonical"]["repo_id"], "Qwen/Qwen3-4B");
        assert_eq!(model["artifact"]["repo_id"], "bartowski/Qwen3-4B-GGUF");
        assert_eq!(model["artifact"]["revision"], "0123456789abcdef");
        assert_eq!(model["artifact"]["quantization"]["namespace"], "gguf");
        assert_eq!(model["artifact"]["quantization"]["scheme"], "Q4_K_M");
        assert_eq!(model["upstream_id"], "Qwen/Qwen3-4B");
        assert_eq!(model["artifact_sha256"], sha256);
    }

    #[test]
    fn unknown_local_files_remain_unresolved_but_keep_exact_artifact_identity() {
        let temporary = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(temporary.path().to_path_buf())).unwrap();
        let sha256 = "b".repeat(64);
        let model = finalize(
            Runtime::Basert,
            &paths,
            Path::new("/tmp/unknown.base"),
            json!({"name": "unknown", "quantization": "base_q4"}),
            &sha256,
        );

        assert!(model["canonical"].is_null());
        assert_eq!(model["artifact"]["format"], "base");
        assert_eq!(model["artifact"]["quantization"]["namespace"], "basert");
        assert_eq!(model["provenance"]["method"], "local_file");
        assert_eq!(model["artifact_sha256"], sha256);
    }

    #[test]
    fn report_notice_never_claims_local_execution_attestation() {
        let unresolved = json!({
            "artifact": {"sha256": "aa"},
            "canonical": null
        });
        assert!(report_identity_notice(&unresolved).contains("unresolved"));
        assert!(report_identity_notice(&unresolved).contains("unverified"));

        let resolvable = json!({
            "artifact": {
                "repo_id": "basecompute/Qwen3-4B",
                "revision": "0123456789abcdef",
                "path": "model.base",
                "sha256": "aa"
            },
            "canonical": {"repo_id": "Qwen/Qwen3-4B"}
        });
        let notice = report_identity_notice(&resolvable);
        assert!(notice.contains("server will independently verify"));
        assert!(!notice.contains("execution verified"));
    }
}
