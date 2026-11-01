#!/usr/bin/env bash
# Headless GNOME session for the `wayland-gnome` job in native-ci.yml (plan task T4b).
#
# Runs inside `dbus-run-session`. It starts mutter as a headless Wayland
# compositor with one virtual 1920x1080 monitor, then PipeWire, WirePlumber,
# xdg-desktop-portal-gnome and the xdg-desktop-portal frontend, and proves the
# stack: mutter's ScreenCast and RemoteDesktop services are on the bus, and the
# RemoteDesktop portal and the GNOME backend are registered.
#
# This job runs NO libei or portal capture test. Both need the RemoteDesktop /
# ScreenCast consent dialog answered: the portal has no headless pre-authorization,
# and the tests do not reuse a seeded token (portal_capture uses a fresh
# XDG_STATE_HOME, portal_libei a fresh token directory, and the runner's
# permission store is empty). portal_libei also asserts nothing about the pointer
# (it sleeps and passes), so it must not be run as a green check. Running those
# two on GNOME headlessly is a follow-up test-harness task (a consent-free path,
# or a real assertion in portal_libei); until then they are device-pass items.
#
# Documentation behind each setting:
# * mutter headless: `mutter --headless --virtual-monitor WxH --wayland --no-x11`
#   (`mutter --help`, GNOME 46): https://gitlab.gnome.org/GNOME/mutter
# * xdg-desktop-portal-gnome is chosen by `gnome-portals.conf` (lookup order in the
#   portals.conf docs): https://flatpak.github.io/xdg-desktop-portal/docs/portals.conf.html
# * RemoteDesktop version 2 (GNOME 46+) returns restore tokens:
#   https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html
#
# Usage (from the workflow): dbus-run-session -- bash .github/wayland/gnome-session.sh
# Logs go to $RUNNER_TEMP/wayland-logs (or /tmp outside Actions).
set -euo pipefail

LOG_DIR="${RUNNER_TEMP:-/tmp}/wayland-logs"
mkdir -p "$LOG_DIR"

PIDS=()
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT

die() {
  echo "wayland-gnome: $*" >&2
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

RUNTIME="$(mktemp -d)"
chmod 700 "$RUNTIME"
export XDG_RUNTIME_DIR="$RUNTIME"
export XDG_CURRENT_DESKTOP=GNOME
export XDG_SESSION_TYPE=wayland
unset WAYLAND_DISPLAY DISPLAY

# The GNOME portal backend is chosen by gnome-portals.conf for the GNOME desktop.
CONF="${XDG_CONFIG_HOME:-$HOME/.config}"
mkdir -p "$CONF/xdg-desktop-portal"
cat > "$CONF/xdg-desktop-portal/gnome-portals.conf" <<'EOF'
[preferred]
default=gnome
EOF

# --- compositor ----------------------------------------------------------------
mutter --headless --virtual-monitor 1920x1080 --wayland --no-x11 >"$LOG_DIR/mutter.log" 2>&1 &
MUTTER_PID=$!
PIDS+=("$MUTTER_PID")

SOCKET=""
for _ in $(seq 1 200); do
  SOCKET="$(find "$RUNTIME" -maxdepth 1 -name 'wayland-[0-9]*' ! -name '*.lock' -printf '%f\n' 2>/dev/null | head -n 1 || true)"
  [ -n "$SOCKET" ] && break
  kill -0 "$MUTTER_PID" 2>/dev/null || die "mutter exited early, see mutter.log"
  sleep 0.1
done
[ -n "$SOCKET" ] || die "mutter created no Wayland socket in 20 s, see mutter.log"
export WAYLAND_DISPLAY="$SOCKET"
echo "mutter is up on $WAYLAND_DISPLAY"

# Mutter's own D-Bus services that the GNOME portal backend drives.
for _ in $(seq 1 100); do
  BUS="$(busctl --user list 2>/dev/null)"
  if has "$BUS" org.gnome.Mutter.RemoteDesktop && has "$BUS" org.gnome.Mutter.ScreenCast; then
    break
  fi
  sleep 0.1
done
BUS="$(busctl --user list 2>/dev/null)"
has "$BUS" org.gnome.Mutter.RemoteDesktop \
  || die "org.gnome.Mutter.RemoteDesktop is not on the session bus, see mutter.log"
has "$BUS" org.gnome.Mutter.ScreenCast \
  || die "org.gnome.Mutter.ScreenCast is not on the session bus, see mutter.log"

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

# WirePlumber starts only after the PipeWire core answered (see sway-session.sh).
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

dbus-update-activation-environment XDG_CURRENT_DESKTOP XDG_SESSION_TYPE \
  XDG_RUNTIME_DIR WAYLAND_DISPLAY

/usr/libexec/xdg-desktop-portal-gnome >"$LOG_DIR/portal-gnome.log" 2>&1 &
PIDS+=("$!")
/usr/libexec/xdg-desktop-portal >"$LOG_DIR/portal.log" 2>&1 &
PIDS+=("$!")

PORTAL_OK=0
for _ in $(seq 1 300); do
  if has "$(busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop 2>/dev/null)" org.freedesktop.portal.RemoteDesktop \
    && has "$(busctl --user list 2>/dev/null)" org.freedesktop.impl.portal.desktop.gnome; then
    PORTAL_OK=1
    break
  fi
  sleep 0.1
done
[ "$PORTAL_OK" = 1 ] || die "RemoteDesktop portal or the GNOME backend is not on the bus, see portal*.log"
echo "portal stack is up: RemoteDesktop on the bus, GNOME backend registered"
busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
  | grep -E 'ScreenCast|RemoteDesktop' || true

echo "wayland-gnome: stack proven. The libei and portal capture tests are not run here (see gnome-session.sh)."
