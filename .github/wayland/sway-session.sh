#!/usr/bin/env bash
# Headless wlroots session for the `wayland-sway` job in native-ci.yml (plan task T4a).
#
# Runs inside `dbus-run-session` (the job calls it that way), so the session bus
# is private to this run. It starts, in order:
#   sway (headless, pixman, no GPU)  ->  a moving client (the "test pattern")
#   ->  pipewire (+ its core check)  ->  wireplumber (+ its liveness check)
#   ->  xdg-desktop-portal-wlr (backend)  ->  xdg-desktop-portal (frontend)
# then waits until the portal frontend exposes ScreenCast on the bus, and runs
# the Rust tests of L5 (wayland_surface) and L6 (input::tests, minus the uinput
# and xtest_ tests; see the cargo line below). L1's portal_capture is skipped
# with a warning annotation: see the comment at the test step for why.
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
#   with a `[screencast]` section (output_name, chooser_type, max_fps), `man 5
#   xdg-desktop-portal-wlr`. Its persist_mode is capped by the XDPW_PERSIST_MODE
#   environment variable (src/screencast/screencast.c, setup_target: anything
#   but "transient"/"permanent" means none). Its method handlers return -1 on
#   several paths (unsupported cursor mode, window source, no instance at Start),
#   which the frontend reports as "A backend call failed: Operation not permitted".
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

# One 1920x1080 headless output and no XWayland. The animated client is started
# by this script (below), not by `exec` here, so its stderr lands in a log file
# and it is restarted if it exits (the first CI run saw "simple-shm exiting" in
# sway.log with no reason attached).
cat > "$RUNTIME/sway.conf" <<'EOF'
output HEADLESS-1 resolution 1920x1080 scale 1
xwayland disable
default_border none
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

# swaymsg finds the IPC socket through SWAYSOCK, which sway exports only to its
# own children; export it here so the diagnostics below say something.
SWAYSOCK="$(find "$RUNTIME" -maxdepth 1 -name 'sway-ipc.*.sock' 2>/dev/null | head -n 1 || true)"
export SWAYSOCK
echo "outputs: $(swaymsg -t get_outputs -r 2>/dev/null | tr -d '\n' | head -c 400)"

# The moving client (the screencast's "test pattern"; portal_capture needs
# motion). Kept alive by a restart loop whose PID we own, with its own log, so
# an early exit is visible and does not leave a static screen.
(
  while true; do
    weston-simple-shm >>"$LOG_DIR/simple-shm.log" 2>&1 || true
    echo "simple-shm exited with $? at $(date +%T), restarting" >>"$LOG_DIR/simple-shm.log"
    sleep 0.5
  done
) &
PIDS+=("$!")
for _ in $(seq 1 50); do
  pgrep -x weston-simple-s >/dev/null && break   # comm is cut to 15 bytes
  sleep 0.1
done
pgrep -x weston-simple-s >/dev/null || die "the animated client (weston-simple-shm) did not start, see simple-shm.log"

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
  XDG_RUNTIME_DIR WAYLAND_DISPLAY XDPW_PERSIST_MODE

# Backend first, then the frontend, each started by us with --replace and at
# full log level, and each awaited through `busctl list` (name ownership only).
# The first CI run probed with `busctl introspect`, which D-Bus-ACTIVATED a
# second frontend from its .service file before ours owned the name; the copy
# that won logged to the bus daemon, not to our files, and the ScreenCast call
# failed with "Operation not permitted" (a backend handler returning -1) with
# nothing in portal-wlr.log to say why. Owning both processes and logging at
# TRACE/--verbose makes the next failure, if any, explain itself.
# XDPW_PERSIST_MODE=permanent: xdg-desktop-portal-wlr forces persist_mode to
# "none" unless this variable allows more (screencast.c, setup_target); the
# portal_capture test checks the restore-token round trip, which needs it.
export XDPW_PERSIST_MODE=permanent
/usr/libexec/xdg-desktop-portal-wlr -r -l TRACE >"$LOG_DIR/portal-wlr.log" 2>&1 &
WLR_PID=$!
PIDS+=("$WLR_PID")
WLR_OK=0
for _ in $(seq 1 150); do
  if has "$(busctl --user list 2>/dev/null)" org.freedesktop.impl.portal.desktop.wlr; then
    WLR_OK=1
    break
  fi
  kill -0 "$WLR_PID" 2>/dev/null || die "xdg-desktop-portal-wlr exited, see portal-wlr.log"
  sleep 0.1
done
[ "$WLR_OK" = 1 ] || die "the wlr backend did not own its bus name in 15 s, see portal-wlr.log"

/usr/libexec/xdg-desktop-portal --replace --verbose >"$LOG_DIR/portal.log" 2>&1 &
XDP_PID=$!
PIDS+=("$XDP_PID")
PORTAL_OK=0
for _ in $(seq 1 300); do
  if has "$(busctl --user list 2>/dev/null)" org.freedesktop.portal.Desktop \
    && has "$(busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop 2>/dev/null)" org.freedesktop.portal.ScreenCast; then
    PORTAL_OK=1
    break
  fi
  kill -0 "$XDP_PID" 2>/dev/null || die "xdg-desktop-portal exited, see portal.log"
  sleep 0.1
done
[ "$PORTAL_OK" = 1 ] || die "ScreenCast portal is not on the bus, see portal*.log"
echo "portal stack is up: ScreenCast on the bus, wlr backend registered (ours: pids $WLR_PID / $XDP_PID)"
echo "pipewire nodes: $(pw-cli ls Node 2>/dev/null | grep -c 'id ' || true)"

# --- tests ---------------------------------------------------------------------
cd "$NATIVE"
# L5: starts its own headless sway. SCT_WAYLAND_TESTS_REQUIRED=1 (set in the
# workflow) makes a missing sway a failure, not a skip.
cargo test -p platform-linux --test wayland_surface -- --nocapture

# L1 (portal_capture: real ScreenCast + PipeWire, 100 frames, restore-token
# round trip) does NOT run here, and that is a known gap, not a pass:
# xdg-desktop-portal-wlr 0.7.1 refuses every cast when the compositor advertises
# zwlr_screencopy_manager_v1 version 3 but sends no linux_dmabuf frame event
# (src/screencast/screencast.c, start_screencast: `screencast_version >= 3 &&
# screencopy_frame_info[DMABUF].format == DRM_FORMAT_INVALID` -> "unable to
# receive a valid format from wlr_screencopy", handler returns -1, the frontend
# reports "A backend call failed: Operation not permitted"). A headless pixman
# sway has no dmabuf-capable allocator, so no linux_dmabuf event ever comes
# (CI runs main #46 and #49, portal-wlr.log at TRACE). Ways to close it, both
# unverified: a virtual DRM device (`modprobe vkms`) with Mesa kms_swrast and
# WLR_RENDERER=gles2 so buffers are dmabufs, or a newer xdg-desktop-portal-wlr
# that accepts shm-only frames. Until then portal_capture is a device-pass item
# (plans/checklists/L1.md). The stack above still proves sway, PipeWire,
# WirePlumber and both portals come up and that ScreenCast is on the bus.
echo "wayland-sway: portal_capture skipped on purpose (xdg-desktop-portal-wlr needs dmabuf with screencopy v3; see the comment in this script)"
echo "::warning title=portal_capture not run::xdg-desktop-portal-wlr 0.7.1 rejects shm-only screencopy v3 frames on the headless pixman compositor; L1 portal capture is a device-pass item"

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
