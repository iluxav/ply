#!/bin/bash
# ply vs Docker under load. Root: ply is rootful; Docker via its socket.
#
#   sudo bench/run.sh                 # full matrix (~38 min: two 10-minute soaks)
#   sudo QUICK=1 bench/run.sh         # ~6 min smoke of the same matrix
#   sudo RUNTIMES=docker bench/run.sh # one runtime only
#   sudo RUNTIMES=ply REF=bench/results/<stamp> PREV=bench/results/<stamp> bench/run.sh
#                                     # ply only; Docker cells borrowed from REF, before→after vs PREV
#   sudo RUNTIMES=ply PHASES=churn bench/run.sh   # one phase only (~4 min)
#
# Spec: docs/superpowers/specs/2026-09-05-ply-vs-docker-bench-design.md
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
. "$HERE/lib.sh"

PLY=${PLY:-$HERE/../target/release/ply}
OHA=${OHA:-$(command -v oha || echo "$HOME/.cargo/bin/oha")}
[ -x "$OHA" ] || OHA=/home/iluxa/.cargo/bin/oha
RUNTIMES=${RUNTIMES:-docker ply}
PHASES=${PHASES:-steady churn soak}   # e.g. PHASES=churn to re-check one phase
phase_on() { case " $PHASES " in *" $1 "*) return 0;; *) return 1;; esac; }
if [ "${QUICK:-0}" = 1 ]; then
  SECS=${SECS:-5}; WARM=${WARM:-2}; SOAK=${SOAK:-60}; CHURN=${CHURN:-20}
else
  SECS=${SECS:-15}; WARM=${WARM:-5}; SOAK=${SOAK:-600}; CHURN=${CHURN:-60}
fi
CONC=${CONC:-64}; SOAK_CONC=${SOAK_CONC:-32}; SOAK_QPS=${SOAK_QPS:-50000}; SAMPLE_EVERY=${SAMPLE_EVERY:-2}
RESULTS=${RESULTS:-$HERE/results/$(date -u +%Y%m%dT%H%M%SZ)}
mkdir -p "$RESULTS"
exec > >(tee -a "$RESULTS/run.log") 2>&1
trap 'echo "=============== RUN FAILED ===============  line $LINENO: $BASH_COMMAND (exit $?)"' ERR
LAN=$(host_lan_ip)
APP_PID=0; DB_PID=0; APP_IP=""

[ -x "$OHA" ] || { echo "error: oha not found (cargo install oha)" >&2; exit 1; }
[ -f "$HERE/ply/api/benchapi-0.1.0-linux-x64.img" ] || { echo "error: run bench/build.sh first" >&2; exit 1; }
case " $RUNTIMES " in *" ply "*) [ "$(id -u)" = 0 ] || { echo "error: the ply half needs root (sudo)" >&2; exit 1; }; [ -x "$PLY" ] || { echo "error: $PLY missing" >&2; exit 1; };; esac

{
  echo "date: $(date -u +%FT%TZ)"; echo "kernel: $(uname -r)"; echo "cpu: $(nproc) x $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs)"
  echo "mem: $(free -g | awk '/^Mem/{print $2}') GiB"; echo "ply: $("$PLY" --version 2>/dev/null) ($PLY, $(stat -c %y "$PLY" | cut -d. -f1))"
  echo "docker: $(docker --version 2>/dev/null)"; echo "oha: $("$OHA" --version)"; echo "nft: $(nft --version 2>/dev/null)"
  echo "lan ip: $LAN"; echo "secs=$SECS warm=$WARM soak=$SOAK churn=$CHURN conc=$CONC soak_conc=$SOAK_CONC soak_qps=$SOAK_QPS runtimes=$RUNTIMES phases=$PHASES"
} | tee "$RESULTS/env.txt"
cell=$((SECS + WARM + 2 * SAMPLE_EVERY + 2)); est=0
case " $RUNTIMES " in *" docker "*) est=$((est + 10 * cell + CHURN + 10 + SOAK + 90));; esac
case " $RUNTIMES " in *" ply "*)    est=$((est + 16 * cell + 2 * (CHURN + 10) + SOAK + 240));; esac
est=$(( (est + 59) / 60 ))
echo "estimated total: ~${est} min (full matrix) — long cells print a heartbeat every 30 s; the run ends with RUN COMPLETE"

cleanup() { sampler_stop; docker_down; ply_down; }
trap cleanup EXIT INT TERM

# Preflight: the bench's own leftovers (an interrupted run whose cleanup was
# itself interrupted) are removed; anything else on the port stops the run
# before it measures the wrong thing.
if docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qE '^bench-'; then
  echo "preflight: removing leftover bench-* containers from an interrupted run"; docker_down
fi
if ss -ltn 2>/dev/null | grep -qE ':18080 '; then
  echo "error: port 18080 is already in use — nothing of ours should be listening before the run:" >&2
  ss -ltnp 2>/dev/null | grep -E ':18080 ' >&2
  exit 1
fi

