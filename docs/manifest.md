---
title: ply.toml reference
description: Every key in the ply manifest — package, dependencies, env, ports, volumes, resources, health, restart, sources.
section: Reference
order: 20
---

# ply.toml reference

The complete manifest surface. Only `[package]` is required.

```toml
[package]
name = "myapp"                    # required; may not contain "-<digit>"
version = "1.2.0"                 # semver; part of the image filename
entrypoint = ["node", "server.js"]
user = "appuser:1000:1000"        # optional: run as name:uid:gid
workdir = "/opt/myapp"            # optional: cwd before exec (default: the app prefix)
stop_signal = "SIGTERM"           # optional: how to ask it to shut down
capabilities = []                 # optional: keep nothing (the default) — see below
base = "debian@13"                # exactly one base per app; or
                                  # { name = "debian", version = "13", source = "alias" }

[dependencies]
node   = "22"                     # range: lowest satisfying version wins (MVS)
ffmpeg = { source = "alias", version = "6.1" }

[env]
NODE_ENV = "production"

[params]                          # optional: named values other apps read as {myapp.x} — see below
api_key = { secret = true }       # minted per stack; add external = true for BYO

[ports]
web = 3000                        # label of what the app binds — not a host claim

[volumes]
data   = "/var/lib/myapp"                                 # per-instance
shared = { path = "/srv/uploads", scope = "shared" }      # opt-in shared
cache  = { path = "/var/cache/myapp", ephemeral = true }  # GC-able

[resources]
mem  = "512M"                     # memory.max (+ memory.high)
cpu  = "1.5"                      # cores
pids = 256                        # always enforced; default guards fork bombs

[health]
port  = 3000                      # TCP connect gate for deploys/restarts
grace = "30s"                     # cold-start budget

[restart]
policy = "on-failure"             # "never" (default) | "on-failure" | "always"
backoff = "1s"                    # doubles per failure…
max_backoff = "60s"               # …up to this cap; resets after healthy uptime

[requires]
abi = "linux-x64-gnu"             # what the app layer's native deps were built against

[sources]                         # OPTIONAL — omit it and the official
alias = "github:org/repo"         # registry is used
```

## Key notes

**`[package]`** — `name` + `version` produce the canonical filename
`<name>-<version>-<os>-<arch>.img`. Names may not contain `-` followed by a
digit (filename-parsing ambiguity). `entrypoint` is exec-style — an argv
array, no shell. To ask for one, write a **string** instead and it becomes
`["/bin/sh", "-c", <string>]`, which a TOML multi-line string makes readable:

```toml
entrypoint = """
[ -f /etc/caddy/Caddyfile ] || cp /opt/edge/Caddyfile /etc/caddy/Caddyfile
exec caddy run --config /etc/caddy/Caddyfile --watch
"""
```
 `user` makes ply create the passwd/group entry,
chown volumes, and drop privileges in the correct order. `base` names the
package that owns `/` (FHS, libc, `/bin/sh`) — `"name@range"`, or
`{ name, version, source }` to pin a source alias; it resolves, locks, and
fetches exactly like a dependency. In a base package's own manifest,
`base = true` marks it as one instead.

**`workdir`** — absolute path to `chdir` into before exec. Omit it and ply
uses the app's own prefix (`/opt/<name>`), which is right for anything ply
built. It exists mostly for [imported images](/docs/docker/), which carry a
`WORKDIR` their entrypoints depend on: start redis anywhere but `/data` and
its `find . -exec chown redis {} +` walks the entire filesystem.

**`stop_signal`** — the signal that means "shut down", default `SIGTERM`.
Not every daemon agrees: nginx drains on `SIGQUIT` and httpd on `SIGWINCH`,
and both would otherwise be killed mid-request when ply's patience runs
out. Case-insensitive, `SIG` optional — `quit`, `SIGQUIT` and `sigquit` are
the same request.

**`capabilities`** — what the app keeps after rights stripping. Omitting it
means **nothing**, which is the right answer for every package ply builds:
a native keg never chowns or setuids, because `user` does that from the
parent before stripping. Two other forms exist for the cases that need
them:

```toml
capabilities = "oci"                          # Docker's default fourteen
capabilities = ["chown", "net_bind_service"]  # exactly these
```

