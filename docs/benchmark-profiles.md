# Benchmark profiles and UX parity

Both adapters present the benchmark plan, resolved executable/model paths, workload
sizes, memory/heat warning, offline-save notice, and the same profile selector:

1. Standard (default): no cooldown waits.
2. Thermally controlled: opt-in waits at the runtime boundary described below.

Picking a profile is the start confirmation: the selector's entries read
`Start — <profile> · <estimate>`, alongside a details entry and `Cancel`, so one
answer both chooses the profile and starts the run. `--cooldown` places the cursor
on the thermally controlled entry.

`--yes` selects standard without prompts. `--yes --cooldown` selects thermally
controlled. Piped input must use `--yes`. Bad model selections or preparation errors return to
the menu; end-of-input exits instead of repeatedly prompting.

## BaseRT cooldown semantics

The currently released BaseRT harness has no external phase markers. ComputeArena
therefore waits once before launching the full benchmark suite, then observes the one
harness process. It does not pass the legacy `--cooldown` option, because that option
belongs to the harness's multi-pass telemetry protocol. The wait adds 10 seconds to
3 minutes with usable sensors, or about 30 seconds with the sensorless fallback.

A future harness can provide native per-workload conditioning together with native
same-run telemetry. ComputeArena uses that path only when the descriptor advertises
`features.same_run_telemetry: true` and `telemetry_schema: basert-telemetry/4`.

## llama.cpp cooldown semantics

llama-bench has no external pause hook between workload phases in a running sweep.
ComputeArena launches each requested PP size with TG0 and the requested TG workload
with PP0 in a **fresh process**. It waits before each launch, then loading and native
warmup occur. This is not equivalent to BaseRT's one pre-suite wait or a reset immediately
before a measured repetition. Standard mode still uses one process.

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
| Progress | BaseRT suite cooldown or llama.cpp per-workload cooldown; telemetry heartbeat while an externally observed process runs |
| Local reports, signature checks, login, preview, submission | Shared commands and handling |
| Model acquisition | BaseRT catalogue/pull hints; GGUF path plus Hugging Face/browser download hints |
| Warmup | BaseRT repetitions/minimum duration; llama.cpp native warmup (positive --warmup enables it) |
| Memory/temperature | Current BaseRT and llama.cpp: observed process windows including loading/warmup; future BaseRT may provide native same-run detail |
| Energy and runtime KV-cache allocation | Not exposed by external observation; available only when a future runtime protocol reports it from the measured run |
| Cooldown location | Current BaseRT: once before the suite; llama.cpp: before fresh workload-process launches; future BaseRT native path: harness-defined and signed |

Hardware cooldown effectiveness and observer throughput overhead still require Metal,
CUDA, and ROCm tests. Automated tests use fake clocks and runtime executables; they
do not run real model/GPU benchmarks or spend minutes waiting for hardware to cool.
