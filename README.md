# ply

**npm for containers.** Your app is a package, its OS-level dependencies are
packages, an image is a resolved lockfile, and the runtime is a boring static
binary that mounts the closure and execs.

ply does not compete with Kubernetes; it competes with *"untar and run."*
Think "the SQLite of containers": one static binary, no daemon, no registry
server, zero config — and an image is a single file you can `scp` around.

```toml
# ply.toml
[package]
name = "myapp"
entrypoint = ["node", ".next/standalone/server.js"]
include = [".next/standalone/"]

[dependencies]
base = "alpine@3.20"
node = "24"

[ports]
web = 3000
```

```sh
ply build .                 # resolve deps, write ply.lock, emit myapp-0.1.0-linux-x64.img
scp myapp-*.img server:     # the deploy artifact is one file
ssh server ply run myapp-0.1.0-linux-x64.img
```

No Dockerfile, no daemon, no `docker push`. Dependencies are fetched by
content hash on first run and shared across every app on the machine.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/iluxav/ply/main/install.sh | sh
```

Rootless containers on Ubuntu 24.04+ need a one-time `sudo ply setup`
(AppArmor user-namespace profile — same requirement as Docker/Chrome).

## How it works

> **Cargo writes it down · Go picks the versions · Nix stores and verifies
> the bytes** — then squashfs + overlayfs + namespaces runs the result.

- **Manifest & lockfile (from Cargo):** `ply.toml` is human intent,
  `ply.lock` is machine truth. Deploys never resolve; upgrades are
  deliberate acts.
- **Resolution (from Go modules):** Minimal Version Selection — the *lowest*
  version satisfying all constraints. Deterministic, no SAT solver. One
  version per package name per graph.
- **Storage (from Nix):** a package *is* its sha256. The store at
  `/var/lib/ply/store/sha256:<hash>/` is content-addressed and immutable;
  transport is untrusted by design — wrong bytes fail the hash.
- **Layout (from Homebrew):** every package owns a private prefix
  (`/opt/node-24.6.0/`); conflicts are impossible by construction. Exactly
  one *base* package owns `/`.
- **Runtime:** loop-mount squashfs layers read-only, stack them with
  overlayfs (deps below dependents), enter namespaces, pivot_root, drop all
  capabilities, seccomp, cgroups v2, exec. Containers behave like
  *processes, not pets*: foreground, SIGTERM works, logs are stdout, exit
  codes propagate.

## Layers as dependencies, not build cache

Docker's layers are anonymous positional diffs — a build cache that leaked
into a distribution format. ply's layers are **named, versioned, semantic
dependencies**: order-independent, substitutable, auditable.
`ply rebase app.img --runtime node@24.6.1` patches the runtime under an app
without rebuilding it — fleet security patching as a metadata operation.

## Where packages come from

Any dumb file host is a registry — sources are URL templates, fetch is
zero-API (`https GET` + hash check):

```toml
[sources]
default = "https://artifacts.example.com/ply"     # or github:org/repo, file:///srv/pkgs
```

- **`apk2pkg`** mechanically converts Alpine's ~30k musl packages
  (`apk2pkg ffmpeg` → `ffmpeg-6.1.1-linux-x64.img`, dependency closure
  vendored into the keg).
- **`ply import docker://nginx`** flattens any OCI image into a
  self-sufficient ply image — the one-way ecosystem bridge.
- **`ply craft`** authors packages interactively: shell into a base,
  `apk add` what you need, `ply craft commit` packs the diff as a package.
  `ply craft edit pkg.img` resumes the session on any machine — the image
  *is* the state.
- **`ply bundle`** flattens an app + closure into one fat image for
  airgapped deploys (zero fetches at run).

## Day-2 is files, not a daemon

```sh
ply ps                        # instance state = JSON files in /run/ply
ply run --scale 3 app.img     # 3 netns, 3 IPs, all really binding :3000
curl http://app.ply:3000      # managed /etc/hosts names (never ports)
ply lb app --format nginx     # emit LB config — ply never proxies traffic
ply systemd app.img           # emit a unit — supervision is systemd's job
ply exec app sh               # setns, same rights-stripping as the app
ply gc                        # store prune: reachability = lockfiles
ply check app.img --against policy.toml   # fleet runtime policy, pure function, CI-able
```

`rm -rf /var/lib/ply` is a factory reset. Nothing else on the machine is
ever touched.

## What ply deliberately does NOT build

No daemon. No registry server. No Dockerfiles. No build-cache DAG. No
install hooks, ever. No orchestrator. No proxy (emit config for Caddy/nginx
instead). No volume drivers. Every "no" is a thing that can't break at 3am.

## Status

Working: build, MVS resolution, lockfiles, fetch-by-hash, run (rootful +
rootless), security tiers 1–3 + cgroups, netns networking + `--scale`,
volumes, exec, gc/rm/audit/outdated, host policy + sync, systemd/LB
emitters, OCI import, bundle, rebase, craft, apk2pkg. Linux only,
x86_64 + aarch64.

Pre-1.0: format and CLI may still change. See `TASKS.md` for the roadmap.

## Development

```sh
make check      # fmt + clippy -D warnings + tests
make install    # build static musl binary → /usr/local/bin/ply
```

Demos resolve from the official registry at
`https://registry.plybox.sh` (see `scripts/apk-catalog.mjs` and
`scripts/registry-push.mjs` for the publish pipeline).

The integration tests spin a fake registry with `python3 -m http.server` and
run the full resolve→fetch→verify path offline in milliseconds.
