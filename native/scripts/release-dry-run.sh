#!/usr/bin/env bash
# Build this machine's release assets locally, exactly as the Release workflow does,
# without publishing anything. Windows (Git Bash), macOS and Linux (Ubuntu 22.04 or
# newer with the packages listed in .github/workflows/build-assets.yml).
#
# Usage: release-dry-run.sh [--skip-build] [--print-build-steps] [windows|macos|linux]
#
#   (no flag)            build + package + SHA256SUMS.txt into native/dist/
#   --print-build-steps  print the cargo commands only (run them from native/app), for
#                        machines where cargo runs somewhere else (a build queue,
#                        a container); then come back with --skip-build
#   --skip-build         package an existing build + write SHA256SUMS.txt (no cargo
#                        unless RELEASE_VERSION is unset, which reads `cargo metadata`)
#
# The OS defaults to the one this runs on. Needs: the Rust toolchain, the Tauri CLI
# (`cargo install tauri-cli --version <TAURI_CLI_VERSION in build-assets.yml> --locked`),
# cargo-about (`cargo install cargo-about --version <CARGO_ABOUT_VERSION> --locked
# --features cli`), and jq or Python 3. Without MAC_SIGN_P12_B64 / MAC_SIGN_P12_PASS the
# macOS app is signed ad-hoc, as in a fork.
#
# All the work is in release-assets.sh; this only picks the OS and adds checksums, so
# the local result and the CI result come from the same commands.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
native="$(cd "$here/.." && pwd)"

mode=all
os=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) mode=package ;;
    --print-build-steps) mode=steps ;;
    windows | macos | linux) os="$1" ;;
    *)
      echo "usage: $0 [--skip-build] [--print-build-steps] [windows|macos|linux]" >&2
      exit 2
      ;;
  esac
  shift
done

if [[ -z "$os" ]]; then
  case "$(uname -s)" in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    MINGW* | MSYS* | CYGWIN*) os=windows ;;
    *)
      echo "error: unknown OS '$(uname -s)'; pass windows, macos or linux" >&2
      exit 2
      ;;
  esac
fi

"$here/release-assets.sh" "$mode" "$os"
if [[ "$mode" != steps ]]; then
  "$here/release-assets.sh" checksums "$native/dist"
  echo "Dry run complete: $os assets and SHA256SUMS.txt are in $native/dist (nothing was published)."
fi
