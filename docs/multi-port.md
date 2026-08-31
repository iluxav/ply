---
title: Multi-port apps
description: One app serving two ports — HTTP + gRPC, an app plus its metrics — and where L4 stops being enough.
section: Guides
order: 12.1
---

# Multi-port apps

Most apps serve one port and none of this matters. This page is for the ones
that serve two: HTTP alongside gRPC, an API alongside a metrics or admin
endpoint.

## Three things that all mention numbers

The single biggest source of confusion here is that three unrelated
mechanisms all talk about ports. They do not interact:

| | what it is | what it does |
|---|---|---|
| `[ports]` | a **declaration** in `ply.toml` | tells ply what the app binds. ply never opens it |
| `[env] PORT` | an **environment variable** | what your app reads to decide where to listen |
| `--publish` | a **host claim** | binds a host port and splices it into the app |

`[ports] web = 3000` does **not** set `PORT`, and it does not make anything
reachable. It is metadata: ply uses it to decide whether to keep
`CAP_NET_BIND_SERVICE` (any declared port below 1024), to refuse a scale that
would collide, to tell dependants where you live, and to render `ply ps`.
Your app listens because *your app* read `PORT` — or because it has 3000
compiled in.

## Declaring two ports

```toml
[package]
name = "myapp"
version = "0.1.0"
entrypoint = ["./server"]
include = ["server"]
base = "debian@13"

[ports]
api     = 8443
grpc    = 50051

[env]
PORT      = "8443"     # whatever your app actually reads
GRPC_PORT = "50051"

[health]
port  = 8443           # the port that means "really serving"
grace = "10s"

[restart]
policy = "on-failure"
```

Pick the health port deliberately: it should be the one that goes bad when
the app goes bad. A metrics endpoint that answers while the API is wedged
will hold a broken deploy open.

## Publishing both

```sh
ply run myapp.img --publish 8443:8443 --publish 50051:50051
```

**Always use the `HOST:INSTANCE` form when you publish more than one port.**
Not `--publish 8443`. The reason is that `PORT` is a single variable, so at
most one port can be communicated to the app that way — the rest are expected
to bind the port you declared. Naming both sides tells ply you have already
settled where the app listens, and it stops trying to help.

The publish grammar is `PORT`, `HOST:INSTANCE`, or `ADDR:PORT[:INSTANCE]`,
where `ADDR` is `internal`, `public`, or an IPv4 address:

```sh
--publish 8443:8443                 # public, host 8443 → instance 8443
--publish internal:9090:9000        # other ply apps on this host only
--publish 127.0.0.1:8080:3000       # an explicit address
```

`internal` is the right scope for anything that is not meant for the
internet — metrics, admin, an internal gRPC API. It keeps the port off
`0.0.0.0` while leaving it reachable by other apps on the host.

## Publish order is load-bearing

The **first** `--publish` is the app's canonical address. It is what
`--after` dependants receive as `<APP>_ADDR` / `<APP>_HOST` / `<APP>_PORT`,
and it is the single backend `ply proxy` emits.

```sh
ply run myapp.img --publish 8443:8443 --publish 50051:50051
#                 ^^^^^^^^^^^^^^^^^^^ this one is "the" address
```

Put the port you consider primary first. Reordering the flags changes what
dependants are told and what the generated proxy config points at.

## `ply proxy` emits one vhost per app

`ply proxy` generates config for a real reverse proxy (Caddy, nginx,
haproxy) that maps **one hostname to one service**:

```caddyfile
myapp.example.com {
	reverse_proxy 127.0.0.1:8443
}
```

A second port is not a second backend for that hostname — it is a different
service, and it needs its own hostname or path. `ply proxy` does not model
that, and deliberately so: which hostname routes to which port is the one
irreducible human decision. For multi-port apps, write the edge config by
hand (see below).

## L4 vs L7: the gRPC problem

`--publish` is a **layer-4 TCP splice**. It copies bytes in both directions
with `TCP_NODELAY` set and no timeouts, and it understands nothing about
what flows through it. That is exactly what you want for gRPC streams — but
it has one consequence worth understanding before you scale.

Backend selection happens **once per TCP connection**, at accept time. The
connection then stays on that backend for its whole life. For HTTP/1.1 that
balances well, because browsers open many short connections. For gRPC it does
not balance at all: a gRPC client opens **one** long-lived HTTP/2 connection
and multiplexes every RPC over it, so one client is pinned to one instance
until it reconnects.

```sh
ply run myapp.img --publish 50051:50051 --scale 4    # 4 instances, no gRPC balancing
```

This is not specific to ply — it is what L4 means, and it is the standard
reason gRPC wants an L7 proxy. If your gRPC traffic needs to spread across a
pool, terminate it somewhere that balances per request.

## The edge, for real protocol routing

Put Caddy in front and keep the app's own ports internal:

```sh
ply run myapp.img --publish internal:8443:8443 --publish internal:50051:50051 --scale 4
```

Then, in the edge's config volume:

```caddyfile
api.example.com {
	reverse_proxy {
		dynamic a myapp.ply 8443
	}
}

grpc.example.com {
	reverse_proxy {
		dynamic a myapp.ply 50051
		transport http {
			versions h2c 2
		}
	}
}
```

Two things are doing real work here:

**`transport http { versions h2c 2 }` is required for gRPC.** Without it
Caddy speaks HTTP/1.1 to the backend and gRPC fails outright. This is the
most common mistake when putting gRPC behind Caddy.

**`dynamic a` re-resolves `<app>.ply` on every request**, and ply keeps those
host entries in step with the pool. That gives you per-request balancing —
which is precisely what the L4 splice cannot do for gRPC — and it means
scale, deploys and crash-respawns need no config edits and no reloads.

The edge keeps its Caddyfile in a **volume**, not in the image, so you edit
it in place and `--watch` hot-reloads. See `demos/edge/` for the full
pattern.

## Scaling a multi-port app

| | `--scale 1` | `--scale N` |
|---|---|---|
| rootful | fine | fine — every instance gets its own bridge IP, so all can bind both ports |
| rootless | fine | **not supported for two ports** |

All instances of one rootless `ply run` share that run's single network
namespace, so a second port collides on the second instance. ply warns when
it sees this. If you need to scale a multi-port app, run it rootful.

## Deploys cut open streams

A rolling deploy replaces instances; the splice has no drain. When an
instance goes away, an open connection through it simply ends and the client
sees EOF. Request/response traffic re-dials and never notices. **Long-lived
server-streaming RPCs will break on every deploy** — make sure your clients
reconnect (most gRPC libraries do by default).

## See also

- [Running & scaling](/docs/running/) — instances, IPs, `--scale`
- [ply.toml reference](/docs/manifest/) — every manifest key
- [Deploys, health & restarts](/docs/deploy/) — health gates and rolling deploys
