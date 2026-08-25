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
curl http://myapp.ply:3000        # rootful only
```

`.ply` names have two limits worth knowing before building on them: they
are **rootful only** (rootless instances share the host network and get no
entries), and each instance copies `/etc/hosts` at launch, so a rolling
deploy of one app leaves its callers holding the old IPs. For app-to-app
traffic prefer `--publish` + `--after` below — the publishing parent owns
one stable address and swaps the pool behind it, so nothing downstream goes
stale.

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

## TLS and the edge

ply terminates no TLS, issues no certificates and binds no `:443`. ACME,
SNI routing, h2/h3 and websocket upgrades are a decade of someone else's
work — the edge is Caddy (or nginx), and ply's job is to say what the
upstreams are.

The upstream worth pointing at is the app's **published address**, not its
instance IPs:

```caddyfile
api.example.com {
	reverse_proxy 127.0.0.1:3000     # the ply parent, not an instance
}
```

The parent already balances across the pool, skips unhealthy backends and
drains on deploy — so this line never changes when you scale, roll, crash
or restart. Point Caddy at instance IPs instead and its config has to be
regenerated on every one of those events.

### Where Caddy itself runs

Either as an ordinary system service, or as a ply app —
[`demos/edge`](https://github.com/iluxav/ply/tree/main/demos/edge) is the
second, with the Caddyfile in a volume so `caddy --watch` hot-reloads it
without a rebuild. Two things it needs before it can do TLS:

**Certificates must live on a volume.** Caddy keeps them under
`$XDG_DATA_HOME/caddy`. Point that at the instance's tmpfs and every restart
re-issues, until Let's Encrypt's duplicate-certificate limit (5 per week, no
appeal) locks the domain out.

```toml
[env]
XDG_DATA_HOME = "/data"
[volumes]
data = { path = "/data" }
```

**Something has to bind :80 and :443.** The tidiest topology is a **rootless
edge over rootful pools**: rootless shares the host network, so Caddy binds
those ports directly with no splice in front, while the pools get
per-instance IPs on the bridge and Caddy reaches them via the gateway.

```sh
sudo ply setup --unprivileged-ports        # once: lets rootless bind :80/:443
ply run edge.img                           # rootless edge
sudo ply run api.img --scale 4 --publish internal:3000
```

Rootful edge works too, but the instance lives in its own netns, so the host
ports reach it via `--publish 443:443 --publish 80:80` — one extra hop.

A system-service Caddy is the choice when TLS should keep working while ply
is stopped entirely: separate supervision, certs in `/var/lib/caddy`, and
nothing to configure beyond the emitted Caddyfile.

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

### Who can reach it

`--publish 5432` binds `0.0.0.0` — on a public host that is your database
on the internet. An address prefix says otherwise:

| spec | binds |
|---|---|
| `--publish 5432` | `0.0.0.0` (default) |
| `--publish public:5432` | `0.0.0.0`, said out loud |
| `--publish internal:5432` | loopback rootless / bridge gateway rootful |
| `--publish 127.0.0.1:8080:3000` | exactly that address |

`internal` is the one to reach for whenever the consumer is another app on
the same host — a database, a queue, an internal API. It resolves to
whatever that mode can actually reach, so the same command is correct
rootless and rootful.

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

### …and where to find it

`--after` already knows the edge, so it also answers the next question. If
the dependency is published, its address arrives as environment:

```sh
ply run pgdb.img --publish internal:5432 &
ply run api.img --scale 4 --publish internal:3000 --after pgdb &
ply run web.img --scale 2 --publish 8080 --after api
#   web sees:  API_ADDR=127.0.0.1:3000   API_HOST=127.0.0.1   API_PORT=3000
#   api sees:  PGDB_ADDR=…               PGDB_HOST=…          PGDB_PORT=…
```

That is the whole multi-service story: three apps, no stack file, no DNS,
no service registry. Each address points at the dependency's **parent**,
which balances across its instances and drains them on deploy — so `web`
keeps working while `api` rolls, and never learns an instance IP that can
go stale.

The variable name is the app name upcased, with anything non-alphanumeric
becoming `_` (`api-server` → `API_SERVER_ADDR`). ply computes the host part
per mode, so you never hardcode loopback-vs-bridge. An explicit `[env]` or
`-e` wins over the injected value, and an unpublished dependency injects
nothing rather than inventing an address that fails further away.

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

### Rootless supervision

A rootless app cannot be supervised by a system unit — that would run it as
root, which is a different mode with a different store, network and
security posture. It needs a **user** unit:

```sh
mkdir -p ~/.config/systemd/user
ply systemd --user myapp.img --scale 4 --publish internal:3000 \
  > ~/.config/systemd/user/ply-myapp.service
systemctl --user enable --now ply-myapp
sudo loginctl enable-linger $USER    # survive logout, start at boot
```

`enable-linger` is the step that is easy to miss and hard to diagnose:
without it the user manager stops at logout, taking every app with it, and
nothing starts at boot. `ply systemd --user` prints all four commands in the
unit's header comment.

For in-process restarts (crash loops with backoff) see
[`[restart]`](/docs/deploy/) — the run parent can respawn failed instances
itself, without systemd.

## Cleaning up

```sh
ply rm myapp             # stop + remove instances (volumes kept)
ply rm myapp --volumes   # …and destroy its data (always explicit)
ply gc                   # delete store entries no app references
```
