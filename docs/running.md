---
title: Running & scaling
description: Instances are processes — foreground by default, one IP each, scaled with --scale N.
section: Guides
order: 12
---

# Running & scaling

## Processes, not pets

```sh
ply run myapp.img        # an image file
ply run .                # an app dir: build if changed, then run
ply run postgres@17      # a prebuilt service from the registry
```

(The name form and the services it serves are covered in
[Databases & services](/docs/services/); this page is about what happens
once something runs.)

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

`.ply` names are **rootful only** — a rootless stack gets its own network
namespace, where members resolve each other by the same names (see
[stacks](stacks.md)).

The names stay current. Each instance's `/etc/hosts` is a bind-mounted file
that ply rewrites whenever an instance comes or goes, so an app that
restarts on a new IP is reachable by name immediately, without restarting
the apps that call it. (Earlier versions took a *copy* at launch, so callers
held the old IP until they were restarted too.)

For app-to-app traffic `--publish` + `--after` below is still the better
default when an app is scaled: a name maps to instance IPs, while the
publishing parent owns one stable address and balances across the pool
behind it.

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
ply proxy                        # every running app (caddy by default)
ply proxy myapp --format nginx   # just one, in nginx form
```

The backend it emits is the app's **published address** — its run parent —
not a list of instance IPs. The parent already balances the pool and drains
on deploy, so the emitted config survives scale, rolls and restarts without
being regenerated.

An app running rootless without `--publish` has no address a proxy can
reach: its instances share the host network and all report `127.0.0.1`.
Naming such an app is an error; a bare `ply proxy` sweep skips it with a
note and still emits everything else.

## TLS and the edge

ply terminates no TLS, issues no certificates and binds no `:443`. ACME,
SNI routing, h2/h3 and websocket upgrades are a decade of someone else's
work — the edge is Caddy (or nginx), and ply's job is to say what the
upstreams are.

### The one-command path: --domain

```sh
sudo ply setup --edge                 # once per host: Caddy + the ply-managed
                                      # config + two systemd units
ply run api.img --publish internal:3000 --domain api.example.com
```

That's HTTPS. `--domain` (repeatable) records the hostname in instance
state; the `ply-proxy` unit (`ply proxy --watch`) notices, renders the
vhost into Caddy's config, and hot-reloads — Caddy obtains the certificate
and serves `https://api.example.com`, proxying to the app's published
pool. Point the domain's DNS at the host and open 80/443 first; that's the
whole ceremony. Scale, deploys and restarts never touch the proxy config,
because the backend is the parent's stable address.

`ply systemd --domain …` bakes the same flag into a unit for production.
Everything below is the manual version of what `--edge` automates — still
fully supported, and what you want when Caddy is configured beyond ply's
defaults.

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

Rootful edge works too — the instance lives in its own netns, so the host
ports reach it through the parent's listeners (`--publish 80:80
--publish 443:443`), one extra hop each. Publishing `:80` as well as `:443`
is worth it: it gives caddy its automatic HTTP→HTTPS redirect and lets ACME
fall back to the HTTP-01 challenge instead of relying on TLS-ALPN-01 alone.

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

### More than one port

`--publish` is repeatable — each spec gets its own host listener and its own
backend pool, all fed by the same instances:

```sh
ply run edge.img --publish 80:80 --publish 443:443        # an edge needs both
ply run api.img --publish internal:3000 --publish internal:9090   # app + metrics
```

The **first** spec is the app's canonical address: what `--after` hands to
dependants and what `ply proxy` emits. Adding a metrics port second therefore
cannot silently repoint your callers.

Two specs claiming the same host port is refused up front, rather than
losing a race at bind time with a confusing "address in use".

Rootless has one limit: instances share the host netns, so ply hands each
its own loopback port as `PORT` — and `PORT` is a single variable. Only the
first spec can be satisfied that way, so `--scale N` with several published
ports warns, and the app must bind the rest itself. An edge reads its ports
from its own config rather than `PORT`, so scale-1 edges are unaffected.

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

`--after` waits; it does not wire. **Write the connection down** — the file
then says what talks to what, and nothing depends on ply and your app
having guessed the same variable name:

```sh
ply run pgdb.img --publish internal:5432 &
ply run api.img --scale 4 --publish internal:3000 --after pgdb \
    -e DATABASE_URL=postgres://app@pgdb.ply:5432/app &
ply run web.img --scale 2 --publish 8080 --after api \
    -e API_ORIGIN=http://api.ply:3000
```

`<name>.ply` resolves to the dependency's **parent**, which balances across
its instances and drains them on deploy — so `web` keeps working while
`api` rolls, and never learns an instance IP that can go stale.

### The injected variables, and why they are not the wiring

A published dependency's address also arrives as environment:
`API_ADDR` / `API_HOST` / `API_PORT`, the app name upcased with anything
non-alphanumeric becoming `_` (`api-server` → `API_SERVER_ADDR`). Handy
when an app already reads those names.

The catch is that it fails **quietly** when it does not. An app expecting
`POSTGRES_HOST` will not see `PLYBOX_DB_HOST`; it finds nothing, concludes
it has no database, and serves happily — no error, no log line, just a
feature that never works. That is a real afternoon, spent once already.

So treat the variables as a convenience, not a contract: name the address
yourself, and an explicit `[env]` or `-e` always wins over the injected
value. An unpublished dependency injects nothing rather than inventing an
address that fails further away.

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

Better: put dev behavior in a gitignorable `ply.dev.toml` next to the
manifest (entrypoint swap, extra links, env) and just `ply run .` — the
overlay applies to dir runs only, never to builds or deploys. And when the
project is several apps, one `[stack]` file runs them all: `ply up`. Both
in [Stacks & local dev](/docs/stacks/).

## Observing

```sh
ply ps                # instances: pid, ip, uptime, health, restarts
ply ps --json         # machine-readable, for scripts and CI
ply stats             # live CPU%, memory, pids, net rx/tx, throttling
ply stats myapp.2     # one instance
ply logs myapp -f     # recent output, followed (bounded ring; journald keeps history)
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
