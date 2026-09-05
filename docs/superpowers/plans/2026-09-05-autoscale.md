# Autoscaling — plan

Spec: `docs/superpowers/specs/2026-09-05-autoscale-design.md`. TDD; `make check` + `make check-darwin` after each task.

- [x] 1. `manifest.rs`: `[scale]` struct + validation; `Resources.mem/cpu` as string-or-range; `Cgroup::create` takes the range's `min`. Tests.
- [x] 2. `runtime/autoscale.rs`: target parsing, `Window`, `decide`, `resize`, Prometheus text parser. Tests.
- [x] 3. `ns/probe.rs` sampling (cgroup, veth, metric GET) + `Cgroup::open/set_memory/set_cpu`. Tests on the pure parts (veth name from ip, HTTP request bytes, response body split).
- [x] 4. `run.rs`: `scale_to` refactor, autoscale tick, pin/auto, events + params facts. `control::Command::ScaleAuto`; CLI `ply scale APP auto`.
- [x] 5. bench/api `/spin`, `/burn`, `/metrics`; docs (`manifest.md`, `cli.md`, new `docs/autoscale.md`, `deploy.md` pointer).
- [ ] 6. Owner: live checks per spec.
