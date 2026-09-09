# Benchmark profiles and UX parity

Both adapters present the benchmark plan, resolved executable/model paths, workload
sizes, memory/heat warning, offline-save notice, and the same profile selector:

1. Standard (default): no cooldown waits.
2. Thermally controlled: opt-in waits before workloads.

`--yes` selects standard without prompts. `--yes --cooldown` selects thermally
controlled. Interactive selection is followed by a separate start confirmation.
Piped input must use `--yes`. Bad model selections or preparation errors return to
the menu; end-of-input exits instead of repeatedly prompting.

## llama.cpp cooldown semantics

llama-bench has no external pause hook between workload phases in a running sweep.
ComputeArena launches each requested PP size with TG0 and the requested TG workload
with PP0 in a **fresh process**. It waits before each launch, then loading and native
warmup occur. This is not equivalent to BaseRT's in-runtime conditioning or a reset
immediately before a measured repetition. Standard mode still uses one process.

The controller uses a fixed set of initially readable CPU/GPU/die-labelled temperature
sensors. It excludes battery/storage/ambient labels. The first stable window establishes
the reference; subsequent windows must stay within reference +3°C. Stability requires
at least 10 seconds and an absolute temperature slope no greater than 0.1°C/s. The
reference is observed, not a guaranteed cold or idle temperature. GPU utilization and
power are not polled by this controller, and exposed sensors may not cover the active GPU.

Each wait is capped at 180 seconds. Missing sensors trigger a timed 30-second rest
(bounded by the remaining maximum wait). Timeouts and sensorless fallback continue
with explicit status and `target_reached: false`; they never claim thermal recovery.
Wait duration, outcome, sensor labels, baseline, end temperature, and policy are signed.

The default eight PP sizes plus TG create nine wait points:

- Approximately 1m 30s–27m of added waits with sensors.
- Approximately 4m 30s of timed rests without sensors.
- Model reloading, native warmup, execution, and observer setup are additional.

The CLI calculates these wait estimates from the selected sweep. There is no calibrated
total runtime prediction yet. It says so instead of reusing BaseRT's replay-duration
estimate, which would be incorrect for llama.cpp.

Conditioned reports use `llama-bench-conditioned-pp-tg/1` with
`execution_layout: one_process_per_workload`. The private backend must include support
for this protocol before accepting those submissions. Existing standard reports still
use `llama-bench-independent-pp-tg/1`. No database migration is needed.

## Audit of remaining differences

| Interaction/capability | Status |
| --- | --- |
| Plan, profiles, confirmation, colors, full binary paths | Shared behavior, with accurate runtime-specific details |
| Progress | Cooldown status and per-workload stages in conditioned mode; telemetry heartbeat in standard mode |
| Local reports, signature checks, login, preview, submission | Shared commands and handling |
| Model acquisition | BaseRT catalogue/pull hints; GGUF path plus Hugging Face/browser download hints |
| Warmup | BaseRT repetitions/minimum duration; llama.cpp native warmup (positive --warmup enables it) |
| Memory/temperature | BaseRT diagnostic replays; llama.cpp observed process windows including loading/warmup |
| Energy and runtime KV-cache allocation | Not exposed by the current llama.cpp adapter; explicitly unavailable |
| Cooldown location | BaseRT inside the harness; llama.cpp before fresh workload-process launches |

Hardware cooldown effectiveness and observer throughput overhead still require Metal,
CUDA, and ROCm tests. Automated tests use fake clocks and runtime executables; they
do not run real model/GPU benchmarks or spend minutes waiting for hardware to cool.
