---
title: Stacks & local dev
description: ply up starts several apps from one stack file — each [[app]] block is one ply run — and ply.dev.toml overlays dev behavior without touching the image.
section: Guides
order: 12.6
---

# Stacks & local dev

A typical project is a database, a server, and maybe a web app. One file
wires them; one command runs them. A stack file is just **several `ply run`s
written down** — each `[[app]]` block maps one-to-one to a run:

```toml
# ply.toml at the project root — pure wiring, no [package]
[stack]
name = "todos"

[[app]]
run  = "postgres@17"                       # → ply run postgres@17
name = "db"                                # → --name db
e    = ["POSTGRES_PASSWORD=$PW", "POSTGRES_DB=todos"]

[[app]]
run   = "./server"                         # → ply run ./server
after = ["db"]                             # → --after db
e     = ["PGPASSWORD=$PW"]

[[app]]
run   = "./web"
after = ["server"]
```

```sh
PW=dev ply up        # everything, dependency-ordered; Ctrl-C stops it all
PW=dev ply up db     # just the database (dependencies of named members come along)
```

Every field is a `ply run` flag: `run`→the image, `name`→`--name`, `e`→`-e`,
`after`→`--after`, `publish`→`--publish`, `volume`→`--volume`,
`domain`→`--domain`, `scale`→`--scale`. There is no stack concept beyond
"these runs, in dependency order." See the full [model](/docs/model/).

## Members

The `run` value is exactly what you'd hand `ply run` — its form decides the
source:

| `run =` | equivalent | what `ply up` does |
|---|---|---|
| `"postgres@17"` | `ply run postgres@17` | fetch the prebuilt [service](/docs/services/) from the registry, cached and lock-pinned |
| `"./server"` | `ply run ./server` | build that directory's own `ply.toml` — skipped when nothing changed — and run it |
| `"https://…/app.img"` | `ply run <url>` | fetch the image at that URL |

A directory member keeps its own manifest — the same one `ply build` and
`ply deploy` use for production. The stack file adds only wiring: there is no
second place where an app is defined, so dev and prod cannot drift.

Each member runs under its own **`--name`** (the member `name`, defaulting to
the image name). That identity is what `after`, the `<member>.ply` bridge
name, `ply ps`, and `ply exec` all key on — so two members may even run the
*same* image under different names. `after` names other members; cycles are a
build error, not a hang.

## Secrets are holes, never values

A `$VAR` in an `e` value is filled from the environment at launch — from your
shell, or from an `--env-file`:

```sh
PW=s3cret ply up
ply up --env-file ./secrets.env
```

An **undefined `$VAR` is a hard error** naming the member and key — never a
silent empty value. So a stack file carries no plaintext passwords: it ships
the *shape* of the wiring, and the secrets stay out of the file (and out of
the registry). `$$` is a literal `$`.

## Every member is a normal app

`ply up` spawns one `ply run` parent per member — the same supervision
process systemd runs in production — and stays in the foreground. `ply ps`
shows the instances; `ply exec db sh` works; a member dying takes the stack
down in reverse order so nothing keeps serving against a dead dependency.

## The stack lock

`run =` registry members resolve once and pin — reference, version, and the
image digest per arch — in the stack dir's `ply.lock`:

```toml
[stack.db]
ref = "postgres@17"
version = "17.10.0"
digest.x64 = "sha256:…"
```

A locked member starts straight from the local store: no index fetch, no
download, and the whole stack starts with the network down. Upgrades are
deliberate:

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
directory members, which use it) — never on plain image runs or deploys.

With the overlay in place, the full dev loop is:

```sh
PW=dev ply up     # db from the registry, server under tsx watch on live code
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

## Publishing and running a stack from the registry

A stack with a `[stack] name` and `version` is publishable — `ply push`
uploads its toml **template** (the `$VAR` holes stay in) and records its
`[[app]]` sequence in the catalog:

```sh
ply push ./umami-stack           # or ./umami.stack.toml
```

Anyone can then run it by name — the toml is fetched and brought up, holes
filled from the environment at launch:

```sh
PW=s3cret ply up iluxav/umami
```

No secret ever ships in the published stack; only the shape does.

## Dev versus production — same file, two behaviors

`ply up` is the **developer** experience: a foreground, supervised group that
starts and stops together, holding nothing on the host.

On a **host**, the same stack file lives in the deployments directory and
reconcile expands it into N independently-managed apps — each its own
`--name`'d unit, shown in `ply ps`, rolled and reverted on its own. Dev wants
atomic all-up/all-down; production wants each app independently reconcilable.
One declaration serves both. See [Deploys](/docs/deploy/) and the
[model](/docs/model/).
