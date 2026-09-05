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
ply build [DIR] [-o FILE] [--arch x64|arm64] [--insecure-source] [--allow-secrets]
```
Resolve dependencies (writing `ply.lock`), produce a deterministic image
named `<name>-<version>-<os>-<arch>.img`. `--arch` cross-builds: packing is
arch-independent and dependencies resolve for the target, so an x64 laptop
builds arm64 droplet images.

**What ships.** `[package] include` is a whitelist — name a path and only that
ships, and a typo is a hard error. **With no `include`, everything in the
directory ships**, and the build says how much:

```
ply: packing 412 files (18.4 MiB) — no `include` in ply.toml, so everything … ships
```

That line matters because squashfs compresses junk away: 200 MB of
`node_modules` can report a few KiB, so image size is no signal.

Credential-shaped files swept in that way (`.env`, `.env.*`, `*.key`, `.npmrc`,
`.netrc`, `.pgpass`, `.ssh*`, `id_rsa`…) **refuse the build**, because an image
is distributable and `ply push` puts it on a public registry. Naming one in
`include` is an explicit choice and is allowed; `--allow-secrets` overrides
wholesale. `.git`, `__pycache__` and other build detritus never ship, at any
depth.

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
            [--egress MODE] [--egress-allow ENTRY]…  # outbound policy — see below
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
address. Rootful, new connections are DNATed to instances by the kernel
(`(kernel dnat)` on the publishing line); rootless, on macOS, and for
`127.0.0.1` the parent relays them itself — see [Running](/docs/running/):

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
# a convenience: an app reading other names sees nothing and fails quietly,
# so prefer naming the address yourself — `-e DATABASE_URL=…@db.ply:5432/…`
```

ply computes the address, so it is right rootless (loopback) and rootful
(bridge gateway) without the author guessing. An explicit `[env]` or `-e`
wins; an unpublished dependency injects nothing.

