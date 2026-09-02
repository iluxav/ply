---
title: Stacks & local dev
description: ply up starts several apps from one stack file — each [[app]] block is one ply run — and ply.dev.toml / stack.dev.toml overlay dev behavior without touching the image.
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
run     = "postgres@17"                    # → ply run postgres@17
name    = "db"                             # → --name db
publish = ["internal:5432"]
params  = { database = "todos" }           # override; password stays minted

[[app]]
run     = "./server"                       # → ply run ./server
e       = ["DATABASE_URL={db.url}"]        # reference IS the edge — see below
publish = ["internal:3001"]

[[app]]
run     = "./web"
e       = ["SERVER_URL={server.base_url}"]
publish = ["3000"]
```

Note what wires the members: a line you wrote, and only one of them.
`{db.url}` in `server`'s env is simultaneously the connection string *and*
the start order — ply derives `after: db` from the reference itself (no
separate `after = ["db"]` to keep in sync), and resolves `db.url` from
postgres's own `[params]` (`postgres://{user}:{password}@{host}:{port}/{database}`)
using the real address and the minted password, neither of which this file
ever names. `web`'s `{server.base_url}` does the same for the next hop.

```sh
ply up            # everything, dependency-ordered; Ctrl-C stops it all
ply up db         # just the database (dependencies of named members come along)
```

Every field is a `ply run` flag: `run`→the image, `name`→`--name`, `env`→`-e`
(spelled `e` in older files; both work, but not both on one member),
`params`→per-param overrides for the member's own declared `[params]`,
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

## `{app.param}` — reading a neighbor's params

A member's `env`/`e` values (and its `params = {…}` overrides) can hold
`{app.param}` holes: interpolation into the named member's resolved
namespace — the
declared `[params]` from its manifest, built-in facts (`host`, `port`,
`addr`, `base_url`, `name`, `version`, `scale`, `arch`, `image`), and
computed values. `{self.x}` reaches the member's own namespace
(`e = ["APP_VERSION={self.version}"]`).

```toml
e = ["DATABASE_URL={db.url}"]                          # common case
e = ["DATABASE_URL={db.url}?sslmode=disable"]          # composed — templates can't do this
e = ["PGHOST={db.host}", "PGPASSWORD={db.password}"]   # discrete vars
```

- **A `{app.param}` reference IS the ordering edge.** No `after = ["db"]`
  needed alongside it — ply derives `db.state == "healthy"` from the
  reference and waits on it before starting the member that wrote it.
  Writing an explicit `after` too is legal and redundant, never
  conflicting; it's how you order without a reference, or state a custom
  condition (next section).
- **`publish` and `domain` take no `{}` holes in v1.** They are handed to
  the runtime verbatim, so a hole there would reach it as the literal text
  `{db.port}`; a stack that writes one is rejected at parse, naming the
  limitation. Write the port literally, or use a `$VAR` for a deploy-time
  value.
- `$VAR` and `{}` coexist and point in different directions: `$` reaches
  the ambient system (shell, or `env_file`), `{}` reaches the stack graph.
  Both may appear in one value.
- Escapes: `{{` → literal `{`, `}}` → literal `}`.
- **No expression language.** No functions, conditionals, or defaults
  syntax inside `{}` — logic belongs in your own code, not the reference.
