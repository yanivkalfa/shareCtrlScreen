#!/usr/bin/env bash
# Sign a macOS .app with the project's self-signed certificate (or ad-hoc), then
# prove the signature is the kind that keeps Screen Recording / Accessibility
# grants across updates.
#
# Usage: macos-sign.sh path/to/ShareCtrlScreen.app
#        macos-sign.sh --cleanup     (needs MAC_SIGN_WORKDIR; see below)
#
# Environment (both or neither; never passed on the command line):
#   MAC_SIGN_P12_B64   base64 of the .p12 made by macos-make-signing-cert.sh
#   MAC_SIGN_P12_PASS  its password
# Both set   -> sign with that certificate; the designated requirement must then
#               contain `certificate leaf =` and must not contain `cdhash`.
# Neither    -> ad-hoc signature ("-"), for development builds only. An ad-hoc
#               identity is the code hash, so every rebuild is a new app to macOS
#               and the user must re-grant permissions.
# Only one   -> error (a half-configured secret pair is a mistake, not a fallback).
#
# The certificate is imported into a throw-away keychain that is deleted on exit.
# MAC_SIGN_WORKDIR (optional) names the directory that keychain lives in, instead of
# a fresh mktemp one. CI sets it so that a separate `if: always()` step can run
# `macos-sign.sh --cleanup` and remove the keychain even when this script was killed
# before its EXIT trap ran (a cancelled job). --cleanup is idempotent.
# Nothing secret is written to stdout/stderr. (`security import -P` puts the
# password in argv for the lifetime of that one process; acceptable on an
# ephemeral CI runner, do not run this on a shared multi-user machine.)
#
# Hardened runtime is NOT enabled: it only matters for notarization, which the
# project does not do (plans/00-DECISIONS.md #9), and it needs entitlements for
# anything beyond the plain case. `--timestamp=none` because Apple's timestamp
# service is for Developer ID certificates and a self-signed one is rejected by it.
#
# References
#   - Apple, Code Signing Guide, "Code Signing Requirement Language" and
#     "Checking Code Signatures" (designated requirement, `codesign -dr -`):
#     https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/RequirementLang/RequirementLang.html
#   - Apple TN2206, "macOS Code Signing In Depth":
#     https://developer.apple.com/library/archive/technotes/tn2206/_index.html
#   - Apple QA1940 (strip extended attributes before signing):
#     https://developer.apple.com/library/archive/qa/qa1940/_index.html
#   - Tauri v2, "macOS Code Signing" (CI keychain import pattern):
#     https://v2.tauri.app/distribute/sign/macos/
#   - GitHub, "Installing an Apple certificate on macOS runners for Xcode
#     development" (temporary keychain, set-key-partition-list):
#     https://docs.github.com/en/actions/use-cases-and-examples/deploying/installing-an-apple-certificate-on-macos-runners-for-xcode-development

set -euo pipefail

