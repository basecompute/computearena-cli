//! Slow/vendor probes run only before and after the benchmark process.
use serde_json::{json, Map, Value};
#[cfg(target_os = "linux")]
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::System;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT: u64 = 65_536;

pub(crate) fn query(program: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    // A file avoids a full stdout pipe blocking the child before wait finishes.
    let mut output = tempfile::tempfile().ok()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().ok()?))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None)
                if Instant::now() < deadline && output.metadata().ok()?.len() <= MAX_OUTPUT =>
            {
                thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    if output.metadata().ok()?.len() > MAX_OUTPUT {
        return None;
    }
    output.seek(SeekFrom::Start(0)).ok()?;
    let mut text = String::new();
    output.take(MAX_OUTPUT).read_to_string(&mut text).ok()?;
    Some(text)
}

fn number(value: &str) -> Option<f64> {
    value
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

fn mebibytes(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn operating_system() -> Value {
    json!({
        "family": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "version": System::long_os_version(),
        "kernel_version": System::kernel_version()
    })
}

fn host_resources() -> Value {
    let system = System::new_all();
    let total_memory = system.total_memory();
    let available_memory = system.available_memory();
    json!({
        "logical_cpu_count": system.cpus().len(),
        "physical_cpu_count": System::physical_core_count(),
        "memory": {
            "unit": "MiB",
            "total": mebibytes(total_memory),
            "available": mebibytes(available_memory),
            "available_percent": if total_memory > 0 {
                Some(available_memory as f64 * 100.0 / total_memory as f64)
            } else {
                None
            }
        },
        "swap": {
            "unit": "MiB",
            "total": mebibytes(system.total_swap()),
            "used": mebibytes(system.used_swap()),
            "active": system.used_swap() > 0
        }
    })
}

#[cfg(target_os = "macos")]
fn memory_pressure() -> Value {
    let text = query(
        Path::new("/usr/bin/memory_pressure"),
        &["-Q"],
        PROBE_TIMEOUT,
    );
    let free_percent = text.as_deref().and_then(|text| {
        text.lines().find_map(|line| {
            line.strip_prefix("System-wide memory free percentage:")
                .and_then(|value| number(value.trim_end_matches('%')))
        })
    });
    json!({"provider":"memory_pressure","available":free_percent.is_some(),
        "system_free_percent":free_percent,
        "reason_if_unavailable":"memory_pressure -Q failed or returned unsupported output"})
}

#[cfg(target_os = "linux")]
fn memory_pressure() -> Value {
    let text = fs::read_to_string("/proc/pressure/memory").ok();
    let mut classes = Map::new();
    if let Some(text) = text.as_deref() {
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let Some(class) = fields.next() else {
                continue;
            };
            let mut values = Map::new();
            for field in fields {
                let Some((key, value)) = field.split_once('=') else {
                    continue;
                };
                if let Some(value) = number(value) {
                    values.insert(key.to_owned(), json!(value));
                }
            }
            classes.insert(class.to_owned(), Value::Object(values));
        }
    }
    json!({"provider":"linux_psi","available":!classes.is_empty(),"classes":classes,
        "reason_if_unavailable":"/proc/pressure/memory is unavailable"})
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn memory_pressure() -> Value {
    json!({"available":false,"reason":"No memory-pressure provider for this OS yet"})
}

#[cfg(target_os = "macos")]
fn apple_accelerators() -> Vec<Value> {
    static DEVICES: OnceLock<Vec<Value>> = OnceLock::new();
    DEVICES
        .get_or_init(|| {
            let Some(text) = query(
                Path::new("/usr/sbin/system_profiler"),
                &["SPDisplaysDataType", "-json"],
                PROBE_TIMEOUT,
            ) else {
                return Vec::new();
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                return Vec::new();
            };
            value["SPDisplaysDataType"]
                .as_array()
                .into_iter()
                .flatten()
                .take(32)
                .map(|display| {
                    json!({
                        "name": display["sppci_model"].as_str().or_else(|| display["_name"].as_str()),
                        "core_count": display["sppci_cores"].as_str().and_then(|value| value.parse::<u32>().ok()),
                        "device_type": display["sppci_device_type"].as_str(),
                        "metal_support": display["spdisplays_metal"].as_str()
                    })
                })
                .collect()
        })
        .clone()
}

#[cfg(not(target_os = "macos"))]
fn apple_accelerators() -> Vec<Value> {
    Vec::new()
}

fn nvidia(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() != 7 {
                return None;
            }
            let index = fields[0].parse::<u32>().ok()?;
            Some(
                json!({"index":index,"name":fields[1],"temperature_c":number(fields[2]),
            "power_w":number(fields[3]),"memory_used_mb":number(fields[4]),
            "memory_total_mb":number(fields[5]),"utilization_percent":number(fields[6])}),
            )
        })
        .take(32)
        .collect()
}

