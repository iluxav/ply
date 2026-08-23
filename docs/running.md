---
title: Running & scaling
description: Instances are processes — foreground by default, one IP each, scaled with --scale N.
section: Guides
order: 12
---

# Running & scaling

## Processes, not pets

```sh
ply run myapp.img
```

The app runs in the foreground: stdout/stderr are your terminal, `SIGTERM`
and `Ctrl-C` work, the exit code propagates. Logs are stdout — pipe them
wherever you like. A tiny built-in init runs as PID 1 inside the container
so your app (PID 2) keeps normal signal semantics and zombies get reaped.

```sh
ply run --scale 3 myapp.img
```

`--scale N` starts N *identical* instances — same image, same env, same
declared port. No per-instance port injection: each instance has its own
network namespace and IP, so they can all genuinely bind port 3000.

## Networking

Each instance gets a veth pair and an IP from the `10.77.0.0/16` bridge.
ply maintains `<app>.ply` names in the host's `/etc/hosts`, mapping to
instance IPs:

```sh
curl http://myapp.ply:3000        # round-robins across instances via DNS
```

Instances reach the internet through the host: each rootful run enables
IPv4 forwarding and a source-NAT rule for `10.77.0.0/16` (nft, or iptables
where that is all the host has) and gives the instance the host's real
upstream resolvers — a `127.0.0.53` systemd-resolved stub is replaced by
what it forwards to. Rootless instances share the host network and need
none of this.

Ports in the manifest are **labels** of what the app binds internally —
not host port claims. Two apps both declaring port 3000 never conflict.

For pretty URLs, TLS, and load balancing, ply deliberately does not proxy —
it emits config for tools that already do it well:

```sh
ply proxy --backend caddy        # Caddyfile for every running app
ply lb myapp --format nginx      # one app's backend pool
```

Instances of the same app round-robin automatically in the emitted config.

## Publishing a pool

```sh
ply run --publish 3100 --scale 4 myapp.img          # rootless: just works
sudo ply run --publish 80:3000 --scale 4 myapp.img  # rootful: host:80 → instances' :3000
```

`--publish` is the explicit exception to "ports are labels": the run parent
binds the host port and L4-balances TCP connections across its instances.
It needs no discovery and no reloads — the parent forked the instances, so
the backend set follows launches, crashes, and rolling deploys by
construction; an unreachable backend is skipped per connection.

The two modes differ in who picks the instance-side port:

- **Rootless** — `--publish` makes `--scale` work without root. Instances
  share the host network, so ply gives each one its own loopback port,
  injected as `PORT` (overriding manifest/CLI values) — a bare
  `--publish 3100` always lines up, provided the app honors `PORT`.
- **Rootful** — instances bind whatever the app decides on their bridge
  IPs; tell the parent where the backends are with `HOST:INSTANCE`
  (`--publish 80:3000` for an app serving :3000), or align the app with
  `-e PORT=` to match a bare `--publish`.

The line it never crosses: TCP only. Hostnames, TLS, HTTP — that's the
edge's job (`ply proxy`); publishing is port exposure, not a proxy.

## Start order

Apps that depend on each other are still separate apps — a server and its
database each have their own manifest, image, volumes and restart policy.
`--after` orders their start without a stack file:

```sh
ply run pgdb.img &
ply run --scale 10 --after pgdb pgapp.img     # blocks until pgdb is healthy
```

"Healthy" is the app's own `[health]` gate (its port accepting a TCP
connection); an app without `[health]` only has to be running. The parent
prints what it is waiting for, `ply ps` lists it as `waiting on pgdb`, and
after `--after-timeout` (default `60s`) it gives up with a non-zero exit.
The same flag goes into a unit: `ply systemd --after pgdb pgapp.img` emits
`After=`/`Wants=ply-pgdb.service` so systemd orders the start and ply gates
on readiness.

## Environment

Composition order (last wins): package contributions → manifest `[env]` →
`-e KEY=VALUE` → `--env-file`.

```sh
ply run -e NODE_ENV=production --env-file /etc/myapp/secrets.env myapp.img
```

Never bake secrets into an image — it's a file people `scp` around. Use
`--env-file` with a root-only file at run time.

## Dev mode

```sh
ply run --link ./src:/opt/myapp myapp.img
```

Bind-mounts live code over the image's app layer — edit on the host, run in
the container, no rebuild loop.

## Observing

```sh
ply ps                # instances: pid, ip, uptime, health, restarts
ply ps --json         # machine-readable, for scripts and CI
ply stats             # live CPU%, memory, pids, net rx/tx, throttling
ply stats myapp.2     # one instance
ply exec myapp sh     # shell into instance 1 of myapp
ply exec myapp.2 sh   # a specific instance
```

`ply stats` reads cgroup v2 files and veth counters straight from the
kernel — there is no metrics agent.

## Supervision

Supervision is systemd's job. The unit's ExecStart carries your run flags:

```sh
ply systemd myapp.img --scale 4 --publish 80:3000 --env-file /etc/myapp/secrets.env \
  | sudo tee /etc/systemd/system/ply-myapp.service
sudo systemctl enable --now ply-myapp
```

For in-process restarts (crash loops with backoff) see
[`[restart]`](/docs/deploy/) — the run parent can respawn failed instances
itself, without systemd.

## Cleaning up

```sh
ply rm myapp             # stop + remove instances (volumes kept)
ply rm myapp --volumes   # …and destroy its data (always explicit)
ply gc                   # delete store entries no app references
```
