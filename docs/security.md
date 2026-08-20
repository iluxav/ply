---
title: Security & rootless
description: Secure by default — namespaces, dropped capabilities, seccomp, cgroups — and a first-class rootless mode.
section: Guides
order: 15
---

# Security & rootless

ply is **secure by default with no knobs**. Every instance gets the full
treatment; weakening would be an explicit, manifest-visible act.

## The three tiers (+ cgroups)

**Tier 1 — namespaces.** Mount, PID, UTS, IPC, network, user, and cgroup
namespaces. With user namespaces, root inside the container is an
unprivileged uid outside.

**Tier 2 — rights stripping.** All capabilities dropped;
`no_new_privs` set; the rootfs is read-only squashfs *by construction*;
scratch space is `noexec,nosuid`; `/proc` is masked; `/dev` is ~10 minimal
nodes.

**Tier 3 — seccomp.** A syscall filter blocks the dangerous surface
(mount, ptrace, kexec, bpf, …).

**cgroups v2.** Memory, CPU, IO limits from `[resources]` — and `pids.max`
is *always* set, so a fork bomb is contained with zero configuration:

```toml
[resources]
mem  = "512M"
cpu  = "1.5"
pids = 256
```

## Running as a non-root user

```toml
[package]
user = "appuser:1000:1000"    # name:uid:gid
```

ply creates the passwd/group entries, chowns the app's volumes, and drops
to the user in the correct order (capability bounding while still root →
setuid → no_new_privs → seccomp).

## Rootless mode

ply runs fully unprivileged: build, fetch, run, scale, exec — no root, no
setuid helpers. The store lives in your home directory; squashfs images
extract to plain directories when loop-mounting isn't available (same hash
identity either way).

On Ubuntu 24.04+ the kernel restricts unprivileged user namespaces; ply
needs an AppArmor profile (the same requirement Docker and Chrome have):

```sh
make install-apparmor       # or see `ply setup`
```

## Host preparation

```sh
sudo ply setup
```

One-time and idempotent: creates the store, the bridge, and the hosts-file
management. The installer runs it automatically when installed as root, and
prints the hint only when a host actually needs it.

## Secrets

Never in the image — it's a file that gets copied around. Pass secrets at
run time from a root-only file:

```sh
ply run --env-file /etc/myapp/secrets.env myapp.img
```

## Supply-chain posture

- Packages are **inert**: no install hooks, no scripts, ever. Activation is
  mount + environment composition.
- Transport is untrusted by design: fetches verify sha256 against the
  lockfile; a compromised mirror can only cause a loud failure.
- `ply audit` reports the risk surface that does exist: shared volumes,
  deprecated runtimes.

## What's out of scope (v1)

SELinux/AppArmor per-app profiles (planned as apply-if-present),
gVisor/Firecracker isolation (the `isolation = "ns" | "vm"` seam exists in
the design), and multi-tenant hosting of mutually hostile workloads —
containers share a kernel; that weight class needs microVMs.