**`--egress`/`--egress-allow`** — the operator's word on outbound policy,
over whatever the image's `[network] egress` claims: `--egress` sets the
mode (`off`, `audit`, `enforce`; defaults to `audit` when the manifest
declares `[network] egress`, else `off`), `--egress-allow` (repeatable)
replaces the manifest's list — pass `--egress-allow ""` for an empty one.
`ply run` prints the effective policy at start, e.g.
`ply: egress enforce, 1 entry (override)`; see
[Security & rootless](/docs/security/#egress-the-contract).

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
ply egress APP [--follow] [--blocked] [--json]
```

**`ply logs`** reads the bounded per-instance ring the run parent tees
(512 KiB ×2 per instance, in the run dir) — identical foreground, under
systemd, rootless. journald remains the unbounded archive on systemd hosts.
No APP lists what has logs; `-f` follows.

**`ply egress`** reads the audit log every instance of `APP` has been
writing since it started (`off` writes none): a table of destination,
name, port, protocol, connection count, first/last seen, and verdict.
`--blocked` narrows to what was not `allowed` — `blocked`, `undeclared`
(audit's word for what enforce would block), `refused`; `--follow`
tails new records; `--json` prints the raw log lines. See
[Security & rootless](/docs/security/#egress-the-contract).

## Lifecycle

```sh
ply deploy IMAGE [--timeout S]     # rolling deploy, health-gated (see Deploys)
ply scale APP N|auto               # grow/shrink the pool; `auto` resumes [scale] after a pin (a command is a file
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

```sh
ply inspect postgres@17 | owner/name@1.2 | ./the.img | ./dir | stack.toml
                         [--json] [--manifest]
```
Show what a package declares, read straight off its manifest — a registry
ref resolves and fetches into the store exactly like `ply run`; a `.img`,
a `.toml`, or a directory's `ply.toml` reads directly, no build. Default
output:

```
$ ply inspect postgres@17
postgres 17.10.7  app  owner: ply
volumes:      /var/lib/postgresql/data
links:        —
dependencies: postgresql17 17, rclone 1.60
params:       reference as {postgres.<name>} from a stack; set with params = { <name> = "…" }
  database  default   postgres
  password  secret, minted
  url       computed  postgres://{user}:{password}@{host}:{port}/{database}
  user      default   postgres
facts:        name version host port addr base_url scale arch image   (built-in, read-only)
live:         state instances started_at restarts   (after conditions only)
```

`owner: ply` shows once the image was built from a manifest declaring
`[package] owner` — an image built before that field existed prints
`owner: —` instead, same as any manifest that omits it.

`--json` prints the record — the same shape `ply push` sends, and what
`ply push --dry-run` prints; `--manifest` prints the embedded
`manifest_toml` verbatim.

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
ply push .                        # app/keg dir, or a stack dir/stack.toml
ply push myapp-1.0.0-linux-x64.img   # a built image (append-only)
ply push myapp-1.0.0-linux-x64.img --src https://…/myapp-{version}-linux-{arch}.img
```

The manifest embedded in what you push (or the stack file's own text) IS
the record `ply push` sends — never the working-copy `ply.toml` for an
app, which may differ from what got built.

| `ply push TARGET …` | build | upload | publish |
|---|---|---|---|
| `TARGET` = app/keg dir | yes | yes | manifest read off the built image; artifact `verified: true` |
| `TARGET` = `.img` | no | yes | same |
| `TARGET` (dir) `--src URL` | yes, for sha256/bytes | no | `verified: false`; `URL` may template `{version}`/`{arch}` |
| `TARGET` = `.img` `--src URL` | no | no | same |
| `TARGET` = stack dir / `stack.toml` | no | no | `type = "stack"`, no artifacts; members must be registry refs or URLs |
| `… --arch arm64` | cross-builds a DIR, as `ply build --arch` | yes/no | appends the arm64 artifact to the version |
| `… --dry-run` | as above | no | prints the record instead of sending it |
| `… --as NAMESPACE` | | | sets `owner` when the manifest has none; conflicts with a different `[package] owner` |

An existing `.img` already knows its arch — its name says so — so `--arch`
there may only confirm it: `ply push ./a-1.0.0-linux-arm64.img --arch x64`
is refused rather than publishing arm64 bytes as x64. `--dry-run` is the
plan and nothing else: it needs no key, no credentials file and no network,
so CI can print exactly what a push would send before anything is
configured (the `owner` the server derives from your key is left unset
unless the manifest or `--as` names one).

A bare `https://` target carries no manifest and is refused: `ply push
./the.img --src https://…` instead. Owner resolution: `[package] owner`
(or `[stack] owner`) wins; `--as` fills a manifest that names none; the two
disagreeing stops the push (`manifest says owner = "ply" but --as other
was given — drop one of them`). Neither, and you haven't chosen a
namespace yet: `ply push` points you at `plybox.sh/account/`.

```
$ ply push .
published ply/postgres@17.10.7
  https://registry.plybox.sh/ply/postgres/postgres-17.10.7.toml
use:
  ply run ply/postgres@17.10.7
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

`ply push` is two HTTP calls: upload the bytes, then publish the record —
the manifest, verbatim and as JSON, plus where the bytes landed. A
pipeline that never installs ply does both with curl; `ply push . --dry-run`
(run once, anywhere ply is installed) prints exactly the JSON the second
call needs.

```sh
# 1. bytes — the registry stores them and hands back the src to cite below
curl -fsS -X POST \
  -H "Authorization: Bearer $PLY_TOKEN" \
  -H "X-Ply-Filename: myapp-1.2.0-linux-x64.img" \
  -H "X-Ply-Sha256: $(sha256sum myapp-1.2.0-linux-x64.img | cut -d' ' -f1)" \
  --data-binary @myapp-1.2.0-linux-x64.img \
  https://plybox.sh/api/upload/

# 2. the record — the manifest is the publish
curl -fsS -X POST \
  -H "Authorization: Bearer $PLY_TOKEN" -H "Content-Type: application/json" \
  -d @record.json \
  https://plybox.sh/api/publish/
```

`record.json` is `{owner, name, version, type, manifest_toml, manifest,
artifacts: [{arch, src, sha256, bytes, verified}]}` — `ply push --dry-run`
prints it. `verified` is always present in what you send, but the server
decides the real value from what it can check, not from what you claim:
an artifact's `src` under `registry.plybox.sh/<owner>/<name>/` must name
an object step 1 uploaded; any other host records as external with
`verified: false` (see `--src` below). Add `X-Ply-Namespace: <owner>` to
step 1 to upload under a namespace other than your login; a stack publish
has no artifacts, so it skips step 1 entirely.

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

`--src` registers an artifact where it already lives instead of uploading
it: `ply push image.img --src https://…` hashes the local bytes itself and
sends only the record — the registry never fetches the URL itself, so the
artifact records `verified: false` (a later verification pass may confirm
it without a protocol change). Your CI keeps publishing to GitHub
Releases; the registry stays a catalog. The image still has to exist
where `ply push` runs, even though nothing uploads — build it, or have it
already on disk — because ply computes the hash locally.

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
