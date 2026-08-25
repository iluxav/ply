---
title: What is ply
description: ply is a daemonless container runtime and package manager — npm for containers, in one static binary.
section: Start
order: 0
---

# What is ply

**ply is npm for containers.** Your app is a package, its OS-level dependencies
are packages, an image is a resolved lockfile, and the runtime is a boring
static binary that mounts the closure and execs your entrypoint.

```sh
curl -fsSL https://plybox.sh/install.sh | sh
```

There is no daemon, no registry server, no Dockerfile, and no build cache.
An image is a single deterministic file you can `scp` around. Any file host —
GitHub Releases, an S3/R2 bucket, a plain directory — is a fully working
package registry.

## The mental model

ply borrows one proven mechanism from each of four tools:

| Borrowed from | What ply takes |
|---|---|
| **Cargo** | TOML manifest (`ply.toml`), flat dependency list, lockfile (`ply.lock`), no install scripts ever |
| **Go modules** | Minimal Version Selection — deterministic resolution, no solver, upgrades only when *you* change the manifest |
| **Nix** | A package *is* its content hash; names are claims, hashes are proof |
| **Homebrew** | Each package owns its own prefix (`/opt/<name>-<version>/`) — conflicts impossible by construction |

At run time these compose: squashfs images loop-mount read-only, overlay
stacks them in dependency order, namespaces + seccomp + cgroups isolate the
process, and your app runs as an ordinary child process. `Ctrl-C` stops it.
`ply ps` reads state files. Nothing is resident.

## Where it shines

- **1–5 VPSs, small teams** — deploys are `scp` + one command; the whole
  platform is a 5 MiB binary
- **CI sandboxes** — hermetic, content-addressed, offline-capable
- **Edge / IoT** — no daemon to babysit, factory reset = `rm -rf /var/lib/ply`
- **AI-agent sandboxing** — thousands of cheap, isolated, throwaway executions

## Where to go next

- [Quickstart](/docs/quickstart/) — running your first app in five minutes
- [Using ply with AI agents](/docs/agents/) — a skill file so your agent stops writing Dockerfiles
- [Dependencies & lockfiles](/docs/dependencies/) — how resolution works
- [Registries & publishing](/docs/registries/) — shipping images anywhere
- [ply vs Docker](/docs/ply-vs-docker/) — an honest comparison
- [Glossary](/docs/glossary/) — the words ply uses, and the ones it bans
