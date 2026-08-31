---
title: Databases & services
description: ply run postgres@17 — prebuilt runnable services from the registry, one command, no manifest.
section: Guides
order: 12.3
---

# Databases & services

You don't write a manifest to run postgres. The registry publishes
**runnable apps** — prebuilt images with a real entrypoint, a data volume,
and a health gate — and `ply run` takes them by name:

```sh
ply run postgres@17 -e POSTGRES_PASSWORD=dev -e POSTGRES_DB=todos
ply run redis@8
```

First run fetches the image (a few KiB — the service binaries come from
shared packages, so ten services share one base); after that it starts in
tens of milliseconds. Data persists in a ply-managed volume across restarts.

## How names resolve

`ply run <name>[@version]` looks the name up in the registry's **apps**
namespace (`--source` points anywhere else, any `[sources]` spec including
`file:///`). The **newest** matching version wins — `postgres@17` means "the
latest published 17.x". That's deliberate: interactive runs want current;
locked, repeatable resolution is what `ply build` + `ply.lock` (and a
[stack's](/docs/stacks/) lock) are for.

Runnable apps live beside the package catalog, not inside it: `ply/redis`
is the inert library package your manifests depend on, `apps/redis` is the
runnable service. Asking `ply run` for a library tells you so instead of
starting nothing.

## The env contract

Services speak the same env vars as their Docker official images — every
tutorial and muscle memory transfers:

| service | vars |
|---|---|
| `postgres` | `POSTGRES_PASSWORD` (set → scram auth; unset → trust, dev only), `POSTGRES_USER` (default `postgres`), `POSTGRES_DB` (created on first boot), `PGPORT` (default 5432), `POSTGRES_LISTEN` (default `*`; use `127.0.0.1` to keep a rootless instance off the LAN) |
| `redis` | `REDIS_PASSWORD` (→ requirepass), `REDIS_PORT` (default 6379), `REDIS_BIND` (default all; `127.0.0.1` for local-only), `REDIS_ARGS` (extra `redis-server` arguments) |

Like Docker's images, init-time vars (`POSTGRES_PASSWORD`, `POSTGRES_DB`)
apply on **first boot only** — an existing data volume keeps its password.
To start over, remove the app's volume (under
`~/.local/share/ply/volumes/<app>/` rootless, `/var/lib/ply/volumes/`
rootful).

## Ports, rootless

A rootless run gets its **own** network namespace, so postgres binds 5432
in there and never fights a system postgres on the host. What can still
collide is the HOST side of `--publish`: if 5432 is taken, move that side and
leave the container's port alone — `--publish internal:5442:5432`, then
connect to 5442.

(If namespace creation fails, ply says `staying on the host network` and
falls back to sharing it — there postgres *does* bind the host directly, and
the note above about a host process shadowing your container applies.)

## Production

The same image is the production story on a droplet: give it a `[restart]`
policy ride under systemd, publish it `internal:` so only your apps reach
it, and gate dependants with `--after`:

```sh
ply systemd postgres-17.10.0-linux-x64.img -e POSTGRES_PASSWORD=… --publish internal:5432 \
  | sudo tee /etc/systemd/system/ply-postgres.service
ply run api.img --after postgres      # waits for health, learns the address
```

## The escape hatch: docker://

Anything the registry doesn't publish, Docker Hub has:

```sh
ply run docker://mongo:7 -e MONGO_INITDB_ROOT_USERNAME=root -e MONGO_INITDB_ROOT_PASSWORD=dev
```

The image is pulled once, converted to a ply image, and cached. It's a
**fat** image — self-contained, sharing nothing — which is why the native
services are the primary path and `docker://` is the fallback. See
[Importing from Docker](/docs/docker/).
