#!/bin/bash
# Live check of scale to zero:
#   sudo bench/sleep-demo.sh            # rootful (kernel DNAT publish)
#   bench/sleep-demo.sh                 # rootless (relay publish), same steps
# Runs sleepapi ([scale] min = 0, max = 1, idle = "20s") published on $PORT,
# pings it, waits for the sleep, measures the parent at zero, times the wake,
# sleeps it by hand, deploys while asleep, and prints `ply why`.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
# Rootless needs the INSTALLED binary: the AppArmor profile grants user
# namespaces to /usr/local/bin/ply by path, and a tree build is not it.
PLY=${PLY:-$(command -v ply 2>/dev/null || echo "$HERE/../target/release/ply")}
PORT=${PORT:-18082}
IMG=${IMG:-$HERE/ply/sleepapi/sleepapi-0.1.0-linux-x64.img}
LOG=$(mktemp)
V2=$(mktemp -d)/sleepapi-v2.img
cp "$IMG" "$V2"
row() { "$PLY" ps 2>/dev/null | grep -E '^sleepapi' || echo "   (no sleepapi row)"; }
lines() { grep -E 'sleep |wake |waking|deploy|scale' "$LOG" | tail -n "${1:-6}" | cut -c1-160; }
wait_for() { # pattern seconds
  local i=0; while [ "$i" -lt "$2" ]; do grep -qE "$1" "$LOG" && return 0; sleep 1; i=$((i+1)); done; return 1
}

echo "== image: $IMG  (mode: $([ "$(id -u)" = 0 ] && echo rootful || echo rootless), ply: $PLY)"
"$PLY" run "$IMG" --publish "$PORT:8080" >"$LOG" 2>&1 &
PARENT=$!
# The trap must survive a parent that is already gone: no `set -e` inside it.
finish() { set +e; kill -TERM "$PARENT" 2>/dev/null; wait "$PARENT" 2>/dev/null; echo; echo "== ply-run log (full) =="; cat "$LOG"; rm -f "$LOG" "$V2"; }
trap finish EXIT
for _ in $(seq 1 60); do curl -fsS -m 2 "http://127.0.0.1:$PORT/ping" >/dev/null 2>&1 && break; sleep 1; done
echo "== up:"; row; grep -E 'sleeps after' "$LOG" || true

echo; echo "== 1. Hands off for 20 s + a tick — expect: sleep event, an 'asleep' row, no instance"
wait_for 'ply: sleep sleepapi' 45 || echo "   (no sleep line within 45 s)"
lines 2; row
echo "   parent pid $PARENT at zero: rss $(ps -o rss= -p "$PARENT" | tr -d ' ') KiB, $(ps -o nlwp= -p "$PARENT" | tr -d ' ') threads"
if [ "$(id -u)" = 0 ] && command -v nft >/dev/null; then
  echo "   nft chains for :$PORT (expect no dnat rule while asleep):"
  nft list table ip ply 2>/dev/null | grep -E "pub_${PORT}_" -A2 | grep -E 'chain|dnat|counter' | head -6 || true
fi

echo; echo "== 2. One request — expect: it is held, answered, and a wake event with the ms until ready"
T0=$(date +%s%N)
curl -fsS -m 70 "http://127.0.0.1:$PORT/ping" && echo "   answered in $(( ($(date +%s%N) - T0) / 1000000 )) ms (client side)"
wait_for 'ply: wake sleepapi' 10 || true
lines 3; row

echo; echo "== 3. ply scale sleepapi 0 — expect: asleep at once"
"$PLY" scale sleepapi 0; sleep 4
lines 2; row

echo; echo "== 4. ply deploy while asleep — expect: complete at once, the marker names the new image"
"$PLY" deploy "$V2" --timeout 20 || true
sleep 1; lines 2; row
echo "   next request wakes the new image:"
curl -fsS -m 70 "http://127.0.0.1:$PORT/ping" >/dev/null && sleep 2 && "$PLY" why sleepapi | sed -n 1,4p

echo; echo "== 5. ply why sleepapi (events)"
"$PLY" why sleepapi | grep -E 'sleep|wake|deploy|scale' | head -8 || true
echo; echo "== done"
