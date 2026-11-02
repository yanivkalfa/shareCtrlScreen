#!/usr/bin/env bash
# The one recipe that turns the native workspace into release assets for one OS.
# Used by .github/workflows/build-assets.yml (and so by release.yml, mac-build.yml and
# linux-build.yml) and by scripts/release-dry-run.sh on a developer machine.
#
# Usage: release-assets.sh <phase> <windows|macos|linux>
#        release-assets.sh checksums <dir>
#
# Phases
#   steps     print the cargo commands of the build phase, one per line (run them from
#             native/app). Lets a machine whose cargo runs elsewhere (a build queue, a
#             container) run exactly the same commands, then call `package` here.
#   build     run those cargo commands.
#   package   no cargo: collect, sign (macOS), wrap (DMG) and verify the bundles, and copy
#             the release assets into native/dist/ (emptied first).
#   all       build, then package.
#   checksums write <dir>/SHA256SUMS.txt over every other file in <dir> (`sha256sum -c`
#             format), the file the GitHub Release carries.
#
# Environment
#   CARGO_TARGET_DIR   honoured, as cargo does (default native/target).
#   RELEASE_VERSION    the version the bundles carry; default: scripts/release-version.sh.
#   MAC_SIGN_P12_B64, MAC_SIGN_P12_PASS, MAC_SIGN_WORKDIR
#                      macOS package phase only, passed to scripts/macos-sign.sh. Neither
#                      key set -> ad-hoc signature (development/fork builds) with a warning.
#
# Release assets in native/dist/
#   windows  ShareCtrlScreen_<v>_x64-setup.exe, sharectrl.exe (portable), sharectrl-service.exe
#   macos    ShareCtrlScreen_<v>_universal.dmg
#   linux    ShareCtrlScreen_<v>_amd64.AppImage, ShareCtrlScreen_<v>_amd64.deb,
#            ShareCtrlScreen-<v>-1.x86_64.rpm
#   all      THIRD-PARTY-LICENSES-<os>.html
#
# Third-party notices: `cargo about generate` renders about.hbs for the target triples of
# this OS only (`--target` overrides about.toml's list; cargo-about generate --help), into
# native/app/THIRD-PARTY-LICENSES.html (git-ignored). tauri.release.conf.json, merged with
# `tauri build --config` (https://v2.tauri.app/reference/cli/#build), adds that file to the
# bundle resources, so each installer carries the notices for the code it contains; the
# same file is also attached to the release. It is generated, never committed: a checked-in
# copy would be stale after every Cargo.lock change.
#
# References
#   - Tauri CLI `build` (`--bundles`, `--target universal-apple-darwin`, `--config`, and
#     `-- <args>` passed to cargo): https://v2.tauri.app/reference/cli/#build
#   - Tauri bundle resources: https://v2.tauri.app/develop/resources/
#   - Tauri distribution docs (NSIS, DMG/app, AppImage, Debian, RPM):
#     https://v2.tauri.app/distribute/
#   - cargo-about: https://embarkstudios.github.io/cargo-about/cli/generate/index.html
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
native="$(cd "$here/.." && pwd)"
app_dir="$native/app"
dist="$native/dist"
notice="$app_dir/THIRD-PARTY-LICENSES.html"

usage() {
  echo "usage: $0 <steps|build|package|all> <windows|macos|linux>" >&2
  echo "       $0 checksums <dir>" >&2
  exit 2
}

