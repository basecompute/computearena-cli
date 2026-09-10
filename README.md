# ComputeArena CLI

`computearena` is the command-line client for [ComputeArena](https://computearena.ai),
a community leaderboard of benchmarks for AI models running on edge devices. It
runs prefill and decode benchmarks through a local inference runtime, saves
each result on your machine as a signed report, lets you inspect and verify
those reports, and submits the ones you choose to the leaderboard.

Benchmarks run and verify offline. Nothing is uploaded until you log in and
submit, and the client shows exactly what will be published before it sends it.

Two runtimes are supported:

| Runtime | Benchmark executable | Models |
| --- | --- | --- |
| [BaseRT](https://github.com/basecompute/baseRT) | `basert-benchmark-harness` | `.base` bundles |
| [llama.cpp](https://github.com/ggml-org/llama.cpp) | `llama-bench` | `.gguf` files |

Both go through one adapter interface that owns executable discovery,
capabilities, model selection, and execution, while the client owns the menus,
signing, report storage, login, and submission. MLX and vLLM adapters could be
added behind the same interface without touching that shared code, but they do
not exist yet. The client never links a runtime: every measurement comes from a
separate executable it launches and hashes.

Prebuilt binaries cover macOS on Apple Silicon and Linux on x86_64 and arm64.
Windows is not supported yet.

## Install

Each [release](https://github.com/basecompute/computearena-cli/releases) ships
one archive per platform, `computearena-<platform>-<version>.tar.gz`, where
`<platform>` is `macos-arm64`, `linux-x86_64`, or `linux-arm64`. The archive
holds the `computearena` binary, `LICENSE`, and this file. Download it, check
it against its `.sha256`, and put the binary on your `PATH`:

```sh
VERSION=0.1.0
PLATFORM=macos-arm64   # or linux-x86_64, linux-arm64
ASSET="computearena-${PLATFORM}-${VERSION}.tar.gz"
curl -fsSLO "https://github.com/basecompute/computearena-cli/releases/download/v${VERSION}/${ASSET}"
curl -fsSLO "https://github.com/basecompute/computearena-cli/releases/download/v${VERSION}/${ASSET}.sha256"
shasum -a 256 -c "${ASSET}.sha256"      # sha256sum -c on Linux
mkdir -p ~/.local/bin
tar -xzf "$ASSET" -C ~/.local/bin computearena
computearena --version
```

The Linux binaries run on glibc 2.31 or newer. The macOS binary is signed with
Base Compute's Developer ID and notarized by Apple, so a copy downloaded in a
browser runs without Gatekeeper objections. Every archive also carries a
Sigstore signature (`.sig` and `.pem`); [docs/releasing.md](docs/releasing.md)
has the `cosign` command that proves an archive was built by this repository's
release workflow.

### Build from source

With Rust 1.85 or newer:

```sh
cargo install --locked --git https://github.com/basecompute/computearena-cli computearena-cli
```

or, from a checkout, `cargo build --release`, which writes
`target/release/computearena`.

## Quick start

1. Install a runtime. BaseRT's installer is
   `curl -LsSf https://basecompute.co/install.sh | sh`; llama.cpp comes from
   `brew install llama.cpp` or its
   [releases](https://github.com/ggml-org/llama.cpp/releases). The client can
   also install either one for you (see [Runtimes](#runtimes)).
2. Have a model on disk: a `.base` bundle for BaseRT, for example from
   `basert pull Qwen/Qwen3-0.6B`, or a `.gguf` file for llama.cpp. For
   llama.cpp the model picker can also search the Hugging Face Hub and
   download a GGUF for you, smallest quantization first; set `HF_TOKEN` in
   your environment for gated repositories.
3. Run `computearena`. In a terminal this opens the full-screen interface:
   arrow keys move, Enter selects, Esc goes back, Ctrl+C leaves, and the
   wheel or PgUp/PgDn scrolls long output. It asks which
   runtime to use only when the answer is not obvious — one installed, or the
   one you used last — shows the executable it found (or offers to install
   one), and opens a menu: run benchmarks, submit previous benchmarks, list
   local benchmarks, verify a local benchmark, account, switch runtime. Piped
   or redirected input gets the printed session instead, so scripts and CI are
   unaffected.
4. A benchmark starts with its plan: the resolved executable and model, the
   workload sizes, a memory and heat warning, and the choice of profile.
   Choosing a profile starts the run; its progress and results stream into the
   interface, and the result is saved locally as a signed report.
5. Log in and submit when you are ready. Submission previews the JSON that
   will become public and asks for confirmation.

Everything in the interface is also a command:

```sh
computearena basert                      # BaseRT session
computearena llama-cpp                   # llama.cpp session
computearena llama-cpp run model.gguf    # benchmark and save a signed report
computearena list                        # saved reports (--json for machines)
computearena inspect <run-id-or-path>    # print one report
computearena verify <run-id-or-path>     # check its signature
computearena login                       # connect this installation to your account
computearena submit                      # upload chosen reports
```

`run`, `list`, `inspect`, `verify`, `login`, `logout`, `install`, and `submit`
work under either runtime selector. Without a selector they act as BaseRT
commands, so scripts written for earlier versions keep working. `--data-dir`,
`--runtime-path` (alias `--harness`), and `--api-url` are accepted anywhere.

## Running benchmarks

`run` measures prefill throughput at each `--pp` size and decode throughput
over `--tg` tokens, `--reps` times, after `--warmup` warmup runs. The defaults
are a prefill sweep from 128 to 16384 tokens, 128 decode tokens, three recorded
repetitions, and three warmup runs. For llama.cpp, any positive `--warmup`
turns on llama-bench's native warmup rather than setting a count.

```sh
computearena basert run model.base --pp 512,2048 --tg 128 --reps 5
computearena llama-cpp run model.gguf --yes --output ./report.json
```

Two profiles are offered before a run starts. Standard runs the workloads
back to back. Thermally controlled (`--cooldown`) waits for the device to cool
before each measured workload, which can add many minutes; the plan shows the
estimate. `--yes` skips the prompts and picks standard unless `--cooldown` is
also given, and piped input must use `--yes`. Details, including how the
llama.cpp cooldown differs from BaseRT's in-runtime conditioning, are in
[docs/benchmark-profiles.md](docs/benchmark-profiles.md).

Telemetry is automatic for both runtimes and needs no flag, credential, or
sudo. BaseRT's harness records its own diagnostics; for llama.cpp the client
observes the process it launched: resident memory, the temperature sensors the
operating system exposes, power state, and NVIDIA or ROCm device snapshots
where those vendor tools exist. What each runtime can and cannot observe is in
[docs/telemetry.md](docs/telemetry.md).

## Runtimes

Discovery looks, in order, at `--runtime-path`, the runtime's environment
variables, a copy installed by ComputeArena, `PATH`, and the runtime's own
default location (`~/.basert` for BaseRT). Before a model is chosen, the
session prints which executable will run and where it came from, for example
`Found llama.cpp on PATH`. An executable that is present but does not speak
the adapter's protocol is reported as unusable rather than used.

If the runtime is missing, the session explains how to install it and offers
to do so:

```sh
computearena basert install
computearena llama-cpp install [--yes] [--archive bundle.tar.gz]
```

The plan is shown before anything is downloaded: release, asset and size, URL,
destination, the backend of that build, and how the download is checked.
BaseRT installs where its official installer does (`~/.basert`, or
`BASERT_INSTALL_DIR`) and is verified against the SHA-256 published with the
release. llama.cpp installs under `runtimes/llama-cpp/<build>` in the
ComputeArena data directory, is not added to `PATH`, and replaces an earlier
copy installed the same way. Neither touches shell profiles. `--archive`
unpacks a bundle you already have instead of contacting GitHub. Prebuilt
runtime bundles exist for macOS arm64 and Linux arm64 with CUDA (BaseRT) and
for macOS and Linux CPU or Metal builds (llama.cpp); other platforms and GPU
builds of llama.cpp are installed by hand.

### BaseRT

The harness must advertise the `basert-benchmark-harness/1` protocol through
`describe --json`. `COMPUTEARENA_BASERT_HARNESS` is the environment equivalent
of `--runtime-path`. BaseRT 0.2.4 and newer can also start this client with
`basert computearena`, provided `computearena` is beside `basert` or on
`PATH`.

### llama.cpp

The adapter asks for a GGUF file rather than scanning the disk, and lists the
last ten files from successful benchmarks, most recent first: pick a number,
`p` to enter another path (absolute, relative, or `~/`), or `0` to go back.
Missing files are marked and can be given a new location. Selecting a file
reads its GGUF header only. The history lives in `recent-gguf.json` in the
data directory, is never part of a report, and can be deleted to reset the
list; a corrupt or unwritable history never blocks a benchmark.

llama.cpp measurements carry their own protocol identifiers and record native
warmup, zero context depth, and the exclusion of sampling and tokenization, so
they are never presented as BaseRT numbers. The measurement contract is in
[docs/runtime-adapters.md](docs/runtime-adapters.md).

## Reports and signatures

Reports live in the data directory: `~/Library/Application Support/basert/computearena`
on macOS, `~/.local/share/basert/computearena` on Linux, or wherever
`--data-dir` or `COMPUTEARENA_HOME` points. Inside it, `reports/` holds one
JSON file per run, `keys/installation.ed25519` is the private signing key
created on first use, `auth.json` holds login sessions, and
`runtimes/` holds llama.cpp builds the client installed. `--output` writes a
report elsewhere instead.

Each report is a `computearena-benchmark/1` envelope: a run ID, a timestamp,
the client version, the runtime (name, version, adapter descriptor, and the
SHA-256 of the executable that ran, hashed before and after the benchmark),
the model (name, quantization, and the SHA-256 of the model file, also hashed
before and after; a change to either file aborts signing), the installation's
public key, and the benchmark itself: raw per-repetition token counts and
timings, the resolved chip identity with its detection source, telemetry, and
any conditioning data. The report is signed with Ed25519 over
`computearena-json-v1` canonical JSON. `verify` checks the signature,
`inspect` prints the report, and `list --json` enumerates them.

What the signature means: the report has not changed since this installation
signed it. What it does not mean: that the runtime, driver, operating system,
or client reported the truth. Whoever controls the machine and its key can
sign invented numbers. The executable and model hashes identify what was
claimed to run; they do not attest the process, its libraries, or GPU kernels,
and they cannot see a file swapped and restored between the two hashes.

Model identity is deliberately left unresolved. The name and quantization
embedded in the file are kept as display metadata and marked `unverified`. No
filename catalogue or override decides what a file really is, and instruct,
MoE, revision, and fine-tune variants are never assumed equivalent.

Chip names pass through one normalization before signing, so known aliases
(M5Pro and Apple M5 Pro, or GB10 and NVIDIA GB10) receive one name while the
runtime's original value and the resolution source stay in the signed
`chip_identity`. Unknown hardware is kept as reported, never guessed or merged
by family. When a runtime reports no chip, a conservative fallback asks the
host: Linux CPU model data or macOS `sysctl` for CPU-only runs, `sysctl` for
Metal on Apple Silicon only, `rocminfo` for ROCm when exactly one GPU is
visible, and a single unambiguously visible NVIDIA device for CUDA harnesses.
Multi-GPU systems and visibility masks stay unresolved, a failed probe leaves
the field unavailable rather than blocking the report, and existing signed
reports are never rewritten.

## Submitting

`login` runs a device flow: it prints a confirmation code, opens the
verification page in your browser when it can, and stores the session for
that API URL. `submit` uploads the reports you pick after showing the JSON that
will become public; local file paths are not sent, and everything you submit
is publicly visible on ComputeArena. `--yes` skips the preview.
`--yes --skip-invalid` uploads the valid reports of a batch that also contains
invalid ones; without it, a non-interactive batch with an invalid report is
refused.

The server compares the signed runtime checksum with its catalogue of official
builds. An unrecognized or custom build is still accepted and shown with
download guidance; only a report whose signature does not verify is rejected.
Submitting the same report again succeeds rather than failing.

The client talks to `https://computearena.ai/api/v1`. `--api-url` or
`COMPUTEARENA_API_URL` point it at another deployment, such as a local
development server; sessions are kept per URL.

## Configuration

| Variable | Effect |
| --- | --- |
| `COMPUTEARENA_HOME` | Data directory, same as `--data-dir` |
| `COMPUTEARENA_API_URL` | API base URL, same as `--api-url` |
| `COMPUTEARENA_BASERT_HARNESS` | Path to `basert-benchmark-harness`, same as `--runtime-path` for BaseRT |
| `BASERT_INSTALL_DIR` | Where BaseRT is looked for and installed; `~/.basert` by default |
| `BASERT_MODELS_DIR` | Where installed BaseRT models are listed from; BaseRT's own model cache by default |
| `CUDA_VISIBLE_DEVICES` | Respected by the CUDA chip fallback; a mask leaves the chip unresolved |
| `NO_COLOR` | Plain output |

The older `BASERT_COMPUTEARENA_HOME`, `BASERT_COMPUTEARENA_API_URL`, and
`BASERT_COMPUTEARENA_HARNESS` names are still accepted. The data directory is
the same as in earlier versions, so upgrading keeps saved reports, sessions,
and the signing key.

## Protocols

- Report envelope: `computearena-benchmark/1`
- BaseRT harness output: `basert-benchmark-harness/1`; the older
  `basert-harness/1` is still accepted by the server
- llama.cpp measurements: `computearena-measurements/1`, executed as
  `llama-bench-independent-pp-tg/1` or, with cooldown,
  `llama-bench-conditioned-pp-tg/1`
- Telemetry: `basert-telemetry/3` for BaseRT, `computearena-telemetry/1` for
  llama.cpp
- Signing: Ed25519 over `computearena-json-v1` canonical JSON

## Development

CI runs `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo test --workspace --all-targets`. [docs/testing.md](docs/testing.md)
explains what the integration suites cover and what passing them does not
prove. [docs/releasing.md](docs/releasing.md) describes the release pipeline,
staging builds, and signing.

## Community

Questions and results are welcome on the
[ComputeArena Discord](https://discord.gg/CCT24GWhPG). The leaderboard, the
privacy policy, and the terms are at [computearena.ai](https://computearena.ai).

## License

Apache-2.0. See [LICENSE](LICENSE).