- A **live** param (`state`, `instances`, `started_at`, `restarts`) in an
  `env`/`e` value is a build error, not a stale value: those change after
  launch, so nothing bakes them into env. Wait on one with `after` (next
  section), or read it at runtime from `/run/ply/<app>/<param>` — see
  [Running & scaling](/docs/running/#start-order).

`params = { key = "value" }` on a member overrides one of **that member's
own** declared params (`db`'s `database` above) — it is not a cross-member
reference. Values may hold `$VAR`; setting a param the member's manifest
doesn't declare is an error naming the declared set.

## Waits — `after`

```toml
after = ["db"]                             # sugar for db.state == "healthy" — today's gate, unchanged
after = ["server.finish_boot"]             # wait until the param exists
after = ["server.finish_boot == 'ok'"]     # wait until equality holds
```

Exactly those three forms — `APP`, `APP.PARAM`, `APP.PARAM == 'value'` (or
`"value"`) — and nothing else: no `!=`, no ordering comparisons, no boolean
operators. An app that needs more computes it itself and publishes a param
from inside its own code (`echo ok > /run/ply/self/finish_boot`, or
`fs.writeFileSync("/run/ply/self/finish_boot", "ok")`) — that's where the
logic belongs, never in the wait grammar.

A condition unmet within the timeout **fails loud**, never hangs — the
launch aborts naming the condition, the current value, and the elapsed
time:

```
waiting for server.finish_boot == 'ok' (currently unset, 30s elapsed)
```

See [Running & scaling](/docs/running/#start-order) for the `/run/ply`
tree these conditions read, and how an app self-publishes into it.

## Secrets: minted files, or `$VAR` holes

The `db` member above declares no password anywhere — postgres's own
`password = { secret = true }` mints a strong value the first time the
stack starts and stores it as a 0600 file (`<stack dir>/.ply/secrets/
db.password` locally; `<deployments dir>/.secrets/<stack>/db.password` on a
host). `{db.url}` already carries it; the stack file and the published
template never hold the plaintext.

Override precedence: a stack `params =` value beats an existing secret
file, which beats minting:

```toml
[[app]]
name   = "db"
params = { password = "$PROD_PW" }     # ambient beats minting
```

```sh
PROD_PW=s3cret ply up
```

`$VAR` in an `env`/`e` value still works exactly as before — filled from
the environment at launch, from your shell or an `--env-file`:

```sh
PW=s3cret ply up
ply up --env-file ./secrets.env
```

An **undefined `$VAR` is a hard error** naming the member and key — never a
silent empty value. `$$` is a literal `$`. Operator surface for minted or
external secrets is files first, `ply secret` as a convenience:

```sh
ply secret ls -C .                  # or --deployments STACK on a host
ply secret set db.password s3cret   # or omit VALUE to read one line from stdin
```

`[stack] env_file` (a file of `KEY=VALUE` lines filling every `$VAR` hole)
is **still supported** — it fills the *shape* a published stack ships
with holes in — but is **superseded for secrets by `[params]`**: a minted
or external secret needs no `env_file` entry, no `$VAR` hole, and never
appears in the published template at all.

### Secrets on a host

`ply reconcile` runs the same resolver as `ply up`, but a host expands a
stack into N independently-managed systemd units instead of one foreground
process group, so delivery goes through two *different* files under the
deployments dir's `.secrets/`:

- **The secret itself** — `<deployments dir>/.secrets/<stack>/<member>.
  <param>` (0600), one file per param: the minted or operator-set value,
  same layout as the local `.ply/secrets/` store. `ply secret ls|set
  --deployments <stack>` manage exactly these files.
- **The delivery file** — every reconcile beat, the reconciler writes a
  member's secret-tainted *resolved env* (which may combine several
  params into one composed value, like a `DATABASE_URL`) to
  `<deployments dir>/.secrets/<stack>/env/<member>.env` (0600, its `env/`
  directory 0700), and that member's unit gets `--env-file` pointed at
  it. Plain, non-secret entries stay ordinary `-e KEY=VALUE` flags baked
  into the unit text — only secret-tainted ones go through the env file,
  so the unit itself (world-readable, `systemctl cat`-able) never carries
  a secret.

**Changing a secret does not, by itself, restart the member.**
`ply reconcile` decides whether to restart by comparing the *unit text* it
would generate against what's on disk, and the unit only names the env
file by **path** — never its contents. So `ply secret set --deployments
<stack> db.password …` (or hand-editing the file) rewrites the delivery
file on the very next beat, but the running process keeps its old value
until something restarts it. Run `ply restart <member>` afterwards to pick
up the change — v1 behavior, by design; there is no content-hash restart
trigger.

## `ply up --plan` — the composed result, inspectable

```sh
ply up --plan
```

Resolves every member's params and env, and prints the composed result —
no minting, no spawn, no lock write. Exits non-zero on a resolution error
(an undeclared param, a live param in env, a missing external secret) —
`--plan` is the validator, not just a preview:

```
db (postgres@17)  internal:5432
  POSTGRES_DB       = todos     params (stack override)
  POSTGRES_PASSWORD = ********  minted  secrets/db.password
server (./server)  internal:3001  after: db (via {db.url})
  DATABASE_URL = postgres://postgres:********@db.ply:5432/todos  {db.url}
web (./web)  3000  after: server (via {server.base_url})
  SERVER_URL = http://server.ply:3001  {server.base_url}
```

What `--plan` lists in v1 is the stack's own `e = [...]` entries and the
params-driven self-config a provider resolves from its own `[params]` —
each with its resolved value and where it came from (`stack e`,
`manifest [env]`, `{db.url}`-style references, `params (stack override)`,
`minted  secrets/db.password`) — plus the derived wait DAG, annotated with
which reference created each edge. It is not the child's whole environment:
plain manifest `[env]` values (the ones with no holes), `-e`, `--env-file`
and ply's own injected variables are not listed. Secret values are masked, including
inside a composed value like `DATABASE_URL` — only a secret the resolver
itself knows about is masked, so a secret typed literally into a stack `e`
value (rather than referenced) still prints verbatim.

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

## stack.dev.toml — the same overlay, one level up

`ply.dev.toml` fixes an app's dev behavior; a stack has its own version of
the problem. The committed stack describes **production**: members reach
each other by their `<name>.ply` bridge names, and secrets are minted files
or `$VAR` holes — never plaintext. A laptop differs in fewer ways than it
used to: a rootless stack gets its own network too, so `<name>.ply` and the
members' real ports mean the same thing here as there. What is still local
is a published port that has to dodge whatever the machine already runs,
and building the checkout next door instead of pulling a release.

Put those local truths in `stack.dev.toml`, beside the stack file:

```toml
# stack.dev.toml   (add it to .gitignore)
[[app]]
name    = "db"                      # WHICH member — matched by name
publish = ["internal:5433:5432"]    # the container still serves 5432; only
                                    # the HOST side moves, because this box
                                    # runs its own postgres there

[[app]]
name = "server"
run  = "../server"                  # the checkout next door — {db.url} still
                                    # resolves correctly, dev password and all
```

- Members are matched by `name`; overriding a name that is not in the stack
  is an error, not a silent no-op.
- `env` and `params` **merge by key** — the override adds or replaces one
  entry and leaves the member's others alone. `publish`, `domain`, `volume`,
  `scale` and `run` replace outright; `[stack] env_file` replaces too.
- Overlays override members, they never add them.

Same structural rule as `ply.dev.toml`: **`ply up` applies it, a host never
does.** `ply reconcile` reads the stack file alone, so the file you commit
(and `ply push`) is the production one, and the deployment cannot inherit a
laptop's loopback address.

## ply run DIR

The building block `ply up` uses is useful alone — cargo run for
containers:

```sh
ply run .             # build this directory (skipped if unchanged) and run it
ply run ./server      # same, from anywhere
```

## Publishing and running a stack from the registry

A stack with a `[stack] name`, `version`, and optional `owner` is
publishable — `ply push` records its toml **template** verbatim (the
`$VAR` holes stay in). There is no image, so nothing builds and nothing
uploads:

```sh
ply push .              # a directory whose ply.toml is a stack, or
ply push stack.toml     # any stack file
```

`owner` picks the namespace the same way `[package] owner` does for an
app: set it in the `[stack]` table, or pass `--as NAMESPACE` when the file
names none. Members must be registry refs (`postgres@17`) or URLs — a
`./dir` member is refused, since it names nothing on someone else's
machine; publish that app first, then reference it by name. The registry
writes the template to `{owner}/{name}/{name}-{version}.toml`.

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