[[ $# -eq 2 ]] || usage
phase="$1"

# --- checksums ---------------------------------------------------------------------------
if [[ "$phase" == checksums ]]; then
  dir="$2"
  [[ -d "$dir" ]] || { echo "error: '$dir' is not a directory" >&2; exit 1; }
  (
    cd "$dir"
    rm -f SHA256SUMS.txt
    shopt -s nullglob
    files=(*)
    [[ ${#files[@]} -gt 0 ]] || { echo "error: '$dir' is empty" >&2; exit 1; }
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum -b -- "${files[@]}" >SHA256SUMS.txt.tmp
    else
      shasum -a 256 -b -- "${files[@]}" >SHA256SUMS.txt.tmp # macOS has no sha256sum
    fi
    # LF line endings and sorted names, so the file is identical whichever OS wrote it.
    tr -d '\r' <SHA256SUMS.txt.tmp | LC_ALL=C sort -k2 >SHA256SUMS.txt
    rm -f SHA256SUMS.txt.tmp
    cat SHA256SUMS.txt
  )
  exit 0
fi

os="$2"
case "$os" in windows | macos | linux) ;; *) usage ;; esac
case "$phase" in steps | build | package | all) ;; *) usage ;; esac

# --- build phase: cargo only -------------------------------------------------------------
# One command per line, no quoting, no shell syntax: `steps` prints them verbatim.
build_steps() {
  local about="cargo about generate --workspace --locked --fail --manifest-path ../Cargo.toml"
  local tauri="cargo tauri build --config tauri.release.conf.json"
  case "$os" in
    windows)
      echo "$about --target x86_64-pc-windows-msvc --output-file THIRD-PARTY-LICENSES.html ../about.hbs"
      # The SYSTEM service is a separate binary, not part of the Tauri bundle.
      echo "cargo build -p service --release --locked"
      echo "$tauri --bundles nsis -- --locked"
      ;;
    macos)
      echo "$about --target aarch64-apple-darwin --target x86_64-apple-darwin --output-file THIRD-PARTY-LICENSES.html ../about.hbs"
      # universal-apple-darwin: the CLI builds both Apple triples and `lipo`s them into
      # one binary in one .app. Only the .app: the DMG is made after signing (package).
      echo "$tauri --target universal-apple-darwin --bundles app -- --locked"
      ;;
    linux)
      echo "$about --target x86_64-unknown-linux-gnu --output-file THIRD-PARTY-LICENSES.html ../about.hbs"
      echo "$tauri --bundles appimage,deb,rpm -- --locked"
      ;;
  esac
}

run_build() {
  if [[ "$os" == linux ]]; then
    # The AppImage tooling Tauri downloads is itself an AppImage; GitHub runners (and
    # containers, WSL) have no FUSE, so let it extract itself instead of mounting.
    export APPIMAGE_EXTRACT_AND_RUN=1
  fi
  local line
  local -a cmd
  while IFS= read -r line; do
    read -r -a cmd <<<"$line"
    echo "+ (native/app) $line"
    (cd "$app_dir" && "${cmd[@]}")
  done < <(build_steps)
}

# --- package phase: no cargo -------------------------------------------------------------

target_dir="${CARGO_TARGET_DIR:-$native/target}"
if command -v cygpath >/dev/null 2>&1; then
  target_dir="$(cygpath -u "$target_dir")" # Git Bash on Windows: C:\x -> /c/x
fi

