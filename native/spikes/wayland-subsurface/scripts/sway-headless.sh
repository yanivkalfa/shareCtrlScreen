#!/bin/bash
# L5-spike verification on a headless wlroots compositor (sway), with REAL seat
# input from a virtual pointer (`drive`) and keyboard (`wtype`), and screenshots
# (`grim`). Needs: sway, grim, wtype, python3.
#
# Usage: scripts/sway-headless.sh <scale> <out-dir> [VAR=value ...]
#   CARGO_TARGET_DIR / PROFILE pick the binaries (default target/, release).
#   Extra VAR=value pairs go to the spike's environment, e.g. WGPU_BACKEND=gl.
#
# Expected log: input Button/Wheel/Key events while the video is shown, web
# `keys` stays 0, web `clicks` becomes 1 through the revealed bar, and the
# `hidden` phase has ~0 ms presents. Screenshots: 1-shown (video over the page),
# 2-revealed / 3-band (blue bar over the video), 4-unrevealed, 5-resized,
# 6-hidden (the page).
set -u
SCALE=${1:?scale}
OUT=${2:?out dir}
shift 2
TARGET=${CARGO_TARGET_DIR:-target}/${PROFILE:-release}
BIN=$TARGET/wayland-subsurface-spike
DRIVE=$TARGET/drive
mkdir -p "$OUT"; rm -f "$OUT"/*.png "$OUT"/*.log
export XDG_RUNTIME_DIR=$(mktemp -d /tmp/sct-sway-XXXXXX)
CONF=$XDG_RUNTIME_DIR/sway.conf
cat > "$CONF" <<EOF
output HEADLESS-1 resolution 1920x1200 scale $SCALE
default_border none
EOF
unset WAYLAND_DISPLAY DISPLAY
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
sway -c "$CONF" > "$OUT/sway.log" 2>&1 &
SWAY_PID=$!
trap 'kill $SWAY_PID 2>/dev/null; rm -rf "$XDG_RUNTIME_DIR"' EXIT
W=
for _ in $(seq 50); do
  W=$(ls "$XDG_RUNTIME_DIR" | grep -E '^wayland-[0-9]+$' | head -1)
  [ -n "$W" ] && break; sleep 0.1
done
export WAYLAND_DISPLAY=$W
export SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* | head -1)
LW=$(python3 -c "print(round(1920/$SCALE))"); LH=$(python3 -c "print(round(1200/$SCALE))")
env GDK_BACKEND=wayland RUST_LOG=info,wgpu_hal=warn,wgpu_core=warn "$@" \
  "$BIN" --seconds 30 --hide-at 23 --show-at 26 > "$OUT/spike.log" 2>&1 &
SPIKE=$!
for _ in $(seq 100); do grep -qs "SPIKE client_size" "$OUT/spike.log" && break; sleep 0.1; done
sleep 1.5
LINE=$(grep "layout origin" "$OUT/spike.log" | tail -1)
OX=$(echo "$LINE" | sed -E 's/.*origin=([0-9-]+),([0-9-]+).*/\1/')
OY=$(echo "$LINE" | sed -E 's/.*origin=([0-9-]+),([0-9-]+).*/\2/')
# Sway places the window by its xdg geometry, which excludes GTK's CSD shadow
# margin, and get_tree does not report that margin. The content spans the full
# window width, so its surface x IS the left margin, and GTK's tiled shadow is
# uniform: screen = window rect + (surface origin - margin).
read -r WX WY < <(swaymsg -t get_tree | python3 -c '
import json,sys
def walk(n):
    if (n.get("name") or "").startswith("ShareCtrlScreen"): return n
    for c in n.get("nodes",[])+n.get("floating_nodes",[]):
        r=walk(c)
        if r: return r
v=walk(json.load(sys.stdin))
print(v["rect"]["x"]+v["window_rect"]["x"], v["rect"]["y"]+v["window_rect"]["y"])')
CX=$WX; CY=$((WY + OY - OX))
echo "content origin: surface $OX,$OY screen $CX,$CY (layout ${LW}x${LH})"
{
  sleep 1
  echo "move $((CX + 400)) $((CY + 400))"; sleep 0.1
  echo "move $((CX + 420)) $((CY + 410))"; sleep 0.1
  echo "press 272"; sleep 0.1; echo "release 272"; sleep 0.1
  echo "press 273"; sleep 0.1; echo "release 273"; sleep 0.1
  echo "wheel 120"; sleep 0.1; echo "wheel -120"; sleep 0.3
  wtype "abc"; sleep 0.3
  # Approach the top edge of the video: the bar reveals within 2 physical px.
  for d in 30 10 5 3 1 0; do echo "move $((CX + 300)) $((CY + d))"; sleep 0.1; done
  sleep 0.4; grim "$OUT/2-revealed.png"
  # Click the web toolbar button through the revealed strip.
  echo "move $((CX + 60)) $((CY + 20))"; sleep 0.2
  echo "press 272"; sleep 0.1; echo "release 272"; sleep 0.4
  # Into the hysteresis band (still revealed), then deep (hidden again).
  echo "move $((CX + 300)) $((CY + 50))"; sleep 0.4
  grim "$OUT/3-band.png"
  echo "move $((CX + 300)) $((CY + 120))"; sleep 0.4
  grim "$OUT/4-unrevealed.png"
  # Drag with the left button held, released outside the video (implicit grab).
  echo "press 272"; sleep 0.1
  echo "move $((CX + 300)) $((CY - 30))"; sleep 0.1
  echo "release 272"; sleep 0.2
  echo "move $((CX + 500)) $((CY + 500))"; sleep 0.2
  grim "$OUT/1-shown.png"
} | "$DRIVE" "$LW" "$LH" > "$OUT/drive.log" 2>&1
swaymsg -q '[title="ShareCtrlScreen.*"] floating enable, resize set 900 560, move position 50 50'
sleep 1.5
grim "$OUT/5-resized.png"
swaymsg -q '[title="ShareCtrlScreen.*"] floating disable'
while ! grep -q "phase shown" "$OUT/spike.log"; do sleep 0.2; done
sleep 0.5
grim "$OUT/6-hidden.png"
wait $SPIKE
echo "spike exit $?"
grep -E "SPIKE|web:|input:|video surface|presenter|wayland:|ERROR|WARN|panick" "$OUT/spike.log" \
  | sed -E 's/\x1b\[[0-9;]*m//g'
