# Runtime adapter regression tests

Run from the CLI repository:

```sh
cargo test --workspace --all-targets
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

For just the black-box contracts:

```sh
cargo test --test adapter_contract
cargo test --test runtime_flow
```

The existing GitHub Actions workflow runs all of these tests on Linux and macOS.
The executable fixtures use POSIX shell, so these integration suites are Unix-only.
They do not execute an installed BaseRT/llama.cpp binary, load real model tensors,
run GPU work, contact GitHub, or upload to ComputeArena. HTTP tests bind only an
ephemeral loopback port. Each fixture has its own temporary model, executable,
installation key, reports, HOME, PATH, and API configuration. The new contract
suite clears inherited environment variables so local credentials and runtime
configuration cannot affect it. Fixture directories are removed automatically.

## Coverage

| Area | Contract |
| --- | --- |
| CLI UX | Runtime-scoped help, no nested runtime selectors, menu exit, common list/inspect/verify commands, NO_COLOR output, explicit install/capability guidance |
| Discovery | PATH discovery, explicit executable path override, paths containing spaces, missing/incompatible executables |
| Runtime invocation | Requested PP sweep forwarded, separate PP/TG samples, llama.cpp depth zero and JSON output, BaseRT telemetry flag, no default cooldown, native warmup disablement |
| Result consistency | Identical raw measurements produce identical token/second metrics and units across adapters; bogus llama.cpp aggregate rates are ignored |
| Protocol differences | llama.cpp records native warmup, zero initial context, and absent telemetry; no invented BaseRT telemetry or cooldown support |
| Runtime failures | Nonzero exit, malformed JSON, executable mutation during a benchmark: no report signed |
| Measurement validation | Missing workloads, token-count mismatches, repetition mismatches, invalid/unsafe durations, nonzero llama.cpp depth, inconsistent build identity |
| Binary identity | Signed SHA-256 equals the executable bytes; platform identity recorded; report remains verifiable after the executable is removed or upgraded |
| Report integrity | Changes to runtime identity, digest, version, timing, rates, token sizes, model, timestamp, or run ID invalidate the signature |
| Signature format | Unsigned/malformed reports, bad algorithm/canonicalization/key ID/signature rejected; whitespace and key-order changes accepted |
| Run identity | New runs get distinct IDs while preserving the installation key |
| Submission | Actual JSON upload body equals the signed report; anonymous submission; public-access notice; informational checksum mismatch/download guidance; duplicate HTTP 200 is successful |
| Batch handling | Invalid reports require explicit skip for noninteractive partial uploads; only valid reports sent; all-invalid batches rejected locally |
| Failure recovery | HTTP 422 allows the next report; HTTP 429/500 stop the queue; saved reports remain available |

## What passing tests do not prove

- Fake runtimes validate the adapter contract, not performance accuracy or compatibility
  with every upstream binary. Test published BaseRT and llama.cpp builds on actual
  hardware before release. No real hardware benchmarks are part of this suite.
- The mock server verifies the CLI's handling of provenance responses. Trusted
  release catalogue matching, archive verification, server-side signature validation,
  and database deduplication belong to the private web repository's tests.
- An installation signature establishes integrity relative to its signing key, not
  benchmark truthfulness. Someone controlling the client/key can invent and sign data,
  including a known binary digest. Before/after hashing is a useful change detector,
  not execution attestation; it does not cover dynamically loaded libraries or a
  binary replaced and restored between the two hashes.
- Scripted menu tests cover text-mode behavior, not visual quality, arrow-key model
  pickers, TTY-only confirmation/preview, or every terminal emulator. Those still
  need an interactive smoke check (including NO_COLOR and a narrow terminal).

The new tests exposed a BaseRT adapter gap: structurally valid samples could describe
different workloads from the CLI request. The adapter now checks the requested PP
groups, TG token count, per-group repetition count, and safe nanosecond values before
signing. Existing reports retain their original signature-verification behavior.
