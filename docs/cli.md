---
title: CLI reference
description: Every ply command with its important flags.
section: Reference
order: 21
---

# CLI reference

`ply <command> --help` is always current; this page is the map.

## Build & validate

```sh
ply init [DIR] [-y] [--force]
```
Write a starter `ply.toml`. Detects Node/Python projects for defaults and
asks a few questions (Enter accepts the default; `-y` accepts all). Never
touches anything but `ply.toml`.

```sh
ply search QUERY [--versions] [--limit N] [--source SPEC] [--json]
```
Search a source's catalog. One line per package, paste-ready:
`ffmpeg = "6.1"   # Multimedia framework   x64 arm64`. `--versions` lists
every published version and arch. The source is `--source`, else the
`[sources] default` of `./ply.toml`, else the official registry.

```sh
ply add NAME[@RANGE] [--source NAME]
```
Add a dependency to `./ply.toml`. Without a range, takes the latest
`major.minor` from the catalog. Comments and formatting are preserved.
Then `ply build` to resolve and lock.

```sh
ply build [DIR] [-o FILE] [--insecure-source]
```
Resolve dependencies (writing `ply.lock`), produce a deterministic image
named `<name>-<version>-<os>-<arch>.img`.

```sh
ply check IMAGE [--against policy.toml]
```
Validate an image; with `--against`, check it against a host runtime policy.
Pure function — wire it into CI.

## Run & observe

```sh
ply run IMAGE [--scale N] [-e K=V]… [--env-file F] [--link HOST:CONTAINER]
             [--publish [ADDR:]PORT[:INSTANCE_PORT]]  # parent binds it, L4-balances the pool
             [--after APP]… [--after-timeout 60s]     # wait for APP, and learn its address
             [--privileged]                           # keep everything; debugging only
```
Foreground, signals work, exit code propagates.

**`--publish`** — `ADDR` is `internal`, `public` (the default) or an IPv4
address:

```sh
ply run api.img --scale 4 --publish 8080          # 0.0.0.0:8080
ply run db.img  --publish internal:5432           # only other ply apps on this host
ply run api.img --publish 127.0.0.1:8080:3000     # exactly that address
```

Reach for `internal` for databases and internal APIs — a bare
`--publish 5432` puts postgres on every interface.

**`--after`** — waits for `APP`'s `[health]` gate, then injects where to
reach it:

```sh
ply run web.img --after api --after db
#   API_ADDR=10.77.0.1:8080   API_HOST=…   API_PORT=…
#   DB_ADDR=…                 DB_HOST=…    DB_PORT=…
```

ply computes the address, so it is right rootless (loopback) and rootful
(bridge gateway) without the author guessing. An explicit `[env]` or `-e`
wins; an unpublished dependency injects nothing.

**`--privileged`** — skips rights stripping entirely: capabilities kept,
`no_new_privs` off, seccomp off. For debugging and triaging imports. Use
`[package] capabilities` for anything you intend to keep running.

```sh
ply ps [--json]
ply stats [APP|APP.N] [--json] [--sample-ms MS]
ply exec APP[.N] CMD…
```

## Lifecycle

```sh
ply deploy IMAGE [--timeout S]     # rolling deploy, health-gated (see Deploys)
ply rm APP [--volumes]             # volumes kept unless --volumes
ply gc                             # drop store entries nothing references
```

## Images

```sh
ply rebase IMAGE --runtime name@x.y.z [-o FILE]   # swap a runtime, no rebuild
ply bundle IMAGE -o FILE                          # flatten to fat mode
ply import docker://image:tag -o FILE             # OCI bridge (fat mode)
```

## Package authoring

```sh
ply craft new|shell|edit|changes|commit|ls|rm
```
Interactive package authoring — shell in, install, commit the diff as an
inert package. See [Making packages](/docs/packages/).

## Host integration

```sh
ply systemd IMAGE [--scale N] [--publish [ADDR:]P[:IP]] [-e K=V] [--env-file F]
                  [--after APP]… [--user]
                                  # emit a unit file (supervision = systemd);
                                  # --user = ~/.config/systemd/user, for rootless
ply proxy [--backend caddy]       # emit reverse-proxy config for all apps
ply lb APP [--format nginx]       # emit one app's LB backend pool
ply setup [--unprivileged-ports [PORT]]
                                  # one-time host prep (idempotent, sudo);
                                  # also reports subuid/newuidmap readiness
ply sync                          # pre-fetch the host policy's packages
```

## Fleet hygiene

```sh
ply audit                         # shared volumes, deprecated runtimes, risk surface
ply outdated                      # dependencies with newer versions available
```

## Conventions

- **`--json` everywhere it matters** — `ps`, `stats` are stable interfaces
  for scripts.
- **Foreground by default** — backgrounding is systemd's job, emitted for
  you.
- **Destructive actions are explicit** — data deletion never rides along
  (`rm` keeps volumes; `--volumes` is the separate act).
- Exit codes propagate — `ply run` in CI behaves like running the binary.