fn rocm(text: &str) -> Vec<Value> {
    let Ok(Value::Object(document)) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    document
        .iter()
        .filter(|(key, _)| {
            key.strip_prefix("card")
                .is_some_and(|n| n.parse::<u32>().is_ok())
        })
        .take(32)
        .map(|(card, value)| {
            let mut fields = Map::new();
            fields.insert(
                "index".into(),
                json!(card.strip_prefix("card").unwrap().parse::<u32>().unwrap()),
            );
            for (source, target, scale) in [
                ("Temperature (Sensor edge) (C)", "temperature_c", 1.0),
                ("Average Graphics Package Power (W)", "power_w", 1.0),
                ("Current Socket Graphics Package Power (W)", "power_w", 1.0),
                ("GPU use (%)", "utilization_percent", 1.0),
                ("VRAM Total Used Memory (B)", "memory_used_mb", 1_048_576.0),
                ("VRAM Total Memory (B)", "memory_total_mb", 1_048_576.0),
            ] {
                let parsed = value[source].as_str().and_then(number).or_else(|| {
                    value[source]
                        .as_f64()
                        .filter(|v| v.is_finite() && *v >= 0.0)
                });
                if let Some(value) = parsed {
                    fields.insert(target.into(), json!(value / scale));
                }
            }
            Value::Object(fields)
        })
        .collect()
}

fn accelerators() -> Value {
    let mut providers = Vec::new();
    let apple_devices = apple_accelerators();
    if !apple_devices.is_empty() {
        providers
            .push(json!({"provider":"system_profiler","available":true,"devices":apple_devices}));
    }
    for (program, args) in [
        ("nvidia-smi", vec!["--query-gpu=index,name,temperature.gpu,power.draw,memory.used,memory.total,utilization.gpu","--format=csv,noheader,nounits"]),
        ("rocm-smi", vec!["--showtemp","--showpower","--showuse","--showmeminfo","vram","--json"]),
    ] {
        if let Some(path) = crate::runtimes::executable_on_path(program) {
            let devices = query(&path, &args, PROBE_TIMEOUT).map(|text|
                if program == "nvidia-smi" { nvidia(&text) } else { rocm(&text) }).unwrap_or_default();
            providers.push(json!({"provider":program,"available":!devices.is_empty(),"devices":devices,
                "reason_if_unavailable":"Probe failed, timed out, or returned unsupported output"}));
        }
    }
    json!({"scope":"all_visible_devices_not_process_attributed","providers":providers,
        "reason_if_unavailable":"No supported NVIDIA/ROCm command found; integrated GPU allocation counters are not exposed here"})
}

