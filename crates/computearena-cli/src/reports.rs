use crate::config::PRIVATE_FILE_MODE;
use crate::protocol::{
    REPORT_SCHEMA, SIGNATURE_ALGORITHM, SIGNATURE_CANONICALIZATION, SIGNATURE_DOMAIN,
};
use crate::ui::{finish_activity, start_activity, TerminalUi};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::OsRng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub(crate) struct Paths {
    pub(crate) root: PathBuf,
    pub(crate) reports: PathBuf,
    pub(crate) secret_key: PathBuf,
    pub(crate) auth: PathBuf,
}
impl Paths {
    pub(crate) fn resolve(override_root: Option<PathBuf>) -> Result<Self> {
        let root = match override_root {
            Some(path) => path,
            None => {
                let base = dirs::data_local_dir()
                    .context("could not determine the local data directory")?;
                default_root(&base)
            }
        };
        Ok(Self {
            reports: root.join("reports"),
            secret_key: root.join("keys").join("installation.ed25519"),
            auth: root.join("auth.json"),
            root,
        })
    }

    pub(crate) fn prepare(&self) -> Result<()> {
        fs::create_dir_all(&self.reports)
            .with_context(|| format!("creating {}", self.reports.display()))?;
        fs::create_dir_all(
            self.secret_key
                .parent()
                .context("installation key path has no parent")?,
        )
        .with_context(|| format!("creating key directory under {}", self.root.display()))?;
        Ok(())
    }
}
/// The data directory under the platform's local data directory:
/// `computearena`, which replaced the `basert/computearena` location used
/// while the client shipped inside BaseRT. The first run after upgrading moves
/// the old directory whole, so reports, the signing key, saved sessions, and
/// installed runtimes come along, and says so once. Both directories existing
/// means the move was done by hand or an older client has run since; the
/// current one wins and the other is mentioned so nothing is silently ignored.
/// Messages go to stderr so `list --json` stays machine-readable.
pub(crate) fn default_root(base: &Path) -> PathBuf {
    let current = base.join("computearena");
    let legacy = base.join("basert").join("computearena");
    if current.exists() {
        if legacy.is_dir() && !is_empty_dir(&legacy) {
            eprintln!(
                "Note: data from an older ComputeArena remains at {} and is not used; the data directory is {}.",
                legacy.display(),
                current.display()
            );
        }
        return current;
    }
    if !legacy.is_dir() {
        return current;
    }
    match fs::rename(&legacy, &current) {
        Ok(()) => {
            // The old parent held only this; removing it fails harmlessly
            // when BaseRT or anything else still keeps files there.
            if let Some(parent) = legacy.parent() {
                let _ = fs::remove_dir(parent);
            }
            eprintln!(
                "Moved ComputeArena data from {} to {}.",
                legacy.display(),
                current.display()
            );
            current
        }
        Err(error) => {
            eprintln!(
                "Warning: could not move ComputeArena data from {} to {} ({error}); using the old location.",
                legacy.display(),
                current.display()
            );
            legacy
        }
    }
}

fn is_empty_dir(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(true)
}

pub(crate) fn load_or_create_installation_key(paths: &Paths) -> Result<SigningKey> {
    if paths.secret_key.is_file() {
        return load_installation_key(&paths.secret_key);
    }
    paths.prepare()?;
    let key = SigningKey::generate(&mut OsRng);
    let parent = paths
        .secret_key
        .parent()
        .context("installation key path has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating installation key under {}", parent.display()))?;
    set_private_permissions(temp.as_file())?;
    temp.write_all(&key.to_bytes())?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(&paths.secret_key) {
        Ok(_) => Ok(key),
        Err(_error) if paths.secret_key.is_file() => load_installation_key(&paths.secret_key),
        Err(error) => Err(error.error)
            .with_context(|| format!("saving installation key to {}", paths.secret_key.display())),
    }
}

fn load_installation_key(path: &Path) -> Result<SigningKey> {
    let bytes =
        fs::read(path).with_context(|| format!("reading installation key {}", path.display()))?;
    signing_key_from_bytes(&bytes)
}

fn signing_key_from_bytes(bytes: &[u8]) -> Result<SigningKey> {
    let key: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Ed25519 private key length"))?;
    Ok(SigningKey::from_bytes(&key))
}

