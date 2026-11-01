#!/usr/bin/env bash
# Headless wlroots session for the `wayland-sway` job in native-ci.yml (plan task T4a).
#
# Runs inside `dbus-run-session` (the job calls it that way), so the session bus
# is private to this run. It starts, in order:
#   sway (headless, pixman, no GPU)  ->  a moving client (the "test pattern")
#   ->  pipewire (+ its core check)  ->  wireplumber (+ its liveness check)
#   ->  xdg-desktop-portal-wlr (backend)  ->  xdg-desktop-portal (frontend)
# then waits until the portal frontend exposes ScreenCast on the bus, and runs
# the Rust tests of L5 (wayland_surface), L1 (portal_capture, ignored) and L6
# (input::tests, minus the uinput and xtest_ tests; see the cargo line below).
#
# Documentation behind each setting:
# * sway headless env: WLR_BACKENDS=headless, WLR_RENDERER=pixman,
#   WLR_LIBINPUT_NO_DEVICES=1; `sway --unsupported-gpu`; `xwayland disable`:
#   `man 5 sway` and the sway wiki (headless notes).
# * LIBSEAT_BACKEND=noop: libseat(7) selects the no-op seat backend, for a
#   runner with no logind or seatd.
# * portals.conf lookup: `$XDG_CONFIG_HOME/xdg-desktop-portal/` is read first,
#   file `<desktop>-portals.conf` for each name in XDG_CURRENT_DESKTOP (lowercased),
#   then `portals.conf`; values are `;`-separated backend lists:
#   https://flatpak.github.io/xdg-desktop-portal/docs/portals.conf.html
# * xdg-desktop-portal-wlr reads `$XDG_CONFIG_HOME/xdg-desktop-portal-wlr/config`
#   with a `[screencast]` section (output_name, chooser_type, max_fps). The
#   chooser_type=none reading is from the wlr project's man page, `man 5
#   xdg-desktop-portal-wlr`; it was not fetched while writing this script, so
#   the first CI run is the check.
# * Activation environment: `dbus-update-activation-environment` pushes variables
#   into the session bus so bus-activated portal services inherit them.
#
# Usage (from the workflow): dbus-run-session -- bash .github/wayland/sway-session.sh
# Logs go to $RUNNER_TEMP/wayland-logs (or /tmp outside Actions) and are uploaded
# by the job when it fails.
set -euo pipefail

LOG_DIR="${RUNNER_TEMP:-/tmp}/wayland-logs"
mkdir -p "$LOG_DIR"
REPO="${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is set by Actions}"
NATIVE="$REPO/native"

PIDS=()
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

die() {
  echo "wayland-sway: $*" >&2
  exit 1
}

# `has <text> <needle>`: a substring test without a `| grep -q` pipe, which
# can report SIGPIPE as failure under pipefail.
has() {
  case "$1" in
    *"$2"*) return 0 ;;
  esac
  return 1
}

# A private runtime dir: sway, pipewire and the portals all live here.
RUNTIME="$(mktemp -d)"
chmod 700 "$RUNTIME"
export XDG_RUNTIME_DIR="$RUNTIME"
export XDG_CURRENT_DESKTOP=sway
export XDG_SESSION_TYPE=wayland
export WLR_BACKENDS=headless
export WLR_RENDERER=pixman
export WLR_LIBINPUT_NO_DEVICES=1
export LIBSEAT_BACKEND=noop
unset WAYLAND_DISPLAY DISPLAY

# --- configuration -----------------------------------------------------------
CONF="${XDG_CONFIG_HOME:-$HOME/.config}"
mkdir -p "$CONF/xdg-desktop-portal" "$CONF/xdg-desktop-portal-wlr"

# ScreenCast and Screenshot come from the wlr backend.
cat > "$CONF/xdg-desktop-portal/sway-portals.conf" <<'EOF'
[preferred]
default=wlr
org.freedesktop.impl.portal.ScreenCast=wlr
org.freedesktop.impl.portal.Screenshot=wlr
EOF
cp "$CONF/xdg-desktop-portal/sway-portals.conf" "$CONF/xdg-desktop-portal/portals.conf"

# Pick the headless output with no chooser, so no dialog needs an answer.
cat > "$CONF/xdg-desktop-portal-wlr/config" <<'EOF'
[screencast]
output_name=HEADLESS-1
chooser_type=none
max_fps=30
EOF

# One 1920x1080 headless output, no XWayland, and an animated client so the
# screencast has frames to deliver (portal_capture needs motion).
cat > "$RUNTIME/sway.conf" <<'EOF'
output HEADLESS-1 resolution 1920x1080 scale 1
xwayland disable
default_border none
exec weston-simple-shm
EOF

