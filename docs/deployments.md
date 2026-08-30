---
title: Deployments & CD
description: A deployment is a file. Two ways to say where truth lives — fetch a built thing, or build one here — and a timer that keeps reality converged. Zero resident processes.
section: Guides
order: 14.5
---

# Deployments & CD

A deployment is a TOML file in `/var/lib/ply/deployments/`. Drop one in and
the app runs; edit it and the app converges; delete it and the app stops.
systemd's inotify watches the directory, a oneshot `ply reconcile` does the
converging, and a timer re-runs it once a minute so *follow-latest*
deployments update themselves. There is no daemon, no agent, no webhook
endpoint — a timer is a clock, not a process.

One-time host setup:

```sh
sudo ply setup --edge        # Caddy + HTTPS, the deployments watcher, the timer
sudo ply setup --swap 2G     # small hosts that will build JS on-droplet
```

## A deployment names where truth lives

Two choices, not five: **fetch a built thing**, or **build one here**.

```toml
# fetch — `from` says where, and its shape says which kind
from = "redis@8.0"                                  # a registry ref
from = "ply/plybox-web"                             # …namespaced
from = "/srv/deploy/myapp-1.2.0-linux-x64.img"      # a file on this host
from = "https://cdn.example.com/myapp-1.2.0-linux-x64.img"   # a URL
from = "github:org/myapp"                           # release assets
```

```toml
# build here — no CI at all: the host clones and builds the repo itself
repo = "https://github.com/org/myapp"
build = "npm ci && npm run build"
runtime = "node@24"
entrypoint = ["node", "dist/index.js"]
include = ["dist/", "node_modules/", "package.json"]
port = 3000
publish = ["internal:3000"]
domain = ["app.example.com"]
```

A whole deployment, then, is that one line plus how to run it:

```toml
from = "redis@8.0"
publish = ["internal:6379"]

[env]
REDIS_PASSWORD = "change-me"
```

If the repo carries its own `ply.toml`, the build lane needs none of the
manifest fields — `repo` plus a `build` command is enough; the repo's
manifest rules.

The older spellings — `app`, `image`, `url`, `github` — still work and mean
exactly what they meant; `from` normalizes into them. They were four names
for "a built artifact, somewhere", which is one idea.

## Continuous deployment is a pull

Every following lane re-resolves on each reconcile run:

