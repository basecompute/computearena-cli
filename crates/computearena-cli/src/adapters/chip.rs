//! Best-effort metadata repair for older CUDA harness releases.
//! This is a host observation, not hardware attestation.
use serde_json::{json, Value};
use std::time::Duration;

fn needs_fallback(report: &Value) -> bool {
    report["backend"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("cuda"))
        && match report.get("chip") {
            None | Some(Value::Null) => true,
            Some(Value::String(s)) => {
                s.trim().is_empty() || s.trim().eq_ignore_ascii_case("unknown")
            }
            _ => false,
        }
}

/// Only a single physical device is sufficient evidence here. CUDA numeric
/// ordinals need not match nvidia-smi indices; never infer a multi-GPU mapping.
fn select_name<'a>(text: &'a str, visible: Option<&str>) -> Option<&'a str> {
    let rows: Vec<_> = text.lines().filter(|s| !s.trim().is_empty()).collect();
    if rows.len() != 1 {
        return None;
    }
    let fields: Vec<_> = rows[0].split(',').map(str::trim).collect();
    if fields.len() != 3 || fields[0] != "0" || !fields[1].starts_with("GPU-") {
        return None;
    }
    if let Some(mask) = visible {
        // Exact UUID only: no prefixes, MIG identifiers or multiple devices.
        if mask != "0" && mask != fields[1] {
            return None;
        }
    }
    let name = fields[2];
    if name.is_empty() || name.eq_ignore_ascii_case("unknown") || name == "N/A" {
        return None;
    }
    Some(name)
}

fn apply(report: &mut Value, text: &str, visible: Option<&str>) {
    if !needs_fallback(report) {
        return;
    }
    let Some(name) = select_name(text, visible) else {
        return;
    };
    let original = report.get("chip").cloned().unwrap_or(Value::Null);
    report["chip"] = json!(name);
    report["chip_detection"] = json!({
        "source": "nvidia-smi",
        "method": "single_physical_gpu",
        "reported_chip": original,
        "resolved_chip": name
    });
}

pub(super) fn resolve(report: &mut Value) {
    if !needs_fallback(report) {
        return;
    }
    let visible = std::env::var_os("CUDA_VISIBLE_DEVICES");
    // Non-Unicode visibility settings are not safe to interpret.
    let mask = match visible.as_ref().map(|s| s.to_str()) {
        Some(None) => return,
        Some(Some(s)) => Some(s),
        None => None,
    };
    let Some(path) = crate::benchmark::executable_on_path("nvidia-smi") else {
        return;
    };
    if let Some(text) = crate::telemetry::snapshots::query(
        &path,
        &[
            "--query-gpu=index,uuid,name",
            "--format=csv,noheader,nounits",
        ],
        Duration::from_secs(2),
    ) {
        apply(report, &text, mask);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const GPU: &str = "0, GPU-example, NVIDIA GB10\n";

    #[test]
    fn repairs_only_missing_cuda_identity_and_preserves_original() {
        for chip in [Value::Null, json!(""), json!(" unknown "), json!("UNKNOWN")] {
            let mut report = json!({"backend":"cuda","chip":chip});
            apply(&mut report, GPU, None);
            assert_eq!(report["chip"], "NVIDIA GB10");
            assert_eq!(report["chip_detection"]["reported_chip"], chip);
        }
        let mut report = json!({"backend":"CUDA"});
        apply(&mut report, GPU, None);
        assert_eq!(report["chip"], "NVIDIA GB10");
        for report in [
            json!({"backend":"cuda","chip":"NVIDIA RTX 5090"}),
            json!({"backend":"metal","chip":"unknown"}),
            json!({"backend":"cpu","chip":"unknown"}),
        ] {
            let mut actual = report.clone();
            apply(&mut actual, GPU, None);
            assert_eq!(actual, report);
        }
    }

    #[test]
    fn rejects_ambiguous_devices_and_visibility_masks() {
        assert_eq!(select_name(GPU, Some("0")), Some("NVIDIA GB10"));
        assert_eq!(select_name(GPU, Some("GPU-example")), Some("NVIDIA GB10"));
        for mask in ["", "-1", "1", "0,1", "GPU-ex", "MIG-example"] {
            assert_eq!(select_name(GPU, Some(mask)), None);
        }
        for text in [
            "",
            "invalid",
            "0, GPU-example, N/A",
            "0, GPU-example, unknown",
            "1, GPU-example, NVIDIA GB10",
            "0, GPU-a, NVIDIA A\n1, GPU-b, NVIDIA B",
        ] {
            assert_eq!(select_name(text, None), None);
        }
    }

    #[test]
    fn fallback_is_covered_by_report_signature() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let mut benchmark = json!({"backend":"cuda","chip":"unknown"});
        apply(&mut benchmark, GPU, None);
        let public = key.verifying_key().to_bytes();
        let mut report = json!({
            "schema": "computearena-benchmark/1",
            "installation": {
                "public_key": crate::b64_encode(&public),
                "key_id": crate::sha256_hex(&public)
            },
            "benchmark": benchmark
        });
        crate::reports::sign_report(&mut report, &key).unwrap();
        crate::reports::verify_report(&report).unwrap();
        report["benchmark"]["chip_detection"]["reported_chip"] = json!("tampered");
        assert!(crate::reports::verify_report(&report).is_err());
    }
}
