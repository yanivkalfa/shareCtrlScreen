#!/bin/bash
# L5-spike input latency on headless sway: a virtual-pointer button sent by
# `drive` → the `VideoInput::Button` the spike logs (compositor → GDK's
# connection → our queue/thread → sink). 120 events while the video presents.
# Needs: sway, python3. Usage: scripts/sway-latency.sh <scale> <out-dir>
set -u
SCALE=${1:?scale}
OUT=${2:?out dir}
TARGET=${CARGO_TARGET_DIR:-target}/${PROFILE:-release}
BIN=$TARGET/wayland-subsurface-spike
DRIVE=$TARGET/drive
mkdir -p "$OUT"; rm -f "$OUT"/*.log
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
env GDK_BACKEND=wayland RUST_LOG=info,wgpu_hal=warn,wgpu_core=warn \
  "$BIN" --seconds 20 > "$OUT/spike.log" 2>&1 &
SPIKE=$!
for _ in $(seq 100); do grep -qs "SPIKE client_size" "$OUT/spike.log" && break; sleep 0.1; done
sleep 1.5
LW=$(python3 -c "print(round(1920/$SCALE))"); LH=$(python3 -c "print(round(1200/$SCALE))")
{
  sleep 1
  echo "move $((LW / 2)) $((LH / 2))"; sleep 0.2
  for _ in $(seq 60); do echo "press 272"; sleep 0.04; echo "release 272"; sleep 0.06; done
} | "$DRIVE" "$LW" "$LH" > "$OUT/drive.log" 2>&1
wait $SPIKE
python3 - "$OUT" <<'PY'
import sys, re, datetime
out = sys.argv[1]
sent = [int(l.split()[1]) for l in open(out + "/drive.log")
        if l.startswith("sent") and ("press" in l or "release" in l)]
recv = []
for l in open(out + "/spike.log"):
    l = re.sub(r"\x1b\[[0-9;]*m", "", l)
    if "input: Button" in l:
        ts = l.split()[0].rstrip("Z")
        dt = datetime.datetime.fromisoformat(ts).replace(tzinfo=datetime.timezone.utc)
        recv.append(int(dt.timestamp() * 1e6))
n = min(len(sent), len(recv))
lat = sorted((recv[i] - sent[i]) / 1000.0 for i in range(n))
q = lambda p: lat[int((len(lat) - 1) * p)]
print(f"events sent={len(sent)} received={len(recv)}")
if lat:
    print(f"latency ms: p50={q(.5):.2f} p95={q(.95):.2f} max={lat[-1]:.2f} min={lat[0]:.2f}")
PY
grep -E "SPIKE phase" "$OUT/spike.log" | sed -E 's/\x1b\[[0-9;]*m//g'