# cleanup_dir <dir>: undo everything a signing run may have left in <dir>. Safe on a
# directory that is half set up, already cleaned, or missing.
cleanup_dir() {
  local dir="$1"
  [[ -d "$dir" ]] || return 0
  # Put the user keychain search list back the way it was before the temporary
  # keychain was prepended (see the signing branch below).
  if [[ -f "$dir/keychains.txt" ]]; then
    local -a restore=()
    while IFS= read -r line; do [[ -n "$line" ]] && restore+=("$line"); done <"$dir/keychains.txt"
    if [[ ${#restore[@]} -gt 0 ]]; then
      security list-keychains -d user -s "${restore[@]}" >/dev/null 2>&1 || true
    fi
  fi
  # The marker is written only after the certificate was added to the admin trust
  # store, so this never removes a trust setting it did not add. Bounded by an
  # alarm: on recent macOS the admin-domain trust change can raise an authorization
  # dialog, and a headless runner would wait on it forever (release run #7 of
  # 2.1.1 hung 24 min in exactly this trap). The runner is ephemeral; a trust
  # setting left behind costs nothing there.
  if [[ -f "$dir/trusted" && -f "$dir/cert.pem" ]]; then
    perl -e 'alarm shift; exec @ARGV' 30 \
      sudo -n security remove-trusted-cert -d "$dir/cert.pem" >/dev/null 2>&1 || true
  fi
  if [[ -f "$dir/signing.keychain-db" ]]; then
    security delete-keychain "$dir/signing.keychain-db" >/dev/null 2>&1 || true
  fi
  rm -rf "$dir"
}

if [[ "${1:-}" == "--cleanup" ]]; then
  if [[ $# -ne 1 || -z "${MAC_SIGN_WORKDIR:-}" ]]; then
    echo "usage: MAC_SIGN_WORKDIR=dir $0 --cleanup" >&2
    exit 2
  fi
  cleanup_dir "$MAC_SIGN_WORKDIR"
  echo "signing work directory cleaned"
  exit 0
fi

if [[ $# -ne 1 ]]; then
  echo "usage: $0 path/to/App.app" >&2
  exit 2
fi
app="$1"
if [[ ! -d "$app/Contents/MacOS" ]]; then
  echo "error: '$app' is not an app bundle" >&2
  exit 1
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: codesign only exists on macOS" >&2
  exit 1
fi

p12_b64="${MAC_SIGN_P12_B64:-}"
p12_pass="${MAC_SIGN_P12_PASS:-}"
if [[ -n "$p12_b64" && -z "$p12_pass" ]] || [[ -z "$p12_b64" && -n "$p12_pass" ]]; then
  echo "error: MAC_SIGN_P12_B64 and MAC_SIGN_P12_PASS must be set together" >&2
  exit 1
fi

if [[ -n "${MAC_SIGN_WORKDIR:-}" ]]; then
  # Must not exist yet: a leftover directory could hold another run's keychain.
  if [[ -e "$MAC_SIGN_WORKDIR" ]]; then
    echo "error: MAC_SIGN_WORKDIR '$MAC_SIGN_WORKDIR' already exists" >&2
    exit 1
  fi
  work="$MAC_SIGN_WORKDIR"
  mkdir -p "$work"
else
  work="$(mktemp -d)"
fi
chmod 700 "$work"
keychain="$work/signing.keychain-db"

trap 'cleanup_dir "$work"' EXIT

# codesign refuses bundles carrying resource forks, Finder info or quarantine.
xattr -cr "$app"

sign_args=(--force --timestamp=none)

if [[ -n "$p12_b64" ]]; then
  keychain_pass="$(openssl rand -hex 24)"
  printf '%s' "$p12_b64" | openssl base64 -d -A -out "$work/identity.p12"

  security create-keychain -p "$keychain_pass" "$keychain"
  security set-keychain-settings -lut 3600 "$keychain"
  security unlock-keychain -p "$keychain_pass" "$keychain"
  security import "$work/identity.p12" -k "$keychain" -f pkcs12 \
    -P "$p12_pass" -T /usr/bin/codesign >/dev/null
  # Lets codesign use the key without a GUI "allow access" prompt.
  security set-key-partition-list -S apple-tool:,apple: -s \
    -k "$keychain_pass" "$keychain" >/dev/null
  # codesign resolves `-s <hash>` through the user keychain SEARCH LIST; the
  # --keychain flag alone does not put a fresh keychain there. Without this,
  # codesign answers "<hash>: no identity found" even though find-identity on
  # the keychain lists it (release run #7 of 2.1.1). Both the GitHub recipe and
  # Tauri's prepend the temporary keychain; the previous list is saved so the
  # cleanup can restore it.
  security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"$//' >"$work/keychains.txt"
  original_keychains=()
  while IFS= read -r line; do [[ -n "$line" ]] && original_keychains+=("$line"); done <"$work/keychains.txt"
  security list-keychains -d user -s "$keychain" "${original_keychains[@]}"

  # Without a trust setting `find-identity -v` hides a self-signed certificate.
  # The admin-domain trust needs passwordless sudo, which CI runners have and a
  # developer machine usually does not; signing by hash below works either way.
  security find-certificate -a -p "$keychain" >"$work/cert.pem"
  if sudo -n true >/dev/null 2>&1 &&
    sudo -n security add-trusted-cert -d -r trustRoot -p codeSign \
      -k /Library/Keychains/System.keychain "$work/cert.pem" >/dev/null 2>&1; then
    : >"$work/trusted"
  else
    echo "note: certificate not added to the trust store; signing by hash instead"
  fi

  identity="$(security find-identity -p codesigning "$keychain" |
    awk '/^[[:space:]]+[0-9]+\)/ { print $2; exit }')"
  if [[ ! "$identity" =~ ^[0-9A-F]{40}$ ]]; then
    echo "error: no code-signing identity found in the imported .p12" >&2
    exit 1
  fi
  sign_args+=(--keychain "$keychain" -s "$identity")
  echo "Signing with the self-signed certificate (SHA-1 $identity)"
else
  sign_args+=(-s -)
  echo "Signing ad-hoc: MAC_SIGN_P12_B64 / MAC_SIGN_P12_PASS not set (development build)"
fi

codesign "${sign_args[@]}" "$app"
codesign --verify --strict --verbose=2 "$app"

requirement="$(codesign -dr - "$app" 2>&1)"
echo "$requirement"

if [[ -n "$p12_b64" ]]; then
  if [[ "$requirement" != *"certificate leaf ="* || "$requirement" == *cdhash* ]]; then
    echo "error: designated requirement is not certificate-based; permissions would not survive an update" >&2
    exit 1
  fi
  echo "OK: designated requirement is bound to the certificate, not to the code hash"
else
  echo "Ad-hoc signature: designated requirement is a cdhash; permissions will not survive an update"
fi
