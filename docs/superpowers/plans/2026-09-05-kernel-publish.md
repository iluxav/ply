# Kernel path for `--publish` — plan

Spec: `docs/superpowers/specs/2026-09-05-kernel-publish-design.md`. TDD
throughout; `make check` + `make check-darwin` after each task.

- [x] 1. `kpublish.rs` pure renderers with tests: chain names, `match_expr`
      per scope (loopback → None), `dnat_expr` for 0/1/N, `sync_script`,
      `teardown_script`, `hairpin_script`, `stale_chains`.
- [x] 2. `Pool` mirror hook (`PoolMirror` trait, `Pool::mirror`), tests with
      a recording mirror: insert/remove call sync with the current addrs.
- [x] 3. `Backend::kernel_publish`, wiring in `run.rs` (mirror on, GC at
      start, teardown at exit), `KernelPublish: PoolMirror` shelling to
      `nft`; ignored sample-script test.
- [x] 4. Drain before stop: `drain_then` + `stop_instance`; test ordering.
- [x] 5. `bench/api` graceful shutdown; docs (`running.md`, `security.md`).
- [x] 6. Owner: `sudo nft -c -f target/publish-sample.nft`, then
      `sudo bench/run.sh`; compare with `bench/RESULTS-2026-09-05.md`.
