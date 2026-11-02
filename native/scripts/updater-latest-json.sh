#!/usr/bin/env bash
# Generate the updater feed latest.json (task U1) from the release assets.
#
# The in-app updater polls
#   https://github.com/<repo>/releases/latest/download/latest.json
# (native/app/tauri.conf.json → plugins.updater.endpoints) and, for the running
# platform's target key, downloads `url` and verifies it against the committed
# public key using the embedded `signature` (the CONTENT of the Tauri `.sig`
# file, not a path). Schema: https://v2.tauri.app/plugin/updater/
#
# This discovers the updater artifacts by the `.sig` files the Tauri bundler
# emits when it is built with `createUpdaterArtifacts` + TAURI_SIGNING_PRIVATE_KEY
# (native/app/tauri.updater.conf.json, wired in release-assets.sh). Each `<asset>.sig`
# names its payload `<asset>`; the payload's extension maps it to a target:
#   *-setup.exe        -> windows-x86_64            (the NSIS installer)
#   *.app.tar.gz       -> darwin-universal, darwin-aarch64, darwin-x86_64
#   *.AppImage         -> linux-x86_64
# .deb/.rpm are deliberately NOT updater targets (the UI tells those users to
# download from Releases). Targets that have no `.sig` are simply left out.
#
# Usage: updater-latest-json.sh <assets-dir> <version> <download-base-url> [notes-file]
#   <download-base-url> e.g. https://github.com/<owner>/<repo>/releases/download/v<version>
#   [notes-file]        optional release notes (plain text; defaults to a short line)
#
# Exits 0 writing <assets-dir>/latest.json when at least one updater target was
# found. With none (a fork build without the signing secret) it warns and writes
# nothing, so a non-updater release still succeeds.
set -euo pipefail

[[ $# -ge 3 ]] || { echo "usage: $0 <assets-dir> <version> <download-base-url> [notes-file]" >&2; exit 2; }
dir="$1"
version="$2"
base="${3%/}"
notes_file="${4:-}"
[[ -d "$dir" ]] || { echo "error: '$dir' is not a directory" >&2; exit 1; }

notes="ShareCtrlScreen $version"
if [[ -n "$notes_file" && -f "$notes_file" ]]; then
  notes="$(cat "$notes_file")"
fi
# RFC 3339, UTC — the schema requires pub_date in that form when present.
pub_date="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# target_keys_for <asset-filename>: echo the latest.json target keys for it, or
# nothing if the asset is not an updater target.
target_keys_for() {
  case "$1" in
    *-setup.exe) echo "windows-x86_64" ;;
    *.app.tar.gz) echo "darwin-universal darwin-aarch64 darwin-x86_64" ;;
    *.AppImage | *.AppImage.tar.gz) echo "linux-x86_64" ;;
    *) : ;;
  esac
}

# Collect "target<TAB>url<TAB>signature" rows, de-duplicated per target.
rows=""
found=0
shopt -s nullglob
for sig in "$dir"/*.sig; do
  asset="$(basename "${sig%.sig}")"
  keys="$(target_keys_for "$asset")"
  [[ -n "$keys" ]] || { echo "note: $asset has a .sig but is not an updater target; skipping" >&2; continue; }
  signature="$(tr -d '\r\n' <"$sig")"
  url="$base/$asset"
  for key in $keys; do
    rows+="$key	$url	$signature"$'\n'
    found=1
  done
done
shopt -u nullglob

if [[ "$found" -eq 0 ]]; then
  echo "::warning title=No updater artifacts::no .sig files in $dir; latest.json not written (was the build signed with TAURI_SIGNING_PRIVATE_KEY?)" >&2
  exit 0
fi

# Assemble the JSON. Python (preinstalled on every runner) does the quoting so a
# signature or note never breaks the file.
VERSION="$version" NOTES="$notes" PUB_DATE="$pub_date" ROWS="$rows" python3 - "$dir/latest.json" <<'PY'
import json, os, sys

out_path = sys.argv[1]
platforms = {}
for line in os.environ["ROWS"].splitlines():
    if not line.strip():
        continue
    target, url, signature = line.split("\t")
    platforms[target] = {"signature": signature, "url": url}

doc = {
    "version": os.environ["VERSION"],
    "notes": os.environ["NOTES"],
    "pub_date": os.environ["PUB_DATE"],
    "platforms": platforms,
}
with open(out_path, "w", encoding="utf-8", newline="\n") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
print("wrote %s with targets: %s" % (out_path, ", ".join(sorted(platforms))))
PY
