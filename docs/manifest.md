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

## Two more files, same grammar

**`[[app]]`** — a file with `[[app]]` blocks is a stack file: several apps
wired for `ply up`, optionally headed by a `[stack]` table (name, version,
`env_file`). It is the `[[app]]` array that makes it a stack — a `[stack]`
table alone does not. Each member is `run = "postgres@17"` (registry app) or
`run = "./server"` (local app dir), plus `env`, `after`, `publish`, `domain`,
`volume`, `scale`. Registry members pin into the stack dir's
`ply.lock` (`ref`, `version`, `digest.<arch>`). See
[Stacks & local dev](/docs/stacks/).

**`ply.dev.toml`** — a gitignorable dev overlay next to an app's ply.toml,
applied only by `ply run DIR` / `ply up` (never by `build` — a shipped
image cannot contain dev configuration). Keys: `entrypoint` (replaces),
`[env]` (merges; `-e` still wins), `links` (extra binds; relative host
paths resolve against the app dir, relative container paths land under
`/opt/<name>/`). See [Stacks & local dev](/docs/stacks/).
