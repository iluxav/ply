# ply CLI reference

`ply <command> --help` is always current; this is the map.

## Build and inspect

```sh
ply init [DIR] [-y]                  # write a starter ply.toml
ply build [DIR] [-o FILE]            # resolve deps → ply.lock + a .img
ply search QUERY [--versions]        # what the registry has
ply add NAME[@RANGE]                 # append a dependency to ply.toml
ply check IMAGE                      # validate, optionally against host policy
ply images                           # what is in the store
```

## Run and observe

```sh
ply run IMAGE [--scale N]
              [--publish [ADDR:]PORT[:INSTANCE_PORT]]   # repeatable
              [--after APP]... [--after-timeout 60s]
              [-e K=V]... [--env-file FILE]
              [--link HOST:CONTAINER]                   # dev bind mount
              [--egress off|audit|enforce] [--egress-allow ENTRY]...   # outbound policy
              [--privileged]                            # debugging only
ply ps [--json]
ply stats [APP|APP.N] [--json]
ply exec APP[.N] CMD...
ply egress APP [--follow] [--blocked] [--json]          # the outbound audit log as a table
```

`--publish` forms:

| form | binds |
|---|---|
| `8080` | `0.0.0.0:8080` |
| `80:3000` | host `:80` → instances' `:3000` |
| `internal:5432` | loopback rootless / bridge gateway rootful |
| `public:80` | `0.0.0.0`, said explicitly |
| `127.0.0.1:8080:3000` | exactly that address |

`IMAGE` may be a file, a directory with a `ply.toml`, a registry reference
(`postgres@17`), a URL, or `docker://image:tag` (imported on demand, cached
by reference; `--pull` refreshes).

Repeating it gives each spec its own listener and pool. The **first** spec is
the app's canonical address — what `--after` hands to dependants and what
`ply proxy` emits — so adding a metrics port second cannot repoint callers.

`--after APP` waits for `APP`'s `[health]` gate, then injects `<APP>_ADDR`,
`<APP>_HOST` and `<APP>_PORT`. An explicit `[env]` or `-e` wins; an
unpublished dependency injects nothing.

`--egress` sets the outbound mode over the manifest's `[network] egress`
claim (default `audit` when declared, else `off`); `--egress-allow`
(repeatable) replaces the declared list. `ply egress APP` renders the audit
log: destination, name, port, protocol, packets, first/last seen, verdict
(`allowed` / `undeclared` / `blocked` / `refused`); `--blocked` keeps
everything but `allowed`.

## Lifecycle

```sh
ply deploy IMAGE [--timeout S]   # rolling, health-gated, reverts on failure
ply scale APP N|auto             # N pins (pauses [scale]); auto hands the count back
ply restart APP                  # rolling restart, health-gated
ply rebase IMAGE --runtime name@x.y.z   # swap a runtime without rebuilding
ply rm APP [--volumes]
ply gc                           # drop store entries nothing references
ply audit ; ply outdated
```

## Host integration

```sh
ply systemd IMAGE [--scale N] [--publish …]... [--after APP]... [--user]
ply proxy [APP]... [--format caddy|nginx|haproxy]
ply setup [--unprivileged-ports [PORT]]
ply sync                         # pre-fetch the host policy's packages
```

`ply systemd --user` emits a unit for `~/.config/systemd/user` — required for
rootless apps, since a system unit would run them as root. Pair it with
`sudo loginctl enable-linger $USER`.

## Ecosystem bridge

```sh
ply import docker://image:tag -o FILE   # OCI → fat ply image
ply bundle IMAGE -o FILE                # flatten a closure into one file
ply craft new|shell|changes|commit      # author a package interactively
```

## Conventions

- **Foreground by default** — backgrounding is systemd's job.
- **Versions are immutable** — there is no `:latest` to move; bump and rebuild.
- **Publishing is copying a file** — any file host works as a registry; the
  sha256 in `ply.lock` is the trust, not a login.
- Docker verbs ply deliberately lacks (`pull`, `push`, `tag`, `compose`,
  `logs`, `network`, …) answer with a one-line pointer to the ply way.
