---
name: ply
description: Package, run, scale, wire and deploy applications with ply — the daemonless Linux container runtime where an app is a package, dependencies are declared in ply.toml, and an image is a resolved lockfile. Use this whenever the user mentions ply, ply.toml, `ply build`/`ply run`/`ply deploy`, plybox.sh, or a .img container image; when they want to containerize an app without Docker or a Dockerfile; when they ask about running services on a single VPS or droplet without Kubernetes; or when they are wiring several services together, publishing ports, adding TLS, or importing a Docker image into ply; when they ask what a container may reach on the network (egress policy, audit, enforcement) or how to autoscale without Kubernetes. Also use it when a ply command errored and they want it diagnosed; and when operating a ply host or fleet — deploying via deployment files, checking deploy status, reading the events journal, scaling, rolling back, or diagnosing a failed build on a server.
---


# ply

ply packages an app the way Cargo or npm packages a library: a `ply.toml`
manifest declares dependencies, `ply build` resolves them into a lockfile and
writes one deterministic `.img` file, and `ply run` mounts that closure and
execs. No daemon, no Dockerfile, no registry to run.

Most mistakes come from importing Docker habits. The four that matter:

| Docker instinct | What ply does instead |
|---|---|
| write a Dockerfile | declare dependencies in `ply.toml`; there are no build steps |
| `-p 8080:80` maps a host port | `[ports]` are **labels**; host binding is `--publish` at run time |
| `docker run -d` detaches | `ply run` is foreground; supervision is systemd's job |
| containers talk over a bridge network by name | `--publish internal:PORT` + `--after` |

## Authoring ply.toml

Start from `ply init` when the directory is a real project — it detects
Node/Python and writes sensible defaults. Otherwise write it directly:

```toml
[package]
name = "myapp"                      # no "-<digit>" (filename grammar)
version = "1.2.0"
entrypoint = ["node", "server.js"]  # exec-style, no shell
include = ["dist/", "package.json"] # ONLY these ship; omit = pack everything
base = "debian@13"                  # exactly one package owns /

[dependencies]
node = "22"                         # a range: lowest satisfying version wins

[env]
NODE_ENV = "production"

[ports]
web = 3000                          # a label, NOT a host claim

[health]
port = 3000                         # gates rolling deploys
grace = "15s"

[restart]
policy = "on-failure"

[sources]
default = "https://registry.plybox.sh/ply/{package}"
```

`include` is worth setting deliberately: without it every file in the
directory ships, which usually means `node_modules`, `.git` and build caches
end up in the image.

**Check the catalog before writing a version range.** The registry is a
curated Debian-derived (glibc) catalog, and what it has for a given name
rarely matches what upstream released — asking for `node = "22"` when the registry
carries 20 and 24 produces a manifest that fails at `ply build`, after the
user has already committed it:

```sh
ply search node --versions     # what actually exists, per arch
ply add node                   # writes a range that resolves
```

Prefer `ply add` over hand-editing `[dependencies]` for exactly this reason.
When you do write a range by hand, know that resolution is Minimal Version
Selection: **`"24"` picks the lowest 24.x in the catalog, not the newest.**
That is deliberate — builds don't drift when the registry gains a version —
but it surprises anyone expecting npm or cargo caret behaviour. Pin exactly
(`"24.18.1"`) when a specific version matters.

Full field reference: `references/manifest.md`.

## Running

```sh
ply build .                         # → myapp-1.2.0-linux-x64.img + ply.lock
ply run myapp-1.2.0-linux-x64.img --scale 4 --publish 8080
```

`--publish` is the only thing that claims a host port. The run parent binds
it and balances new connections across instances — rootful Linux hands that
to the kernel (`(kernel dnat)` on the publishing line, no CPU spent in ply);
rootless, macOS and `127.0.0.1` are relayed by the parent. It forked the
instances, so the backend set follows launches, crashes and rolling deploys
with no discovery or reload; an instance joins only once its port accepts.

