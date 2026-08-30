---
title: Importing from Docker
description: The one-way ecosystem bridge — ply import docker://image runs mainstream Docker Hub images unmodified.
section: Guides
order: 17
---

# Importing from Docker

Docker's ecosystem is enormous, and ply doesn't pretend otherwise. The
bridge is one-way and explicit:

```sh
ply import docker://nginx:1.27 -o nginx.img
ply run nginx.img
```

`ply import` pulls the OCI image, flattens its layers, and emits a
**fat-mode** ply image — self-sufficient, no ply dependencies, one file.
You lose ply's shared-dependency model for that image (it's a snapshot,
not a composition), but you gain the entire Docker library instantly.

`ply run docker://image:tag` does the import on demand and caches it, so
one command works too.

Check [Databases & services](/docs/services/) first, though: the registry
publishes native runnable images for the common services (`ply run
postgres@17`), which share their base and packages with everything else on
the host — a fat import shares nothing. `docker://` is the escape hatch for
the long tail, not the primary path.

## What actually runs

Measured, not asserted. `scripts/import-compat.mjs` imports each image and
runs it; these are its numbers:

| | rootful | rootless |
|---|---|---|
| Databases — postgres, mysql, mariadb, mongo, redis, memcached, rabbitmq | 7/7 | 7/7 |
| Language runtimes — node, python, golang, ruby, php, java | 6/6 | 6/6 |
| Base images — alpine, debian, busybox | 3/3 | 3/3 |
| Web servers — nginx, httpd, caddy, traefik | 4/4 | 0/4 |
| **Total** | **21/21** | **17/21** |

