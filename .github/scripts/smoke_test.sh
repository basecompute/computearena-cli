#!/bin/sh
# Smoke-test a packaged ComputeArena CLI bundle the way a user would: verify the
# checksum, extract the archive, and run the binary offline through its real
# code paths — version, help, list, a benchmark run against a stub llama-bench,
# verify, inspect, and rejection of a tampered report. No network, no real
# runtime, no GPU. Mirrors tests/runtime_flow.rs, but against the shipped
# binary rather than a test build.
#
#   smoke_test.sh <bundle.tar.gz> <expected-version>
#
# POSIX sh on purpose: the Linux legs run inside a stock ubuntu:22.04 container
# (the arm64 one under QEMU) to prove the glibc floor on an older distro.
set -eu

BUNDLE="${1:?usage: smoke_test.sh <bundle.tar.gz> <expected-version>}"
EXPECTED="${2:?missing expected version}"

say() { printf '[smoke] %s\n' "$*"; }
fail() { printf '::error::[smoke] %s\n' "$*" >&2; exit 1; }

[ -f "$BUNDLE" ] || fail "bundle $BUNDLE not found"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT HUP INT TERM

# 1. The checksum sidecar must exist and match: the publish step uploads both,
#    and an installer verifies the archive with it.
SIDECAR="$BUNDLE.sha256"
[ -f "$SIDECAR" ] || fail "checksum sidecar $SIDECAR is missing"
(
  cd "$(dirname "$BUNDLE")"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -c "$(basename "$SIDECAR")"
  else
    shasum -a 256 -c "$(basename "$SIDECAR")"
  fi
) >/dev/null || fail "checksum mismatch for $(basename "$BUNDLE")"
say "checksum verified"

# 2. Layout: flat archive with the binary, license, and readme.
mkdir "$WORK/bundle"
tar -xzf "$BUNDLE" -C "$WORK/bundle"
for f in computearena LICENSE README.md; do
  [ -f "$WORK/bundle/$f" ] || fail "bundle is missing $f"
done
BIN="$WORK/bundle/computearena"
[ -x "$BIN" ] || fail "computearena is not executable"
say "bundle layout OK"

# Every invocation runs in a scrubbed environment with its own HOME and data
# directory, so a runner's (or developer's) saved session, signing key, and
# COMPUTEARENA_*/BASERT_* settings cannot leak in.
HOME_DIR="$WORK/home"
DATA="$WORK/data"
mkdir -p "$HOME_DIR" "$DATA"
cli() {
  env -i PATH="$PATH" HOME="$HOME_DIR" NO_COLOR=1 TERM=dumb "$BIN" --data-dir "$DATA" "$@"
}

# 3. Version, help, and an empty listing.
VERSION_OUT="$(cli --version)" || fail "--version exited non-zero"
case "$VERSION_OUT" in
  "computearena $EXPECTED"|"computearena $EXPECTED "*) say "version: $VERSION_OUT" ;;
  *) fail "--version printed '$VERSION_OUT', expected 'computearena $EXPECTED'" ;;
esac
cli --help >/dev/null || fail "--help failed"
cli list >/dev/null || fail "list failed on an empty data directory"

# 4. Offline benchmark round-trip against a stub llama-bench. The stub answers
#    --help with the feature flags the adapter probes for and otherwise prints
#    one row per requested workload. Decode starts with one seed token; the
#    headline PP512/TG128 pair must run before the remaining PP128 sweep.
#    The "model" is a GGUF header: magic, version 3, zero counts.
MODEL="$WORK/Qwen3-4B.gguf"
{ printf 'GGUF\003'; head -c 19 /dev/zero; } > "$MODEL"
STUB="$WORK/llama-bench"
cat > "$STUB" <<'SH'
#!/bin/sh
set -eu
if [ "${1:-}" = --help ]; then
  printf '%s\n' '--n-prompt --n-gen --n-depth --repetitions --no-warmup json'
  exit 0
fi
pp= tg= depth= reps= format= model=
while [ "$#" -gt 0 ]; do
  case "$1" in
    -m) model="$2"; shift 2 ;;
    -p) pp="$2"; shift 2 ;;
    -n) tg="$2"; shift 2 ;;
    -d) depth="$2"; shift 2 ;;
    -r) reps="$2"; shift 2 ;;
    -o) format="$2"; shift 2 ;;
    --no-warmup) shift ;;
    *) printf 'Unexpected fixture argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done
[ -f "$model" ] && [ "$reps" = 2 ] && [ "$format" = json ] || exit 2
case "$pp:$tg:$depth" in
  512:0:0|0:128:1|128:0:0) ;;
  *) printf 'Unexpected fixture workload: %s\n' "$pp:$tg:$depth" >&2; exit 2 ;;
esac
printf '%s\n' "$pp:$tg:$depth" >> "$0.calls"
printf '[{"build_commit":"abc123","build_number":123,"model_type":"Qwen3 Q4_K_M","model_filename":"Qwen3-4B.gguf","model_size":24,"model_n_params":4000000000,"n_prompt":%s,"n_gen":%s,"n_depth":%s,"n_gpu_layers":99,"gpu_info":"Apple M5 Pro","cpu_info":"Apple M5 Pro","backends":"Metal","samples_ns":[100000000,200000000]}]\n' "$pp" "$tg" "$depth"
SH
chmod 0755 "$STUB"

REPORT="$WORK/report.json"
if ! cli llama-cpp --runtime-path "$STUB" run "$MODEL" --pp 128,512 --reps 2 --yes --output "$REPORT" >"$WORK/run.log" 2>&1; then
  cat "$WORK/run.log" >&2
  fail "offline llama-cpp benchmark run failed"
fi
EXPECTED_CALLS="$(printf '%s\n' '512:0:0' '0:128:1' '128:0:0')"
[ "$(cat "$STUB.calls")" = "$EXPECTED_CALLS" ] || fail "expected PP512, TG128 (depth 1), then PP128, exactly once each"
say "headline-first workload order and decode seed depth verified"
[ -f "$REPORT" ] || fail "run did not write $REPORT"
grep -q '"name": *"llama-cpp"' "$REPORT" || fail "report does not record the llama-cpp runtime"
grep -q "\"computearena_version\": *\"$EXPECTED\"" "$REPORT" || fail "report does not record computearena_version $EXPECTED"
say "offline benchmark run signed a report"

cli verify "$REPORT" >/dev/null 2>"$WORK/verify.err" || { cat "$WORK/verify.err" >&2; fail "verify rejected the freshly signed report"; }
cli inspect "$REPORT" >/dev/null || fail "inspect failed on the report"
say "verify and inspect accepted the report"

# 5. A report altered after signing must be rejected — this is the property the
#    whole client exists to provide, so a build that lost it must not publish.
sed 's/"sha256": *"[0-9a-f]\{64\}"/"sha256":"0000000000000000000000000000000000000000000000000000000000000000"/' "$REPORT" > "$WORK/tampered.json"
cmp -s "$REPORT" "$WORK/tampered.json" && fail "could not produce a tampered report (no sha256 field found)"
if cli verify "$WORK/tampered.json" >/dev/null 2>&1; then
  fail "verify accepted a tampered report"
fi
say "tampered report rejected"

say "all checks passed for $(basename "$BUNDLE")"
