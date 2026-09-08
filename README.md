# BaseRT ComputeArena

`computearena/` is an independently buildable Cargo workspace included in the
BaseRT repository. Its `computearena-cli` package builds the
`basert-computearena` executable, which the main launcher exposes as:

```console
basert computearena
```

The controller owns the user workflow, local report store, installation
identity, report finalization, and signature verification. Benchmark execution
stays behind the separate `basert-harness` process boundary so this module can
be extracted into an independent binary without importing BaseRT engine
internals.

Build and test this workspace independently:

```console
cargo build --manifest-path computearena/Cargo.toml
cargo test --manifest-path computearena/Cargo.toml
```

The workspace owns its version, dependency set, lockfile, and CI job. BaseRT
integration is deliberately limited to the `basert computearena` launcher
dispatch, the separately built `basert-harness` executable, release packaging,
and two temporary Rust path dependencies used to read `.base` metadata and
reuse signing helpers. Moving model inspection behind the harness adapter and
owning the small report-signing implementation are the remaining steps before
the directory can move to another repository unchanged.

## Current commands

```console
basert computearena run /path/to/model.base
basert computearena list
basert computearena inspect <run-id-or-path>
basert computearena verify <run-id-or-path>
```

The default text workload measures prefill at powers of two from PP128 through
PP16384 (`128,256,512,1024,2048,4096,8192,16384`) and decode at TG128. Use
`run --pp <comma-separated-values> --tg <tokens>` to override the sweep. The
public headline remains PP512/TG128 so new reports stay comparable with
previous submissions while retaining the additional long-context measurements
in their signed JSON.

Before execution, the CLI shows the selected model, exact PP/TG workloads,
warmup and recorded repetitions, platform telemetry behavior, device-load
warning, and local report policy. It asks for confirmation before starting.
Use `run --yes` for non-interactive automation; the plan is still printed, but
the prompt is skipped.

Running without a subcommand opens the interactive menu. Login and submission
are menu/CLI placeholders until the `computearena.ai` server API is available;
benchmark creation and verification do not require a network connection or a
user account.

The interactive UI embeds its BaseRT wordmark and gradient directly from the
shared `tools/basert_banner.h` source at compile time, avoiding a second copy of
the ASCII art and any runtime asset dependency. It uses BaseCompute terminal
colors and reports potentially slow work—model discovery, benchmark execution,
report loading, verification, and signing—before it begins. Selecting
verification presents available reports as a numbered list, so users do not
need to know report paths or IDs. ANSI color is disabled when output is
redirected or `NO_COLOR` is set. Benchmark-harness progress uses the same
BaseCompute palette: blue marks active work, lime marks completed measurements,
neutral gray carries secondary detail, and red is reserved for timeouts or
errors. Interactive cooldown updates replace one line in place; redirected logs
retain plain periodic progress lines.

For a source-tree build, build the companion harness once before running a real
benchmark:

```console
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build --target baseRT_bench_multidevice
```

This produces both the internal `build/baseRT_bench_multidevice` name and the
stable `build/basert-harness` name. Published BaseRT archives place the harness
beside `basert-computearena`, so no separate build step is required there.

## Local data and signing

The default data root is the platform's local application-data directory under
`basert/computearena`. `BASERT_COMPUTEARENA_HOME` or `--data-dir` can override
it. Reports are immutable JSON files under `reports/`.

The first benchmark creates a random Ed25519 installation key using the OS
CSPRNG. On Unix, the private-key file is mode `0600`. A report is signed only
after the harness exits and every report field has been assembled. The account
ID is deliberately absent: a report may be generated offline and associated
with an authenticated account during a later upload.

The signature proves that the JSON has not changed since this installation
finalized it. It does not prove that user-controlled hardware or software ran
honestly; server-side validation and trust classification remain necessary.

Each new report stores its human-readable model `name` inside the signed JSON.
Models from the BaseRT cache also include their canonical `id` and `variant`,
along with architecture and quantization metadata. The full local model path is
not stored. This lets `computearena list` display the model without rescanning
or parsing installed model files; model discovery is performed only when the
user chooses to run a benchmark.

## Process contracts

- Harness output: `basert-harness/1`
- Saved report: `computearena-benchmark/1`
- Signature canonicalization: `computearena-json-v1`

The harness includes raw `{tokens, elapsed_ns}` samples for prefill and
`{generated_tokens, elapsed_ns}` samples for decode. Aggregate throughput is
retained for local display, but the server can recompute it from raw samples.
Available start/end thermal readings and current process-residency snapshots
are captured beside the throughput measurements. A separately labelled
process-lifetime RSS high-water mark remains as a coarse fit indicator.

ComputeArena invokes text benchmarks with `--telemetry`, which adds an optional
`benchmark.telemetry` object using the nested `basert-telemetry/3` schema.
The `basert-telemetry/3` protocol conditions every independent PP/TG phase
before measurement. It establishes a stable idle baseline, waits for a
ten-second stable window within the configured temperature, power, and
utilization limits, then performs the requested warmups for at least three
seconds. The adaptive wait is capped at three minutes; unavailable sensors use
a recorded 30-second fallback. Recorded repetitions remain contiguous, with no
cooldown or observer work inside their timing window.

The runtime-neutral `computearena-conditioning/1` object records the policy,
baseline, actual wait, timeout/fallback result, temperature slope, and warmup
work for headline performance and every diagnostic replay. Older signed
`basert-telemetry/2` reports remain readable and submittable.

Headline throughput samples remain uninstrumented: power-state, temperature,
vendor power, cumulative energy, and peak-RSS snapshots are read only before
or after those timers. The snapshots also record BaseRT's model-memory
accounting and, where available, accelerator memory use. Every diagnostic
replay also carries a `computearena-runtime-memory/1` boundary observation:
runtime-allocated memory plus KV-cache capacity and logical usage (and block
occupancy for paged caches). The provider-labelled shape is deliberately
runtime-neutral so future llama.cpp, vLLM, and MLX adapters can populate the
same fields without treating process RSS and accelerator allocations as the
same measurement. The harness then runs five-second diagnostic replays that
sample current process memory every 25 ms
for every prefill length and decode. Each workload reports its baseline,
observed peak, peak increase, ending footprint, sample count, missed sampling
deadlines, and observer read time. On Apple Silicon, these replays also sample
temperature, while energy uses separate replays:

- energy uses start/end IOReport counters, a 1.5-second idle baseline, and
  reports gross and idle-adjusted joules for every prefill length and decode;
- temperature is sampled every 500 ms in the same diagnostic replay and reports
  the maximum and mean die temperature for every workload; and
- each replay records its elapsed time, iterations, and processed tokens so its
  telemetry is auditable and is never confused with the headline performance
  run.

IOReport counters are system-wide estimates, so Apple energy values are
advisory and are most useful on an otherwise-idle machine. NVIDIA and ROCm
currently use deliberately basic accelerator telemetry: start/end GPU
temperature, board power, memory use and cumulative energy when exposed by
`nvidia-smi` or `rocm-smi`, plus Linux power-source and CPU-governor data. They
use the same per-workload process-memory replay through `/proc/self/statm`;
per-workload accelerator energy and continuous temperature collection remain
explicitly unavailable until those paths have been measured on supported
hardware.