The rootless web-server row is a Linux limitation, not a ply one — see
[Rootless and privileged ports](#rootless-and-privileged-ports). Reproduce
any of it:

```sh
sudo ./scripts/import-compat.mjs --run-timeout 20   # rootful
./scripts/import-compat.mjs --run-timeout 20        # rootless
```

## What import translates

An official image is not just a filesystem — its OCI config carries the
things that make it start correctly. `ply import` reads all of it into the
synthesized manifest:

| OCI config | becomes | why it matters |
|---|---|---|
| `Entrypoint` + `Cmd` | `[package] entrypoint` | what to exec |
| `Env` | `[env]` | `PATH` above all |
| `ExposedPorts` | `[ports]` | labels, not host claims |
| `WorkingDir` | `[package] workdir` | redis runs `find . -exec chown redis {} +` — from the wrong directory that walks the whole filesystem |
| `User` | `[package] user` | memcached exits rather than run as root; the name resolves against the **image's own** `/etc/passwd` |
| `StopSignal` | `[package] stop_signal` | nginx drains on `SIGQUIT`, httpd on `SIGWINCH`; `SIGTERM` just kills them |

## Capabilities: imports get Docker's set, yours get none

Official images assume Docker's posture. Their entrypoints do:

```sh
chown -R redis:redis /data && exec gosu redis redis-server
```

which needs `CAP_CHOWN` and `CAP_SETUID`/`CAP_SETGID`. ply's default is
**zero capabilities**, so that script cannot work — and it is the pattern
almost every official service image uses.

So `ply import` marks the manifest:

```toml
[package]
capabilities = "oci"
```

That grants exactly Docker's default fourteen and nothing more:
`CAP_SYS_ADMIN`, `CAP_SYS_MODULE`, `CAP_SYS_PTRACE` and `CAP_NET_ADMIN`
stay denied, as they are under Docker.

**Packages ply builds keep the empty default**, and should: a native keg
never chowns or setuids, because `[package] user` does that from the parent
*before* rights stripping. The asymmetry is the point — you are not paying
Docker's permissions for your own code. See
[Security & rootless](/docs/security/).

## Rootless

Two extra requirements, both one-time.

**A delegated uid range.** A user namespace maps exactly one id by default,
so `chown redis` and `setuid(70)` fail with `EINVAL` — which breaks every
image that drops privileges, and `[package] user` too:

```sh
sudo apt install uidmap    # newuidmap / newgidmap
sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER
ply setup                  # reports whether both are in place
```

### Rootless and privileged ports

Rootless shares the host's network namespace, and `CAP_NET_BIND_SERVICE`
inside a user namespace does not authorize binding below 1024 out there.
nginx, httpd, caddy and traefik all bind `:80` themselves, so they are
rootful-only until you lower the floor:

```sh
sudo ply setup --unprivileged-ports    # net.ipv4.ip_unprivileged_port_start = 80
```

That is host-wide — every unprivileged process gains the same right — so it
is opt-in and never applied for you. Rootless Docker and Podman have the
identical limitation. The alternative is to bind above 1024 and let the
edge proxy own `:443`.

## Fat mode in general

Any ply image can be flattened into a single self-sufficient file:

```sh
ply bundle myapp.img -o myapp-full.img
```

The bundle inlines the app *and* every dependency from its lockfile. Use
cases:

- **Airgapped / offline hosts** — one file, no fetches, no sources
- **Legacy distribution** — hand someone a single artifact with zero setup
- **Freezing** — a bundle has no external references to rot

The trade-off is size and sharing: ten bundled apps duplicate their
runtimes; ten split-mode apps share them in the store. Same file format,
same hash identity, same `ply run` — fat is a packing choice, not a
different kind of thing.

## The three image modes

| Mode | Contents | When |
|---|---|---|
| **thin** | static binary + manifest | Go / Rust / compiled apps |
| **split** | app layer + dependency references (default) | Node / Python / JVM |
| **fat** | everything flattened | imports, airgap, legacy |

## Coming from Docker: the translation table

Muscle-memory map. Where a Docker verb has no ply row, that's a design
decision, not a gap — the third column says why.

| Docker | ply | Why it's different |
|---|---|---|
| `docker build .` | `ply build .` | manifest + lockfile, not a Dockerfile script; output is a deterministic file |
| `docker search` | `ply search` | same idea; the catalog is a static file next to the images, no API |
| `docker init` | `ply init` | writes a manifest, not a Dockerfile |
| `docker run -d IMG` | `ply systemd IMG \| sudo tee …` | no daemon to detach into — supervision is systemd's job |
| `docker run -p 8080:80` | `ply run --publish 8080:80` | not a port *mapping*: the run parent load-balances the whole pool |
| `docker run -v` / `volume` | `[volumes]` in ply.toml | volumes are declared per app; plain host directories underneath |
| `docker run -e` / `--env-file` | same flags | identical on purpose |
| `docker compose up` | `ply up` — a `[stack]` in ply.toml wires the members | registry apps + local dirs, `after` waits on the health gate; members reach each other at `<name>.ply` — write the connection down rather than relying on the injected `<APP>_ADDR`; see [Stacks](/docs/stacks/) |
| `docker ps` / `exec` / `stats` | same verbs | identical on purpose |
| `docker logs` | stdout / `journalctl -u ply-<app>` | apps are foreground processes; logging is the supervisor's job |
| `docker pull` | — | lockfiles fetch exact hashes on demand; `ply sync` pre-fetches a host's policy set |
| `docker push` | copy a file | any file host is a registry — GitHub Releases, a bucket, a directory |
| `docker tag` | — | versions are immutable; there is no `:latest` to move. Bump the version, rebuild |
| `docker login` | — | registries have no accounts; the lockfile's sha256 is the trust |
| `docker commit` | `ply craft` | same idea, done deliberately: session → inert, content-addressed package |
| `docker save` / `load` | `scp` | an image already is a single file |
| `docker network` + service DNS | `--publish internal:PORT` + `--after` | the publishing parent already balances the pool; the dependant is told where it is |
| `docker rmi` / `system prune` | `ply gc` | reachability from lockfiles decides; `rm -rf /var/lib/ply` is the factory reset |

Typing the old habit is fine, too: `ply pull`, `ply tag`, `ply logs` and
friends answer with a one-line pointer to the row above instead of an error.

## What doesn't cross the bridge

Dockerfiles (ply builds from a manifest, not a script), compose files as a
format (`ply up` reads a `[stack]` in ply.toml instead), docker-compose
(ply's unit is the app; wiring is `[ports]`, names, and emitted proxy
config), and Docker volumes/networks (redeclare in the manifest). The
comparison is laid out honestly in [ply vs Docker](/docs/ply-vs-docker/).

An imported image is also **fat**: a flattened snapshot, not a composition.
`redis:7` imports at over 100 MiB where the native `redis` service image is
1.9 MiB and shares its debian base with every other app on the box. Import
is the safety net that means you never have to say "ply can't run that" —
the native path is still the one worth being on.
