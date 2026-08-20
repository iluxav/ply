---
title: Architecture
description: The four borrowed mechanisms, the ten-step run path, and the discipline of what ply refuses to build.
section: Concepts
order: 30
---

# Architecture

> **Cargo writes it down · Go picks the versions · Nix stores and verifies
> the bytes** — then squashfs + overlay + namespaces runs the result.

ply is deliberately unoriginal. Each mechanism is stolen from a tool that
proved it for a decade; the novelty is only the composition.

## The four mechanisms

**From Cargo: the manifest/lockfile split.** `ply.toml` is what a human
wants; `ply.lock` is what a machine verified. No install scripts —
packages are inert files, activation is mount + env composition.

**From Go: Minimal Version Selection.** The lowest version satisfying all
constraints wins. Deterministic without a solver; resolution changes only
when a manifest changes. Upgrades are deliberate acts.

**From Nix: content addressing.** A package *is* its hash. The store
directory name is the sha256; the filesystem is the database. Trust lives
in the hash, so transport (any file host) is untrusted by design. NOT
taken: the Nix language, source builds, recipe hashing.

**From Homebrew: prefix ownership.** Every package owns
`/opt/<name>-<version>/`. Two packages cannot conflict over a path, which
is what makes layers order-independent and substitutable.

## The run path — the whole runtime

```
1.  read manifest + lockfile from the image
2.  ensure the store has every digest (fetch by hash if missing)
3.  loop-mount the squashfs files, read-only
4.  private mount namespace
5.  overlay: lowerdirs in topological order (deps below dependents), tmpfs upper
6.  PID / UTS / IPC / net / user namespaces
7.  pivot_root; mount /proc, minimal /dev, tmpfs /tmp; bind volumes
8.  cgroup v2 limits
9.  compose env (packages → manifest → CLI; last wins)
10. drop capabilities, no_new_privs, seccomp; execve entrypoint
```

That's it. The "daemon" is the kernel. A tiny init holds PID 1 inside the
container so your app keeps normal signal semantics; state is JSON files
under `/run/ply/`; supervision, TLS, and load balancing are emitted config
for systemd and Caddy — tools that already spent decades getting it right.

## Composition vs stacking

Users declare a *set* of dependencies; overlay needs an *ordered stack* of
directories. The resolver flattens the graph deterministically (ties broken
by digest) so the same lockfile always produces the same stack. This is the
inverse of Docker: there, the stack is authored at build time and baked;
here, it's derived at run from a parts list — which is why swapping a part
(`ply rebase`) is a metadata edit, not a rebuild cascade.

## What ply refuses to build

No daemon. No registry server. No Dockerfiles. No build-cache DAG. No
install hooks. No volume drivers. No overlay networks or multi-host
anything. No orchestrator. No process supervisor. No proxy. No snapshots.

Every "no" is a subsystem that cannot break at 3am, and the discipline is
the product: ply competes with "untar and run," not with Kubernetes. Think
*the SQLite of containers* — one static binary, a file format, and the
promise that boring things stay boring.

## Implementation notes

Rust workspace (`ply-core` library + `ply-cli`), true static musl binaries
for x64/arm64, no async runtime — a CLI that mounts filesystems needs zero
tokio. Blocking I/O, direct syscalls, and a SQLite-grade obsession with
determinism: deterministic squashfs, deterministic resolution, deterministic
layer order. The integration-test registry is `python3 -m http.server` in a
tempdir; the full resolve→fetch→verify path tests offline in milliseconds.