- a registry ref (and a stack member's `run =`) resolves the newest matching
  version, including a namespaced one like `ply/plybox-web` — CI publishes,
  the host converges, nothing on the host names a version.
- `github:org/app` resolves the **latest release** (a `version = "1.2"`
  prefix follows patch releases; an exact `"1.2.3"` pins).
- `repo =` fetches the branch tip (`ref = "main"`, default: remote HEAD)
  and rebuilds only when the commit or the spec actually changed.

With the timer installed, that *is* CD: push code or tag a release, and the
host converges within a minute. Nothing on the host listens; nothing in CI
holds your server's keys. GitHub is a shelf, not an actor.

Two controls:

```toml
auto = false     # manual only: background runs leave this deployment alone
```

Intent is the file's mtime. Touching or editing the spec — which is what
the dashboard's *deploy now* button does — always converges it, `auto`
notwithstanding. And a deployment whose last attempt **failed** backs off
for ten minutes instead of re-running an expensive build every beat.

The third pattern — CI pushing over ssh with `ply deploy` — still works and
still has its place: when CI must be the decider (test-gated deploys,
centrally orchestrated fleets). See [Deploys, health & restarts](/docs/deploy/).

## Private repos: one credential

A fine-grained personal access token with **Contents: read** on that one
repo covers everything: `https` clones for the build lane, release
downloads for `github:`, and the dashboard's update checks.

```toml
repo = "https://github.com/org/private-app"
token_file = ".keys/private-app.token"      # root-owned file, 0600
```

Relative `token_file` and `deploy_key` paths resolve against the
deployments dir. The token is injected into git per-invocation — it never
lands in `.git/config`. An SSH `deploy_key` remains supported for the
build lane.

## Building on the host

Builds run in a throwaway, memory-fenced container (a real ply app
named `<name>-builder` — it shows up in `ply ps` and the dashboard, and its
log ring is the build log). The checkout persists between builds, so
`node_modules` and framework caches *are* the cache.

Measured on the smallest DigitalOcean droplet (1 vCPU / 512 MB / $4), with
2G swap, while serving other apps: a Next.js cold build in 209s,
incremental rebuilds in 80s, and the running apps never stuttered — the
fence kept the builder at low CPU weight and ~60% of RAM.

Rules of thumb: Go, Python and static sites build fine on 512 MB;
JavaScript wants `ply setup --swap 2G` (ply refuses a JS build on a small
host without swap, and tells you the fix); Rust belongs in CI — build it
there and deploy with `from`.

## What the host reports back

- `deployments/.status/<name>.status` — one JSON line per deployment: the
  last reconcile verdict (`deployed …`, `building @ <commit>…`,
  `unchanged (…)`, or the failure).
- `<apps>/events.log` — an append-only journal of deploys, scales,
  restarts, and crash respawns. `tail -f` it, or read it in the dashboard.

## Spec reference

| key | meaning |
|---|---|
| `from` / `repo` | the source — exactly one. `from` takes a registry ref, a path, a URL, or `github:org/repo`; `app`/`image`/`url`/`github` remain as older spellings |
| `version` | registry/github lanes: exact pin, prefix follow, or blank = latest |
| `asset` | github lane: app name in `<asset>-<ver>-linux-<arch>.img`; default = deployment name |
| `ref` | repo lane: branch or committish; default = remote HEAD |
| `build`, `runtime` | repo lane: build command + toolchain keg (`node@24`) |
| `entrypoint`, `include`, `port` | repo lane, when the repo has no `ply.toml` |
| `token_file` / `deploy_key` | private-repo credential (PAT file / SSH key) |
| `publish`, `domain`, `env`, `env_file`, `scale`, `after` | passed through to `ply run` |
| | a relative `env_file` (`.env/site.env`) resolves against the deployments dir |
| `grant_links` | mount the `[requests]` links the image asks for (dashboard-style apps) |
| `auto` | `false` = converge only when the file is touched |
| `source` | registry override for the `app` lane |

## Shared env files

Secrets never belong in a spec — a fleet repo is at its best public.
The convention: env files live in `deployments/.env/<name>.env` (0600,
host-local, never synced), and specs carry the *reference*:

```toml
env_file = ".env/site.env"
```

Relative paths resolve against the deployments dir, exactly like
`token_file` and `deploy_key`. Several apps sharing one file is the
point — a stack's database password is written once. The dashboard has
an editor for these (with a *save & apply* that touches every
referencing spec so the apps restart onto the new values); over ssh,
`vi` and `touch` do the same job.

## GitOps fleet

One host is a deployments dir; a fleet is that dir **synced from git**:

```sh
sudo ply setup --fleet git@github.com:you/infra.git          # this host = $(hostname)
sudo ply setup --fleet … --fleet-host web-2 --fleet-key /root/.ssh/fleet_ro
```

The repo's layout is the fleet's desired state:

```
infra/
  shared/            # every host gets these
    notify.toml
  hosts/
    web-1/
      plybox-web.toml
    web-2/
      api.toml
      postgres.toml
```

Every reconcile beat pulls the repo and applies `shared/` + `hosts/<host>/`
into the deployments dir — content-compared, so an unchanged file is never
rewritten and the mtime-is-intent rules (`auto = false`, deploy-now) keep
working. Git manages exactly the files it introduced: deployments created
locally or from the dashboard coexist untouched. Remove a file from the
repo and the app retires on the next beat; sync state lands in
`deployments/.status/fleet.json` and on the dashboard's deploy page.

What falls out:

- a fleet-wide change is a **pull request** — the diff you review is the
  diff production executes
- rollback is `git revert`; history is `git log`
- a new server is three commands: create it, `install.sh`,
  `setup --fleet` — it converges to its folder within a minute
- there is **no control plane**: each host holds one read-only deploy key;
  no machine anywhere can command the fleet
- a git outage changes nothing: hosts keep converging on what they have

Secrets never enter the repo: specs carry references
(`token_file = ".keys/app.token"`), the keys live on the host.

## Stacks

An app plus its database plus its cache is not a special object — it is
**several deployment files that reference each other**:

```toml
# shop-db.toml                      # shop.toml
app = "postgres"                    repo = "https://github.com/you/shop"
stack = "shop"                      stack = "shop"
publish = ["internal:5432"]         after = ["postgres"]
                                    publish = ["internal:3000"]
[env]                               [env]
POSTGRES_PASSWORD = "…"             POSTGRES_PASSWORD = "…"
```

`after` waits for the service to be healthy. Wiring is a line you write:
`DATABASE_URL=postgres://postgres:$PW@todos-db.ply:5432/todos` says what
talks to what, in the file, without depending on ply and the app having
picked the same variable name (they injected `POSTGRES_HOST` here; an app
reading something else would find nothing and quietly run without a
database). `stack` is a label:
members render grouped in the dashboard. Because a stack is just files,
it inherits everything files already have: per-member rollback and
freshness, fleet sync, git review.

The dashboard's wizard does the wiring for you: a **"needs a database?"**
step creates `<name>-db` / `<name>-cache` alongside your app, generates
the shared password, and writes the `after` line. One pass, one running
stack. (One instance of each service per host — two stacks wanting
their own postgres is a planned refinement.)
