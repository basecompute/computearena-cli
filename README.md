# ComputeArena CLI

The workspace builds the standalone `computearena` client. It owns local
report creation, Ed25519 signing, inspection, verification, device login, and
submission. Runtime-specific measurements stay behind executable adapters, so
the CLI does not link the BaseRT engine.

BaseRT and llama.cpp are supported through a shared RuntimeAdapter interface.
MLX and vLLM can implement the same interface without duplicating report,
authentication, or submission code.

## Build

From the ComputeArena repository root:

```sh
cargo build --release
```

The binary is written to `target/release/computearena`. Copy it to a
directory on `PATH` if you want to invoke it globally.

## BaseRT adapter

ComputeArena looks for `basert-benchmark-harness` on `PATH`. The harness must
advertise the `basert-benchmark-harness/1` result protocol through
`describe --json`. If it is not on `PATH`, specify it explicitly:

```sh
computearena basert \
  --harness /absolute/path/to/basert-benchmark-harness
```

The runtime selector starts the interactive session:

```sh
computearena basert
```

Common actions can also be called directly:

```sh
computearena run /path/to/model.base
computearena list
computearena inspect <run-id-or-path>
computearena verify <run-id-or-path>
computearena login
computearena submit
```

`COMPUTEARENA_BASERT_HARNESS` is the environment equivalent of `--harness`.
The older `BASERT_COMPUTEARENA_HARNESS` name remains accepted during migration.

BaseRT releases containing the ComputeArena launcher shim can also delegate to
this binary:

```sh
basert computearena
```

For that command, both `basert` and `computearena` must be installed, and the
standalone `computearena` binary must be beside `basert` or on `PATH`.

## llama.cpp adapter

Install llama.cpp from https://github.com/ggml-org/llama.cpp/releases and make
`llama-bench` available on PATH, or provide its path:

```sh
computearena llama-cpp
computearena llama-cpp --runtime-path /path/to/llama-bench
computearena llama-cpp run /path/to/model.gguf
```

The same menus, report commands, login and submission flow work for both runtimes.
Starting `computearena` without arguments offers a runtime chooser.
The llama.cpp adapter asks for a GGUF file; it does not scan the disk.
It uses native warmup and automatically collects whole-run process memory,
available temperature sensors, and power/device snapshots. Opt into the thermally
controlled profile for per-workload cooldowns; see [profiles and UX parity](docs/benchmark-profiles.md).
Positive `--warmup` values enable native warmup; they do not set its repetition count.

Runtime binary checksums are recorded offline and checked by the server at
submission. An unrecognized/custom build receives informational download guidance,
not a report-integrity error. See [adapter details](docs/runtime-adapters.md).

### Recently used GGUF models

When choosing a llama.cpp model, ComputeArena lists the last 10 GGUF files from
successful benchmarks, most recent first. Enter its number, choose `p` to enter
another path (absolute, relative, or `~/`), or `0` to go back. Filenames and full
paths distinguish models with similar names; missing files are marked, and you
can enter their new location. Selecting an entry checks its GGUF header without
scanning model tensors.

History starts with benchmarks run by this version, including explicit
`llama-cpp run <path>` commands. It is stored only in `recent-gguf.json` under
your local data directory (`--data-dir` / `COMPUTEARENA_HOME`), never included in
reports or uploads. Delete that history file to clear the list. A corrupt or
unwritable history does not prevent benchmarking; corrupt files are left intact
and can be removed to reset history.

## API and local data

Production is the default. Override it for local or staging development:

```sh
computearena --api-url http://127.0.0.1:3000/api/v1 login
```

`COMPUTEARENA_API_URL` and `COMPUTEARENA_HOME` provide environment overrides.
The older `BASERT_COMPUTEARENA_API_URL` and `BASERT_COMPUTEARENA_HOME` names
remain accepted. The default data directory intentionally remains the existing
`basert/computearena` platform data directory so upgrading does not hide saved
reports, credentials, or the installation signing key.

Benchmarks can be generated and verified offline. Login is required for every
upload: run `computearena --api-url <server>/api/v1 login` before submitting.
A report signature detects modification
after the client finalized the file; it does not prove that a modified client,
harness, driver, or operating system reported truthful measurements.

All runtimes pass through shared chip-name normalization before signing.
Known aliases (such as M5Pro / Apple M5 Pro and GB10 / NVIDIA GB10) receive one
name; signed `chip_identity` metadata retains the runtime's original value and
the resolution source. Unknown hardware is not guessed or merged by family.
For CUDA harnesses reporting a missing or unknown chip, the CLI
uses a best-effort NVIDIA device-name fallback after measurement and before
signing. It only resolves a single physical GPU with unambiguous visibility;
multi-GPU systems and unsupported visibility masks remain unresolved. The
signed benchmark includes `chip_detection` with the original `reported_chip`,
the `resolved_chip`, and detection source. Valid harness names are preserved.
The probe times out after two seconds and failures do not prevent saving a
report. Existing signed reports are never rewritten.

## Hardware resolution (macOS and Linux)

- Runtime-reported names remain the primary evidence for every backend.
- Missing ROCm/HIP names use `rocminfo` only when exactly one GPU is present
  and no GPU visibility masks are set. Marketing names are used, not ISA IDs.
- Missing CPU-only names use Linux CPU model data or macOS `sysctl`.
- Missing Metal names use `sysctl` only on Apple Silicon, never Intel Macs.
- Radeon 8060S ROCm and known RADV spellings normalize to `AMD Radeon 8060S`.
- Vulkan/OpenCL device lists are not guessed from host inventories. Unknown
  vendors and multi-device strings remain intact; distinct SKUs stay distinct.

Detection failures remain unavailable rather than preventing report creation.
Windows hardware discovery and end-to-end support are planned for a future
release; this implementation does not claim Windows compatibility.

## Protocol compatibility

- Report envelope: `computearena-benchmark/1`
- New BaseRT harness output: `basert-benchmark-harness/1`
- Legacy BaseRT harness output remains accepted by the server:
  `basert-harness/1`
- Telemetry: `basert-telemetry/3`
- Signing: Ed25519 over `computearena-json-v1` canonical JSON

Community and support: [ComputeArena Discord](https://discord.gg/vENxergRG6).

## Upstream model identity

Reports retain the original model/package name and quantization. Upstream identity
is unresolved: no filename catalogue or `--model-id` override is used. Embedded
names remain unverified display metadata, not proof of origin. Instruct, MoE,
revisions and fine-tunes must not be inferred to be equivalent.

Reports mark identity verification as `unverified`. The CLI hashes the complete
model file before and after measurement and records `model.artifact_sha256`
inside the signed report. Hashing is outside benchmark timing (but adds disk I/O
and may warm the filesystem cache). A changed file aborts signing. This detects
persistent file changes, not malicious runtimes, forged metadata, or A/B/A swaps.
A hash identifies an artifact; it does not establish its upstream origin.
