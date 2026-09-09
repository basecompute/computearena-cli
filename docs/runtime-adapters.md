# Runtime adapters and report consistency

ComputeArena owns menus, authentication, signing, report storage, and submission.
RuntimeAdapter implementations own executable discovery, capabilities, model selection,
benchmark execution, and translation into the shared raw sample/metric structure.
BaseRT keeps its native harness schema; llama.cpp uses computearena-measurements/1.
Both retain the computearena-benchmark/1 signed envelope, so old reports remain readable.

## Commands

- computearena: choose BaseRT or llama.cpp, then open the shared interactive session.
- computearena basert [run|list|inspect|verify|login|logout|submit]
- computearena llama-cpp [run|list|inspect|verify|login|logout|submit]
- Global report/authentication commands continue to work without a runtime selector.
- Legacy computearena run remains a BaseRT action.
- Runtime selectors cannot nest: basert computearena llama-cpp is rejected.
- --runtime-path overrides executable discovery; --harness remains an alias.

## Measurement contract

All adapters store positive per-repetition token counts and elapsed nanoseconds.
The server recomputes arithmetic mean throughput from these samples.
llama.cpp must return every requested independent PP/TG workload exactly once,
with the expected repetition count and zero initial context depth. Mixed model,
build, device, and runtime settings in one run are rejected.
Build identity comes from llama-bench JSON's build_number and build_commit.
Capability probing uses --help because --version is not implemented consistently.

The initial llama.cpp adapter uses native warmup and records it as runtime_native.
--warmup 0 disables it; any positive value enables native warmup, not that number
of repetitions. The plan states this before execution. Automatic external telemetry
is collected over the whole process (see telemetry.md). Optional cooldown runs each
workload in a fresh process using llama-bench-conditioned-pp-tg/1 (see benchmark-profiles.md).
Native effective settings are preserved.
No equivalence between BaseRT and GGUF quantization names is assumed.

The PP/TG counts alone do not establish equivalent timing semantics. llama.cpp
records a distinct protocol ID and its exclusion of sampling/tokenization.
Consumers must retain runtime, quantization, context depth, and protocol when
comparing results. Existing BaseRT measurements are not retroactively relabelled.

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