# churn_run RUNTIME MODE ACTION-FUNCTION — a CHURN-second /ping run over
# published-lan while ACTION runs alongside; the cell is the run's numbers.
churn_run() {
  local rt=$1 mode=$2 action=$3
  local cell="churn/$rt/published-lan/ping/$mode" ojson="$RESULTS/oha/churn_${rt}_${mode}.json"
  mkdir -p "$RESULTS/oha"; log "cell $cell (${CHURN}s, $action alongside)"
  sampler_start "$rt" "$cell" "$APP_PID" "$DB_PID"
  "$OHA" -z "${CHURN}s" -c "$CONC" --no-tui --output-format json "http://$LAN:18080/ping" > "$ojson" &
  local opid=$!
  heart_start "$cell" "$CHURN"
  sleep 5
  "$action" || echo "warning: churn action $action exited $? (the cell still records the load run)"
  wait "$opid" || echo "warning: oha exited $? on $cell (recorded as a failed cell)"
  heart_stop
  sleep "$SAMPLE_EVERY"; sampler_stop
  python3 "$HERE/cell.py" --phase churn --runtime "$rt" --path published-lan --endpoint ping --mode "$mode" \
    --cell "$cell" --conc "$CONC" --secs "$CHURN" --oha "$ojson" --samples "$RESULTS/samples.csv" | tee -a "$RESULTS/cells.jsonl"
}

# ---------------------------------------------------------------- docker -----
docker_scale_churn() {
  local i; for i in 2 3 4; do docker run -d --name bench-api-$i --network bench -e DB_ADDR=bench-pgdb:5432 benchapi:local >/dev/null; done
  sleep $((CHURN / 4)); for i in 2 3 4; do docker rm -f bench-api-$i >/dev/null; done
}
docker_suite() {
  log "docker: up"; docker_up; seed "http://127.0.0.1:18080"
  if phase_on steady; then
    steady_cells docker direct "http://$APP_IP:8080"
    steady_cells docker published-lan "http://$LAN:18080"
    steady_cells docker published-lo "http://127.0.0.1:18080"
  fi
  if phase_on churn; then
    run_cell churn docker published-lan ping keepalive-off "$CONC" "$SECS" "http://$LAN:18080/ping" --disable-keepalive
    churn_run docker scale-churn docker_scale_churn
  fi
  if phase_on soak; then
    run_soak docker "http://$LAN:18080/users/[1-9][0-9][0-9][0-9]" --rand-regex-url
  fi
  log "docker: down"; docker_down
}

# ------------------------------------------------------------------- ply -----
ply_scale_churn() { "$PLY" scale benchapi 4; sleep $((CHURN / 4)); "$PLY" scale benchapi 1; }
ply_rolling()     { "$PLY" scale benchapi 2; sleep 5; "$PLY" restart benchapi; }
ply_suite() {
  log "ply: up"; ply_pgdb_up; ply_api_up --after benchpg -e "DB_ADDR=$RELAY_DB_ADDR"; seed "http://127.0.0.1:18080"
  if phase_on steady; then
    steady_cells ply direct "http://$APP_IP:8080"
    steady_cells ply published-lan "http://$LAN:18080"
    steady_cells ply published-lo "http://127.0.0.1:18080"
    # the DB hop without the relay: talk to the pgdb instance directly
    ply_api_down; ply_api_up -e "DB_ADDR=$PGDB_IP:5432"
    run_cell steady ply published-lan read db-direct "$CONC" "$SECS" "http://$LAN:18080/users/[1-9][0-9][0-9][0-9]" --rand-regex-url
    run_cell steady ply published-lan write db-direct "$CONC" "$SECS" "http://$LAN:18080/users" -m POST -H content-type:application/json -d '{"name":"bench"}'
    # egress contract on the same workload
    local mode; for mode in audit enforce; do
      ply_api_down; ply_api_up --after benchpg -e "DB_ADDR=$RELAY_DB_ADDR" --egress "$mode"
      run_cell steady ply published-lan ping "egress-$mode" "$CONC" "$SECS" "http://$LAN:18080/ping"
      run_cell steady ply published-lan read "egress-$mode" "$CONC" "$SECS" "http://$LAN:18080/users/[1-9][0-9][0-9][0-9]" --rand-regex-url
    done
    ply_api_down; ply_api_up --after benchpg -e "DB_ADDR=$RELAY_DB_ADDR"
  fi
  if phase_on churn; then
    run_cell churn ply published-lan ping keepalive-off "$CONC" "$SECS" "http://$LAN:18080/ping" --disable-keepalive
    churn_run ply scale-churn ply_scale_churn
    churn_run ply rolling-restart ply_rolling
    "$PLY" scale benchapi 1 >/dev/null 2>&1 || true; sleep 3
  fi
  if phase_on soak; then
    run_soak ply "http://$LAN:18080/users/[1-9][0-9][0-9][0-9]" --rand-regex-url
  fi
  log "ply: down"; ply_down
}

for rt in $RUNTIMES; do "${rt}_suite"; done
trap - EXIT
log "report"
# REF=<results dir>: borrow that run's Docker cells (RUNTIMES=ply);
# PREV=<results dir>: show ply before → after against that run.
python3 "$HERE/report.py" "$RESULTS" ${REF:+--ref "$REF"} ${PREV:+--prev "$PREV"} >/dev/null
echo; echo "=============== RUN COMPLETE ==============="; echo "results: $RESULTS"; echo "report:  $RESULTS/report.md"
