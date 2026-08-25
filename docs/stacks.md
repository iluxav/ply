---
title: Stacks & local dev
description: ply up starts several apps from one [stack] file — compose for ply — and ply.dev.toml overlays dev behavior without touching the image.
section: Guides
order: 12.6
---

# Stacks & local dev

A typical project is a database, a server, and maybe a web app. One file
wires them; one command runs them:

```toml
# ply.toml at the project root — pure wiring, no [package]
[stack]
db     = { run = "postgres@17", env = { POSTGRES_PASSWORD = "dev", POSTGRES_DB = "todos", PGPORT = 5442 } }
server = { path = "./server", after = "db", env = { PGPORT = 5442, PGPASSWORD = "dev" } }
web    = { path = "./web", after = "server" }
```

```sh
ply up          # everything, dependency-ordered; Ctrl-C stops it all
ply up db       # just the database (dependencies of named members come along)
```

## Members

Each member is one of the two things you already know how to run:

| member | equivalent | what `ply up` does |
|---|---|---|
| `run = "postgres@17"` | `ply run postgres@17` | fetch the prebuilt [service](/docs/services/) from the registry, cached |
| `path = "./server"` | `ply run ./server` | build that directory's own `ply.toml` — skipped entirely when nothing changed — and run the image |

`path` members keep their own manifest — the same one `ply build` and
`ply deploy` use for production. The stack file adds only wiring: there is
no second place where an app is defined, so dev and prod cannot drift.

`env` sets per-member variables. `after` orders startup on the readiness
gate (`[health]` port answering, or just running when there is none) — the
same `--after` machinery [production wiring](/docs/running/) uses, address
injection included. `after` takes a member name or an array of them; cycles
are a build error, not a hang.

## Every member is a normal app

`ply up` spawns one `ply run` parent per member — the same supervision
process systemd runs in production — and stays in the foreground. `ply ps`
shows the instances; `ply exec server sh` works; a member dying takes the
stack down in reverse order so nothing keeps serving against a dead
dependency.

## The stack lock

`run =` members resolve once and pin — reference, version, and the image
digest per arch — in the stack dir's `ply.lock`:

```toml
[stack.db]
ref = "postgres@17"
version = "17.10.0"
digest.x64 = "sha256:…"
```

A locked member starts straight from the local store: no index fetch, no
download, and the whole stack starts with the network down. Upgrades are
deliberate, as everywhere in ply:

```sh
ply up --refresh     # re-resolve run= members, re-pin
```

## ply.dev.toml — the dev overlay

Your production manifest says `node dist/index.js`. Your dev loop wants
`tsx watch` on live source. Neither belongs in the other's config — so dev
overrides live in a separate, gitignorable file next to the app's
`ply.toml`:

```toml
# server/ply.dev.toml   (add it to .gitignore)
entrypoint = ["npm", "run", "dev"]
links = ["./src:src"]

[env]
NODE_ENV = "development"
```

- **`entrypoint`** replaces the image's argv at spawn — the image itself is
  untouched.
- **`links`** are extra bind mounts. Relative host paths resolve against the
  app dir; a relative container path lands under the app's prefix
  (`src` → `/opt/server/src`). Absolute paths pass through.
- **`[env]`** merges over the manifest's; explicit `-e` flags still win.

The overlay is **runtime-only, structurally**: `ply build` never reads it,
so a shipped image cannot contain dev configuration, and editing the overlay
never triggers a rebuild. It applies on the `ply run DIR` form (and `ply up`
`path` members, which use it) — never on plain image runs or deploys. When
it applies, ply says so:

```
ply: applying ply.dev.toml (entrypoint, env(1), links(1))
```

With the overlay in place, the full dev loop is:

```sh
ply up            # db from the registry, server under tsx watch on live code
# edit server/src/*.ts — hot reload inside the container
# Ctrl-C — everything stops
```

Delete the file (or clone fresh) and the identical tree runs the production
entrypoint. Nothing to remember, nothing to ship.

## ply run DIR

The building block `ply up` uses is useful alone — cargo run for
containers:

```sh
ply run .             # build this directory (skipped if unchanged) and run it
ply run ./server      # same, from anywhere
```

## What stacks are not

A stack is a **dev-loop convenience**, not an orchestrator. In production
each member is its own systemd unit with its own deploy cadence — see
[Deploys](/docs/deploy/). And two stack members (or a member and a manual
`ply run`) that resolve to the same app name share one instance pool — app
names are per-host.
