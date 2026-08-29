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
e      = ["POSTGRES_PASSWORD=$PW", "POSTGRES_DB=umami"]

[[app]]
run     = "umami@3"
after   = ["umami-db"]                 # → --after umami-db   (a member name)
publish = ["internal:3000"]            # → --publish internal:3000
e       = ['DATABASE_URL=postgresql://postgres:$PW@umami-db.ply:5432/umami']
```

Every `[[app]]` block is exactly one `ply run`. Fields map 1:1 to flags:
`run`→the image, `name`→`--name`, `e`→`-e`, `publish`→`--publish`,
`after`→`--after`, `volume`→`--volume`, `domain`→`--domain`,
`scale`→`--scale`. There is no stack concept beyond "these runs, ordered."

### Filling `$VAR`

`$VAR` in a stack file is substituted from the environment at launch:

- **`ply up`** — from the shell environment (and any `--env-file`).
- **host** — from the deployment's own `env` / `env_file`.

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
          // everything below is DERIVED from the image manifest at push:
          "volumes": ["/var/lib/postgresql/data"],
          "links": [],
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
          "src": "https://registry.plybox.sh/ply/umami/umami-3.0.0.stack.toml",
          "img": null,                // a stack has no image of its own
          "apps": [                   // mirrors the [[app]] array, verbatim
            { "run": "postgres@17", "name": "umami-db",
              "volume": ["/var/lib/postgresql/data"], "e": ["POSTGRES_PASSWORD=$PW"] },
            { "run": "umami@3", "after": ["umami-db"], "publish": ["internal:3000"],
              "e": ["DATABASE_URL=postgresql://postgres:$PW@umami-db.ply:5432/umami"] }
          ]
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
2. **Every version field except the curated header (description / license /
   homepage) is DERIVED at push** from the pushed toml + the image manifest.
   `volumes`, `links`, `dependencies`, `apps` are never hand-written, so they
   cannot drift from the artifact. *This is the fix for today's
   git-meta-vs-R2 drift.*
3. **A `stack` version has `img: null`, a `src` pointing at its stack toml,
   and an `apps[]` mirroring the `[[app]]` array** — enough to display and
   deploy the stack without fetching anything first.

## push = toml + (img | src)

Every push is two things: the assembly/stack toml (the *what*) and the bytes
(the *where*).

- `ply push ./myapp.img` — upload the image, read its manifest, write the
  catalog entry with `src` = the uploaded URL.
- `ply push ./myapp.toml --src https://…/myapp.img` — register a version that
  points at *your* URL; we store the entry, never the bytes.
- `ply push ./umami.stack.toml` — a `stack` entry; `apps[]` comes from the
  `[[app]]` array. `--src` optional (defaults to the uploaded toml's URL).

One push → one catalog entry, always derived from the toml. There is no
second place to edit, so there is nothing to keep in sync.

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