#[cfg(target_os = "macos")]
fn power_state() -> Value {
    let source = query(Path::new("/usr/bin/pmset"), &["-g", "batt"], PROBE_TIMEOUT);
    let source = source.as_deref().and_then(|text| {
        if text.contains("'AC Power'") {
            Some("ac")
        } else if text.contains("'Battery Power'") {
            Some("battery")
        } else {
            None
        }
    });
    let settings = query(Path::new("/usr/bin/pmset"), &["-g"], PROBE_TIMEOUT);
    let setting = |name: &str| {
        settings.as_deref().and_then(|text| {
            text.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                (fields.next()? == name).then(|| fields.next()).flatten()
            })
        })
    };
    let low_power = setting("lowpowermode").and_then(|value| match value {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });
    let power_mode = setting("powermode").and_then(|value| value.parse::<u32>().ok());
    let high_power = setting("highpowermode").and_then(|value| match value {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });
    let performance_mode = if low_power == Some(true) || power_mode == Some(1) {
        Some("low_power")
    } else if high_power == Some(true) || power_mode == Some(2) {
        Some("high_power")
    } else if power_mode == Some(0) {
        Some("automatic")
    } else if low_power == Some(false) {
        Some("standard_or_automatic")
    } else {
        None
    };
    json!({"provider":"pmset","power_source":source,"low_power_mode":low_power,
        "performance_mode":performance_mode,"performance_mode_raw":power_mode,
        "high_power_mode":high_power})
}

#[cfg(target_os = "linux")]
fn power_state() -> Value {
    let read = |path: &str| fs::read_to_string(path).ok().map(|s| s.trim().to_owned());
    let sources: Vec<Value> = fs::read_dir("/sys/class/power_supply")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .take(32)
        .filter_map(|entry| {
            let kind = fs::read_to_string(entry.path().join("type")).ok()?;
            if !matches!(kind.trim(), "Mains" | "USB" | "USB_C" | "USB_PD") {
                return None;
            }
            let online = fs::read_to_string(entry.path().join("online")).ok()?;
            Some(json!({"type":kind.trim(),"online":online.trim()=="1"}))
        })
        .collect();
    json!({"provider":"linux_sysfs","external_power_sources":sources,
        "platform_profile":read("/sys/firmware/acpi/platform_profile"),
        "cpu0_scaling_governor":read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        "low_power_mode":null,"note":"Governor/profile reported directly; no universal low-power boolean"})
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn power_state() -> Value {
    json!({"available":false,"reason":"No power-state provider for this OS yet"})
}

pub(super) fn capture() -> Value {
    let started = Instant::now();
    let operating_system = operating_system();
    let host = host_resources();
    let pressure = memory_pressure();
    let power = power_state();
    let accelerators = accelerators();
    json!({"operating_system":operating_system,"host":host,"memory_pressure":pressure,
        "power_state":power,"accelerators":accelerators,
        "probe_elapsed_ms":started.elapsed().as_secs_f64()*1000.0})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_parsers_do_not_turn_missing_sensors_into_zero_or_expose_raw_output() {
        let gpu = nvidia("0, Test GPU, 42, N/A, 2048, 8192, 70\n");
        assert_eq!(gpu[0]["temperature_c"], 42.0);
        assert!(gpu[0]["power_w"].is_null());
        assert!(nvidia("not a supported driver response").is_empty());
        let gpu = rocm(
            r#"{"card0":{"VRAM Total Used Memory (B)":"1048576","GPU use (%)":"9","secret":"omit"}}"#,
        );
        assert_eq!(gpu[0]["memory_used_mb"], 1.0);
        assert!(gpu[0].get("secret").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn probe_timeout_reaps_child_and_returns_unavailable() {
        let start = Instant::now();
        assert!(query(Path::new("/bin/sleep"), &["2"], Duration::from_millis(30)).is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn host_snapshot_reports_os_memory_swap_and_cpu_shape() {
        let os = operating_system();
        let host = host_resources();
        assert_eq!(os["family"], std::env::consts::OS);
        assert_eq!(os["architecture"], std::env::consts::ARCH);
        assert!(host["logical_cpu_count"].as_u64().unwrap_or(0) > 0);
        assert!(host["memory"]["total"].as_f64().unwrap_or(0.0) > 0.0);
        assert_eq!(host["swap"]["unit"], "MiB");
        assert!(host["swap"]["active"].is_boolean());
    }
}
