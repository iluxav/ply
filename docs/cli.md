---
title: CLI reference
description: Every ply command with its important flags.
section: Reference
order: 21
---

# CLI reference

`ply <command> --help` is always current; this page is the map.

## Build & validate

```sh
ply init [DIR] [-y] [--force]
```
Write a starter `ply.toml`. Detects Node/Python projects for defaults and
asks a few questions (Enter accepts the default; `-y` accepts all). Never
touches anything but `ply.toml`.

```sh
ply search QUERY [--versions] [--limit N] [--source SPEC] [--json]
```
Search a source's catalog. One line per package, paste-ready:
`ffmpeg = "6.1"   # Multimedia framework   x64 arm64`. `--versions` lists
every published version and arch. The source is `--source`, else the
`[sources] default` of `./ply.toml`, else the official registry.

```sh
ply add NAME[@RANGE] [--source NAME]
```
Add a dependency to `./ply.toml`. Without a range, takes the latest
`major.minor` from the catalog. Comments and formatting are preserved.
Then `ply build` to resolve and lock.

```sh
ply build [DIR] [-o FILE] [--arch x64|arm64] [--insecure-source]
```
Resolve dependencies (writing `ply.lock`), produce a deterministic image
named `<name>-<version>-<os>-<arch>.img`. `--arch` cross-builds: packing is
arch-independent and dependencies resolve for the target, so an x64 laptop
builds arm64 droplet images.

```sh
ply check IMAGE [--against policy.toml]
```
Validate an image; with `--against`, check it against a host runtime policy.
Pure function — wire it into CI.

## Run & observe

```sh
ply run WHAT [--scale N] [-e K=V]… [--env-file F] [--link HOST:CONTAINER]
            [--publish [ADDR:]PORT[:INSTANCE_PORT]]  # parent binds it, L4-balances the pool
            [--after APP]… [--after-timeout 60s]     # wait for APP, and learn its address
            [--source SPEC]                          # registry for name references
            [--privileged]                           # keep everything; debugging only
```
Foreground, signals work, exit code propagates. `WHAT` is any of four
forms:

```sh
ply run app-1.0.0-linux-x64.img    # an image file
ply run .                          # an app dir: build (skipped when unchanged), run;
                                   #   applies ply.dev.toml if present
ply run postgres@17                # a registry name — newest matching version,
                                   #   fetched and cached (see Databases & services)
ply run docker://mongo:7           # OCI import, converted once and cached
```

**`--publish`** — `ADDR` is `internal`, `public` (the default) or an IPv4
address:

```sh
ply run api.img --scale 4 --publish 8080          # 0.0.0.0:8080
ply run db.img  --publish internal:5432           # only other ply apps on this host
ply run api.img --publish 127.0.0.1:8080:3000     # exactly that address
ply run edge.img --publish 80:80 --publish 443:443  # repeatable
```

Repeating it gives each spec its own listener and pool. The first is the
app's canonical address (what `--after` hands to dependants).

Reach for `internal` for databases and internal APIs — a bare
`--publish 5432` puts postgres on every interface.

**`--after`** — waits for `APP`'s `[health]` gate, then injects where to
reach it:

```sh
ply run web.img --after api --after db
#   API_ADDR=10.77.0.1:8080   API_HOST=…   API_PORT=…
#   DB_ADDR=…                 DB_HOST=…    DB_PORT=…
```

ply computes the address, so it is right rootless (loopback) and rootful
(bridge gateway) without the author guessing. An explicit `[env]` or `-e`
wins; an unpublished dependency injects nothing.

**`--privileged`** — skips rights stripping entirely: capabilities kept,
`no_new_privs` off, seccomp off. For debugging and triaging imports. Use
`[package] capabilities` for anything you intend to keep running.

