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

## Capabilities

The default is **none**. Not "a safe subset" — the bounding set is emptied,
so after `execve` the app has nothing, and that is what every package ply
builds should stay on. A native keg never needs `CAP_CHOWN` or
`CAP_SETUID`, because `[package] user` does that work from the parent
*before* stripping.

Two escape hatches exist, both manifest-visible:

```toml
[package]
capabilities = "oci"                          # Docker's default fourteen
capabilities = ["chown", "net_bind_service"]  # exactly these
```

`"oci"` is what [`ply import`](/docs/docker/) writes, because official
Docker images assume Docker's posture — their entrypoints run
`chown -R x:x /data && exec gosu x …`, which needs `CAP_CHOWN` and
`CAP_SETUID`/`CAP_SETGID`. It grants Docker's set and **no more**:
`CAP_SYS_ADMIN`, `CAP_SYS_MODULE`, `CAP_SYS_PTRACE` and `CAP_NET_ADMIN`
stay denied, exactly as under Docker.

The asymmetry is deliberate and worth stating plainly: **imported images
run with Docker's permissions; your own packages run with none.** Adopting
the ecosystem does not mean adopting its defaults for your own code.

A declared port below 1024 additionally keeps `CAP_NET_BIND_SERVICE`
without you asking — a declared port is a promise ply keeps rather than
making you spell it twice.

For debugging there is `ply run --privileged`, which skips all three tiers
and says so loudly on every start. Triage, never production.

## Running as a non-root user

```toml
[package]
user = "appuser:1000:1000"    # name:uid:gid
```

ply creates the passwd/group entries, chowns the app's volumes, and drops
to the user in the correct order (capability bounding while still root →
setuid → no_new_privs → seccomp).

## Rootless mode

ply runs fully unprivileged: build, fetch, run, exec — no root, no setuid
helpers. The store lives in your home directory; squashfs images extract to
plain directories when loop-mounting isn't available (same hash identity
either way). Rootless instances share the host network (no per-instance
IPs), so `--scale` needs [`--publish`](/docs/running/) — the run parent
gives each instance its own loopback port and balances the published one.

Three host-level facts decide how far rootless gets, and `ply setup`
reports all three.

**User namespaces.** On Ubuntu 24.04+ the kernel restricts unprivileged
user namespaces; ply needs an AppArmor profile (the same requirement Docker
and Chrome have). `sudo ply setup` installs it — the installer runs setup
automatically whenever it can escalate.

**A delegated uid range.** A user namespace maps exactly one id by default
— root inside is you outside, and no other uid exists. Anything switching
user (`[package] user`, or an imported image running `gosu`) then fails
with `EINVAL`. The fix is a `/etc/subuid` delegation plus the setuid
helpers that apply it:

```sh
sudo apt install uidmap
sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER
```

ply then maps `1..65536` inside to your delegated range, so service uids
exist and `chown`/`setuid` work. It deliberately does **not** write
`setgroups: deny` on that path — that is irreversible, and `gosu` calls
`setgroups()`.

**Privileged ports.** Rootless shares the host's netns, and
`CAP_NET_BIND_SERVICE` inside a user namespace does not authorize binding
below 1024 out there. Rootless Docker and Podman have the same limitation.
Either bind above 1024 and let the edge own `:443`, or lower the floor with
`sudo ply setup --unprivileged-ports` — host-wide, so it is opt-in and
never applied for you.

## Host preparation

```sh
sudo ply setup                       # AppArmor profile + readiness report
sudo ply setup --unprivileged-ports  # …and lower the privileged-port floor
```

One-time and idempotent: creates the store, the bridge, and the hosts-file
management. It installs what is safe to install and *reports* what is not —
a subuid delegation re-assigns another account's ids, and the port floor is
a host-wide policy change, so both are printed with the exact command
rather than done behind your back. Forwarding and the bridge's NAT rule are (re)applied by every
rootful `ply run`, so a rebooted host needs nothing extra. The installer runs it automatically when installed as root, and
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
