#!/bin/bash
# Live check of horizontal + vertical autoscaling (rootful):
#   sudo bench/autoscale-demo.sh
# Runs autoapi ([scale] 1..4 on cpu 50%, cooldown 20s; mem 64M..512M, cpu 0.5..2),
# pushes CPU, watches the count grow; stops, watches it shrink; pushes memory,
# watches the limit grow; pins and resumes. Prints ply's own lines and the events.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
PLY=${PLY:-$HERE/../target/release/ply}
OHA=${OHA:-/home/iluxa/.cargo/bin/oha}
PORT=18081
# IMG: which autoapi image — the cpu policy (default) or the custom-metric one
# (bench/ply/autoapi-metric: signal = "metric:benchapi_inflight", target = "4").
IMG=${IMG:-$HERE/ply/autoapi/autoapi-0.1.0-linux-x64.img}
LOG=$(mktemp)
EVENTS=/var/lib/ply/apps/events.log
[ "$(id -u)" = 0 ] || { echo "needs root (ply is rootful)"; exit 1; }
count() { "$PLY" ps --json 2>/dev/null | python3 -c 'import json,sys; print(sum(1 for s in json.load(sys.stdin) if s["app"]=="autoapi"))'; }
watch() { # label seconds
  local i=0; while [ "$i" -lt "$2" ]; do printf '   %s t+%3ds instances=%s\n' "$1" "$i" "$(count)"; sleep 5; i=$((i+5)); done
}
events_since() { grep -E 'scale-up|scale-down|resize|pinned|resumed' "$LOG" | tail -n "${1:-12}" | cut -c1-160; }

echo "== image: $IMG"
"$PLY" run "$IMG" --publish $PORT:8080 >"$LOG" 2>&1 &
PARENT=$!
trap 'kill -TERM $PARENT 2>/dev/null; wait $PARENT 2>/dev/null; echo; echo "== ply-run log =="; grep -E "autoscale|scale|resize|publishing|pinned|resumed" "$LOG"; rm -f "$LOG"' EXIT
for _ in $(seq 1 60); do curl -fsS -m 2 "http://127.0.0.1:$PORT/ping" >/dev/null 2>&1 && break; sleep 1; done
echo "== up: $(count) instance(s); policy line:"; grep -E 'autoscale' "$LOG" || true

# --disable-keepalive: real traffic turns connections over (many clients,
# idle timeouts, an edge in front). Connection-level balancing — ply's DNAT
# and a Kubernetes Service alike — only spreads NEW connections, so a
# client holding 16 connections forever would pin them all to instance 1
# and the new instances would sit idle.
echo; echo "== 1. CPU pressure for 90s (oha /spin?ms=3, c=16, connections turn over) — expect scale-up events, count → 4"
"$OHA" -z 90s -c 16 --disable-keepalive --no-tui --output-format quiet "http://127.0.0.1:$PORT/spin?ms=3" >/dev/null 2>&1 &
OPID=$!
watch "load" 90
wait "$OPID" 2>/dev/null || true
echo "-- events:"; events_since 6

echo; echo "== 2. Load off for 100s — expect scale-down one step per 20s cooldown, count → 1"
watch "idle" 100
echo "-- events:"; events_since 8

echo; echo "== 3. Memory pressure: /burn?mb=200 with a 64M..512M range — expect resize events, no OOM restart"
curl -fsS -m 10 "http://127.0.0.1:$PORT/burn?mb=200" || true
sleep 12; curl -fsS -m 5 "http://127.0.0.1:$PORT/burn?mb=200" || true
sleep 12
echo "-- events:"; events_since 6
echo "-- restarts (should be 0):"; "$PLY" ps 2>/dev/null | grep autoapi || true
curl -fsS -m 5 "http://127.0.0.1:$PORT/burn?mb=0" >/dev/null || true

echo; echo "== 4. Operator pin: ply scale autoapi 2 → pinned; load again → no scale-up; ply scale autoapi auto → resumes"
"$PLY" scale autoapi 2; sleep 6; echo "   instances=$(count)"
"$OHA" -z 40s -c 16 --disable-keepalive --no-tui --output-format quiet "http://127.0.0.1:$PORT/spin?ms=3" >/dev/null 2>&1 &
OPID=$!
watch "pinned+load" 40; wait "$OPID" 2>/dev/null || true
echo "   instances=$(count) (expect 2)"
"$PLY" scale autoapi auto; sleep 3
echo "-- last events:"; events_since 6
echo; echo "== done"
