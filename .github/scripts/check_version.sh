#!/usr/bin/env bash
# Gate the version a release branch or tag advertises against the Cargo workspace.
#
#   check_version.sh <X.Y.Z>             the workspace version must equal X.Y.Z
#   check_version.sh <X.Y.Z> --release   additionally: X.Y.Z is newer than every
#                                        existing v* tag, and HEAD is on origin/main
#
# The root Cargo.toml `[workspace.package].version` is the single version site
# (the crate inherits it), so nothing needs rewriting; Cargo.lock is covered by
# the `--locked` builds. The rc branch name and the release tag both carry the
# version, and this is what stops `rc-0.2.0` from staging a tree that still says
# 0.1.0, or `v0.2.0` from shipping one.
set -euo pipefail

usage() {
  echo "usage: $0 <X.Y.Z> [--release]" >&2
  exit 2
}

EXPECTED="${1:-}"
[ -n "$EXPECTED" ] || usage
RELEASE=0
case "${2:-}" in
  "") ;;
  --release) RELEASE=1 ;;
  *) usage ;;
esac

if ! printf '%s\n' "$EXPECTED" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "::error::version '$EXPECTED' is not a plain MAJOR.MINOR.PATCH"
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

# `cargo pkgid` prints "<source>#<version>", or "<source>#<name>@<version>" when
# the package name differs from its directory. --locked also refuses a stale
# Cargo.lock, which would otherwise surface later as a failed --locked build.
PKGID="$(cargo pkgid --locked -p computearena-cli)"
ACTUAL="${PKGID##*#}"
ACTUAL="${ACTUAL##*@}"

if [ "$ACTUAL" != "$EXPECTED" ]; then
  echo "::error::Cargo workspace version is $ACTUAL but this ref advertises $EXPECTED." \
    "Set [workspace.package].version in Cargo.toml to $EXPECTED, run 'cargo check' so Cargo.lock follows, and commit both."
  exit 1
fi
echo "Cargo workspace version $ACTUAL matches $EXPECTED."

[ "$RELEASE" -eq 1 ] || exit 0

# $1 > $2 for MAJOR.MINOR.PATCH, without relying on `sort -V` (absent on macOS).
semver_gt() {
  local a1 a2 a3 b1 b2 b3
  IFS=. read -r a1 a2 a3 <<<"$1"
  IFS=. read -r b1 b2 b3 <<<"$2"
  [ "$a1" -ne "$b1" ] && { [ "$a1" -gt "$b1" ]; return; }
  [ "$a2" -ne "$b2" ] && { [ "$a2" -gt "$b2" ]; return; }
  [ "$a3" -gt "$b3" ]
}

NEWEST=""
while read -r tag; do
  [ -n "$tag" ] || continue
  v="${tag#v}"
  [ "$v" != "$EXPECTED" ] || continue
  if [ -z "$NEWEST" ] || semver_gt "$v" "$NEWEST"; then
    NEWEST="$v"
  fi
done < <(git tag --list 'v*' | grep -Ex 'v[0-9]+\.[0-9]+\.[0-9]+' || true)

if [ -n "$NEWEST" ]; then
  if ! semver_gt "$EXPECTED" "$NEWEST"; then
    echo "::error::v$EXPECTED is not newer than the existing release tag v$NEWEST"
    exit 1
  fi
  echo "v$EXPECTED is newer than the previous release v$NEWEST."
else
  echo "No previous v* release tag; v$EXPECTED would be the first."
fi

# Releases are cut from main only, after the rc pull request merges.
if ! git rev-parse -q --verify origin/main >/dev/null; then
  git fetch -q --no-tags origin main:refs/remotes/origin/main
fi
if ! git merge-base --is-ancestor HEAD origin/main; then
  echo "::error::commit $(git rev-parse --short HEAD) is not on origin/main;" \
    "tag a commit on main once the release candidate has merged"
  exit 1
fi
echo "Commit $(git rev-parse --short HEAD) is on origin/main."
