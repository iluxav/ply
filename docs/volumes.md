---
title: Volumes & data
description: The three-tier write model — read-only rootfs, ephemeral scratch, named volumes.
section: Guides
order: 13
---

# Volumes & data

An instance's filesystem has three write tiers:

1. **Rootfs** — read-only squashfs by construction; nothing can modify it
2. **Scratch** — the overlay upper layer; writable, `noexec,nosuid`, gone
   when the instance goes
3. **Named volumes** — declared in the manifest, survive everything

## Declaring volumes

```toml
[volumes]
data   = "/var/lib/myapp"                                  # per-instance (default)
shared = { path = "/srv/uploads", scope = "shared" }       # opt-in shared
cache  = { path = "/var/cache/myapp", ephemeral = true }   # GC-able
```

Volumes declare **what**, not where — ply materializes them at
`/var/lib/ply/volumes/<app>/<name>.<n>/` and bind-mounts them over the
declared path.

## Per-instance by default

Scaling to three instances gives each its own `data` volume. This is
deliberate: single-writer state (SQLite files, Postgres data dirs) can
never be silently corrupted by `--scale`. Sharing is an explicit opt-in
(`scope = "shared"`) and is surfaced by `ply audit` as risk surface.

Instance slots are stable: instance 2 of an app gets volume `.2` today,
after a restart, and after an upgrade — state follows the slot.

## Lifecycle

- Volumes **survive** stop, start, crash, upgrade, and `ply rm`
- `ply rm myapp --volumes` destroys them — data deletion is always a
  separate, explicit act
- `ephemeral = true` marks a volume as a cache: still persistent across
  restarts, but `ply gc` may reclaim it

## Ownership

If your app drops privileges, declare the runtime user and ply will create
the passwd entry and chown volumes to match:

```toml
[package]
user = "postgres:70:70"       # name:uid:gid
```

## What ply doesn't do

No volume drivers, no NFS/cloud volumes, no snapshots, no cross-host
replication. The host manages storage (mount your NFS/EBS wherever you
like); ply bind-mounts paths. Snapshots are the filesystem's job.

## Dev mode is the same mechanism

```sh
ply run --link ./src:/opt/myapp myapp.img
```

`--link` is a bind-mount like any volume — about fifty lines of shared code,
which is the point.
