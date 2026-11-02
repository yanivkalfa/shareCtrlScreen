#!/usr/bin/env bash
# Generate the UPDATER signing keypair (task U1). This is SEPARATE from macOS
# code signing (plans/00-DECISIONS.md #9): it signs the update payloads the
# in-app updater verifies against the public key in native/app/tauri.conf.json.
#
# The orchestrator runs this ONCE, blind (like native/scripts/macos-make-signing-cert.sh),
# then creates two repository secrets from the files it writes:
#
#   TAURI_SIGNING_PRIVATE_KEY           = contents of <out>/updater.key
#   TAURI_SIGNING_PRIVATE_KEY_PASSWORD  = contents of <out>/updater.key.password
#
# and pastes the PUBLIC key this prints into native/app/tauri.conf.json under
# plugins.updater.pubkey (replacing the REPLACE_WITH_… placeholder). The public
# key is public by design; nothing secret is printed to stdout.
#
# Usage: updater-make-key.sh [output-dir]
#   output-dir  where to write the key files (default: a fresh mktemp -d). The
#               password is taken from $UPDATER_KEY_PASSWORD, or a random one is
#               generated and written to <out>/updater.key.password.
#
# References
#   - Tauri v2 updater signing + `tauri signer generate` (-w/--write-keys,
#     -p/--password, -f/--force, --ci): https://v2.tauri.app/plugin/updater/
#   - Tauri CLI reference (signer generate): https://v2.tauri.app/reference/cli/
set -euo pipefail

out="${1:-$(mktemp -d)}"
mkdir -p "$out"
key="$out/updater.key"
pub="$key.pub"
pwfile="$key.password"

# tauri-cli is invoked as the project does everywhere else.
tauri() { cargo tauri "$@"; }
if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo (with the Tauri CLI, 'cargo install tauri-cli') is required" >&2
  exit 1
fi

# A password protects the private key at rest; keep it beside the key so the
# orchestrator can copy both into the secrets. Random when not supplied.
password="${UPDATER_KEY_PASSWORD:-}"
if [[ -z "$password" ]]; then
  password="$(head -c 48 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c 40)"
fi
umask 077
printf '%s' "$password" >"$pwfile"

# --ci: never prompt. -f: overwrite a stale key in the output dir. -w writes the
# private key to "$key" and the public key to "$key.pub".
CI=true tauri signer generate --ci -f -p "$password" -w "$key" >/dev/null

if [[ ! -f "$pub" ]]; then
  echo "error: expected the public key at $pub but it was not written; run with -v to debug" >&2
  exit 1
fi

cat <<EOF

================================ UPDATER KEYPAIR ================================
Files written (keep the private key and password OUT of git):
  private key : $key
  password    : $pwfile
  public key  : $pub

Create these repository secrets (Settings → Secrets and variables → Actions):
  TAURI_SIGNING_PRIVATE_KEY           ← contents of $key
  TAURI_SIGNING_PRIVATE_KEY_PASSWORD  ← contents of $pwfile

Paste this PUBLIC key into native/app/tauri.conf.json → plugins.updater.pubkey
(replacing the REPLACE_WITH_… placeholder), commit it, and cut the first
updater-enabled release:
--------------------------------------------------------------------------------
$(cat "$pub")
--------------------------------------------------------------------------------
================================================================================
EOF
