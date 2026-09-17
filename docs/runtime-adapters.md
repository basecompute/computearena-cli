# Runtime adapters and report consistency

ComputeArena owns menus, authentication, signing, report storage, and submission.
RuntimeAdapter implementations own executable discovery, capabilities, model selection,
benchmark execution, and translation into the shared raw sample/metric structure.
BaseRT keeps its native harness evidence; llama.cpp uses computearena-measurements/1.
Adapters retain versioned timing/context semantics in their protocol metadata while
retaining their runtime-specific evidence. Both retain the computearena-benchmark/1
signed envelope, so old reports remain readable.

## Commands

- computearena: opens the full-screen interface (ratatui) on a terminal — pick BaseRT or
  llama.cpp, asked only when neither one runtime is installed nor a previous choice is
  remembered, see which executable will run (or how to obtain one), then the shared
  session. Piped or redirected input, and platforms other than unix, fall back to the
  printed session with numbered menus.
- Model selection for llama.cpp offers the Hugging Face Hub alongside recent files: search
  models filtered to GGUF, list a repository's .gguf files smallest first, download into
  the ComputeArena data directory, and go straight to that model's plan. `HF_TOKEN` is
  forwarded when set, for gated repositories. Downloads use immutable revisions and verify
  published LFS SHA-256 values when available.
- Model selection for BaseRT offers BaseRT's public catalogue alongside installed files.
  ComputeArena invokes the installed `basert list --remote` and `basert pull` commands so
  BaseRT owns backend selection, split downloads, conversion, and `hub.json` provenance.
- computearena basert [run|list|inspect|verify|identify|login|logout|submit|install]
- computearena llama-cpp [run|list|inspect|verify|identify|login|logout|submit|install]
- install downloads the runtime's prebuilt release after showing the plan; --archive
  unpacks a local bundle instead. Discovery prefers --runtime-path, then environment
  variables, then a ComputeArena-installed copy, then PATH, then the runtime's default
  install location.
- Global report/authentication commands continue to work without a runtime selector.
- Legacy computearena run remains a BaseRT action.
- Runtime selectors cannot nest: basert computearena llama-cpp is rejected.
- --runtime-path overrides executable discovery; --harness remains an alias.

## Measurement contract

All adapters store positive per-repetition token counts and elapsed nanoseconds.
The server recomputes arithmetic mean throughput from these samples.
llama.cpp must return every requested independent PP/TG workload exactly once,
with the expected repetition count. PP runs with `n_depth=0`; TG runs in a
separate process with `n_prompt=0` and `n_depth=1`, creating one untimed seed
token before timed generation. Mixed model, build, device, and runtime settings
in one report are rejected.
Build identity comes from llama-bench JSON's build_number and build_commit.
Capability probing uses --help because --version is not implemented consistently.

The initial llama.cpp adapter uses native warmup and records it as runtime_native.
--warmup 0 disables it; any positive value enables native warmup, not that number
of repetitions. The plan states this before execution. Standard mode uses a PP512
process, then a TG process, then a remaining-PP-sweep process; optional cooldown uses one fresh process per
workload (see benchmark-profiles.md). Automatic external telemetry is collected
for each process and native effective settings are preserved.
No equivalence between BaseRT and GGUF quantization names is assumed. Both adapters emit
the runtime-neutral `computearena-model/1` identity described in model-identity.md.

The PP/TG counts alone do not establish equivalent timing semantics. BaseRT's
new `features.headline_context_capacity` selects `--headline-first`: 4K reserved
capacity for PP512/TG128 first, then the remaining PP sweep. It produces
`computearena-throughput/3` with nested `basert-throughput-protocol/2` evidence.
Only capacity and order change, not the harness's warmup or timed operations.
Older isolated harnesses still use `/2`; legacy harnesses retain their old path.

llama.cpp remains `/2` with additional order and context-request evidence. Its
unmodified native capacity request is PP+TG+initial-depth, not forced to 4K.
Physical allocation padding is not inferred. No patched runtime, extra depth,
or unsupported flag is introduced. Both runtimes run the requested headline
first when supported, and use their existing native warmup/timing policies.
Consumers retain runtime, quantization, protocol and signed runtime evidence.
Existing measurements are not retroactively relabelled or excluded from rankings.

## ik_llama.cpp builds

The llama.cpp adapter also drives the llama-bench of ik_llama.cpp, recognised at the
--help probe by `-gp <pp,tg>` and `--warmup <0|1>`. There is no separate runtime
selector: both projects name their executable llama-bench, so the one that was found
is identified rather than guessed. The workloads, order, processes and samples_ns
timing are unchanged; only their spelling differs:

| | llama.cpp | ik_llama.cpp |
| --- | --- | --- |
| TG from one untimed seed token | `-p 0 -n 128 -d 1` | `-p 0 -n 0 -gp 1,128`; the row must be labelled `tg128@pp1` |
| PP from an empty context | `-d 0` | always (no depth option) |
| Disable warmup | `--no-warmup` | `-w 0` |
| Backend | `backends` | first of the `cuda`, `vulkan`, `metal`, `sycl` flags, else CPU |

ik rows are translated into llama.cpp's fields before the shared validation, and
ik's own settings (`mla_attn`, `fused_moe`, `ser`, ...) are kept in
runtime_configuration. ik names the GPU for CUDA and SYCL builds only, and the CPU on
Linux only. An unnamed device is reported as `unknown` and resolved by the shared host
lookup before signing (Apple silicon through sysctl, recorded in chip_detection); where
that lookup declines to guess, as for Vulkan, the chip stays unresolved. A ROCm build
reports the `cuda` flag and is recorded as CUDA.

The report's runtime name stays `llama-cpp`. The signed descriptor carries
`dialect: "ik_llama.cpp"` and runtime_version reads `ik_llama.cpp b<build> (<commit>)`,
so the server can tell the two apart.

## Binary provenance

Hash the selected executable before execution and check it again afterward.
Sign its SHA-256, platform, architecture, reported version and adapter descriptor
alongside the report. A changed executable prevents finalization.
No internet lookup occurs during the benchmark. Reports remain verifiable after
the local executable is upgraded or removed.

At submission the server compares the signed hash with its trusted catalogue.
Recognized, unrecognized/custom, mismatch and not-recorded statuses are separate
from signature validity. Mismatches do not reject otherwise valid submissions.
They display neutral guidance and the runtime's official download link.
Verification failures still show their actual reasons.

This identifies a claimed executable, not truthful execution. It does not attest
the process, shared libraries, driver or GPU kernels.

## Validation

cargo test
cargo clippy --all-targets -- -D warnings

The integration test runs a fake llama-bench process and signs a synthetic GGUF
benchmark. It checks that replacing the executable later does not invalidate the
saved report, while changing the signed hash does. No GPU is used.
