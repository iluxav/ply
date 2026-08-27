---
title: Deployments & CD
description: A deployment is a file. Four ways to say where truth lives — registry, image, GitHub releases, or a repo built on the host — and a timer that keeps reality converged. Zero resident processes.
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

Exactly one of four sources per file:

```toml
# 1 · registry — a runnable app from your registry, newest matching version
app = "redis"
version = "8.0"
publish = ["internal:6379"]

[env]
REDIS_PASSWORD = "change-me"
```

```toml
# 2 · image — a file already on this host (CI scp'd it, you built it, whatever)
image = "/srv/deploy/myapp-1.2.0-linux-x64.img"
```

```toml
# 3 · github — your CI builds the image and attaches it to a GitHub release;
#     the host pulls it. `version` blank = follow the latest release.
github = "org/myapp"
publish = ["internal:8080"]
domain = ["app.example.com"]
```

```toml
# 4 · repo — no CI at all: the host clones and builds the repo itself
repo = "https://github.com/org/myapp"
build = "npm ci && npm run build"
runtime = "node@24"
entrypoint = ["node", "dist/index.js"]
include = ["dist/", "node_modules/", "package.json"]
port = 3000
publish = ["internal:3000"]
domain = ["app.example.com"]
```

If the repo carries its own `ply.toml`, lane 4 needs none of the manifest
fields — `repo` plus a `build` command is enough; the repo's manifest rules.

## Continuous deployment is a pull

Both git-backed lanes re-resolve on every reconcile run:

- `github =` resolves the **latest release** (a `version = "1.2"` prefix
  follows patch releases; an exact `"1.2.3"` pins).
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
repo covers everything: `https` clones for lane 4, release downloads for
lane 3, and the dashboard's update checks.

```toml
repo = "https://github.com/org/private-app"
token_file = ".keys/private-app.token"      # root-owned file, 0600
```

Relative `token_file` and `deploy_key` paths resolve against the
deployments dir. The token is injected into git per-invocation — it never
lands in `.git/config`. An SSH `deploy_key` remains supported for lane 4.

## Building on the host

Lane 4 builds run in a throwaway, memory-fenced container (a real ply app
named `<name>-builder` — it shows up in `ply ps` and the dashboard, and its
log ring is the build log). The checkout persists between builds, so
`node_modules` and framework caches *are* the cache.

Measured on the smallest DigitalOcean droplet (1 vCPU / 512 MB / $4), with
2G swap, while serving other apps: a Next.js cold build in 209s,
incremental rebuilds in 80s, and the running apps never stuttered — the
fence kept the builder at low CPU weight and ~60% of RAM.

Rules of thumb: Go, Python and static sites build fine on 512 MB;
JavaScript wants `ply setup --swap 2G` (ply refuses a JS build on a small
host without swap, and tells you the fix); Rust belongs in CI — use lane 3.

## What the host reports back

- `deployments/.status/<name>.status` — one JSON line per deployment: the
  last reconcile verdict (`deployed …`, `building @ <commit>…`,
  `unchanged (…)`, or the failure).
- `<apps>/events.log` — an append-only journal of deploys, scales,
  restarts, and crash respawns. `tail -f` it, or read it in the dashboard.

## Spec reference

| key | meaning |
|---|---|
| `app` / `image` / `github` / `repo` | the source — exactly one |
| `version` | registry/github lanes: exact pin, prefix follow, or blank = latest |
| `asset` | github lane: app name in `<asset>-<ver>-linux-<arch>.img`; default = deployment name |
| `ref` | repo lane: branch or committish; default = remote HEAD |
| `build`, `runtime` | repo lane: build command + toolchain keg (`node@24`) |
| `entrypoint`, `include`, `port` | repo lane, when the repo has no `ply.toml` |
| `token_file` / `deploy_key` | private-repo credential (PAT file / SSH key) |
| `publish`, `domain`, `env`, `env_file`, `scale`, `after` | passed through to `ply run` |
| `grant_links` | mount the `[requests]` links the image asks for (dashboard-style apps) |
| `auto` | `false` = converge only when the file is touched |
| `source` | registry override for the `app` lane |
