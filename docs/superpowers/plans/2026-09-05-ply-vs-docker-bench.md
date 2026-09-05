# ply vs Docker bench — plan

Spec: `docs/superpowers/specs/2026-09-05-ply-vs-docker-bench-design.md`.
All under `bench/`. Claude's shell can build everything and run the Docker
half; the ply half and the full run need the owner's `sudo bench/run.sh`.

- [x] 1. `bench/api/` Go server (+ `go test` for the handlers against a
      fake store) — `/ping`, `/users/{id}`, `POST /users`, `POST /seed`.
- [x] 2. Images: `bench/ply/api/ply.toml`, `bench/ply/pgdb/ply.toml`,
      `bench/docker/Dockerfile`, `bench/build.sh` (go build static → ply
      build both → docker build). Verify: images exist, `ply check` ok.
- [x] 3. `bench/report.py` — TDD with `bench/test_report.py` (ratios,
      flags, soak slope) on synthetic `cells.jsonl`/`samples.csv`.
- [x] 4. `bench/lib.sh` — sampler (`sample_start/stop`), `run_cell`
      (warm-up + measured oha → one JSON line), process-set discovery.
- [x] 5. `bench/run.sh` — env capture, docker phase (up, seed, phases 1–3,
      down), ply phase (same), report. `bash -n`; Docker half executed
      here end to end with `QUICK=1`.
- [x] 6. Owner runs `sudo bench/run.sh`; Claude reads `bench/results/…`,
      writes the comparison.