`"oci"` is what `ply import` writes, because official images assume
Docker's posture. Names are case-insensitive and the `CAP_` prefix is
optional; a typo fails at `ply build`, not at 3am. See
[Security & rootless](/docs/security/).

**`[dependencies]`** — the key IS the package name. String values are
version ranges against the `default` source; table values pick a source
alias. Version syntax: `"22"` = any 22.x.y, `"6.1"` = any 6.1.x,
`"1.2.3"` = exactly 1.2.3. Resolution is
[Minimal Version Selection](/docs/dependencies/). Names containing a dot
must be TOML-quoted (`"boost1.84" = "1.84"`) — a bare dotted key means a
nested table in TOML.

**`[env]`** — composed after package contributions, before CLI overrides
(`-e`, `--env-file`); last wins.

**`[params]`** — named values other apps interpolate with `{app.param}`,
and this manifest's own `[env]` can reference with bare `{param}`. Secrets,
computed values, and built-in facts (`host`, `port`, …) all go through the
same namespace. See below.

**`[ports]`** — documentation the tooling reads: `ply proxy` falls back to
these ports for an unpublished app, and `[health]` checks them. Never a host port
binding — that is `--publish`, deliberately a run-time decision rather than
a manifest one.

**`[volumes]`** — see [Volumes & data](/docs/volumes/). Per-instance by
default; `scope = "shared"` and `ephemeral = true` are the two modifiers.

**`[resources]`** — cgroup v2 limits. `pids` is set even if you omit it.

**`[health]` / `[restart]`** — see
[Deploys, health & restarts](/docs/deploy/).

**`[requires]`** — declares the ABI your app layer's native artifacts were
built against; the resolver refuses mismatched runtimes loudly instead of
letting you segfault at 2am.

**`[requests]`** — host access the image asks for:
`links = ["/abs/host:/abs/container", …]`, or the spelled-out
`links = [{ host = "/abs/host", at = "/abs/container" }]`. Both paths must be
absolute in either spelling. Never applied on its own (a manifest ships inside the image — an image must
not grant itself host access); `ply run --grant-links` is the operator's
explicit yes, `ply systemd --grant-links` bakes the expansion into a unit.
Without the flag the requests are listed and not mounted.

**`[sources]`** — URL templates; see
[Registries & publishing](/docs/registries/). `{package}` expands to the
package name, letting one base URL serve per-package directories.
**The whole table is optional:** a dependency with no `source`, in a manifest
with no `[sources] default`, resolves from the official registry. Declare
`[sources]` when you actually fetch from somewhere else.

**`[volumes]`** — `name = "/path"` is the common form. The table form
(`{ path, scope, ephemeral }`) is for when you need `scope = "shared"` or
`ephemeral = true`.

## `[params]`

A param is a named value a package exposes; consumers interpolate it with
`{app.param}` — see [Stacks & local dev](/docs/stacks/) for the consumer
side. Here is the provider side, the shape a real keg ships:

```toml
[params]
user     = "postgres"                                               # plain value = default
database = "postgres"
password = { secret = true }                                        # minted per stack
url      = "postgres://{user}:{password}@{host}:{port}/{database}"  # computed — interpolates others

[env]                                  # the provider configures ITSELF from the same namespace
POSTGRES_USER     = "{user}"
POSTGRES_DB       = "{database}"
POSTGRES_PASSWORD = "{password}"
```

- A plain string is a default value. `{ secret = true }` mints a strong
  32-character value the first time a stack starts and stores it as a
  0600 file — never write a value alongside `secret = true`; minting *is*
  the default, so a manifest that tries fails to build. Add
  `external = true` and ply never mints: startup refuses until the
  operator provides the value (`ply secret set`, or a stack
  `params =`/`$VAR` override) — named, loud, no silent empty string.
- A computed param is just a param whose value contains `{other}` holes,
  resolved against the same package's own namespace. `url` above is not a
  special mechanism — it is a param that references its neighbors, same as
  any other.
- **Bare `{param}` in `[env]` reaches this same namespace** — declared
  params plus the built-in facts below — the moment a `[params]` table
  exists, even an empty one. A manifest with no `[params]` table is
  unchanged: `{`/`}` in `[env]` stay literal text, so nothing here breaks
  an existing manifest that hasn't opted in. Escapes: `{{` → literal `{`,
  `}}` → literal `}`. `$VAR` keeps its own rules and runs first — a value
  can mix both (`$VAR` reaches the ambient system, `{}` reaches the
  params namespace).