pub(crate) fn b64_encode(bytes: &[u8]) -> String {
    BASE64_STANDARD.encode(bytes)
}

fn b64_decode(value: &str) -> Result<Vec<u8>> {
    BASE64_STANDARD
        .decode(value)
        .context("invalid base64 encoding")
}

#[cfg(unix)]
pub(crate) fn set_private_permissions(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn set_private_permissions(_file: &fs::File) -> Result<()> {
    Ok(())
}

pub(crate) fn sign_report(report: &mut Value, key: &SigningKey) -> Result<()> {
    if report.get("signature").is_some() {
        bail!("refusing to sign a report that already has a signature");
    }
    let payload = signature_payload(report)?;
    let signature = key.sign(&payload);
    report
        .as_object_mut()
        .context("report must be a JSON object")?
        .insert(
            "signature".to_string(),
            json!({
                "algorithm": SIGNATURE_ALGORITHM,
                "canonicalization": SIGNATURE_CANONICALIZATION,
                "value": b64_encode(&signature.to_bytes())
            }),
        );
    Ok(())
}

pub(crate) fn verify_report(report: &Value) -> Result<String> {
    if report.get("schema").and_then(Value::as_str) != Some(REPORT_SCHEMA) {
        bail!("unsupported or missing report schema (expected {REPORT_SCHEMA})");
    }
    let signature_value = report.get("signature").context("report is unsigned")?;
    if signature_value.get("algorithm").and_then(Value::as_str) != Some(SIGNATURE_ALGORITHM) {
        bail!("unsupported signature algorithm");
    }
    if signature_value
        .get("canonicalization")
        .and_then(Value::as_str)
        != Some(SIGNATURE_CANONICALIZATION)
    {
        bail!("unsupported signature canonicalization");
    }
    let signature_bytes = b64_decode(
        signature_value
            .get("value")
            .and_then(Value::as_str)
            .context("signature value is missing")?,
    )?;
    let signature =
        Signature::from_slice(&signature_bytes).context("invalid Ed25519 signature length")?;
    let installation = report
        .get("installation")
        .context("installation identity is missing")?;
    let public_bytes = b64_decode(
        installation
            .get("public_key")
            .and_then(Value::as_str)
            .context("installation public key is missing")?,
    )?;
    let public_array: [u8; 32] = public_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid Ed25519 public key length"))?;
    let public = VerifyingKey::from_bytes(&public_array).context("invalid Ed25519 public key")?;
    let expected_key_id = sha256_hex(&public_array);
    let key_id = installation
        .get("key_id")
        .and_then(Value::as_str)
        .context("installation key ID is missing")?;
    if key_id != expected_key_id {
        bail!("installation key ID does not match its public key");
    }
    let mut unsigned = report.clone();
    unsigned
        .as_object_mut()
        .context("report must be a JSON object")?
        .remove("signature");
    let payload = signature_payload(&unsigned)?;
    public.verify(&payload, &signature).map_err(|_| {
        anyhow::anyhow!(
            "signature verification failed; the report was modified after signing or has an invalid signature"
        )
    })?;
    Ok(key_id.to_string())
}

fn signature_payload(unsigned_report: &Value) -> Result<Vec<u8>> {
    let mut payload = Vec::from(SIGNATURE_DOMAIN);
    write_canonical_json(unsigned_report, &mut payload)?;
    Ok(payload)
}

// ComputeArena JSON v1 supports ordinary JSON values, recursively sorts object
// keys by UTF-8 bytes, preserves array order, and uses serde_json's stable
// string/number encoding. The named protocol keeps this independent from the
// pretty-printed file representation and leaves room for an RFC 8785 migration.
pub(crate) fn write_canonical_json(value: &Value, out: &mut Vec<u8>) -> Result<()> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(value) => out.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => out.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => out.extend_from_slice(serde_json::to_string(value)?.as_bytes()),
        Value::Array(values) => {
            out.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical_json(value, out)?;
            }
            out.push(b']');
        }
        Value::Object(values) => {
            out.push(b'{');
            let mut keys: Vec<&String> = values.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(serde_json::to_string(key)?.as_bytes());
                out.push(b':');
                write_canonical_json(&values[*key], out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

pub(crate) fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    if path.exists() {
        bail!("refusing to overwrite existing report: {}", path.display());
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating report under {}", parent.display()))?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)
        .map_err(|error| error.error)
        .with_context(|| format!("saving report to {}", path.display()))?;
    Ok(())
}

pub(crate) fn list_reports(paths: &Paths, as_json: bool) -> Result<()> {
    let ui = TerminalUi::detect();
    let reports = if as_json {
        report_summaries(paths)?
    } else {
        let started = start_activity(ui, "Loading and verifying saved benchmarks…");
        let reports = report_summaries(paths)?;
        finish_activity(
            ui,
            started,
            format!("Found {} saved benchmark(s)", reports.len()),
        );
        reports
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
        return Ok(());
    }
    if reports.is_empty() {
        println!("No local benchmarks found in {}", paths.reports.display());
        return Ok(());
    }
    println!(
        "{}",
        ui.brand_bold(format!("Local benchmarks ({})", reports.len()))
    );
    // Oldest first, so the newest benchmark is the one left in front of you
    // when the listing ends — at the prompt, or at the bottom of a log pane.
    // `--json` keeps the newest-first order machines and the pickers use.
    for (index, report) in reports.iter().rev().enumerate() {
        let status = report["status"].as_str().unwrap_or("invalid");
        let status_label = if status == "valid" {
            ui.success("VALID")
        } else {
            ui.error("INVALID")
        };
        println!(
            "\n  {} {}  [{}]",
            ui.brand_bold(format!("{}.", index + 1)),
            report["model"].as_str().unwrap_or("Unknown model"),
            status_label
        );
        println!(
            "     Ran: {}",
            report["created_at"].as_str().unwrap_or("Unknown time")
        );
        println!(
            "     Device: {} ({})",
            report["device"].as_str().unwrap_or("Unknown device"),
            report["backend"].as_str().unwrap_or("unknown backend")
        );
        println!(
            "     Runtime: {} {} | {} | {}",
            report["runtime"].as_str().unwrap_or("basert"),
            report["runtime_version"].as_str().unwrap_or("unknown"),
            report["architecture"]
                .as_str()
                .unwrap_or("unknown architecture"),
            report["quantization"]
                .as_str()
                .unwrap_or("unknown quantization")
        );

        println!("     Throughput:");
        let prefill = report["prefill"].as_array().cloned().unwrap_or_default();
        if prefill.is_empty() {
            println!("       Prefill    Unavailable");
        } else {
            for (sample_index, value) in prefill.iter().enumerate() {
                println!(
                    "       {:<10} {:>5} tokens   {:>10.1} tok/s",
                    if sample_index == 0 { "Prefill" } else { "" },
                    value["tokens"].as_u64().unwrap_or(0),
                    value["tokens_per_second"].as_f64().unwrap_or(0.0)
                );
            }
        }
        if let Some(decode) = report["decode"].as_object() {
            println!(
                "       {:<10} {:>5} tokens   {:>10.1} tok/s",
                "Decode",
                decode.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                decode
                    .get("tokens_per_second")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
            );
        } else {
            println!("       Decode     Unavailable");
        }

        let mut telemetry = Vec::new();
        if let Some(memory) = report["peak_memory_mb"].as_f64() {
            telemetry.push(format!("observed peak memory {memory:.0} MiB"));
        }
        if let Some(temperature) = report["ending_temperature_c"].as_f64() {
            telemetry.push(format!("ending temperature {temperature:.1}°C"));
        }
        if !telemetry.is_empty() {
            println!("     Telemetry: {}", telemetry.join(" | "));
        }
        println!(
            "     Report ID: {}",
            report["short_id"].as_str().unwrap_or("unknown")
        );
        if status != "valid" {
            println!("     Verification: {status}");
        }
    }
    println!("\nStored in: {}", paths.reports.display());
    Ok(())
}

pub(crate) fn report_summaries(paths: &Paths) -> Result<Vec<Value>> {
    if !paths.reports.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = fs::read_dir(&paths.reports)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    let mut reports = Vec::new();
    for path in files {
        match read_report(&path) {
            Ok(value) => {
                let status = match verify_report(&value) {
                    Ok(_) => "valid".to_string(),
                    Err(error) => format!("invalid: {error}"),
                };
                let (model_id, variant) = model_identity_for_report(&value);
                let model = match variant.as_deref() {
                    Some(variant) => format!("{model_id} ({variant})"),
                    None => model_id.clone(),
                };
                let (prefill, decode) = throughput_for_report(&value);
                let created_at_unix_ms = value
                    .get("created_at_unix_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let run_id = value
                    .get("run_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                reports.push(json!({
                    "run_id": run_id,
                    "short_id": short_id(run_id),
                    "model": model,
                    "model_id": model_id,
                    "variant": variant,
                    "architecture": value.pointer("/model/architecture").and_then(Value::as_str),
                    "quantization": value.pointer("/model/quantization").and_then(Value::as_str),
                    "created_at_unix_ms": created_at_unix_ms,
                    "created_at": format_unix_ms(created_at_unix_ms),
                    "device": value.pointer("/benchmark/chip").and_then(Value::as_str),
                    "backend": value.pointer("/benchmark/backend").and_then(Value::as_str),
                    "runtime": value.pointer("/runtime/name").and_then(Value::as_str),
                    "runtime_version": value.pointer("/benchmark/runtime_version").and_then(Value::as_str)
                        .or_else(|| value.pointer("/runtime/computearena_version").and_then(Value::as_str)),
                    "prefill": prefill,
                    "decode": decode,
                    "peak_memory_mb": peak_memory_for_report(&value),
                    "ending_temperature_c": value.pointer("/benchmark/thermal/die_end_c").and_then(Value::as_f64),
                    "status": status,
                    "path": path
                }));
            }
            Err(error) => reports.push(json!({
                "run_id": "unknown",
                "short_id": "unknown",
                "model": "unknown",
                "created_at_unix_ms": null,
                "created_at": "Unknown time",
                "status": format!("invalid: {error}"),
                "path": path
            })),
        }
    }
    reports.sort_by(|left, right| {
        right["created_at_unix_ms"]
            .as_u64()
            .cmp(&left["created_at_unix_ms"].as_u64())
    });
    Ok(reports)
}

pub(crate) fn model_identity_for_report(report: &Value) -> (String, Option<String>) {
    if let Some(id) = report
        .pointer("/model/id")
        .or_else(|| report.pointer("/model/name"))
        .and_then(Value::as_str)
    {
        return (
            id.to_string(),
            report
                .pointer("/model/variant")
                .and_then(Value::as_str)
                .map(str::to_string),
        );
    }

    let file_name = report
        .pointer("/model/file_name")
        .and_then(Value::as_str)
        .unwrap_or("Unknown model");
    if file_name != "model.base" {
        return (file_name.to_string(), None);
    }
    let architecture = report
        .pointer("/model/architecture")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let quantization = report
        .pointer("/model/quantization")
        .and_then(Value::as_str)
        .unwrap_or("unknown quantization");
    (
        format!("Legacy {architecture} model ({quantization}; name unavailable)"),
        None,
    )
}

pub(crate) fn throughput_for_report(report: &Value) -> (Vec<Value>, Value) {
    let metrics = report
        .pointer("/benchmark/metrics")
        .and_then(Value::as_object);
    let mut prefill: Vec<(u64, f64)> = metrics
        .into_iter()
        .flat_map(|metrics| metrics.iter())
        .filter_map(|(name, value)| {
            let tokens = name
                .strip_prefix("pp")?
                .strip_suffix("_t_s")?
                .parse()
                .ok()?;
            Some((tokens, value.as_f64()?))
        })
        .collect();
    prefill.sort_by_key(|(tokens, _)| *tokens);
    let prefill = prefill
        .into_iter()
        .map(|(tokens, tokens_per_second)| {
            json!({"tokens": tokens, "tokens_per_second": tokens_per_second})
        })
        .collect();

    let decode = metrics
        .and_then(|metrics| metrics.get("decode_t_s"))
        .and_then(Value::as_f64)
        .map(|tokens_per_second| {
            json!({
                "tokens": report.pointer("/benchmark/params/tg").and_then(Value::as_u64),
                "tokens_per_second": tokens_per_second
            })
        })
        .unwrap_or(Value::Null);
    (prefill, decode)
}

pub(crate) fn peak_memory_for_report(report: &Value) -> Option<f64> {
    let memory_replay = report.pointer("/benchmark/telemetry/memory_replay/workloads");
    let prefill = memory_replay
        .and_then(|workloads| workloads.get("prefill"))
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|workloads| workloads.values());
    let decode = memory_replay
        .and_then(|workloads| workloads.get("decode"))
        .into_iter();
    prefill
        .chain(decode)
        .filter_map(|workload| workload.get("process_peak_mb").and_then(Value::as_f64))
        .reduce(f64::max)
        .or_else(|| {
            report
                .pointer("/benchmark/memory/process_lifetime_peak_rss_mb")
                .and_then(Value::as_f64)
        })
        .or_else(|| {
            report
                .pointer("/benchmark/memory/process_peak_rss_mb")
                .and_then(Value::as_f64)
        })
}

