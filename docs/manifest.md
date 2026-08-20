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

[dependencies]
base   = "alpine@3.20"            # exactly one base per app
node   = "22"                     # range: lowest satisfying version wins (MVS)
ffmpeg = { source = "alias", version = "6.1" }

[env]
NODE_ENV = "production"

[ports]
web = 3000                        # label of what the app binds — not a host claim

[volumes]
data   = { path = "/var/lib/myapp" }                      # per-instance
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
abi = "linux-x64-musl"            # what the app layer's native deps were built against

[sources]
default = "https://registry.plybox.sh/ply/{package}"
alias   = "github:org/repo"
```

## Key notes

**`[package]`** — `name` + `version` produce the canonical filename
`<name>-<version>-<os>-<arch>.img`. Names may not contain `-` followed by a
digit (filename-parsing ambiguity). `entrypoint` is exec-style (no shell
unless you ask for one). `user` makes ply create the passwd/group entry,
chown volumes, and drop privileges in the correct order.

**`[dependencies]`** — string values are version ranges against the
`default` source; table values pick a source alias. Version syntax:
`"22"` = any 22.x.y, `"6.1"` = any 6.1.x, `"1.2.3"` = exactly 1.2.3,
`"alpine@3.20"` = package name + range in one string. Resolution is
[Minimal Version Selection](/docs/dependencies/).

**`[env]`** — composed after package contributions, before CLI overrides
(`-e`, `--env-file`); last wins.

**`[ports]`** — documentation the tooling reads: `ply lb`/`ply proxy` emit
backends for these ports, and `[health]` checks them. Never a host port
binding — every instance has its own IP.

**`[volumes]`** — see [Volumes & data](/docs/volumes/). Per-instance by
default; `scope = "shared"` and `ephemeral = true` are the two modifiers.

**`[resources]`** — cgroup v2 limits. `pids` is set even if you omit it.

**`[health]` / `[restart]`** — see
[Deploys, health & restarts](/docs/deploy/).

**`[requires]`** — declares the ABI your app layer's native artifacts were
built against; the resolver refuses mismatched runtimes loudly instead of
letting you segfault at 2am.

**`[sources]`** — URL templates; see
[Registries & publishing](/docs/registries/). `{package}` expands to the
package name, letting one base URL serve per-package directories.