**Who can reach it is a decision, not a default.** `--publish 5432` binds
`0.0.0.0`, which on a public host means your database is on the internet:

```sh
ply run api.img --publish 8080                  # 0.0.0.0 — a public web port
ply run db.img  --publish internal:5432         # only ply apps on this host
ply run edge.img --publish 80:80 --publish 443:443   # repeatable
```

Reach for `internal:` whenever the consumer is another app on the same host.
It resolves to loopback rootless and the bridge gateway rootful, so the same
command is correct in both modes.

## Wiring several services together

This is where Docker habits hurt most. The compose equivalent is a
`[stack]` table in a ply.toml (registry services + local app dirs, wired
with `after` and per-member env) started by `ply up`; prebuilt services run
by name (`ply run postgres@17 -e POSTGRES_PASSWORD=dev`). Underneath both,
`--after` declares a dependency edge, waits for the dependency's `[health]`
gate, **and injects its address**:

```sh
ply run pgdb.img --publish internal:5432 &
ply run api.img --scale 4 --publish internal:3000 --after pgdb &
ply run web.img --scale 2 --publish 8080 --after api
#   web sees:  API_ADDR=…  API_HOST=…  API_PORT=…
#   api sees:  PGDB_ADDR=… PGDB_HOST=… PGDB_PORT=…
```

The variable name is the app name upcased, non-alphanumerics → `_`
(`api-server` → `API_SERVER_ADDR`). Read those from the app's config rather
than hardcoding a host — ply computes the right value per mode.

Each address points at the dependency's **parent**, which balances across its
instances and drains them on deploy. So `web` keeps serving while `api`
rolls, and never learns an instance IP that can go stale.

`<app>.ply` hostnames also exist, but prefer the above: they are rootful-only
and each instance snapshots `/etc/hosts` at launch, so a roll leaves callers
holding dead IPs.

## TLS and the public edge

ply terminates no TLS and issues no certificates — that is Caddy's job. Point
Caddy at the app's **published address**, not its instance IPs:

```caddyfile
app.example.com {
	reverse_proxy 10.77.0.1:3000
}
```

Because the parent absorbs all pool churn, that line never changes on scale,
rolls or restarts. `ply proxy [APP]... [--format caddy|nginx|haproxy]` emits
it from live state.

Caddy can be an ordinary system service or a ply app itself. If it is a ply
app, its certificate directory **must** be a volume — Caddy stores certs
under `$XDG_DATA_HOME/caddy`, and on the instance's tmpfs every restart
re-issues until Let's Encrypt's 5-per-week duplicate limit locks the domain
out. See `references/edge-tls.md` for a working manifest and the ACME
gotchas.

## Deploying to a host

An image is one file, so deploying is copying it:

```sh
scp myapp-1.2.0-linux-x64.img root@host:/srv/myapp/
ssh root@host ply deploy /srv/myapp/myapp-1.2.0-linux-x64.img
```

`ply deploy` rolls instances one at a time, each gated by `[health]`, holding
the published listener across the roll — a failed gate aborts and reverts
that slot. For reboots, emit a unit:

```sh
ply systemd myapp.img --scale 4 --publish internal:3000 \
  | sudo tee /etc/systemd/system/ply-myapp.service
sudo systemctl enable --now ply-myapp
```

Rootless apps need a **user** unit instead — `ply systemd --user` — plus
`sudo loginctl enable-linger $USER`, or everything stops at logout and
nothing starts at boot.

## Using Docker images

`ply import docker://mongo:7 -o mongo.img` pulls an OCI image,
flattens it, and translates its config (entrypoint, env, ports, workdir,
user, stop signal) into a ply manifest. Mainstream images run unmodified.

`ply run docker://redis:7-alpine` imports on demand and caches (pinned to
the first pull; `--pull` refreshes); a stack member can be
`run = "docker://…"` the same way. Reach for it when the registry lacks something — check `ply run <name>`
(prebuilt services: postgres, redis, …) first. Prefer the native package
when it exists — `redis` imports at ~14 MiB fat versus ~3 MiB as a package
that shares its base with every other app on the box. Check first with
`ply search redis`.

