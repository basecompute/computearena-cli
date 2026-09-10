#!/usr/bin/env bash
# Developer ID signing and notarization of the macOS CLI binary.
#
#   macos_sign.sh sign <binary>       import the certificate into a temporary
#                                     keychain, sign with the hardened runtime
#                                     and a trusted timestamp, verify
#   macos_sign.sh notarize <binary>   submit to Apple's notary service and wait
#                                     for acceptance
#   macos_sign.sh cleanup             delete the temporary keychain and key
#                                     files (run it with `if: always()`)
#
# Credentials arrive through the environment, never as arguments:
#   APPLE_DEVELOPER_ID_P12            base64 of the "Developer ID Application"
#                                     certificate plus private key (.p12)
#   APPLE_DEVELOPER_ID_P12_PASSWORD   the password chosen when exporting it
#   APPLE_DEVELOPER_ID_IDENTITY       optional: identity to sign with; defaults
#                                     to the Developer ID Application identity
#                                     found in the .p12
#   APPLE_NOTARY_KEY_P8               App Store Connect API key (.p8), as raw
#                                     PEM or base64
#   APPLE_NOTARY_KEY_ID               its key ID
#   APPLE_NOTARY_ISSUER_ID            its issuer ID
#
# A bare Mach-O binary cannot carry a stapled ticket (stapling needs an app,
# package, or disk image), so Gatekeeper looks the notarization up online the
# first time a quarantined copy runs. Copies installed with gh or curl piped
# into tar are never quarantined in the first place.
set -euo pipefail

TMP="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
KEYCHAIN="$TMP/computearena-signing.keychain-db"
P12_FILE="$TMP/computearena-developer-id.p12"
KEY_FILE="$TMP/computearena-notary-key.p8"
ZIP_FILE="$TMP/computearena-notarize.zip"
IDENTIFIER="co.basecompute.computearena"

say() { echo "[sign] $*"; }
fail() { echo "::error::[sign] $*" >&2; exit 1; }
need() { [ -n "${!1:-}" ] || fail "$1 is not set"; }

cmd="${1:-}"
shift || true
case "$cmd" in
  sign)
    BIN="${1:?usage: macos_sign.sh sign <binary>}"
    [ -f "$BIN" ] || fail "$BIN not found"
    need APPLE_DEVELOPER_ID_P12
    need APPLE_DEVELOPER_ID_P12_PASSWORD

    printf '%s' "$APPLE_DEVELOPER_ID_P12" | base64 --decode > "$P12_FILE" \
      || fail "APPLE_DEVELOPER_ID_P12 is not valid base64"
    KEYCHAIN_PASSWORD="$(openssl rand -hex 24)"
    rm -f "$KEYCHAIN"
    security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
    security set-keychain-settings -lut 21600 "$KEYCHAIN"
    security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
    security import "$P12_FILE" -P "$APPLE_DEVELOPER_ID_P12_PASSWORD" \
      -A -t cert -f pkcs12 -k "$KEYCHAIN" >/dev/null
    rm -f "$P12_FILE"
    # Let Apple's own tools use the private key without a UI prompt.
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
      -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
    if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
      # Ephemeral runner: make the temporary keychain the one that is searched.
      security list-keychains -d user -s "$KEYCHAIN"
    fi

    IDENTITY="${APPLE_DEVELOPER_ID_IDENTITY:-}"
    if [ -z "$IDENTITY" ]; then
      # `find-identity -v` lists only identities whose chain is valid for code
      # signing; the second field is the certificate hash, which is unambiguous.
      IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN" \
        | awk '/Developer ID Application/ { print $2; exit }')"
      if [ -z "$IDENTITY" ]; then
        security find-identity -p codesigning "$KEYCHAIN" >&2 || true
        fail "no valid 'Developer ID Application' identity in the certificate"
      fi
    fi

    say "signing $(basename "$BIN") with $IDENTITY"
    codesign --force --sign "$IDENTITY" --keychain "$KEYCHAIN" \
      --options runtime --timestamp --identifier "$IDENTIFIER" "$BIN"
    codesign --verify --strict --verbose=2 "$BIN"
    INFO="$(codesign -dvv "$BIN" 2>&1)"
    printf '%s\n' "$INFO" | grep -E '^(Identifier|Authority|Timestamp|TeamIdentifier)=' || true
    printf '%s\n' "$INFO" | grep -q '^Authority=Developer ID Application' \
      || fail "signature is not from a Developer ID Application certificate"
    printf '%s\n' "$INFO" | grep -Eq '^CodeDirectory .*flags=.*runtime' \
      || fail "hardened runtime flag is missing from the signature"
    say "signed and verified"
    ;;

  notarize)
    BIN="${1:?usage: macos_sign.sh notarize <binary>}"
    [ -f "$BIN" ] || fail "$BIN not found"
    need APPLE_NOTARY_KEY_P8
    need APPLE_NOTARY_KEY_ID
    need APPLE_NOTARY_ISSUER_ID

    case "$APPLE_NOTARY_KEY_P8" in
      *"BEGIN PRIVATE KEY"*) printf '%s\n' "$APPLE_NOTARY_KEY_P8" > "$KEY_FILE" ;;
      *) printf '%s' "$APPLE_NOTARY_KEY_P8" | base64 --decode > "$KEY_FILE" \
           || fail "APPLE_NOTARY_KEY_P8 is neither a PEM key nor valid base64" ;;
    esac
    chmod 0600 "$KEY_FILE"
    rm -f "$ZIP_FILE"
    ditto -c -k --keepParent "$BIN" "$ZIP_FILE"

    say "submitting $(basename "$BIN") to Apple's notary service"
    RESULT="$TMP/computearena-notary-result.json"
    if ! xcrun notarytool submit "$ZIP_FILE" \
        --key "$KEY_FILE" --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER_ID" \
        --wait --timeout 20m --output-format json > "$RESULT"; then
      cat "$RESULT" >&2 || true
      fail "notarytool submit failed"
    fi
    ID="$(jq -r '.id // empty' "$RESULT")"
    STATUS="$(jq -r '.status // empty' "$RESULT")"
    say "submission ${ID:-?}: ${STATUS:-?}"
    if [ "$STATUS" != "Accepted" ]; then
      if [ -n "$ID" ]; then
        xcrun notarytool log "$ID" \
          --key "$KEY_FILE" --key-id "$APPLE_NOTARY_KEY_ID" --issuer "$APPLE_NOTARY_ISSUER_ID" >&2 || true
      fi
      fail "notarization was not accepted (status: ${STATUS:-unknown})"
    fi
    rm -f "$KEY_FILE" "$ZIP_FILE" "$RESULT"
    say "notarized"
    ;;

  cleanup)
    if [ -f "$KEYCHAIN" ]; then
      security delete-keychain "$KEYCHAIN" && say "removed temporary keychain"
    fi
    rm -f "$P12_FILE" "$KEY_FILE" "$ZIP_FILE"
    ;;

  *)
    echo "usage: $0 sign <binary> | notarize <binary> | cleanup" >&2
    exit 2
    ;;
esac
