#!/usr/bin/env bash
# seed-cache.sh — give a task worktree warm cargo caches by hard-linking the
# integration branch's artifacts instead of copying them.
#
#   bash .claude/lane/seed-cache.sh <TASK>          # from anywhere inside the worktree (Git Bash)
#   bash .claude/lane/seed-cache.sh --wsl <TASK>    # internal: the WSL half, run via wsl.exe
#
# What is linked: target/<triple>/debug/{deps,build,.fingerprint} of third-party
# crates. Hard links cost no space, and cargo never rewrites those files: a
# third-party artifact's name carries its metadata hash, so a change produces a
# new file, not an in-place write.
# What is NOT taken: `incremental/` (per-tree, large, never reusable) and every
# artifact of our own workspace crates. Their metadata hash includes the package
# path, so a worktree can never reuse them anyway; leaving them out keeps the
# base's files from being touched and the worktree's view clean.
set -euo pipefail

WORKSPACE_CRATES="capture codec codec-openh264 elevation engine input platform-api platform-linux platform-macos platform-windows protocol render signaling surface-wgpu transport gen-keymap smoketest app service wayland-subsurface"

# seed_profile <base/debug> <dest/debug>
seed_profile() {
  local base="$1" dest="$2" d name u
  [ -d "$base/deps" ] || { echo "  no cache at $base, skipping"; return 0; }
  if [ -d "$dest/deps" ]; then echo "  $dest already seeded"; return 0; fi
  mkdir -p "$dest"
  for d in deps build .fingerprint; do
    [ -d "$base/$d" ] && cp -al "$base/$d" "$dest/$d"
  done
  # Drop our own crates' entries (both name spellings: hyphen and underscore).
  for name in $WORKSPACE_CRATES; do
    u="${name//-/_}"
    find "$dest/deps" -maxdepth 1 \( -name "lib${u}-*" -o -name "${u}-*" -o -name "${name}-*" \) -exec rm -rf {} + 2>/dev/null || true
    find "$dest/.fingerprint" "$dest/build" -maxdepth 1 \( -name "${name}-*" -o -name "${u}-*" \) -exec rm -rf {} + 2>/dev/null || true
  done
  echo "  seeded $dest from $base ($(find "$dest" -type f | wc -l) linked files)"
}

if [ "${1:-}" = "--wsl" ]; then
  task="${2:?task id}"
  base="$HOME/sct/base"; dest="$HOME/sct/$task"
  echo "WSL: $base -> $dest"
  for triple in "" ; do
    seed_profile "$base/${triple:+$triple/}debug" "$dest/${triple:+$triple/}debug"
  done
  [ -f "$base/CACHEDIR.TAG" ] && cp -a "$base/CACHEDIR.TAG" "$dest/" 2>/dev/null || true
  exit 0
fi

task="${1:?usage: seed-cache.sh <TASK>}"
root="$(git rev-parse --show-toplevel)"
main="$(git worktree list --porcelain | head -1 | sed 's/^worktree //')"
if [ "$root" = "$main" ]; then echo "refusing: run this inside a task worktree, not the main checkout"; exit 1; fi
base="$main/native/target"; dest="$root/native/target"
echo "Windows: $base -> $dest"
for triple in "" aarch64-apple-darwin x86_64-apple-darwin; do
  seed_profile "$base/${triple:+$triple/}debug" "$dest/${triple:+$triple/}debug"
done
[ -f "$base/CACHEDIR.TAG" ] && cp -a "$base/CACHEDIR.TAG" "$dest/" 2>/dev/null || true

# WSL half: same script, Linux paths.
if command -v wsl.exe >/dev/null 2>&1; then
  script_win="$(cygpath -m "$0")"
  drive="$(echo "${script_win:0:1}" | tr 'A-Z' 'a-z')"
  script_wsl="/mnt/$drive/${script_win:3}"
  wsl.exe -e bash -lc "bash '$script_wsl' --wsl '$task'"
fi
echo "done"
