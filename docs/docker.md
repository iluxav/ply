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

## What doesn't cross the bridge

Dockerfiles (ply builds from a manifest, not a script), docker-compose
(ply's unit is the app; wiring is `[ports]`, names, and emitted proxy
config), and Docker volumes/networks (redeclare in the manifest). The
comparison is laid out honestly in [ply vs Docker](/docs/ply-vs-docker/).
