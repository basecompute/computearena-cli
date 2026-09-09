# ComputeArena CLI

The workspace builds the standalone `computearena` client. It owns local
report creation, Ed25519 signing, inspection, verification, device login, and
submission. Runtime-specific measurements stay behind executable adapters, so
the CLI does not link the BaseRT engine.

Only BaseRT is supported today. llama.cpp, MLX, and vLLM adapters can be added
without moving the report, authentication, or submission code.

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

Benchmarks can be generated and verified offline. Login is only required to
associate submissions with a profile. A report signature detects modification
after the client finalized the file; it does not prove that a modified client,
harness, driver, or operating system reported truthful measurements.

## Protocol compatibility

- Report envelope: `computearena-benchmark/1`
- New BaseRT harness output: `basert-benchmark-harness/1`
- Legacy BaseRT harness output remains accepted by the server:
  `basert-harness/1`
- Telemetry: `basert-telemetry/3`
- Signing: Ed25519 over `computearena-json-v1` canonical JSON

Community and support: [ComputeArena Discord](https://discord.gg/vENxergRG6).