pub(crate) fn short_id(run_id: &str) -> &str {
    run_id.get(..12).unwrap_or(run_id)
}

pub(crate) fn format_unix_ms(milliseconds: u64) -> String {
    if milliseconds == 0 {
        return "Unknown time".to_string();
    }
    let seconds = milliseconds / 1000;
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_date_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

// Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

pub(crate) fn resolve_report(paths: &Paths, reference: &str) -> Result<PathBuf> {
    let direct = PathBuf::from(reference);
    if direct.is_file() {
        return Ok(direct);
    }
    let exact = paths.reports.join(reference);
    if exact.is_file() {
        return Ok(exact);
    }
    let exact_json = paths.reports.join(format!("{reference}.json"));
    if exact_json.is_file() {
        return Ok(exact_json);
    }
    if !paths.reports.is_dir() {
        bail!("report not found: {reference}");
    }
    let mut matches = Vec::new();
    for entry in fs::read_dir(&paths.reports)? {
        let path = entry?.path();
        if path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stem.starts_with(reference))
        {
            matches.push(path);
        }
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => bail!("report not found: {reference}"),
        _ => bail!("report prefix is ambiguous: {reference}"),
    }
}

pub(crate) fn read_report(path: &Path) -> Result<Value> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("opening report {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing report {}", path.display()))
}
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod data_directory_tests {
    use super::*;

    #[test]
    fn a_fresh_install_uses_the_computearena_directory_without_creating_it() {
        let base = tempfile::tempdir().unwrap();
        assert_eq!(default_root(base.path()), base.path().join("computearena"));
        assert!(!base.path().join("computearena").exists());
    }

    #[test]
    fn an_older_install_is_moved_whole_and_its_empty_parent_removed() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("basert").join("computearena");
        fs::create_dir_all(legacy.join("reports")).unwrap();
        fs::write(legacy.join("reports").join("run.json"), b"{}").unwrap();
        fs::create_dir_all(legacy.join("keys")).unwrap();
        fs::write(legacy.join("keys").join("installation.ed25519"), b"key").unwrap();
        fs::create_dir_all(legacy.join("runtimes").join("llama-cpp").join("b1")).unwrap();

        let root = default_root(base.path());
        assert_eq!(root, base.path().join("computearena"));
        assert!(root.join("reports").join("run.json").is_file());
        assert!(root.join("keys").join("installation.ed25519").is_file());
        assert!(root.join("runtimes").join("llama-cpp").join("b1").is_dir());
        assert!(!legacy.exists());
        assert!(!base.path().join("basert").exists());
    }

    #[test]
    fn a_basert_parent_with_other_files_is_left_in_place() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("basert").join("computearena");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(base.path().join("basert").join("other.txt"), b"x").unwrap();

        let root = default_root(base.path());
        assert_eq!(root, base.path().join("computearena"));
        assert!(!legacy.exists());
        assert!(base.path().join("basert").join("other.txt").is_file());
    }

    #[test]
    fn the_current_directory_wins_when_both_exist() {
        let base = tempfile::tempdir().unwrap();
        let legacy = base.path().join("basert").join("computearena");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("auth.json"), b"{}").unwrap();
        fs::create_dir_all(base.path().join("computearena")).unwrap();

        let root = default_root(base.path());
        assert_eq!(root, base.path().join("computearena"));
        assert!(legacy.join("auth.json").is_file());
    }
}
