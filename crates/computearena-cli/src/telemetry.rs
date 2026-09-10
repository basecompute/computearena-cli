//! External, runtime-neutral telemetry. Never claim replay/per-phase attribution.
pub(crate) mod snapshots;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Components, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);
const MAX_SENSORS: usize = 128;

#[derive(Default)]
struct Summary {
    count: u64,
    sum: f64,
    first: f64,
    last: f64,
    peak: f64,
}

impl Summary {
    fn add(&mut self, value: f64) {
        if !value.is_finite() || value < 0.0 {
            return;
        }
        if self.count == 0 {
            self.first = value;
        }
        self.count += 1;
        self.sum += value;
        self.last = value;
        self.peak = self.peak.max(value);
    }

    fn json(&self) -> Value {
        if self.count == 0 {
            return json!({"available":false,"sample_count":0,"reason":"No readable samples during the observation window"});
        }
        json!({"available":true,"sample_count":self.count,"first":self.first,
            "last":self.last,"peak":self.peak,"mean":self.sum/self.count as f64})
    }
}

struct Collector {
    system: System,
    components: Components,
    memory: Summary,
    temperatures: BTreeMap<String, Summary>,
    attempts: u64,
    read_seconds: f64,
    missed_deadlines: u64,
    process_start: Option<u64>,
}

impl Collector {
    fn new() -> Self {
        Self {
            system: System::new(),
            components: Components::new_with_refreshed_list(),
            memory: Summary::default(),
            temperatures: BTreeMap::new(),
            attempts: 0,
            read_seconds: 0.0,
            missed_deadlines: 0,
            process_start: None,
        }
    }

    fn sample(&mut self, pid: Pid) {
        let started = Instant::now();
        self.attempts += 1;
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory().without_tasks(),
        );
        if let Some(process) = self.system.process(pid) {
            let identity = self.process_start.get_or_insert(process.start_time());
            if *identity == process.start_time() {
                // sysinfo exposes bytes on every supported platform. Zero may be
                // a departed/zombie process or an unavailable kernel reading.
                if process.memory() > 0 {
                    self.memory.add(process.memory() as f64 / 1_048_576.0);
                }
            }
        }
        // Refresh already-discovered sensors only: no repeated discovery scans.
        for component in self.components.iter_mut().take(MAX_SENSORS) {
            component.refresh();
            if let Some(value) = component
                .temperature()
                .filter(|v| v.is_finite() && *v >= 0.0 && *v < 150.0)
            {
                let label = component.label().chars().take(160).collect::<String>();
                self.temperatures
                    .entry(label)
                    .or_default()
                    .add(f64::from(value));
            }
        }
        self.read_seconds += started.elapsed().as_secs_f64();
    }

    fn finish(self, elapsed: Duration) -> Value {
        let temperatures: Vec<Value> = self.temperatures.into_iter().map(|(label, stats)|
            json!({"sensor":label,"unit":"celsius","scope":"device_sensor","statistics":stats.json()})).collect();
        json!({"schema":"computearena-telemetry/1","coverage":"basic",
            "measurement_relation":"concurrent_observer","scope":"whole_runtime_process",
            "includes":["model_loading","warmup","all_prefill_and_decode_workloads","runtime_teardown"],
            "excludes":["runtime_child_processes"],
            "observer":{"provider":"sysinfo","requested_interval_ms":SAMPLE_INTERVAL.as_millis(),
                "attempt_count":self.attempts,"read_time_ms":self.read_seconds*1000.0,
                "missed_deadlines":self.missed_deadlines,"elapsed_s":elapsed.as_secs_f64(),
                "overhead_note":"Read time is not measured throughput slowdown; brief peaks may be missed"},
            "process_memory":{"provider":"sysinfo_process_memory","metric":"resident_set_size",
                "unit":"MiB","measurement_relation":"concurrent_observer","statistics":self.memory.json()},
            "temperature":{"available":!temperatures.is_empty(),"provider":"sysinfo_components",
                "scope":"device_sensors_not_process_attributed","sensors":temperatures,
                "reason_if_unavailable":"No readable temperature sensors exposed by the OS"},
            "energy":{"available":false,"reason":"Boundary power snapshots do not measure energy; no supported energy counter collector yet"},
            "runtime_memory":{"available":false,"reason":"The external executable does not expose KV-cache/allocation queries"},
            "per_workload":{"available":false,"reason":"No phase markers; whole-run samples cannot be assigned to individual PP/TG workloads"}})
    }
}

struct Sampler {
    stop: mpsc::Sender<()>,
    target: Option<mpsc::Sender<u32>>,
    thread: Option<thread::JoinHandle<Value>>,
}