```sh
ply up [MEMBER…] [-C DIR] [--refresh] [--source SPEC] [--after-timeout 60s]
```
Start a `[stack]` — several apps from one ply.toml, dependency-ordered,
one Ctrl-C teardown. Named members start with their `after` dependencies;
no members = everything. `run =` members pin version + digest in the stack
`ply.lock` (offline-capable); `--refresh` re-resolves. See
[Stacks & local dev](/docs/stacks/).

```sh
ply ps [--json]
ply stats [APP|APP.N] [--json] [--sample-ms MS]
ply exec APP[.N] CMD…
ply logs [APP[.N]] [-f] [-n LINES]
```

**`ply logs`** reads the bounded per-instance ring the run parent tees
(512 KiB ×2 per instance, in the run dir) — identical foreground, under
systemd, rootless. journald remains the unbounded archive on systemd hosts.
No APP lists what has logs; `-f` follows.

## Lifecycle

```sh
ply deploy IMAGE [--timeout S]     # rolling deploy, health-gated (see Deploys)
ply scale APP N                    # grow/shrink the pool (a command is a file
                                   # in the app's control dir; parent acts in ~2s)
ply restart APP                    # rolling restart, health-gated
ply reconcile                      # converge systemd units to
                                   # /var/lib/ply/deployments/*.toml — fired
                                   # automatically by systemd's dir watch;
                                   # a deployment is a file (root)
ply rm APP [--volumes]             # volumes kept unless --volumes
ply gc                             # drop store entries nothing references
```

## Images

```sh
ply rebase IMAGE --runtime name@x.y.z [-o FILE]   # swap a runtime, no rebuild
ply bundle IMAGE -o FILE                          # flatten to fat mode
ply import docker://image:tag -o FILE             # OCI bridge (fat mode)
```

## Package authoring

```sh
ply craft new|shell|edit|changes|commit|ls|rm
```
Interactive package authoring — shell in, install, commit the diff as an
inert package. See [Making packages](/docs/packages/).

## Host integration

```sh
ply systemd IMAGE [--scale N] [--publish [ADDR:]P[:IP]] [-e K=V] [--env-file F]
                  [--after APP]… [--user]
                                  # emit a unit file (supervision = systemd);
                                  # --user = ~/.config/systemd/user, for rootless
ply proxy [APP]... [--format caddy|nginx|haproxy] [--watch] [--out FILE]
                                  # emit reverse-proxy config; no APP = every
                                  # running app. Backends are the published
                                  # address, so scale/rolls need no re-emit.
                                  # --watch keeps FILE current and reloads
                                  # Caddy — installed as a unit by --edge
ply setup [--unprivileged-ports [PORT]] [--edge]
                                  # one-time host prep (idempotent, sudo);
                                  # also reports subuid/newuidmap readiness.
                                  # --edge installs Caddy + the proxy watcher:
                                  # after it, --domain is all an app needs
                                  # for HTTPS
ply sync                          # pre-fetch the host policy's packages
```

## Registry account

```sh
ply login                         # GitHub device flow; first sign-in chooses a username
ply whoami                        # your namespace, and any others you may publish to
ply push myapp-1.0.0-linux-x64.img   # publish under <you>/ (append-only)
ply push https://github.com/you/app/releases/download/v1.0.0/myapp-1.0.0-linux-x64.img
```

### Publishing from CI

A runner has no browser, so it cannot do the device flow. It publishes
with a **key** instead: mint one where you *are* logged in, store it as a
repository secret, and set `PLY_TOKEN` in the workflow. `PLY_TOKEN` wins
over `~/.config/ply/credentials`, and the registry derives the owner from
the key itself — a key can only ever publish to its own namespace.

```sh
ply key new --note "ci: myapp"    # printed once; only its hash is stored
ply key ls                        # ids, notes, last use — never the keys
ply key rm 3                      # revoke; anything using it stops now
```

```yaml
- run: ply push myapp-${{ github.ref_name }}-linux-x64.img
  env:
    PLY_TOKEN: ${{ secrets.PLY_TOKEN }}
```

