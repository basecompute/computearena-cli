//! Platform probes are advisory and run after measurement, never in its hot path.
use serde_json::{json, Value};
use std::{path::Path, time::Duration};

fn query(program: &Path, args: &[&str]) -> Option<String> {
    crate::telemetry::snapshots::query(program, args, Duration::from_secs(2))
}

/// rocminfo contains CPU agents and many nested Name fields. Only parse the
/// top-level agent header, and require exactly one GPU with a marketing name.
#[cfg(any(target_os = "linux", test))]
pub(super) fn amd_name(text: &str) -> Option<String> {
    let mut devices = Vec::new();
    let mut name = None;
    let mut gpu = false;
    let mut agent = false;
    for line in text.lines().chain(std::iter::once("Agent end")) {
        let trimmed = line.trim();
        if trimmed.starts_with("Agent ") {
            if agent && gpu {
                devices.push(name.take());
            }
            name = None;
            gpu = false;
            agent = true;
        } else if agent {
            if let Some(value) = trimmed.strip_prefix("Marketing Name:") {
                name = Some(value.trim().to_string());
            }
            if let Some(value) = trimmed.strip_prefix("Device Type:") {
                gpu = value.trim() == "GPU";
            }
        }
    }
    if devices.len() != 1 {
        return None;
    }
    devices
        .pop()
        .flatten()
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("unknown"))
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn cpu_name(text: &str) -> Option<String> {
    let mut names = std::collections::BTreeSet::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.trim() == "model name" && !value.trim().is_empty() {
            names.insert(value.trim().to_string());
        }
    }
    // Never invent one model for a heterogeneous CPU configuration.
    if names.len() == 1 {
        names.pop_first()
    } else {
        None
    }
}

pub(super) fn resolve(report: &mut Value) {
    let missing = match report.get("chip") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty() || s.trim().eq_ignore_ascii_case("unknown"),
        _ => false,
    };
    if !missing {
        return;
    }
    let backend = report["backend"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    let detected = detect(&backend);
    if let Some((name, source)) = detected {
        report["chip_detection"] = json!({
            "source": source,
            "method": "unambiguous_host_device",
            "reported_chip": report.get("chip").cloned().unwrap_or(Value::Null),
            "resolved_chip": name
        });
        report["chip"] = json!(name);
    }
}

fn detect(backend: &str) -> Option<(String, &'static str)> {
    #[cfg(target_os = "linux")]
    {
        if matches!(backend, "rocm" | "hip") {
            // These APIs use different ordinal spaces; do not guess mappings,
            // including a mask of "0", because rocminfo may itself be filtered.
            if [
                "ROCR_VISIBLE_DEVICES",
                "HIP_VISIBLE_DEVICES",
                "CUDA_VISIBLE_DEVICES",
                "GPU_DEVICE_ORDINAL",
            ]
            .iter()
            .any(|k| std::env::var_os(k).is_some())
            {
                return None;
            }
            let executable = crate::benchmark::executable_on_path("rocminfo").or_else(|| {
                Path::new("/opt/rocm/bin/rocminfo")
                    .is_file()
                    .then(|| Path::new("/opt/rocm/bin/rocminfo").to_path_buf())
            })?;
            return amd_name(&query(&executable, &[])?).map(|s| (s, "rocminfo"));
        }
        if backend == "cpu" {
            let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
            return cpu_name(&text).map(|s| (s, "linux_cpuinfo"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        if matches!(backend, "cpu" | "metal" | "mtl") {
            let name = query(
                Path::new("/usr/sbin/sysctl"),
                &["-n", "machdep.cpu.brand_string"],
            )?;
            let name = name.trim();
            // An Intel Mac's CPU brand cannot identify its discrete Metal GPU.
            if !name.is_empty() && (backend == "cpu" || name.starts_with("Apple M")) {
                return Some((name.to_string(), "sysctl"));
            }
        }
    }
    // Vulkan/OpenCL inventories do not prove which device the runtime used.
    // Preserve the runtime's names instead of falling back to a vendor probe.
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn amd_agents_are_not_confused_with_cpus_or_isa_names() {
        let one = "Agent 1\n Marketing Name: AMD Ryzen CPU\n Device Type: CPU\nAgent 2\n Marketing Name: AMD Radeon 8060S Graphics\n Device Type: GPU\n Name: gfx1151\n";
        assert_eq!(amd_name(one).as_deref(), Some("AMD Radeon 8060S Graphics"));
        assert_eq!(
            amd_name(
                &(one.to_owned() + "Agent 3\n Marketing Name: Other GPU\n Device Type: GPU\n")
            ),
            None
        );
        assert_eq!(amd_name("Agent 1\n Device Type: GPU\n"), None);
        assert_eq!(amd_name("garbage"), None);
    }
    #[test]
    fn cpu_models_require_consistency() {
        assert_eq!(
            cpu_name("model name : Intel CPU\nmodel name : Intel CPU").as_deref(),
            Some("Intel CPU")
        );
        assert_eq!(
            cpu_name("model name : Intel CPU\nmodel name : AMD CPU"),
            None
        );
        assert_eq!(cpu_name("Hardware : generic ARM"), None);
    }
}
