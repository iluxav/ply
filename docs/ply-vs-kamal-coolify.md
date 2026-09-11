---
title: ply vs Kamal vs Coolify
description: An honest comparison for deploying to your own servers — where each fits, and where ply is still behind.
section: Concepts
order: 32
---

# ply vs Kamal vs Coolify

If you deploy your own apps to a handful of Linux servers you rent, these
three are the honest field. Docker and Kubernetes are a layer below and
above; this is the middle, "I have a VPS or five and an app to run."

Short version:

- **Kamal** deploys your Docker containers to your servers over SSH. A CLI, no UI, battle-tested (it runs 37signals' production). It needs Docker on the hosts and a registry to ship through.
- **Coolify** is a self-hosted PaaS: a web UI, git-push deploys, a big one-click service catalog, databases with scheduled S3 backups. A control-plane app you install, built on Docker, with a large community.
- **ply** is a daemonless runtime and package manager: no Docker, one static binary, a manifest resolved to a lockfile into a deterministic image, supervised by systemd. It has a dashboard, sealed secrets, snapshots, notifications and an egress contract, and it is the youngest and smallest of the three by a wide margin.

They overlap on the goal, deploy to your own boxes, and differ on almost
everything about how.

## At a glance

|  | Kamal | Coolify | ply |
|---|---|---|---|
| Shape | CLI | Web PaaS (control plane) | CLI + optional dashboard |
| Needs Docker on the host | Yes | Yes | **No** (own runtime; can import Docker images) |
| Resident service | Docker daemon | Coolify + Docker | **None** (systemd supervises; a single static binary) |
| Runs on a 512 MB box | Not really (Docker daemon) | No — 2 GB minimum, ~1 GB gone before you deploy | **Yes** — nothing resident, so the RAM is yours |
| Deploy trigger | `kamal deploy` (SSH) | git push / webhook / UI | git push (follow-latest), `ply deploy`, or the dashboard |
| Artifact | Docker image (tag/digest) | Docker image | Deterministic image + **lockfile** (byte-identical rebuilds) |
| Ships images via | A registry (Docker Hub / private) | Builds on the host | Any HTTPS host + hash; a curated registry; Docker import |
| Zero-downtime rollout | Yes (kamal-proxy) | Yes | Yes, health-gated; a failed gate reverts the slot |
| Databases | Accessories (you run them) | One-click, many | Registry services (`postgres@17`, …) or `docker://` |
| Backups | You wire them | **Scheduled + S3, one-click** | `ply snapshot`/`restore` any volume; Postgres self-dumps to S3 |
| Secrets | From env / 1Password at deploy | In the UI / env | Env files, or **sealed values committable to the repo** |
| TLS | kamal-proxy (Let's Encrypt) | Built in | Caddy (`--edge`); ply issues none itself |
| Notifications | — | Yes | Yes (Telegram/Discord/webhook/command), on the reconcile beat |
| Outbound policy | — | — | **Declared, enforced per instance, audited** |
| Multi-server | Yes | Yes | Single host today; multi-host is roadmap |
| Web UI | No | **Yes, mature** | Yes, younger |
| Service catalog | — | **Hundreds** | Curated (a dozen), plus Docker import |
| Community / maturity | Large, 37signals-backed | **Large** | **Small, new, one primary author** |

Bold marks where a tool is clearly ahead.

## Build on the box you run on

This is the difference you feel first, on the invoice.

Coolify's own install docs ask for 2 GB of RAM and two cores, and the
control plane uses roughly 1 GB of that before you deploy a single app — so a
512 MB or 1 GB VPS is off the table from the start. Kamal keeps the app
server lighter, but it expects you to build somewhere else (your laptop or
CI) and ship through a registry, because building on a small box under the
Docker daemon is its own headache.

ply has nothing resident. The single static binary runs when you invoke it and
exits; between deploys the only thing alive is your app under systemd. That
changes what a cheap machine can do:

- **The whole box is the build's.** With no daemon holding a gigabyte, a 512 MB VPS can `ply build` a Next.js app — the memory Coolify's control plane would have taken is instead available to the build.
- **Building doesn't disturb serving.** A build is just a process. Your running instances sit in their own systemd cgroups with reserved memory and CPU, so a build you cap or nice can't starve the traffic they're already handling — there is no shared engine for it to choke.
- **No registry in the loop.** ply builds to a single `.img` file in place, so the same small box builds and runs it, no registry hop. You can still copy that file to another host; you just don't have to.

So the machine that costs a few dollars a month is enough to build and serve
a real app, rather than being below the floor a control plane needs just to
boot.

## Where ply is genuinely different

Three things none of the others have, because they all sit on Docker:

- **A lockfile and a shared store.** Dependencies are declared and resolved to pinned hashes, so a rebuild is byte-identical and every app on the host shares one copy of, say, Node on disk. Kamal and Coolify hand you a Docker image and its tag; reproducibility is your problem.
- **No daemon, no Dockerfile.** One static binary; the kernel is the only thing running between deploys. Nothing to patch, a tiny attack surface, and `ply run` starts a container in ~67 ms with nothing resident.
- **The egress contract.** What an app may reach is declared in its manifest, enforced per instance, and audited to a file, with no sidecar. That is a compliance answer the others send you to Falco or Cilium for.

The day-to-day operations story is filled in too: sealed secrets you can commit, volume snapshots with restore, and notifications, all available from the dashboard.

## Where ply is behind — plainly

Not hedged, because pretending otherwise wastes your time:

- **Adoption and community.** Coolify and Kamal have large, active communities and years of production behind them. ply is new, with one primary author. For infrastructure, that track record is a real reason to wait, and a fair one.
- **Service catalog.** Coolify offers hundreds of one-click services; ply curates about a dozen and leans on Docker import for the rest. If you want to click "deploy Plausible, Umami, Ghost, and a dozen others," Coolify is built for that and ply is not.
- **Multi-server.** Kamal and Coolify deploy across many hosts today. ply is single-host; a multi-host story is planned, not shipped.
- **No Windows, and macOS is dev-only** (via a microVM). The production target is Linux.
- **Docker-native ecosystem.** Kamal speaks Docker, so every Docker image, registry and tool works unchanged. ply runs Docker images via import, but its native path is its own packages.

## Choose which

- **Kamal** if you already build Docker images, want a proven CLI, deploy across several hosts, and don't want a UI. It is the least opinionated of the three and the most battle-tested.
- **Coolify** if you want a Heroku-like web experience, a big catalog of one-click apps and databases with backups out of the box, and you don't mind running a control-plane app and Docker under it.
- **ply** if you want one small binary with no daemon, reproducible images from a lockfile, secrets you can commit, an egress policy, and per-host backups and notifications, for one to a few Linux servers, and you're comfortable being early.

The honest test: if your bottleneck is "too many moving parts to run one app safely on one box," ply is aimed exactly at you. If your bottleneck is "I need a mature platform my team already trusts, across many servers, with a huge app catalog," reach for Coolify or Kamal today and check back on ply as it grows.
