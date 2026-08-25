# ply.toml reference

Only `[package]` is required.

## [package]

| key | notes |
|---|---|
| `name` | required; may not contain `-` followed by a digit (filename grammar) |
| `version` | required, semver; part of the image filename |
| `entrypoint` | argv, exec-style — no shell unless you ask for one. Absent = a library/runtime package, not an app |
| `include` | paths that ship. **Absent packs everything**, which usually drags in `node_modules`, `.git`, build caches |
| `base` | `"alpine@3.20"` or `{ name, version, source }`. Exactly one package per graph owns `/`. In a base package's own manifest, `base = true` |
| `user` | `"name:uid:gid"` — ply writes passwd/group, chowns volumes, and drops privileges in the right order |
| `workdir` | absolute cwd before exec. Default: the app's prefix (`/opt/<name>`) |
| `stop_signal` | default `SIGTERM`. nginx wants `SIGQUIT`, httpd `SIGWINCH` |
| `capabilities` | default none. `"oci"` = Docker's fourteen (what `ply import` sets), or an explicit list like `["chown"]` |
| `provides_abi` | for runtime packages, e.g. `"linux-x64-musl"` |
| `isolation` | `"ns"` (default) |

## [dependencies]

The key IS the package name. String = a version range against the `default`
source; table = `{ source = "alias", version = "6.1" }`.

Ranges: `"22"` = any 22.x.y, `"6.1"` = any 6.1.x, `"1.2.3"` = exactly that.
Resolution is Minimal Version Selection — the **lowest** version satisfying
all constraints wins, so builds don't drift. Names containing a dot must be
quoted (`"boost1.84" = "1.84"`).

## [env]

Composed after package contributions, before `-e` / `--env-file`. Last wins.

## [ports]

Labels of what the app binds internally. `ply proxy` falls back to them for
an unpublished app and `[health]` checks them. **Never a host claim** — that
is `--publish`, deliberately a run-time decision.

A declared port below 1024 causes ply to keep `CAP_NET_BIND_SERVICE`, so an
edge can bind `:80`/`:443` without extra configuration.

## [volumes]

```toml
data   = { path = "/var/lib/myapp" }                     # per-instance (default)
shared = { path = "/srv/uploads", scope = "shared" }     # explicit opt-in
cache  = { path = "/var/cache/myapp", ephemeral = true } # GC-able
```

Per-instance is the default so scaling can never silently corrupt
single-writer state. Plain host directories underneath.

## [resources]

cgroup v2 limits: `mem = "512M"`, `cpu = "1.5"`, `pids = 256`. `pids` is set
even when omitted, so a fork bomb is contained with zero configuration.

## [health]

`port` = a TCP connect gate for deploys and restarts; `grace` = the
cold-start budget. Without `[health]`, an instance only has to survive.

## [restart]

`policy` = `"never"` (default) | `"on-failure"` | `"always"`; `backoff`
doubles per consecutive failure up to `max_backoff`, resetting after healthy
uptime.

## [requires]

`abi = "linux-x64-musl"` — what the app layer's native artifacts were built
against. The resolver refuses a mismatched runtime loudly instead of letting
it segfault later.

## [sources]

URL templates. `{package}` expands to the package name, so one base URL can
serve per-package directories. `default` applies to deps without an explicit
source; other keys are aliases usable as `source = "<alias>"`.