Imported images are marked `capabilities = "oci"` so they get Docker's
default fourteen capabilities, because their entrypoints do
`chown … && exec gosu …`. **Packages you write should keep the default of
none** — a native package never needs them, since `[package] user` drops
privileges from the parent before rights stripping. Adding capabilities to
your own manifest is nearly always a sign something else is wrong.

## Outbound policy: the egress contract

What an app may reach is declared in its manifest, enforced per instance,
and audited to a file — no sidecar, no vendor agent:

```toml
[network]
egress = ["api.stripe.com", "*.amazonaws.com", "1.1.1.1"]   # or [] = talks to nobody
```

The claim alone only audits (`ply egress APP` shows every destination with
`allowed`/`undeclared`). The operator turns it on: `ply run … --egress
enforce` or a stack member's `egress = { mode = "enforce" }`; `--egress-allow`
replaces the list. Under enforce an undeclared name is REFUSED at DNS in
milliseconds and an undeclared address is dropped; `ply egress APP
--blocked` lists both, and `egress-blocked` lands in the events journal.
Rootful Linux only (rootless prints one warning and runs unpoliced);
`enforce` refuses images that keep `CAP_NET_RAW` (imported Docker images do).

## Autoscaling

The run parent already owns the instance count, so scaling is a manifest
section, not a daemon:

```toml
[scale]
min = 2
max = 8
signal = "cpu"            # cpu | memory | net | metric:<name>  (Prometheus text on the first published port)
target = "70%"            # per instance, 30 s window; "40MB/s" for net, a number for metric

[resources]
mem = { min = "256M", max = "2G" }   # a RANGE is resized live; a plain "512M" is fixed
cpu = { min = "0.5", max = "4" }
```

Up goes straight to `ceil(current × avg / target)` when more than 10 % over;
down is one instance at a time, only when every instance is below target.
`ply scale APP N` pins an autoscaled app (policy paused); `ply scale APP
auto` resumes. Every step is an event with its reason (`scale-up 2 -> 4:
cpu 84% > 70% over 30s`). Pick custom metrics that mean something over
seconds (queue depth, rates) — an instantaneous gauge is noise at a 5 s
sample. Keep-alive connections stick to their instance: new instances take
load as connections turn over.

## Debugging

```sh
ply ps                     # instances, IPs, ports, uptime, restarts
ply stats [APP]            # live CPU/memory/pids from cgroups
ply exec APP[.N] sh        # shell inside a running instance
ply check IMAGE            # validate an image
ply audit                  # shared volumes, deprecated runtimes
```

Common failures and what they actually mean are in
`references/troubleshooting.md` — read it before guessing, especially for
`EACCES` on mount (AppArmor), `EINVAL` on chown/setuid (rootless uid map),
and `Address in use` (a stray parent still holding the port).

## Operating a host (deployments, CD, diagnosis)

On a server, apps are DEPLOYMENT FILES: write
`/var/lib/ply/deployments/<name>.toml` naming a source (`app =` registry,
`github =` release assets, `repo =` build-on-host, `image =` local file)
and `ply reconcile` — inotify + a 1-minute timer — converges to it.
Read the verdict at `deployments/.status/<name>.status`, the history at
`/var/lib/ply/apps/events.log`, logs (dead builders included) at
`/run/ply/logs/<app>.<n>.log`. Scale/restart are files under
`<apps>/<app>/control/`. Touch the spec = deploy now; pin
`version =`/`ref =` = rollback. The full file map, the diagnose loop and
the cautions (atomic writes, fleet-managed hosts, secrets) are in
`references/operating.md` — read it before operating a host.

## Reference files

- `references/manifest.md` — every `ply.toml` field
- `references/cli.md` — every command and flag
- `references/edge-tls.md` — Caddy edge, ACME, certificate persistence
- `references/troubleshooting.md` — error → cause → fix
- `references/operating.md` — running a HOST: deployments, CD, fleet, diagnosis
