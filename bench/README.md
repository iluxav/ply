# bench — ply vs Docker under load

Same workload, both runtimes, one box; Docker is the reference. Design and
the reasoning behind every cell:
`docs/superpowers/specs/2026-09-05-ply-vs-docker-bench-design.md`.

```sh
./build.sh                  # API binary, two ply images, Docker image (no root)
sudo ./run.sh               # full matrix, ~35 min (two 10-minute soaks)
sudo QUICK=1 ./run.sh       # ~6 min smoke of the same matrix
sudo RUNTIMES=docker ./run.sh
```

Needs: `oha` (`cargo install oha`), Docker running, `go`, the ply binary at
`../target/release/ply` (or `PLY=...`). Root is for ply's rootful mode; the
Docker half runs unprivileged too.

## What runs

- `api/` — Go, `pgx`, pool of 32: `GET /ping`, `GET /users/{id}`,
  `POST /users`, `POST /seed?n=`. One static binary in both images.
- Postgres 17: ply `pgdb` keg manifest vs the `postgres:17` image, trust auth.
- Load: `oha`, keep-alive on, c=64 for 15 s per cell after a 5 s warm-up.

| phase | cells |
|---|---|
| steady | {direct instance IP, published via LAN IP, published via 127.0.0.1} × {ping, read, write}, both runtimes; ply also: DB reached directly instead of through the `internal:` relay; egress audit and enforce |
| churn | keep-alive off; scale up/down under load; ply rolling restart under load at scale 2 |
| soak | `/users/{id}` at c=32 for 10 min per runtime, sampled every 2 s |

The sampler (`sample.py`) follows the *runtime's* processes, not the app:
ply's run parents; Docker's `dockerd`, `containerd`, shims, `docker-proxy`.
RSS, CPU seconds, FDs, threads, plus `nf_conntrack_count`.

## Output

`results/<UTC stamp>/report.md` (from `report.py`), `cells.jsonl` (one
object per cell), `samples.csv`, `oha/*.json` (raw), `env.txt`,
`ply-run.log`.

A cell is a **finding** when ply is below 0.8× the reference's rps, above
1.25× its p99 or runtime CPU, or has any errors; a ply-only variant is
judged against ply's own base cell. The soak is a finding when the
runtime's RSS grows faster than 1 MiB/min or its FD count faster than
1/min. Raw numbers sit next to every ratio.

Noise: a laptop's clocks and thermals move between runs. Compare cells
within one run; treat a single-run ratio near a threshold as "look again",
not as a verdict.
