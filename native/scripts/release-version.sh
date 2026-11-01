#!/usr/bin/env bash
# Print the release version of ShareCtrlScreen, read from Cargo's own metadata.
#
# Usage:
#   release-version.sh                      run `cargo metadata` and print the version
#   release-version.sh --metadata FILE      parse a saved `cargo metadata --format-version 1`
#                                           output instead of running cargo (tests, lanes)
#   ... --expect-tag vX.Y.Z                 also fail unless the tag is exactly "v<version>"
#
# The version is `[workspace.package] version` in native/Cargo.toml, which every crate
# inherits. It is read through `cargo metadata --format-version 1` (the stable, documented
# JSON interface: https://doc.rust-lang.org/cargo/commands/cargo-metadata.html), not by
# grepping Cargo.toml, so `version.workspace = true`, comments and table order cannot
# fool it. The package looked at is `app`, the crate the Tauri bundles are built from.
#
# tauri.conf.json carries no `version`: Tauri then takes the version from the app's
# Cargo.toml (https://v2.tauri.app/reference/config/#version). The script fails if a
# `version` reappears there, because two copies of the version drift apart.
#
# JSON is parsed with jq when present (all GitHub-hosted runners have it), else Python 3.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
native="$(cd "$here/.." && pwd)"

metadata_file=""
expect_tag=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --metadata)
      metadata_file="${2:?--metadata needs a file}"
      shift 2
      ;;
    --expect-tag)
      expect_tag="${2:?--expect-tag needs a tag}"
      shift 2
      ;;
    *)
      echo "usage: $0 [--metadata FILE] [--expect-tag vX.Y.Z]" >&2
      exit 2
      ;;
  esac
done

# json_query <python-expression-over-d> <jq-filter>  (JSON on stdin)
# Prints the value, or an empty line when it is missing or null.
json_query() {
  if command -v jq >/dev/null 2>&1; then
    jq -r "($2) // empty" | tr -d '\r'
  else
    local py
    py="$(command -v python3 || command -v python || true)"
    if [[ -z "$py" ]]; then
      echo "error: need jq or python3 to read JSON" >&2
      return 1
    fi
    "$py" -c "import json,sys
d=json.load(sys.stdin)
v=$1
print('' if v is None else v)" | tr -d '\r'
  fi
}

if [[ -n "$metadata_file" ]]; then
  metadata="$(cat "$metadata_file")"
else
  # --no-deps: workspace members only, so no registry access and no dependency resolution.
  metadata="$(cargo metadata --no-deps --format-version 1 --manifest-path "$native/Cargo.toml")"
fi

version="$(printf '%s' "$metadata" | json_query \
  "next((p['version'] for p in d['packages'] if p['name']=='app'), None)" \
  '[.packages[] | select(.name == "app")][0].version')"

if [[ -z "$version" ]]; then
  echo "error: package 'app' not found in cargo metadata" >&2
  exit 1
fi
# SemVer 2.0.0 core + optional pre-release / build metadata (https://semver.org/).
semver='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'
if [[ ! "$version" =~ $semver ]]; then
  echo "error: '$version' is not a SemVer version" >&2
  exit 1
fi

conf_version="$(json_query "d.get('version')" '.version' <"$native/app/tauri.conf.json")"
if [[ -n "$conf_version" ]]; then
  echo "error: native/app/tauri.conf.json has \"version\": \"$conf_version\"; remove it so Tauri uses the Cargo version ($version)" >&2
  exit 1
fi

if [[ -n "$expect_tag" && "$expect_tag" != "v$version" ]]; then
  echo "error: tag '$expect_tag' does not match the Cargo version '$version' (expected tag 'v$version')" >&2
  exit 1
fi

echo "$version"
