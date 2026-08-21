---
title: ply vs Docker
description: An honest comparison — different mental models, where each wins, and when to use which.
section: Concepts
order: 31
---

# ply vs Docker

An honest comparison. Short version: **Docker is a universal container platform; ply is a deliberately smaller tool for a specific shape of deployment** — one team, a handful of Linux hosts, apps that are "a runtime + some files + a port." Docker's breadth is real, and so is the cost of carrying it when you don't need it.

## Different mental models

|                  | Docker                                                                                                                                | ply                                                                                                                                                                                                                                                                  |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Core metaphor    | Ship a machine (a filesystem you built imperatively)                                                                                  | Ship a package (a manifest you declared, resolved to a lockfile)                                                                                                                                                                                                     |
| Layers are       | Build-cache diffs: anonymous, positional, order-dependent                                                                             | Dependencies: named, versioned, order-independent, substitutable                                                                                                                                                                                                     |
| Layer model      | A **chain** — each layer diffs against the specific layer below it; the stack is authored and fixed at build                          | A **composition** — you declare an unordered set; the resolver derives the stack deterministically, so any layer is swappable                                                                                                                                        |
| Assembled        | At build time: the composition is baked into the artifact; changing the base invalidates every layer above it (rebuild → push → pull) | At run time: the image carries a parts list (lockfile); parts stay separate store files, so substituting one is a metadata edit (`rebase`) — no rebuild cascade. Which parts is still pinned by hash: substitution is always an explicit act, never runtime guessing |
| Image identity   | Tag (mutable) or digest (opaque)                                                                                                      | Content hash — the store dir *is* the sha256; filename is the claim, hash is the proof                                                                                                                                                                               |
| Build recipe     | Dockerfile (imperative script, hidden cache state)                                                                                    | `ply.toml` + MVS resolution (declarative; rebuild is byte-identical)                                                                                                                                                                                                 |
| Runtime          | Daemon (`dockerd` + `containerd` + shims)                                                                                             | One static binary; the kernel is the only "daemon"                                                                                                                                                                                                                   |
| Distribution     | Registry protocol (registry server required)                                                                                          | Any file host: HTTPS GET + hash check; GitHub Releases is a registry                                                                                                                                                                                                 |
| Version upgrades | Rebuild image, re-push, re-pull                                                                                                       | `ply rebase app.img --runtime node@x.y.z` — metadata operation                                                                                                                                                                                                       |

## Where ply wins

**Startup latency and footprint.** Measured on the same machine, same kernel, both rootful: `ply run` 67 ms vs `docker run` 167 ms per container — with no daemon running between invocations. The binary is 4.7 MiB; a clean install touches exactly one file.

**Determinism.** Same source dir → byte-identical image, always (sorted entries, zeroed timestamps, fixed ownership). Same manifest → same lockfile → same resolution, forever. Docker builds are famously time- and cache-dependent; `RUN apt-get update` alone makes two builds differ.

**The artifact is a file.** `scp app.img server && ssh server ply run app.img` is the whole deploy. No registry to run, no login, no push/pull protocol. Dependencies fetch by hash on first run from any dumb file host — and the transport is untrusted by design (wrong bytes fail the hash), so a compromised mirror can't hurt you.

**Shared dependencies, smaller apps.** A Next.js app image is ~15 MiB because node and alpine are *references*, not copies; ten Node apps on one host share one node keg in the store. Docker layer sharing achieves some of this but only when images happen to share base-layer history.

**Security posture by default.** All capabilities dropped (Docker keeps ~14 by default, including SETUID/CHOWN/NET_RAW history), `no_new_privs`, seccomp, `pids.max` always set (fork bombs contained with zero config), noexec scratch. Weakening would be an explicit, manifest-visible act.

**Cheap substitution → cheap rollouts.** Because assembly happens at run and parts are content-addressed, swapping a runtime under 50 apps is 50 metadata edits and one new store file — not 50 image rebuilds. Canary deploys fall out free: run old and new images side by side (each instance has its own IP), `ply lb` emits the mixed backend set. Strict blue-green (atomic traffic flip) works today via bring-up-verify-switch but has no first-class primitive yet.

**Fleet legibility.** State is files (`/run/ply/state/*.json`), policy is a file (`/etc/ply/runtimes.toml`), `ply check` is a pure function usable in CI, `ply audit` reads it all without an agent. `rm -rf /var/lib/ply` is a complete factory reset.

**No 3am surface.** No daemon to hang, no build-cache corruption, no registry outage in the deploy path, no install hooks executing arbitrary code. Every subsystem Docker carries and ply refuses to build is a subsystem that cannot page you.

## Where Docker wins

**The ecosystem — and it's not close.** Millions of prebuilt images; virtually every README on earth says `docker run`. ply's answer is bridges (`ply import docker://`, `apk2pkg`, `ply craft`), which are good but one-way and younger. If your workflow is "grab postgres, redis, grafana off the shelf," Docker is simply where those shelves are.

**Maturity and battle-testing.** Fifteen years of production hardening, CVE response process, an enormous body of operational knowledge, answers on every search page. ply is pre-1.0 with one design team's test coverage.

**Platform reach.** Docker Desktop covers macOS and Windows; ply is Linux-only by scope (macOS dev means a VM or remote host).

**Compose and orchestration on-ramps.** `docker compose up` for multi-service dev environments is genuinely excellent, and the same images carry to Kubernetes when you outgrow a host. ply deliberately stops at one host — if you need overlay networks, service discovery across hosts, or an orchestrator, ply's answer is "that's not this tool."

**Build caching for slow builds.** Docker's layer cache (and BuildKit's graph) shines when builds are expensive — big compiles, multi-stage toolchains. ply has no build cache by design; it assumes your own toolchain (`npm run build`, `cargo build`) produced the files and packaging them is cheap. True for most server apps, not for all.

**Dev-container tooling.** IDE integrations, devcontainers, Testcontainers, CI services with Docker baked in. ply has `--link` bind mounts and a fake-registry story, but nothing approaching that tooling gravity.

**Dynamic runtime features.** Live `docker commit`, `docker cp`, pause/checkpoint, runtime plugins, GPU passthrough conventions, log drivers. ply's runtime is intentionally minimal: foreground process, stdout logs, systemd for supervision.

## Honest failure modes of each

- **Docker's:** the daemon as single point of failure; mutable tags (`:latest` bit someone at every company); build works-on-my-machine drift; the security defaults you meant to tighten; the registry you now operate.
- **ply's:** young code; ecosystem bootstrap depends on bridges; one-version-per-name is a hard rule (two apps needing two ffmpeg majors on one graph must vendor); no cross-host story at all, ever, by charter.

## When to choose which

**Choose Docker when:** you need off-the-shelf images daily; your team spans macOS/Windows; you're headed to Kubernetes; builds are expensive enough to need caching; you rely on the surrounding tooling (compose, devcontainers, Testcontainers).

**Choose ply when:** you have 1–5 Linux hosts and resent the weight; deploys should be `scp` + run; you want deterministic, auditable artifacts with a lockfile; CI needs hermetic sandboxes that start in tens of milliseconds; you're an appliance/edge vendor where a daemon is unacceptable; you're sandboxing thousands of cheap throwaway executions.

**Use both:** `ply import docker://` exists precisely so Docker's ecosystem feeds ply's runtime. Build with the world's images; run with a single static binary.
