# The ply model

> The one rule: **software must be predictable.** Every choice below is
> made so that the command tells you what will happen, the catalog is
> *derived* and never hand-authored, and a source is always a URL. When
> elegance and predictability conflict, predictability wins.

## Three artifact types

| type | what it is | how it runs |
|---|---|---|
| **app** | one runnable squashfs image with an entrypoint | `ply run <app>` |
| **layer** | a non-runnable keg under `/opt/<name>-<ver>/`, no entrypoint | consumed via `[dependencies]` |
| **stack** | no image of its own — a list of runs, wired | `ply up` (dev) / reconcile (host) |

Reserved, and **not** a package type: **fleet** = multiple *hosts* synced
from a git repo (`fleet.toml`, `hosts/`). A stack is multiple *apps* on one
host. Never conflate the two names.

The hierarchy: a **fleet** syncs files to N **hosts**; a host runs **stacks**
and **apps**; a **stack** is **apps** wired together; an app may pull in
**layers**.

## ply.toml is assembly, never runtime

`ply.toml` is the recipe for *building one image*. It declares:

- **identity** — `[package] name`, `version`
- **contents** — `base`, `[dependencies]`, `include`
- **the default boot** — `entrypoint`, `user`, `workdir`, `stop_signal`
- **assembly glue** — `[env]` values that make the relocated keg work at all
  (paths like go-forge's `MAGICK_CONFIGURE_PATH`), never tuning
- **declared shape the image needs** — `volumes` (paths that must persist),
  `[requests] links`, a declared `port` (a label + health hint)

It does **not** declare runtime values. Env *values*, port *mappings*,
`scale`, `domain`, secrets, restart policy — those are decided per
deployment and live on `ply run`. The toml is identical on every host; the
flags differ per environment.

> **Litmus:** if two deployments of the same image could sensibly differ on
> it, it is a `ply run` flag, not a line in `ply.toml`.

## ply run is one image plus its runtime flags

```
ply run <image> [--name N] [-e K=V]… [--publish P]… [--domain D]…
                [--volume PATH]… [--after APP]… [--scale N] [--env-file F]
```

`<image>` is a registry ref (`postgres@17`), a local path (`./dir`, `x.img`),
or a URL. Everything after it is runtime, layered on at launch. `--name`
gives the instance an identity distinct from its image (two `postgres`
instances → `--name db1`, `--name db2`).

## A stack is the runs, written down

A stack file is `ply run` × N, in dependency order. Nothing else.

```toml
# umami.stack.toml
[stack]
name = "umami"
description = "Privacy-first web analytics — app + its database."

[[app]]
run    = "postgres@17"                 # → ply run postgres@17
name   = "umami-db"                    # → --name umami-db   (default: image name)
volume = ["/var/lib/postgresql/data"]  # → --volume …
env    = ["POSTGRES_PASSWORD=$PW", "POSTGRES_DB=umami"]

[[app]]
run     = "umami@3"
after   = ["umami-db"]                 # → --after umami-db   (a member name)
publish = ["internal:3000"]            # → --publish internal:3000
env     = ['DATABASE_URL=postgresql://postgres:$PW@umami-db.ply:5432/umami']
```

Every `[[app]]` block is exactly one `ply run`. Fields map 1:1 to flags:
`run`→the image, `name`→`--name`, `env`→`-e` (older files spell it `e`;
both work, but not both on one member), `publish`→`--publish`,
`after`→`--after`, `volume`→`--volume`, `domain`→`--domain`,
`scale`→`--scale`. There is no stack concept beyond "these runs, ordered."

### Filling `$VAR`

`$VAR` is substituted at launch — in a stack file's `env`, `publish` and
`domain`, **and in a single-app deployment's** too:

- **`ply up`** — from the shell environment (and any `--env-file`).
- **host** — from the deployment's own `env_file`, then the process
  environment. A stack uses `[stack] env_file`; a single-app spec uses its
  own `env_file`, which defaults to `.env/<name>.env` when that file exists.

An undefined `$VAR` is a **hard error at launch**, never a silent empty
value. A missing password fails loudly at deploy — not at 3am.

## Running a stack: two situations, two behaviors

| | how | lifecycle |
|---|---|---|
| **dev** | `ply up <stack.toml>` | foreground supervised **group**; Ctrl-C stops all; nothing persists |
| **host** | the stack file lands in the deployments dir (by hand, the dashboard, or a fleet git sync) | reconcile **expands** it into N `--name`'d apps — each independently reconcile-managed, shown in `ply ps`, rolled on its own |

It is the **same file**. The verb (`ply up`) or the location (the
deployments dir) tells you which behavior you get. Dev wants atomic
all-up/all-down; production wants each app independently reconcilable (roll
just the server without disturbing the database). One declaration serves
both.

## The registry catalog

One flat array of packages. Each package is `namespace` + `name` + `type` +
`versions`.

```jsonc
{
  "packages": [
    {
      "namespace": "ply",           // who published
      "name": "postgres",
      "type": "app",                  // app | layer | stack
      "description": "…", "license": "…", "homepage": "…",
      "versions": [
        {
          "version": "17.10.3",
          "arch": "x64",
          // src is ALWAYS a full, http-fetchable URL — never a bare path.
          "src": "https://registry.plybox.sh/ply/postgres/postgres-17.10.3-linux-x64.img",
          "bytes": 41234567,
          "pushed_at": "2026-08-28T22:00:00Z",
          // everything below is DERIVED, server-side, from the manifest:
          "volumes": ["/var/lib/postgresql/data"],
          "links": [],
          // an ARRAY, and it stays one: a catalog is parsed in a single
          // pass into typed structs, so changing a field's TYPE fails the
          // whole document for every ply already installed. Additive means
          // new keys, never new types.
          "dependencies": [{ "name": "rclone", "version": "1.68" }]
        }
      ]
    },
    {
      "namespace": "ply", "name": "umami", "type": "stack",
      "description": "Privacy-first web analytics.",
      "versions": [
        {
          "version": "3.0.0",
          "img": null,                // a stack has no image of its own
          "src": "https://registry.plybox.sh/ply/umami/umami-3.0.0.toml"
          // no `apps[]` — the [[app]] array lives only in that .toml, which
          // is the record's manifest_toml, verbatim; a consumer (ply up,
          // the site) fetches it and reads the [[app]] blocks directly.
        }
      ]
    }
  ]
}
```

Three rules keep it predictable:

1. **`src` is always a full, http-fetchable URL.** No `path`, no
   namespace-in-the-address. A protected artifact → the consumer supplies a
   key. One resolution rule for our R2, your GitHub, or anyone's host.
2. **Every version field — the header (description / license / homepage)
   included — is DERIVED, server-side, from the pushed manifest**, never
   hand-authored and never computed by the client. The registry reads
   `volumes`, `links`, `dependencies`, `params`, and the header straight
   off the manifest the moment it (re)writes `state.json`; there is no
   separate curated file for any of it to drift from. *This is the fix
   for today's git-meta-vs-R2 drift.*
3. **A `stack` version has `img: null` and a `src` pointing at its
   `.toml`.** There is no `apps[]` mirror — the `[[app]]` array lives
   only in that `.toml`, so deploying it (`ply up owner/stack`, or the
   site) means fetching the file, not reading a pre-expanded array.

## push = toml + (img | src)

Every push is two things: the manifest (the *what* — verbatim, never
derived) and the bytes (the *where*).

- `ply push ./myapp.img` — upload the image, read its embedded manifest,
  publish a record with `src` = the uploaded URL, `verified: true`.
- `ply push ./myapp.img --src https://…/myapp.img` — publish the record
  without uploading; `src` points at *your* URL, hashed locally,
  `verified: false`. (`ply push ./myapp --src https://…` builds the
  directory first, to get the hash — a bare app *manifest* is never a
  push target: `ply push ./myapp.toml` is refused, since the manifest a
  push records is the one embedded in the built image, not the
  working-copy toml.)
- `ply push ./umami-stack` (a directory) or `ply push umami.toml` — the
  stack's own toml text IS the record's manifest, verbatim; no image, so
  nothing uploads.

One push → one record. There is no derivation on the client and no second
place to edit, so there is nothing to keep in sync.

## Running a published stack by name

`ply up <namespace>/<name>` fetches a published stack's toml (the template,
`$VAR` holes intact), parses it, and brings it up — filling the holes from
your shell or `--env-file`, exactly as a local `ply up`:

```sh
PW=s3cret ply up iluxav/umami           # fetch + run the whole stack
ply up iluxav/umami db                  # just the db member (+ its deps)
ply up ./umami.stack.toml               # a local stack file
```

The verb still tells you the shape: `ply run <namespace>/<name>` on a stack
doesn't guess — it points you at `ply up`.
