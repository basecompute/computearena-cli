#!/usr/bin/env bash
# Package one platform bundle of the ComputeArena CLI.
#
#   package.sh <binary> <bundle-name> <out-dir>
#
# Writes <out-dir>/<bundle-name>.tar.gz and <bundle-name>.tar.gz.sha256. The
# archive is flat, like the BaseRT engine bundles: `computearena`, `LICENSE`,
# `README.md`, so `tar -xzf bundle.tar.gz -C <dir> computearena` is an install.
# The checksum line names only the archive's basename so `sha256sum -c` (or
# `shasum -a 256 -c`) verifies it from inside <out-dir>.
set -euo pipefail

BIN="${1:?usage: package.sh <binary> <bundle-name> <out-dir>}"
NAME="${2:?missing bundle name}"
OUT="${3:?missing output directory}"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[ -f "$BIN" ] && [ -x "$BIN" ] || { echo "::error::$BIN is not an executable file"; exit 1; }
case "$NAME" in
  *[!0-9A-Za-z._-]*|"") echo "::error::bundle name '$NAME' has characters not allowed in an asset name"; exit 1 ;;
esac

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
install -m 0755 "$BIN" "$STAGE/computearena"
install -m 0644 "$ROOT/LICENSE" "$STAGE/LICENSE"
install -m 0644 "$ROOT/README.md" "$STAGE/README.md"

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
# COPYFILE_DISABLE keeps macOS bsdtar from adding AppleDouble `._` entries that
# GNU tar would extract as junk files beside the binary.
COPYFILE_DISABLE=1 tar -czf "$OUT/$NAME.tar.gz" -C "$STAGE" computearena LICENSE README.md
(
  cd "$OUT"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256"
  else
    shasum -a 256 "$NAME.tar.gz" > "$NAME.tar.gz.sha256"
  fi
)
echo "Packaged $OUT/$NAME.tar.gz"
cat "$OUT/$NAME.tar.gz.sha256"
