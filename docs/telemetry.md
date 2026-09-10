# Automatic telemetry

Telemetry is automatic for both runtime adapters; no extra flag, credentials, or
sudo is required. The benchmark plan states its coverage. Reports remain offline,
signed, and explicitly submitted using the same commands and confirmation flow.

Current BaseRT harnesses expose replay-based `basert-telemetry/3`. ComputeArena
deliberately does not request that mode: it runs the requested PP/TG suite once and
wraps that process with the same external observer used for llama.cpp. Both adapters
therefore record `computearena-telemetry/1` under `benchmark.telemetry`:

- Process resident memory, in MiB: first, last, mean, observed peak, sample count.
  Only the launched process is sampled, not unrelated processes or its children.
- Available OS temperature sensors: separate labelled sensor summaries in Celsius.
  Device sensors are not attributed to the benchmark process or assumed to be GPU sensors.
- Before/after power-state snapshots: macOS AC/battery source and low-power setting;
  Linux external supply state, platform profile, and CPU0 frequency governor when exposed.
- Before/after NVIDIA and ROCm snapshots when their vendor tools are installed:
  temperature, power draw, used/total GPU memory, utilization. These describe all
  visible devices, not necessarily the device used by the benchmark. Unsupported
  readings are absent/null with provider availability reasons, never fabricated zeroes.
- Observer timing: requested interval, attempts, missed deadlines, elapsed window,
  and time spent reading sensors. Aggregate storage is bounded (up to 128 temperature
  sensors), rather than accumulating an unlimited time series.

## Measurement boundaries

The worker is initialized before launching the runtime and samples every second until
it exits. For a standard BaseRT or llama.cpp run it covers model loading, runtime
warmup, the entire PP/TG sweep, and teardown. These are **concurrently observed
whole-run measurements**, not per-PP diagnostic results. One-second sampling can miss
short peaks or a very short-lived process.

The existing `benchmark.memory.process_peak_rss_mb` compatibility field carries the
observed peak alongside its scope, units, and caveat, so existing CLI/backend memory
summaries can consume it. It is not a process-lifetime kernel high-water mark.
Detailed temperature and power data remain in the signed telemetry object.

Vendor tools run only outside the runtime process window. Each invocation has a
two-second timeout, a 64-KiB output cap checked while running, and is killed/reaped
on timeout. Temporary output files are removed automatically. The worker owns its
OS sensor handles and is stopped/joined on completion or failure.

Energy, runtime allocator/KV-cache queries, and per-workload attribution are explicitly
unavailable in the external-observer path. Power snapshots are not energy measurements,
and a whole-run sensor reading is not a per-token metric. Optional llama.cpp cooldown
groups telemetry by isolated workload process, still including loading/warmup. Current
BaseRT cooldown performs one external wait before the suite. See benchmark-profiles.md.

## BaseRT capability transition

Runtime selection is capability-based, not tied to a BaseRT version string:

- Missing or false `features.same_run_telemetry` selects one externally observed
  harness run and omits both `--telemetry` and the legacy multi-pass `--cooldown`.
- `features.same_run_telemetry: true` together with
  `telemetry_schema: basert-telemetry/4` selects the native `--telemetry` path.
- Advertising native same-run telemetry with an unknown or missing schema fails
  closed instead of silently changing measurement semantics.

The future native schema may contain energy, KV-cache, allocator, and per-workload
data, but only if those values are collected during the same runs that produce the
signed throughput samples. This descriptor contract lets a new BaseRT release plug
in without requiring a corresponding ComputeArena release.

## Overhead and validation

Process refresh requests only the target PID's memory, disables task enumeration,
and avoids CPU, disk, command-line, environment, or unrelated process scans. Temperature
refresh reuses discovered sensors. No polling subprocesses run inside timed execution.
This reduces work but does **not** establish zero overhead or validated throughput
equivalence. Observer read time is diagnostic, not a measured slowdown percentage.
Actual GPU overhead and vendor behavior still need tests on Metal/CUDA/ROCm hardware.

Run `cargo test --workspace --all-targets`. Tests include current-process memory,
thread shutdown, vendor parsing, command timeouts, observed fake-child execution,
telemetry signature tampering, JSON round trips, current external BaseRT invocation,
and future native capability selection.
These tests may read local sensors/power settings but do not run GPU benchmarks.
The CLI enables serde_json's `float_roundtrip` feature so telemetry decimals retain
their signed representation after being saved and parsed again.
