---
title: Autoscaling
description: The run parent grows and shrinks the instance count — and each instance's CPU and memory limits — from a policy in the manifest. No daemon, no orchestrator.
section: Guides
order: 14.9
---

# Autoscaling

Docker cannot scale on its own; people reach for an orchestrator for that.
ply already has a controller per app — the run parent, which owns the
instance count, the health gate, the published pool and the kernel's
routing table — so autoscaling is a policy it evaluates, not a new process.

```toml
[scale]
min = 2
max = 8
signal = "cpu"          # cpu | memory | net | metric:<name>
target = "70%"          # per instance, averaged over the last 30 s
cooldown = "60s"        # at most one step per cooldown (default)

[resources]
mem = { min = "256M", max = "2G" }   # resized live between these
cpu = { min = "0.5", max = "4" }
```

## Horizontal: the instance count

Every five seconds the parent samples each instance and, when the window
of the last 30 seconds says so, changes the count:

- **up**, when the average is more than 10 % over target, straight to
  `ceil(current × average / target)`, capped at `max`;
- **down**, one instance at a time, only when the average is under 70 % of
  the target — a noisy signal cannot flap the pool;
- at most one step per `cooldown`; instances younger than 30 s are warming
  up and do not vote.

A new instance takes traffic only once its published port accepts a
connection; a retiring one leaves the pool first and gets a second to
finish (see [Deploys](/docs/deploy/)). `--scale N` and a stack member's
`scale = N` set the starting count, clamped into `[min, max]`.

**Connections stick to their instance.** Published ports balance new
connections; a client holding a keep-alive connection stays where it
landed. So a new instance takes load only as connections turn over — many
clients, idle timeouts, an edge in front all do that; one benchmark tool
holding sixteen connections forever does not. The policy knows: it never
scales down while any instance is above target on its own, even when the
average is low, so an idle newcomer is not removed while its neighbour is
saturated.

### Signals

| `signal` | measured as | `target` reads like |
|---|---|---|
| `cpu` | % of the instance's CPU limit (of one core when unlimited), from its cgroup | `"70%"` |
| `memory` | % of `resources.mem`, which must be set | `"80%"` |
| `net` | bytes per second in and out, from the instance's veth | `"40MB/s"` |
| `metric:NAME` | the gauge or counter `NAME` from the app's Prometheus-text endpoint, `GET /metrics` on the first published port (`metrics_path` changes the path); labels are summed | `"100"` |

CPU, memory and net cost nothing and need nothing from the app. A custom
metric is one HTTP request per instance per tick; any OpenTelemetry SDK can
expose one through its Prometheus exporter.

**Pick a metric that means something over seconds.** The parent reads it
once per tick and averages six ticks. A queue depth, a rate the app keeps
itself, a p99 it tracks — those scale well. An instantaneous gauge of a
millisecond-scale quantity (requests inside a 3 ms handler right now) is
mostly noise at a 5 s sample and will sit on the threshold; measured live,
such a gauge read 7 on one instance and 2 on each of two, against a target
of 4.

### The operator's hand

`ply scale APP N` on an autoscaled app **pins** it at N and pauses the
policy:

```
ply: scale pinned at 5 by operator (autoscale paused; `ply scale web auto` resumes)
```

`ply scale APP auto` hands the count back. A restart of the parent starts
from `--scale` with the policy active.

## Vertical: the limits

A `[resources]` range starts at `min` and is resized live, per instance,
with cgroup writes — no restart:

- **memory** grows by half when an instance uses more than 85 % of its
  current limit, or the moment it is OOM-killed (its slot restarts with the
  larger limit); it shrinks by a quarter after a cooldown under 40 %.
- **cpu** grows by half when the cgroup reports throttling near the quota;
  shrinks the same way when idle.

Ranges work with or without a `[scale]` section. A host where cgroups are
not writable (rootless without delegation) says so once and keeps the
horizontal side.

## Evidence

Every step is a line on the parent's stderr and an entry in the events
journal (`<apps>/events.log`), with its reason:

```
ply: scale-up web 2 -> 4: cpu 84% > 70% over 30s
ply: scale-down web 4 -> 3: cpu 31% < 49% over 30s
ply: resize web.1 memory 256M -> 384M: 91% used
```

## Limits (v1)

Open connections as a signal, several rules per app, scale to zero, an OTLP
push endpoint, and anything across hosts are not here. Rootful Linux only
for sampling; the macOS backend runs the policy loop without samples.
