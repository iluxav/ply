#!/bin/bash
# Shared pieces for run.sh: cells, sampling, and the two runtimes' lifecycles.
# Everything writes under $RESULTS; nothing here parses numbers — cell.py does.

log() { printf '\n== %s  [%s]\n' "$*" "$(date -u +%H:%M:%S)"; }

host_lan_ip() { ip -4 route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src") print $(i+1)}' | head -1; }

# wait_http URL [seconds]
wait_http() {
  local url=$1 secs=${2:-90} i=0
  while [ "$i" -lt "$secs" ]; do
    if curl -fsS -m 2 "$url" >/dev/null 2>&1; then return 0; fi
    sleep 1; i=$((i+1))
  done
  echo "error: $url not answering after ${secs}s" >&2
  return 1
}

# --- sampler -----------------------------------------------------------------
SAMPLER_PID=
sampler_start() { # runtime cell app_pid db_pid
  python3 "$HERE/sample.py" --runtime "$1" --cell "$2" --out "$RESULTS/samples.csv" \
    --app-pid "${3:-0}" --db-pid "${4:-0}" --interval "$SAMPLE_EVERY" &
  SAMPLER_PID=$!
}
sampler_stop() {
  heart_stop
  [ -n "$SAMPLER_PID" ] || return 0
  kill -TERM "$SAMPLER_PID" 2>/dev/null; wait "$SAMPLER_PID" 2>/dev/null || true
  SAMPLER_PID=
}

# --- heartbeat: a long cell says it is alive every 30 s --------------------------
HEART_PID=
heart_start() { # label total-seconds
  ( local n=0; while :; do sleep 30; n=$((n+30)); printf '   .. %s: %ds of %ds\n' "$1" "$n" "$2"; done ) &
  HEART_PID=$!
}
heart_stop() { [ -n "$HEART_PID" ] && { kill "$HEART_PID" 2>/dev/null; wait "$HEART_PID" 2>/dev/null || true; }; HEART_PID=; }

# --- one cell ------------------------------------------------------------------
# run_cell PHASE RUNTIME PATH ENDPOINT MODE CONC SECS URL [oha args...]
# The URL is passed to oha last; --rand-regex-url etc. go in [oha args].
run_cell() {
  local phase=$1 runtime=$2 path=$3 endpoint=$4 mode=$5 conc=$6 secs=$7 url=$8; shift 8
  local cell="$phase/$runtime/$path/$endpoint/${mode:-base}"
  local ojson="$RESULTS/oha/$(echo "$cell" | tr '/' '_').json"
  mkdir -p "$RESULTS/oha"
  log "cell $cell  (c=$conc, ${secs}s, warm ${WARM}s)"
  "$OHA" -z "${WARM}s" -c "$conc" --no-tui --output-format quiet "$@" "$url" >/dev/null 2>&1 || true
  sampler_start "$runtime" "$cell" "$APP_PID" "$DB_PID"
  sleep "$SAMPLE_EVERY"
  heart_start "$cell" "$secs"
  "$OHA" -z "${secs}s" -c "$conc" --no-tui --output-format json "$@" "$url" > "$ojson" \
    || echo "warning: oha exited $? on $cell (recorded as a failed cell)"
  heart_stop
  sleep "$SAMPLE_EVERY"
  sampler_stop
  python3 "$HERE/cell.py" --phase "$phase" --runtime "$runtime" --path "$path" --endpoint "$endpoint" \
    --mode "$mode" --cell "$cell" --conc "$conc" --secs "$secs" --oha "$ojson" \
    --samples "$RESULTS/samples.csv" | tee -a "$RESULTS/cells.jsonl"
}

# The three endpoints as (label, url-suffix, extra oha args) — read via `ep_*`.
ep_url()  { case $1 in ping) echo "$2/ping";; read) echo "$2/users/[1-9][0-9][0-9][0-9]";; write) echo "$2/users";; esac; }
ep_args() { case $1 in read) echo "--rand-regex-url";; write) echo "-m POST -H content-type:application/json -d {\"name\":\"bench\"}";; *) echo "";; esac; }

# steady_cells RUNTIME PATH BASEURL
steady_cells() {
  local rt=$1 path=$2 base=$3 ep
  for ep in ping read write; do
    # shellcheck disable=SC2046
    run_cell steady "$rt" "$path" "$ep" "" "$CONC" "$SECS" "$(ep_url "$ep" "$base")" $(ep_args "$ep")
  done
}

# run_soak RUNTIME URL [oha args...] — SOAK seconds as 60 s segments at
# SOAK_QPS: oha keeps every request's latency in memory (9.7 GB and an OOM
# kill at 256k rps for 5 min), so each segment is bounded and the soak is a
# fixed, sustained rate — drift is what it measures, not peak throughput.
run_soak() {
  local rt=$1 url=$2; shift 2
  local cell="soak/$rt/published-lan/read/base" n=$(( SOAK / 60 )); [ "$n" -lt 1 ] && n=1
  local seg=$(( SOAK / n )) i files=""
  mkdir -p "$RESULTS/oha"
  log "cell $cell  (c=$SOAK_CONC, q=$SOAK_QPS, ${n} x ${seg}s)"
  "$OHA" -z "${WARM}s" -c "$SOAK_CONC" -q "$SOAK_QPS" --no-tui --output-format quiet "$@" "$url" >/dev/null 2>&1 || true
  sampler_start "$rt" "$cell" "$APP_PID" "$DB_PID"
  sleep "$SAMPLE_EVERY"
  for i in $(seq 1 "$n"); do
    local f="$RESULTS/oha/soak_${rt}_seg$(printf %02d "$i").json"
    "$OHA" -z "${seg}s" -c "$SOAK_CONC" -q "$SOAK_QPS" --no-tui --output-format json "$@" "$url" > "$f" \
      || echo "warning: oha exited $? on $cell segment $i"
    files="$files $f"
    printf '   .. %s: segment %d/%d done\n' "$cell" "$i" "$n"
  done
  sleep "$SAMPLE_EVERY"
  sampler_stop
  # shellcheck disable=SC2086
  python3 "$HERE/cell.py" --phase soak --runtime "$rt" --path published-lan --endpoint read --mode "" \
    --cell "$cell" --conc "$SOAK_CONC" --secs "$SOAK" --oha $files --samples "$RESULTS/samples.csv" | tee -a "$RESULTS/cells.jsonl"
}

