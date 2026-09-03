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
redirected or `NO_COLOR` is set.

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
Available start/end thermal readings and peak process RSS are captured beside
the throughput measurements.
