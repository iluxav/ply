# Params — one namespace instead of ten env layers

**Date:** 2026-09-01
**North star check:** everything is a file (secrets are files, live state is
a file tree, env is synthesized only at exec); no daemon, no DSL, no
injection an app didn't write on its own lines.

## The problem being deleted

A value reaching a todos-stack process today can come from ten places: the
app's dotenv `.env`, package contributions, manifest `[env]`,
`ply.dev.toml`, stack `e = [...]`, `$VAR` holes, `[stack] env_file`,
`stack.dev.toml`, `-e`, `--env-file` — plus `--after`'s injected
`*_ADDR/HOST/PORT`. The symptoms:

- `POSTGRES_PASSWORD=$POSTGRES_PASSWORD` — a pass-through declaration
  written in interpolation syntax; one logical value appears 4× across files.
- `DATABASE_URL=postgres://postgres:$POSTGRES_PASSWORD@db.ply:5432/todos` —
  a human string-concatenating a hostname ply chose, a port ply knows, and a
  secret ply transports.
- dev/prod stacks diverge exactly where the secrets are.

Root cause: env vars carry three unrelated kinds of value — app config,
wiring (topology), and secrets. App config already has a home (`[env]`).
This spec gives the other two theirs. **One new concept: params.** A package
exposes named values; consumers interpolate them. URLs, secrets, health,
and readiness are all params — none is a special mechanism.

## `[params]` — the provider side

```toml
# postgres keg ply.toml
[params]
user     = "postgres"                                               # plain value = default
database = "postgres"
password = { secret = true }                                        # minted per stack (below)
url      = "postgres://{user}:{password}@{host}:{port}/{database}"  # computed param

[env]                                  # the provider configures ITSELF from the same namespace
POSTGRES_USER     = "{user}"
POSTGRES_DB       = "{database}"
POSTGRES_PASSWORD = "{password}"
```

- Inside a manifest, bare `{param}` refers to the package's own namespace
  (declared params + built-ins). `[env]` values may carry holes; a manifest
  with no `[params]` behaves exactly as today.
- A computed param is just a param whose default interpolates others. `url`
  is not special; there is no offer/expose object, no provider-owned env
  name, no URL template mechanism.
- Provider and consumer read the same param, so they agree by construction —
  the tautology is structurally impossible.

**Built-in facts** every app exposes with zero declaration, resolved from
the graph and the lockfile:

```
name  version  host  port  addr  base_url  scale  arch  image
```

`addr = "{host}:{port}"`; `base_url = "http://{host}:{port}"` (http by
convention, only for apps with a published/labeled port); `image` = digest.
`host`/`port` resolve per mode (rootless loopback, rootful bridge, stack
netns) — the reference is mode-independent, the value is not. Built-in
names — the facts above plus the live set (`state instances started_at
restarts`) — are **reserved**: declaring one in `[params]` is a build
error, and a self-published write to a parent-owned file (`state`, …) is
refused (read-only bind for those paths even inside `self`).

## `{app.param}` — the consumer side

```toml
[stack]
name = "todos"

[[app]]
run     = "postgres@17"
name    = "db"
publish = ["internal:5432"]
params  = { database = "todos" }            # override; password stays minted

[[app]]
run     = "./server"
e       = ["DATABASE_URL={db.url}"]         # reference IS the edge (see Waits)
publish = ["internal:3001"]

[[app]]
run     = "./web"
e       = ["SERVER_URL={server.base_url}"]
publish = ["3000"]
```

Consumer chooses its altitude; all four are the same mechanism:

```toml
e = ["DATABASE_URL={db.url}"]                     # common case
e = ["DATABASE_URL={db.url}?sslmode=disable"]     # composed — templates can't do this
e = ["DATABASE_URL=postgres://{db.user}:{db.password}@{db.host}:{db.port}/{db.database}"]
e = ["PGHOST={db.host}", "PGPASSWORD={db.password}"]   # discrete vars
```

Rules:

- `{app.param}` resolves from the named stack member's namespace. `{self.*}`
  is the app's own (`e = ["APP_VERSION={self.version}"]`).
- `$VAR` and `{…}` coexist and point in different directions: `$` reaches
  the ambient system (shell / `env_file`), `{}` reaches the graph. Both may
  appear in one value.
- Escapes: `{{` → literal `{`, `}}` → literal `}`. `$` keeps today's rules.
- **No expression language.** No functions, no conditionals, no defaults
  syntax in references. A consumer needing logic puts it in its own code.
