---
title: Importing from Docker
description: The one-way ecosystem bridge — ply import docker://image, and ply bundle for airgapped hosts.
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

Use it for the off-the-shelf infrastructure pieces — databases, caches,
dashboards — while your own apps use the native manifest path.

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
| `docker compose up` | one ply.toml per app, `ply run --after db app.img` | apps find each other by `<app>.ply` names; `--after` waits for the other app's health gate |
| `docker ps` / `exec` / `stats` | same verbs | identical on purpose |
| `docker logs` | stdout / `journalctl -u ply-<app>` | apps are foreground processes; logging is the supervisor's job |
| `docker pull` | — | lockfiles fetch exact hashes on demand; `ply sync` pre-fetches a host's policy set |
| `docker push` | copy a file | any file host is a registry — GitHub Releases, a bucket, a directory |
| `docker tag` | — | versions are immutable; there is no `:latest` to move. Bump the version, rebuild |
| `docker login` | — | registries have no accounts; the lockfile's sha256 is the trust |
| `docker commit` | `ply craft` | same idea, done deliberately: session → inert, content-addressed package |
| `docker save` / `load` | `scp` | an image already is a single file |
| `docker network` | — | every instance has its own IP and `.ply` name; nothing to create |
| `docker rmi` / `system prune` | `ply gc` | reachability from lockfiles decides; `rm -rf /var/lib/ply` is the factory reset |

Typing the old habit is fine, too: `ply pull`, `ply tag`, `ply compose` and
friends answer with a one-line pointer to the row above instead of an error.

## What doesn't cross the bridge

Dockerfiles (ply builds from a manifest, not a script), docker-compose
(ply's unit is the app; wiring is `[ports]`, names, and emitted proxy
config), and Docker volumes/networks (redeclare in the manifest). The
comparison is laid out honestly in [ply vs Docker](/docs/ply-vs-docker/).
