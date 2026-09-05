# ply vs Docker under load — design

**Purpose.** ply has never carried production load; Docker has for a decade.
This harness runs one workload matrix on both runtimes on the same box and
uses Docker as the *reference*, not the target: a ply cell within noise of
Docker is uninteresting, a cell several times worse, or one that drifts while
Docker's stays flat, is a finding. Output is a table of findings, not a
headline number.

## Where a runtime can hide cost (what the matrix isolates)

| Suspect | Mechanism in ply | Cell that exposes it |
|---|---|---|
| Published-port path | run parent relays bytes in user space: `std::io::copy`, two threads per connection (`publish.rs`) | direct instance IP vs published port; keep-alive off |
| App→DB hop | `--publish internal:` puts the same relay between app and DB | DB via parent vs DB direct (`pgdb.ply`) |
| Egress contract | nft sets touched per new connection; DNS via forwarder | off / audit / enforce on one workload |
| Runtime footprint | one parent per app vs dockerd + containerd + shim + docker-proxy | runtime CPU-seconds per run, RSS, FDs, threads |
| Time | leaks in parent, relay threads, egress thread, audit log | soak with 2 s sampling; slope of RSS/FDs |
| Churn | pool map updates, rolls | scale up/down under load; rolling deploy under load |

Not measured here: squashfs read cost at app start (a Go binary never shows
it; a Node app would — separate test), TLS, HTTP/2, multi-host.

## Workload

`bench/api`: one static Go binary, `pgx/v5`, pool of 32, three endpoints —
`GET /ping` (no DB), `GET /users/{id}` (point read, ids 1..10000),
`POST /users` (one insert). DB address from `PGDB_ADDR` (ply `--after`
injection) or `DB_ADDR` (Docker). Postgres 17 with trust auth, fresh data
dir per run, schema + 10 000 rows seeded by the harness through the API's
`POST /seed`.

Same binary in both runtimes. ply: `debian@13` base + binary. Docker:
`debian:13-slim` + binary. DB: ply `pgdb` (postgresql17 keg, the demo
manifest) vs `postgres:17` image. Both Debian-based, both 17.x; the DB build
differs slightly and is noted, not corrected.

Load generator: `oha` 1.16 on the host, `-z <secs> -c <conc>`, JSON output.
5 s warm-up run discarded before each measured run. Runs are sequential;
nothing else of ours runs meanwhile.

## Paths

- **direct** — `http://<instance ip>:8080`, bypassing both runtimes' port
  machinery (ply: bridge IP from `ply ps --json`; Docker: container IP).
- **published-lan** — `http://<host LAN ip>:18080`: ply relay; Docker kernel
  DNAT (OUTPUT-chain rule for local addresses).
- **published-lo** — `http://127.0.0.1:18080`: ply relay; Docker
  `docker-proxy` (user space) — the like-for-like userspace comparison.
- **DB path** (ply only): app→DB through `--publish internal:5432` (relay) vs
  app→`pgdb.ply:5432` (direct).

## Matrix

**Phase 1, steady state** (keep-alive on, c=64, 15 s each):
runtime {ply, docker} × path {direct, published-lan, published-lo} ×
endpoint {ping, read, write} = 18 cells; plus ply DB-direct × {read, write}
= 2; plus ply egress {audit, enforce} × published-lan × {ping, read} = 4
(egress off is the base). 24 cells ≈ 8 min with warm-ups.

**Phase 2, churn** (published-lan, /ping unless noted):
- keep-alive off, c=64, 15 s, both runtimes;
- scale churn: 60 s load while ply goes 1→4→1 (`ply scale`) / Docker starts
  and stops 3 extra containers (only the first has the port) — errors and
  p99 are the numbers;
- rolling deploy under load, ply only: `ply deploy` of the same image during
  a 60 s run; errors are the number.

**Phase 3, soak**: published-lan `/users/{id}`, c=32, `SOAK_SECONDS`
(default 600) per runtime, sampler at 2 s. `QUICK=1` sets 60.

## Sampler

Every 2 s: for the runtime's process set — ply: every `ply` process (the run
parents; instances are `server`/`postgres`), Docker: `dockerd`, `containerd`,
`containerd-shim*`, `docker-proxy` — sum of RSS, CPU seconds
(`/proc/<pid>/stat` utime+stime), open FDs, threads; plus
`nf_conntrack_count`; plus the app's and DB's own RSS. CSV
`samples.csv`: `ts,runtime,cell,rt_rss_kb,rt_cpu_s,rt_fds,rt_threads,
app_rss_kb,db_rss_kb,conntrack`.

## Outputs

`bench/results/<UTC stamp>/`: `cells.jsonl` (one object per cell: phase,
runtime, path, endpoint, mode, conc, secs, rps, p50_ms, p90_ms, p99_ms,
errors, rt_cpu_s, rt_rss_peak_kb), `samples.csv`, `env.txt` (kernel, ply
version, docker version, cpu, mem), `report.md` from `bench/report.py`.

`report.py` prints per phase a table with ply and Docker side by side and
the ratio, and **flags** a cell when rps < 0.8× Docker, p99 > 1.25× Docker,
or runtime CPU > 1.25× Docker; for the soak it fits a line to RSS and FDs
over time and flags a slope > 1 MiB/min or > 1 FD/min. Flags are the
findings list; unflagged cells are one line.

## Running

`bench/build.sh` builds the binary, the two ply images, the Docker image.
`sudo bench/run.sh` runs everything (root: ply is rootful; Docker via the
socket). Docker daemon must be up; `nft`, `oha`, `go` present. The harness
never touches the host firewall beyond what `ply run` itself does.

## Fairness rules

Same binary, same endpoints, same oha flags, same host, back to back; each
runtime torn down fully before the other starts; results are from the
measured run only; the report shows raw numbers next to every ratio.