Keys are also minted (and revoked) on
[plybox.sh/account](https://plybox.sh/account/) — the lane to use when no
machine is logged in yet.

### Publishing without ply installed

`ply push` is a convenience over one HTTP request. A pipeline that never
installs ply publishes with curl:

```sh
# bytes — the registry stores them
curl -fsS -X POST \
  -H "Authorization: Bearer $PLY_TOKEN" \
  --data-binary @myapp-1.2.0-linux-x64.img \
  https://plybox.sh/api/push/<namespace>/myapp-1.2.0-linux-x64.img

# a URL — the registry fetches it once, hashes it, and stores no bytes
curl -fsS -X POST \
  -H "Authorization: Bearer $PLY_TOKEN" -H "Content-Type: application/json" \
  -d '{"url":"https://github.com/you/app/releases/download/v1.2.0/myapp-1.2.0-linux-x64.img"}' \
  https://plybox.sh/api/push/<namespace>
```

The namespace in the path must be one your key may publish to; omit the
path entirely (`/api/push` with an `X-Ply-Filename` header) and it defaults
to your login. What the CLI adds is the derived catalog metadata — the
`X-Ply-Meta` header carrying the image's volumes, links and dependencies,
read out of its own manifest. Curl-published packages simply catalog
without it.

### Namespaces

Your namespace is a **username you choose once** on first sign-in — not
your GitHub handle. The account itself is keyed on your verified email, so
renaming on GitHub never moves your namespace, a freed handle never hands
it to someone else, and signing in later through another provider with the
same verified address reaches the same account. Until you have chosen one,
`ply push` says so and publishes nothing.

Anything beyond your own username is an explicit grant: the official `ply`
and `apps` shelves are reserved (never claimable by registering a matching
name), and a shared org namespace is a row in `namespace_grants`. Your
account page lists everything you may publish to; `ply whoami` and
`GET /api/cli/whoami` report the same list.

Operators grant the official shelves declaratively — `PLY_ADMIN_LOGINS` on
the site deployment (comma-separated GitHub logins) grants them on that
user's next sign-in.

The URL form registers an image where it already lives — the registry
fetches it once, verifies the squashfs magic, and pins the sha256, but
never stores the bytes. Your CI keeps publishing to GitHub Releases;
the registry stays a catalog. URLs must be https, carry no query string
(signed links expire), and end in the canonical
`<name>-<x.y.z>-linux-<arch>.img` filename.

Installing needs no account — reads are public files. Signing out is
deleting `~/.config/ply/credentials`; revoke keys with `ply key rm` or at
plybox.sh/account/.

## Keeping ply current

```sh
ply self-update                   # fetch + verify + atomically replace this binary
ply self-update --check           # just report what's newer
```

Hosts prepared by `ply setup --edge` run this daily on a jittered timer;
`ply ps` marks instances whose supervisor predates the installed binary
with `up*`.

## Fleet hygiene

```sh
ply audit                         # shared volumes, deprecated runtimes, risk surface
ply outdated                      # dependencies with newer versions available
ply volume ls                     # every data volume: size, in use / idle / orphaned
ply volume rm myapp/data.1        # delete one (refused while its instance runs)
ply volume rm --orphans           # sweep volumes no installed app claims
```

Volumes survive `ply rm` and deleted deployments on purpose — `volume ls`
is where you see what survived, and `volume rm` is the deliberate act.
Wiping a database volume before a `BACKUP_RESTORE` redeploy lives here too.

## Conventions

- **`--json` everywhere it matters** — `ps`, `stats` are stable interfaces
  for scripts.
- **Foreground by default** — backgrounding is systemd's job, emitted for
  you.
- **Destructive actions are explicit** — data deletion never rides along
  (`rm` keeps volumes; `--volumes` is the separate act).
- Exit codes propagate — `ply run` in CI behaves like running the binary.