- **Built-in facts** — free on every package, no declaration needed:

  ```
  name  version  host  port  addr  base_url  scale  arch  image
  ```

  `addr` is `{host}:{port}`; `base_url` is `http://{host}:{port}` (http by
  convention). `port` is the container-side port of the first `--publish`,
  falling back to the package's own `[ports]` entry when it declares
  exactly one — so a keg that labels its port reads `{port}` even with no
  `--publish`. `host` is the `<name>.ply` address, which exists inside a
  stack and for a rootful run, but not for a bare rootless one.
  Referencing one that this run doesn't have is an error naming the gap,
  never a blank value — but only where it is *read*: a computed param
  nothing references (a keg's `url` on a run that publishes nothing) is
  simply unavailable, not fatal. `host`/`port` resolve per run mode
  (rootless loopback, rootful bridge, stack netns) — the reference is the
  same everywhere, the value is not.
- **Reserved names** — the built-ins above, plus the live set (populated by
  the runtime, never user-declared: `state instances started_at
  restarts`), plus `self` — cannot be redeclared in `[params]`:

  ```
  name version host port addr base_url scale arch image state instances started_at restarts self
  ```

  Referencing a *live* name (`state`, `instances`, `started_at`,
  `restarts`) from `[env]` or a computed param's default is a manifest
  error: those values change after launch, so baking one into env would be
  a stale-config bug waiting to happen. Read a live value from
  `/run/ply/self/<name>` at runtime, or wait on it with a stack `after`
  condition — see [Stacks & local dev](/docs/stacks/).

Images that carry `[params]` need ply 0.1.69 or newer — an older binary
rejects the unknown table outright (a manifest is parsed with unknown
fields denied), so update ply before pulling an image that declares one.

### Running a keg with params directly

`[params]`/`[env]` holes aren't only a stack thing: a bare `ply run` on an
image whose manifest declares `[params]` resolves that manifest's own
hole-y `[env]` too, against the app's own declared defaults and facts —
`ply run postgres@17 -e POSTGRES_PASSWORD=dev` gets `POSTGRES_USER` and
`POSTGRES_DB` defaulted to `postgres`/`postgres` with no further flags,
and either is still overridable with `-e`. A declared param nothing in
`[env]` reads never gets in the way: postgres's computed
`url = "…@{host}:{port}/…"` wants an address a rootless run doesn't have,
and that only matters if something asks for `url`.

A hole that reads a **secret** param is different: a standalone run has
nowhere durable to keep a minted value the way a stack does, so it refuses
to start rather than silently mint one or leave the literal `{password}`
in the child's environment:

```
postgres: [env] POSTGRES_PASSWORD reads a secret param — pass -e POSTGRES_PASSWORD=… , or run it from a stack (ply up mints secrets)
```

Pass the value yourself (`-e POSTGRES_PASSWORD=dev`) for a standalone run,
or run the same image inside a stack instead — `ply up` mints and stores
it. An explicit `-e KEY=value` always wins and is never re-resolved,
whether or not the key it names reads a secret. A manifest with no
`[params]` table is untouched by any of this.

## Two more files, same grammar

**`[[app]]`** — a file with `[[app]]` blocks is a stack file: several apps
wired for `ply up`, optionally headed by a `[stack]` table (name, version,
`env_file`). It is the `[[app]]` array that makes it a stack — a `[stack]`
table alone does not. Each member is `run = "postgres@17"` (registry app) or
`run = "./server"` (local app dir), plus `name`, `env`, `params`, `after`,
`publish`, `domain`, `volume`, `scale`. Registry members pin into the stack
dir's `ply.lock` (`ref`, `version`, `digest.<arch>`). See
[Stacks & local dev](/docs/stacks/).

**`ply.dev.toml`** — a gitignorable dev overlay next to an app's ply.toml,
applied only by `ply run DIR` / `ply up` (never by `build` — a shipped
image cannot contain dev configuration). Keys: `entrypoint` (replaces),
`[env]` (merges; `-e` still wins), `links` (extra binds; relative host
paths resolve against the app dir, relative container paths land under
`/opt/<name>/`). See [Stacks & local dev](/docs/stacks/).