- Nothing is ever injected. An app's env is exactly its written lines
  (manifest `[env]` + stack `e` + CLI), post-interpolation. The legacy
  `--after` `*_ADDR/HOST/PORT` injection survives for compat but is
  deprecated in docs the day this ships — params are its replacement.
- Stack-side `params = {…}` sets/overrides declared params for that member.
  Values may use `$VAR` (`password = "$PROD_PW"` — ambient beats minting).
  Setting an undeclared param is an error naming the keg's declared set.

## Two tiers: facts and state

| tier | examples | consumer | why the split |
|---|---|---|---|
| facts — knowable at plan time | declared params, built-ins above | `{}` interpolation into env | frozen at exec is correct |
| state — live | `state instances started_at restarts` + self-published | `after` conditions; reading files | frozen at exec is a lie |

`{db.state}` in an `e =` line is a **build-time error** ("live param — wait
on it with `after`, or read /run/ply/db/state"), not a warning. One volatile
value baked into env would seed stale-config bugs for years.

## The file tree — `/run/ply/<app>/`

The parent already owns every live fact (it forked the instances, runs the
health gate, balances the pool). It writes them to a tmpfs tree, updated on
every transition:

```
/run/ply/db/state          # starting | healthy | unhealthy | stopped
/run/ply/db/instances
/run/ply/db/started_at
/run/ply/db/restarts
/run/ply/self/…            # THIS app's node, writable (bind-mounted rw at self only)
```

- One file per param, value is the file's content, no format to parse.
  Facts appear in the tree too (read-only) — the tree is the namespace;
  env interpolation is just one reader of it. Dashboards, scripts, probes
  read the same files. No API, no socket, no client library.
- **Self-publishing** (sd_notify, done as a file): an app writes its own
  node and the value becomes a live param dependents can wait on:

  ```js
  fs.writeFileSync("/run/ply/self/finish_boot", "ok");   // after migrations
  ```

  (`process.env.X = …` cannot work — a process mutating its own environment
  is invisible outside; `/proc/PID/environ` is frozen at exec.)
- Trust falls out of the mount table: only `self` is writable, so nobody
  fakes a neighbor's readiness. Instances of one app share the node
  (last-writer-wins; readiness writes are idempotent in practice).
- **Secrets never enter the tree.** The tree is stack-readable by design;
  minted values travel only through interpolation into the referencing
  app's env. `cat /run/ply/db/password` from a neighbor must be ENOENT.

## Waits — `after` reads the state tier

```toml
after = ["db"]                             # sugar for db.state == "healthy" — today's gate, unchanged
after = ["server.finish_boot"]             # wait until the param exists
after = ["server.finish_boot == 'ok'"]     # wait until equality holds
```

- **Equality against a string literal is the entire grammar.** No `!=`, no
  ordering comparisons, no boolean operators (the list is already AND). An
  app needing `migrations >= 12` computes that itself and publishes
  `finish_boot=ok` — that's where the logic belongs.
- A `{db.*}` reference anywhere in an app's lines derives the edge
  (Terraform-style: the reference is explicit in the consumer's own block,
  so ordering isn't restated). Implied edge = `db.state == "healthy"`.
  Explicit `after` remains for referenceless ordering and for custom
  conditions; writing both is legal and redundant.
- The built-in health gate is not privileged: ply runs `[health]` and writes
  `state`; your app runs migrations and writes `finish_boot`. Same tree,
  same wait.
- **Fail loud, never hang:** a condition unmet within the dependency's
  `grace` (or an explicit `timeout = "60s"` on the waiting app) aborts the
  launch with condition, current value, and elapsed:
  `waiting for server.finish_boot == 'ok' (currently unset, 30s elapsed)`.

## Secrets — minted, stored as files, tainted forever

- `password = { secret = true }` declares a mintable secret. First
  `ply up`/deploy generates a strong value and persists it:

  ```
  <stack-state>/secrets/db.password        # 0600, owner = the run user
  ```

  Stack state = the stack's existing state dir (local: the stack's `.ply/`;
  droplet: alongside deployments-as-files, so reconcile and backups see
  plain files). Regenerating never happens implicitly — the file is the
  truth until deleted.
- Operator surface is files first, CLI as convenience:
  `ply secret ls [STACK]`, `ply secret set todos/db.password [VALUE|-]`,
  or just write the file. Override precedence: stack `params` `$VAR` >
  existing file > mint.
- `{ secret = true, external = true }`: never minted; ply **refuses to
  start** until the operator provides it (file or `$VAR`) — named, loud.
  This is the STRIPE_KEY case.
- **Taint propagates through interpolation.** Any value containing a secret
  param (`{db.url}` includes `{db.password}`) is masked in `plan`, `ps`,
  logs, error messages, and the dashboard. The mark travels with the value,
  not the variable name.
- Delivery is env at exec (compat: apps read `DATABASE_URL` unchanged).
  `*_FILE` delivery (`/run/ply/secrets/<name>`, private mount per
  referencing app, `POSTGRES_PASSWORD_FILE` convention) is a planned
  refinement, out of v1.
- Dev = prod: dev gets its own minted file the same way. No dev password in
  any toml; `stack.dev.toml` shrinks to genuinely local facts (host ports).

## `ply up --plan` — the composed result, inspectable

The cure for "which of ten layers wins" is not memorizing precedence — it's
never guessing. `--plan` resolves everything, executes nothing:

```
db (postgres@17)          internal:5432
  POSTGRES_DB       = todos                    params (stack override)
  POSTGRES_PASSWORD = ********                 minted  secrets/db.password
server (./server)         internal:3001        after: db (via {db.url})
  DATABASE_URL      = postgres://postgres:****@10.77.0.1:5432/todos   {db.url}
  NODE_ENV          = production               manifest [env]
  PORT              = 3001                     manifest [env]
web (./web)               :3000                after: server (via {server.base_url})
  SERVER_URL        = http://10.77.0.1:3001    {server.base_url}
```

- Every var, its resolved value, its source — including legacy layers
  (`-e`, `--env-file`, `ply.dev.toml`, package contribution, injected),
  which get attributed even before anyone migrates. Secrets and tainted
  values masked.
- Also the derived DAG with which reference/condition created each edge.
- Exit non-zero on resolution errors (undeclared param, live param in env,
  missing external secret) — plan is the validator.
- Foundation for reconcile drift-diff (plan vs running) later; out of v1.

## Precedence and migration

- Composition order gains no new layers; interpolation happens where stack
  `e` values are composed today. Explicit `-e` still beats everything —
  unchanged rule.
- Everything is additive. Existing stacks, manifests, `$VAR` holes,
  `env_file`, dev overlays keep working byte-for-byte. Params enter a stack
  the first time someone writes `{…}` or `[params]`.
- Registry: add `[params]` + `[env]` holes to the core kegs (~10; postgres,
  redis, memcached first) — we own the registry, one `registry-push`.
- `ply import docker://…` images get built-in facts for free (`{redis.host}`,
  `{redis.port}`, `{redis.base_url}` work with zero metadata); declared
  params require a hand-added `[params]` — documented as the escape hatch,
  no stack-level template override mechanism.
- Deprecated in docs at ship time, removed later: `--after` env injection,
  `[stack] env_file` (superseded by minted secrets + `$VAR` for the rest).

## What we are deliberately not building

- An expression language (no functions/conditionals in `{}`, only `==` in
  waits). The moment strings grow logic, DATABASE_URL assembly is back with
  more steps.
- `use`-style injection / provider-chosen env names (Heroku addons'
  most-cursed feature; rejected early).
- Provider URL-template objects (`[offer]`) — dissolved into computed
  params.
- A variables/modules indirection layer (Terraform's own fatigue reborn).
- A secrets daemon/vault. Files under the stack state dir, 0600, done.

## Open questions (settle during implementation planning)

1. Param namespace of scaled apps: `{db.host}` is the parent/balancer by
   design; is any per-instance addressing ever exposed, or explicitly never?
2. `base_url` scheme: fixed `http` convention vs a `[ports]`-level
   `scheme =` label for the rare https-internal app.
3. Secret rotation UX: `ply secret rotate` (regenerate + rolling restart of
   referencing apps) — v1.1 candidate, needs the taint graph anyway.
4. Cross-stack references — out of scope until stacks can see each other.
5. `timeout` spelling on waits: per-edge (`after = [{on = "db", timeout =
   "60s"}]`) vs per-app single knob.

## v1 cut line

In: `[params]` (defaults, secret, computed), `[env]` holes, built-in facts,
`{app.param}`/`{self.*}` interpolation, reference-derived edges, `after`
conditions (exists / `==`), `/run/ply` tree with self-publishing, minted +
external secrets as files, `ply secret ls|set`, taint masking, `ply up
--plan` with full source attribution.

Out (explicitly): `*_FILE` delivery, rotation, drift-diff reconcile,
cross-stack, per-instance params, any grammar beyond `==`.