impl Sampler {
    fn start() -> std::io::Result<Self> {
        let (stop, receiver) = mpsc::channel();
        let (target, pid_receiver) = mpsc::channel();
        let (ready, initialized) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("computearena-telemetry".into())
            .spawn(move || {
                // Apple HID handles are thread-affine: create, use, and destroy
                // sensor resources on this worker. Initialize before model loading.
                let mut collector = Collector::new();
                let _ = ready.send(());
                let Ok(pid) = pid_receiver.recv() else {
                    return unavailable("Runtime did not start");
                };
                let started = Instant::now();
                let mut next_progress = PROGRESS_INTERVAL;
                loop {
                    let sample_start = Instant::now();
                    collector.sample(Pid::from_u32(pid));
                    if started.elapsed() >= next_progress {
                        let memory = if collector.memory.count > 0 {
                            format!(
                                "observed peak process memory {:.0} MiB",
                                collector.memory.peak
                            )
                        } else {
                            "process memory unavailable".to_owned()
                        };
                        eprintln!(
                            "{}",
                            crate::ui::TerminalUi::detect().neutral(format!(
                                "Benchmark running — {}s elapsed; {memory} (whole run)",
                                started.elapsed().as_secs()
                            ))
                        );
                        next_progress = started.elapsed() + PROGRESS_INTERVAL;
                    }
                    let duration = sample_start.elapsed();
                    if duration >= SAMPLE_INTERVAL {
                        collector.missed_deadlines += 1;
                    }
                    let remaining = SAMPLE_INTERVAL
                        .saturating_sub(duration)
                        .max(Duration::from_millis(1));
                    match receiver.recv_timeout(remaining) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
                collector.finish(started.elapsed())
            })?;
        if initialized.recv().is_err() {
            let _ = thread.join();
            return Err(std::io::Error::other("Telemetry initialization failed"));
        }
        Ok(Self {
            stop,
            target: Some(target),
            thread: Some(thread),
        })
    }

    fn attach(&mut self, pid: u32) {
        if let Some(target) = self.target.take() {
            let _ = target.send(pid);
        }
    }

    fn finish(mut self) -> Value {
        self.target.take();
        let _ = self.stop.send(());
        self.thread
            .take()
            .unwrap()
            .join()
            .unwrap_or_else(|_| unavailable("Telemetry worker failed"))
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        self.target.take();
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn unavailable(reason: &str) -> Value {
    json!({"schema":"computearena-telemetry/1","available":false,"reason":reason})
}

/// Start/end command probes are outside the runtime's measured execution.
/// stdout is drained by wait_with_output while the independent observer runs.
pub(crate) fn run_observed(command: &mut Command) -> Result<(Output, Value)> {
    let before = snapshots::capture();
    let mut sampler = Sampler::start();
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting benchmark runtime")?;
    if let Ok(sampler) = &mut sampler {
        sampler.attach(child.id());
    }
    let output = child
        .wait_with_output()
        .context("waiting for benchmark runtime");
    let mut telemetry = match sampler {
        Ok(sampler) => sampler.finish(),
        Err(error) => unavailable(&format!("Could not start telemetry worker: {error}")),
    };
    telemetry["boundaries"] = json!({"measurement_relation":"outside_runtime_execution",
        "before":before,"after":snapshots::capture()});
    Ok((output?, telemetry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_decimals_round_trip_without_changing_canonical_signature_input() {
        for i in 1..1024 {
            let original = json!({"read_time_ms":(f64::from(i) / 7.0) * 1.23456789,
                "mean":58.921749999999996});
            let reloaded: Value =
                serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
            let mut before = Vec::new();
            let mut after = Vec::new();
            crate::reports::write_canonical_json(&original, &mut before).unwrap();
            crate::reports::write_canonical_json(&reloaded, &mut after).unwrap();
            assert_eq!(before, after);
        }
    }

    #[test]
    fn aggregation_preserves_units_and_missing_is_not_zero() {
        let mut summary = Summary::default();
        assert_eq!(summary.json()["available"], false);
        for value in [12.0, 32.0, 16.0, f64::NAN, -1.0] {
            summary.add(value);
        }
        assert_eq!(summary.json()["peak"], 32.0);
        assert_eq!(summary.json()["mean"], 20.0);
        assert_eq!(summary.json()["sample_count"], 3);
    }

    #[test]
    fn current_process_memory_is_observed_and_stop_does_not_wait_for_interval() {
        let mut collector = Collector::new();
        collector.sample(Pid::from_u32(std::process::id()));
        assert!(collector.memory.count > 0);
        let mut sampler = Sampler::start().unwrap();
        sampler.attach(std::process::id());
        let result = sampler.finish();
        assert_eq!(result["scope"], "whole_runtime_process");
        assert!(result["observer"]["elapsed_s"].as_f64().unwrap() < 1.0);
        assert!(
            result["process_memory"]["statistics"]["peak"]
                .as_f64()
                .unwrap()
                > 0.0
        );
    }
}
