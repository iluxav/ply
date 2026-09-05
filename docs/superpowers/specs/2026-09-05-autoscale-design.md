# Autoscaling in the run parent — design

**Why.** Docker cannot scale on its own; people reach for Kubernetes for that.
ply already has a per-app controller — the run parent owns the instance
count, the health gate, the pool and the routing table — so autoscaling is a
policy inside it, not a new process. Single binary, no daemon, unchanged.

## Manifest

```toml
[scale]
min = 2                      # both required
max = 8
signal = "cpu"               # cpu | memory | net | metric:<name>
target = "70%"               # cpu/memory: percent of the instance's limit (cpu: of one core
                             # when no limit); net: bytes/s like "40MB/s" (rx+tx); metric: a number
cooldown = "60s"             # default 60s; minimum gap between two steps
metrics_path = "/metrics"    # metric:* only; default "/metrics"; scraped on the first
                             # published instance port, Prometheus text format

[resources]                  # vertical: a range is resized live between min and max
mem = { min = "256M", max = "2G" }     # a plain "512M" still means a fixed limit
cpu = { min = "0.5", max = "4" }
```

Validation (`ply build`/`check`/`run`): `min ≥ 1`, `max ≥ min`, `signal`
one of the four, `target` parsable for the signal, `memory` requires a
memory limit, `metric:` requires a published port. `--scale N` / stack
`scale = N` sets the starting count, clamped into `[min, max]`.

## Signals

Sampled every 5 s per instance, by the parent, from what it already has:

| signal | source | per-instance value |
|---|---|---|
| cpu | cgroup `cpu.stat usage_usec` delta | % of the instance's cpu limit, else of one core |
| memory | cgroup `memory.current` / `memory.max` | % |
| net | host-side veth `rx_bytes + tx_bytes` delta (`/sys/class/net/<veth>/statistics`) | bytes/s |
| metric:NAME | `GET http://<ip>:<port><path>`, Prometheus text; gauge or counter `NAME`, labels ignored (summed) | the number |

Instances younger than the window (30 s) are warming up and excluded. A
sample that fails (file gone, scrape error) is skipped; if no instance has
a sample the tick is a hold.

## Decision (pure, tested)

Window: the last 30 s of samples (6 ticks). `avg` = mean over instances of
each instance's window mean. Kubernetes' formula, with hysteresis:

- `desired = ceil(current × avg / target)`, clamped to `[min, max]`.
- **Up**: `desired > current` → scale to `desired` at once.
- **Down**: only when `avg < 0.7 × target`, one instance at a time.
- At most one step per `cooldown`; a step resets the window.
- Empty window (all warming up) → hold.

## Vertical (pure, tested)

Per instance, same tick, when the resource is a range:

- memory: usage > 85 % of the current limit, or `memory.events oom_kill`
  grew → `limit = min(limit × 1.5, max)`; usage < 40 % for a cooldown →
  `limit = max(limit × 0.75, min)`. An OOM-killed instance's slot restarts
  with the raised limit.
- cpu: `cpu.stat nr_throttled` grew and usage ≥ 90 % of quota → quota × 1.5
  (≤ max); usage < 40 % for a cooldown → quota × 0.75 (≥ min).
- Writes: `memory.max` (+ `memory.high` = 90 %), `cpu.max`. A failed write
  (rootless without delegation) is said once; horizontal continues.

## Operator interaction

`ply scale APP N` on an autoscaled app **pins** it at N and pauses the
policy — `ply: scale pinned at N by operator (autoscale paused; ply scale
APP auto resumes)`. `ply scale APP auto` resumes. Control command
`Scale(u32)` gains `ScaleAuto`. The pin is in the parent's memory: a
restart of the parent resumes the policy at `--scale`.

## Evidence

Every step is an event and a stderr line with its reason:
`scale-up` / `scale-down` `"2 -> 4: cpu 84% > 70% over 30s"`, `resize`
`"benchapi.1 memory 256M -> 384M: 91% used"`. Params tree live facts:
`scale/desired`, `scale/reason`, `scale/pinned`.

## Code shape

- `ply-core/src/runtime/autoscale.rs`: `Policy` (from manifest), `Sample`,
  `Window`, `decide(...) -> Step`, `resize(...) -> Vec<Resize>`, the
  Prometheus text parser — all pure.
- `ply-core/src/runtime/ns/probe.rs`: read cgroup/veth for one instance;
  `fetch_metric(ip, port, path, name)` with a minimal HTTP/1.0 GET over
  `TcpStream` (no new dependency).
- `ply-core/src/runtime/ns/cgroup.rs`: `Cgroup::open(app, n)` +
  `set_memory(bytes)`, `set_cpu(cores)`.
- `manifest.rs`: `Scale` struct; `Resources.mem/cpu` accept string or
  `{min,max}` (untagged enum); `Cgroup::create` uses `min` for a range.
- `run.rs`: the Scale command's body becomes `scale_to(target, reason)`;
  an `autoscale` tick every 5 s next to the control poll; pin state; the
  `Membership` readiness and drain from today apply unchanged.
- CLI: `ply scale APP auto`.
- bench/api: `GET /spin?ms=N` (busy loop), `GET /burn?mb=N` (allocate and
  hold), `GET /metrics` (Prometheus text with `benchapi_inflight` and
  `benchapi_burned_mb`) for the live demo.

## Non-goals (v1)

Connections as a signal (needs conntrack parsing), multiple rules per app,
scale-to-zero, fleet-level policy, OTLP push endpoint.

## Verification

Unit: policy math (up, down, hysteresis, clamp, cooldown, warm-up), target
parsing per signal, Prometheus parsing, resize math, manifest validation.
Live (owner, rootful): bench API with `[scale] min=1 max=4 signal=cpu
target=50%` under `oha /spin?ms=2` → `ply ps` reaches 4 with `scale-up`
events; load off → back to 1 one step per cooldown. `[resources] mem =
{min="64M", max="512M"}` with `/burn?mb=200` → `resize` event, no OOM kill.
`ply scale benchapi 2` under load → pinned line, no further steps;
`ply scale benchapi auto` → resumes.
