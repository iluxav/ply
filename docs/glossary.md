---
title: Glossary
description: The words ply uses, what each one means, and the naming rule that keeps them consistent.
section: Reference
order: 23
---

# Glossary

One word per concept, used the same way everywhere — docs, CLI output,
error messages. If a page contradicts this glossary, the page is wrong.

## Artifacts

| Term | Meaning |
|---|---|
| **image** | a `.img` file — one immutable, deterministic squashfs |
| **package** | an inert image: files plus a manifest, no entrypoint |
| **app** | a package *with* an entrypoint — something runnable |
| **base** | the one package per graph that owns `/` (FHS, libc, `/bin/sh`) |
| **keg** | a package's private prefix directory, `/opt/<name>-<version>/` |
| **store** | the content-addressed local cache (`store/sha256:<hash>/`) |
| **source** | a URL template dependencies are fetched through |
| **registry** | the file host a source points at — any dumb file host qualifies |

## Running things

| Term | Meaning |
|---|---|
| **instance** | one running copy of an app (`foo.3`) — own IP, own netns, own cgroup |
| **slot** | the numbered position `<n>` in `<app>.<n>`; persists across restarts and anchors volumes and state |
| **scale** | the instance count of an app on a host — `--scale 3` |
| **container** | the isolation *mechanism* wrapped around an instance: namespaces, cgroup, seccomp |
| **process** | the entrypoint PID inside an instance |
| **volume** | a persistent directory bind-mounted into an instance — per-slot by default, shared by opt-in |

The three running-layer words name three different things and are not
interchangeable. An **instance** is the unit of identity — what you scale,
list in `ply ps`, and route traffic to. It runs *in* a **container** — say
"container" when talking about isolation and security, never when counting.
Its entrypoint is a **process** — say "process" when talking about signals,
exit codes, and restarts.

## Traffic and machines

| Term | Meaning |
|---|---|
| **pool** | an app's running instances as a traffic target — what `ply lb` emits backends for |
| **publish** | expose a pool on a real host port: `ply run --publish` makes the run parent bind it and L4-balance the pool (the explicit exception to "ports are labels") |
| **edge** | the proxy app (e.g. Caddy) fronting pools; ply emits its config and never proxies itself |
| **host** | one machine running ply |
| **fleet** | the declared list of hosts — an inventory file, not a membership protocol |
| **deploy** | replacing an app's version by rolling its pool one instance at a time, health-gated |

## Banned words

- **cluster** — industry-wide it means machines under a shared control
  plane. ply has no control plane; machines are a *fleet*, an app's
  instances are a *pool*.
- **node** — implies membership in a coordinated graph. Say **host**.
- **pod / replica / orchestrate** — they drag in semantics ply
  deliberately doesn't have.

## The naming rule

Borrow a word only when ply's semantics match the industry's exactly
(*host*, *pool*, *edge*, *fleet*). Coin a word when the concept is
genuinely ply's own (*keg*, *slot*, *craft*). Ban words that promise a
control plane ply refuses to build (*cluster*, *node*, *pod*).