# --- compositor ----------------------------------------------------------------
sway --unsupported-gpu -c "$RUNTIME/sway.conf" >"$LOG_DIR/sway.log" 2>&1 &
SWAY_PID=$!
PIDS+=("$SWAY_PID")

SOCKET=""
for _ in $(seq 1 150); do
  SOCKET="$(find "$RUNTIME" -maxdepth 1 -name 'wayland-[0-9]*' ! -name '*.lock' -printf '%f\n' 2>/dev/null | head -n 1 || true)"
  [ -n "$SOCKET" ] && break
  kill -0 "$SWAY_PID" 2>/dev/null || die "sway exited early, see sway.log"
  sleep 0.1
done
[ -n "$SOCKET" ] || die "sway created no Wayland socket in 15 s, see sway.log"
export WAYLAND_DISPLAY="$SOCKET"
echo "sway is up on $WAYLAND_DISPLAY"

# pgrep -f: the process name (comm) is cut to 15 bytes, so -x never matches.
for _ in $(seq 1 50); do
  pgrep -f weston-simple-shm >/dev/null && break
  sleep 0.1
done
pgrep -f weston-simple-shm >/dev/null || die "the animated client (weston-simple-shm) did not start, see sway.log"

# --- PipeWire, then the session manager ----------------------------------------
pipewire >"$LOG_DIR/pipewire.log" 2>&1 &
PIPEWIRE_PID=$!
PIDS+=("$PIPEWIRE_PID")
PW_UP=0
for _ in $(seq 1 100); do
  if pw-cli info 0 >/dev/null 2>&1; then
    PW_UP=1
    break
  fi
  kill -0 "$PIPEWIRE_PID" 2>/dev/null || die "pipewire exited, see pipewire.log"
  sleep 0.1
done
[ "$PW_UP" = 1 ] || die "PipeWire core is not reachable, see pipewire.log"

# WirePlumber 0.4 exits at once if PipeWire is not up yet and does not retry,
# so it starts only after the core answered above.
wireplumber >"$LOG_DIR/wireplumber.log" 2>&1 &
WP_PID=$!
PIDS+=("$WP_PID")
WP_UP=0
for _ in $(seq 1 100); do
  if wpctl status >/dev/null 2>&1; then
    WP_UP=1
    break
  fi
  kill -0 "$WP_PID" 2>/dev/null || die "wireplumber exited, see wireplumber.log"
  sleep 0.1
done
[ "$WP_UP" = 1 ] || die "WirePlumber is not running with the PipeWire session, see wireplumber.log"
echo "pipewire and wireplumber are up"

# Export what the portals and the bus-activated services need.
dbus-update-activation-environment XDG_CURRENT_DESKTOP XDG_SESSION_TYPE \
  XDG_RUNTIME_DIR WAYLAND_DISPLAY

/usr/libexec/xdg-desktop-portal-wlr >"$LOG_DIR/portal-wlr.log" 2>&1 &
PIDS+=("$!")
/usr/libexec/xdg-desktop-portal >"$LOG_DIR/portal.log" 2>&1 &
PIDS+=("$!")

# Wait until the frontend exposes ScreenCast and the wlr backend owns its name.
PORTAL_OK=0
for _ in $(seq 1 300); do
  if has "$(busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop 2>/dev/null)" org.freedesktop.portal.ScreenCast \
    && has "$(busctl --user list 2>/dev/null)" org.freedesktop.impl.portal.desktop.wlr; then
    PORTAL_OK=1
    break
  fi
  sleep 0.1
done
[ "$PORTAL_OK" = 1 ] || die "ScreenCast portal or the wlr backend is not on the bus, see portal*.log"
echo "portal stack is up: ScreenCast on the bus, wlr backend registered"

# --- tests ---------------------------------------------------------------------
cd "$NATIVE"
# L5: starts its own headless sway. SCT_WAYLAND_TESTS_REQUIRED=1 (set in the
# workflow) makes a missing sway a failure, not a skip.
cargo test -p platform-linux --test wayland_surface -- --nocapture

# L1: real portal ScreenCast + PipeWire, 100 frames, restore-token round trip.
cargo test -p platform-linux --test portal_capture -- --ignored --nocapture

# L6, wlroots virtual input against a private headless sway (the test starts its
# own sway, as L5 does) and the in-process EIS server (no peer needed).
# Filtered here, both on purpose:
# * uinput_frames_reach_evdev needs write access to /dev/uinput and the evdev
#   handler to read events back. This job runs as an unprivileged runner user;
#   T3's x11-tests job (sudo, modprobe uinput) runs it. A filter, not a pass.
# * xtest_* need an X server (Xvfb) and SCT_X11_TESTS_REQUIRED, which this job
#   does not set; they run in T3's x11-tests job.
cargo test -p platform-linux --lib input::tests -- --nocapture \
  --skip uinput_frames_reach_evdev --skip xtest_
echo "wayland-sway: all steps passed"