seed() { curl -fsS -m 120 -X POST "$1/seed?n=10000" >/dev/null && echo "seeded 10000 rows"; }

# --- docker ----------------------------------------------------------------------
docker_up() {
  docker network create bench >/dev/null 2>&1 || true
  docker run -d --name bench-pgdb --network bench -e POSTGRES_HOST_AUTH_METHOD=trust postgres:17 >/dev/null
  docker run -d --name bench-api --network bench -p 18080:8080 -e DB_ADDR=bench-pgdb:5432 benchapi:local >/dev/null
  wait_http "http://127.0.0.1:18080/ping" 120
  APP_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' bench-api)
  APP_PID=$(docker inspect -f '{{.State.Pid}}' bench-api)
  DB_PID=$(docker inspect -f '{{.State.Pid}}' bench-pgdb)
  echo "docker: api $APP_IP pid $APP_PID, db pid $DB_PID"
}
docker_down() {
  docker rm -f bench-api bench-pgdb bench-api-2 bench-api-3 bench-api-4 >/dev/null 2>&1 || true
  docker network rm bench >/dev/null 2>&1 || true
}

# --- ply -------------------------------------------------------------------------
PLY_PIDS=""
ply_bg() { # run a `ply run` in the background, remember the parent; die loudly if it dies at once
  "$PLY" run "$@" >>"$RESULTS/ply-run.log" 2>&1 &
  local pid=$!
  PLY_PIDS="$PLY_PIDS $pid"
  sleep 2
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "error: ply run $* exited immediately — ply-run.log says:" >&2
    tail -3 "$RESULTS/ply-run.log" >&2
    return 1
  fi
}
ply_instance() { # APP FIELD -> value of the first instance
  "$PLY" ps --json 2>/dev/null | python3 -c '
import json,sys
app, field = sys.argv[1], sys.argv[2]
for s in json.load(sys.stdin):
    if s["app"] == app:
        print(s[field]); break' "$1" "$2"
}
ply_wait_instance() { # APP [secs]
  local i=0; while [ "$i" -lt "${2:-120}" ]; do
    [ -n "$(ply_instance "$1" ip)" ] && return 0; sleep 1; i=$((i+1)); done
  echo "error: ply app $1 never appeared in ply ps" >&2; return 1
}
# The bench database is its own package (`benchpg`), so it never shares a
# volume with the `pgdb` demo — a stale data dir there is what a run trips
# over otherwise. `internal:5432` puts the relay at the bridge gateway.
RELAY_DB_ADDR=10.77.0.1:5432
ply_pgdb_up() {
  ply_bg "$HERE/ply/pgdb/benchpg-17.10.0-linux-x64.img" --publish internal:5432
  ply_wait_instance benchpg 180
  DB_PID=$(ply_instance benchpg pid); PGDB_IP=$(ply_instance benchpg ip)
  echo "ply: benchpg $PGDB_IP pid $DB_PID (relay at $RELAY_DB_ADDR)"
}
# ply_api_up [extra ply run flags...] — the caller says where the DB is
# (DB_ADDR=$RELAY_DB_ADDR through the relay, DB_ADDR=$PGDB_IP:5432 direct).
ply_api_up() {
  ply_bg "$HERE/ply/api/benchapi-0.1.0-linux-x64.img" --publish 18080:8080 "$@"
  wait_http "http://127.0.0.1:18080/ping" 180
  ply_wait_instance benchapi 30
  APP_IP=$(ply_instance benchapi ip); APP_PID=$(ply_instance benchapi pid)
  # Something answered on 18080, but it must be OUR instance — a leftover
  # Docker container answered an entire run once.
  [ -n "$APP_IP" ] && [ -n "$APP_PID" ] || { echo "error: 18080 answers but ply ps has no benchapi instance — who owns the port? (ss -ltnp | grep 18080)" >&2; return 1; }
  echo "ply: api $APP_IP pid $APP_PID ($*)"
}
ply_api_down() {
  local pids p i=0
  pids=$(pgrep -f "ply run .*benchapi" || true)
  for p in $pids; do kill -TERM "$p" 2>/dev/null || true; done
  # The parent drains for a second, stops the instance, tears down its
  # chains, then exits and frees the port — wait for the PROCESS, not the
  # state file, or the next parent finds the port taken.
  while [ "$i" -lt 60 ]; do
    local alive=""; for p in $pids; do kill -0 "$p" 2>/dev/null && alive="$alive $p"; done
    [ -z "$alive" ] && break
    sleep 1; i=$((i+1))
  done
  [ "$i" -lt 60 ] || { echo "error: benchapi parent(s)$alive still alive after 60s" >&2; return 1; }
  sleep 1
}
ply_down() {
  local p
  for p in $PLY_PIDS; do kill -TERM "$p" 2>/dev/null || true; done
  for p in $PLY_PIDS; do wait "$p" 2>/dev/null || true; done
  PLY_PIDS=""
}