# one_file <dir> <pattern>: print the single file in <dir> matching the glob <pattern>;
# fail on none or several (a second match means a stale bundle from another version is
# lying in the target dir). The directory is quoted and only the pattern is a glob, so a
# path with spaces (C:\Users\John Smith\...) is not split.
one_file() {
  local -a m
  shopt -s nullglob
  # shellcheck disable=SC2206 # the pattern must expand here; the directory is quoted
  m=("$1"/$2)
  shopt -u nullglob
  if [[ ${#m[@]} -ne 1 ]]; then
    echo "error: expected exactly one file matching $1/$2, found ${#m[@]}: ${m[*]:-}" >&2
    return 1
  fi
  printf '%s\n' "${m[0]}"
}

summary() {
  # GitHub step summary when running in Actions; a no-op elsewhere.
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then printf '%s\n' "$@" >>"$GITHUB_STEP_SUMMARY"; fi
}

package_windows() {
  local bundle="$target_dir/release/bundle/nsis" setup
  setup="$(one_file "$bundle" "ShareCtrlScreen_${version}_*-setup.exe")"
  # tauri-build copies bundle resources next to the binary; a missing copy means the
  # release overlay was not applied and the installer lacks the notices.
  [[ -f "$target_dir/release/THIRD-PARTY-LICENSES.html" ]] || {
    echo "error: THIRD-PARTY-LICENSES.html was not bundled (build without tauri.release.conf.json?)" >&2
    return 1
  }
  cp "$setup" "$dist/"
  # The bare app binary ships too, under its build name: a portable copy for people who
  # run it from a folder instead of installing (it is the same file the installer puts
  # in Program Files; it needs the WebView2 runtime, present on Windows 10/11).
  cp "$target_dir/release/sharectrl.exe" "$dist/"
  cp "$target_dir/release/sharectrl-service.exe" "$dist/"
}

package_macos() {
  local app="$target_dir/universal-apple-darwin/release/bundle/macos/ShareCtrlScreen.app"
  local dmg="$dist/ShareCtrlScreen_${version}_universal.dmg"
  [[ -d "$app" ]] || { echo "error: $app not found; run the build phase first" >&2; return 1; }

  local archs
  archs="$(lipo -archs "$app/Contents/MacOS/sharectrl")"
  echo "architectures: $archs"
  [[ "$archs" == *arm64* && "$archs" == *x86_64* ]] || {
    echo "error: the app binary is not universal (got: $archs)" >&2
    return 1
  }
  [[ -f "$app/Contents/Resources/THIRD-PARTY-LICENSES.html" ]] || {
    echo "error: THIRD-PARTY-LICENSES.html is not in the .app (build without tauri.release.conf.json?)" >&2
    return 1
  }

  # Signing policy (plans/00-DECISIONS.md #9): with the secrets, the certificate
  # signature is mandatory; macos-sign.sh exits non-zero if it cannot produce one, so a
  # broken secret fails the build instead of shipping ad-hoc. With neither secret
  # (forks, development) the app is signed ad-hoc and this says so loudly.
  local kind=certificate
  if [[ -z "${MAC_SIGN_P12_B64:-}" && -z "${MAC_SIGN_P12_PASS:-}" ]]; then
    kind=adhoc
    echo "::warning title=macOS build is ad-hoc signed::MAC_SIGN_P12_B64 / MAC_SIGN_P12_PASS are not set; macOS permissions will not survive an update of this build"
  fi
  local sign_log
  sign_log="$(mktemp)"
  "$here/macos-sign.sh" "$app" | tee "$sign_log"

  # The DMG is built here, after signing, rather than by Tauri's dmg bundler: the
  # bundler would pack the app before macos-sign.sh re-signs it.
  local stage mnt
  stage="$(mktemp -d)"
  ditto "$app" "$stage/ShareCtrlScreen.app"
  ln -s /Applications "$stage/Applications"
  hdiutil create -volname "ShareCtrlScreen" -srcfolder "$stage" -ov -format UDZO "$dmg"
  rm -rf "$stage"

  # Verify what the user gets: the app as it comes out of the DMG.
  mnt="$(mktemp -d)"
  hdiutil attach -readonly -nobrowse -mountpoint "$mnt" "$dmg" >/dev/null
  local ok=0 requirement
  if codesign --verify --strict --verbose=2 "$mnt/ShareCtrlScreen.app"; then
    # `|| …` instead of letting set -e exit: the image must still be detached below.
    requirement="$(codesign -dr - "$mnt/ShareCtrlScreen.app" 2>&1)" || {
      echo "error: codesign -dr failed on the app in the DMG: $requirement" >&2
      ok=1
    }
    if [[ "$kind" == certificate && ("$requirement" != *"certificate leaf ="* || "$requirement" == *cdhash*) ]]; then
      echo "error: the app in the DMG is not certificate-signed; refusing to ship it" >&2
      ok=1
    fi
  else
    ok=1
  fi
  hdiutil detach "$mnt" >/dev/null
  [[ $ok -eq 0 ]] || return 1

  summary "### macOS signature: $kind" '```' "$(grep -E 'designated|Signing' "$sign_log" || true)" '```'
  rm -f "$sign_log"
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then echo "mac-signing=$kind" >>"$GITHUB_OUTPUT"; fi
}

package_linux() {
  local bundle="$target_dir/release/bundle" appimage deb rpm
  appimage="$(one_file "$bundle/appimage" "ShareCtrlScreen_${version}_*.AppImage")"
  deb="$(one_file "$bundle/deb" "ShareCtrlScreen_${version}_*.deb")"
  # RPM's Version field cannot hold '-', so a pre-release version is spelled differently
  # in the file name; match on the core x.y.z only.
  rpm="$(one_file "$bundle/rpm" "ShareCtrlScreen-${version%%[-+]*}*.rpm")"

  if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --info "$deb"
    # Listing captured first: `dpkg-deb | grep -q` would let grep exit at the first match,
    # dpkg-deb die of SIGPIPE, and pipefail turn a found file into a failure.
    local listing
    listing="$(dpkg-deb --contents "$deb")"
    [[ "$listing" == *"/THIRD-PARTY-LICENSES.html"* ]] || {
      echo "error: THIRD-PARTY-LICENSES.html is not in the .deb (build without tauri.release.conf.json?)" >&2
      return 1
    }
  fi
  cp "$appimage" "$deb" "$rpm" "$dist/"
}

run_package() {
  version="${RELEASE_VERSION:-$("$here/release-version.sh")}"
  [[ -f "$notice" ]] || { echo "error: $notice missing; run the build phase first" >&2; exit 1; }
  rm -rf "$dist"
  mkdir -p "$dist"
  "package_$os"
  cp "$notice" "$dist/THIRD-PARTY-LICENSES-$os.html"
  echo "Release assets for $os ($version):"
  ls -l "$dist"
}

case "$phase" in
  steps) build_steps ;;
  build) run_build ;;
  package) run_package ;;
  all)
    run_build
    run_package
    ;;
esac
