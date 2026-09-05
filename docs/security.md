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
either way). A rootless run gets its own network namespace, but every instance of that
run shares it — so there are still no per-instance IPs, and `--scale` needs
[`--publish`](/docs/running/): the run parent gives each instance its own
loopback port and balances the published one.

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

Never in the image — it's a file that gets copied around, and its manifest is
readable with `unsquashfs` in one command. `[env]` in `ply.toml` is therefore
the wrong place for a password. `ply build` refuses to pack credential-shaped
files that were swept in implicitly (see [CLI reference](/docs/cli/#build-validate)).

Pass secrets at run time from a root-only file:

```sh
ply run --env-file /etc/myapp/secrets.env myapp.img
```

**Env-file format.** `KEY=VALUE` per line; `#` starts a comment only at the
beginning of a line; blank lines are ignored. The value is trimmed, and one
matched pair of surrounding quotes is removed — so `PW="s3cret"` delivers
`s3cret`, and quoting is how a value *keeps* deliberate spaces
(`PW="  padded  "`). Splitting is on the **first** `=`, so a URL with `=` in
its query survives. A leading `export ` is an error, not a weird key name:
this is an env file, not a shell script.

On a host, a deployment's `env_file` holds the values while the spec holds
only the *reference* — which is what keeps a fleet repo publishable. For a
single-app spec, a file named `.env/<deployment>.env` is picked up
automatically; a stack file names its own with `[stack] env_file`, and a
stack *reference* takes `env_file` beside `stack =`. See
[Deployments & CD](/docs/deployments/).

## Egress: the contract

An instance may only reach what its author declared and its operator
allowed — and ply can show, at any time, what it actually reached. Four
pieces:

- the manifest's `[network] egress` is the **claim**, published with the
  image and visible on the registry page;
- the stack file's `egress = { mode, allow }` (or `ply run
  --egress`/`--egress-allow`) is the operator's **policy** — the mode,
  and optionally a list that replaces the claim;
- the runtime **enforces** the effective list inside the instance's own
  network namespace, as one nftables table per instance;
- an **audit log** is written in every mode but `off`, and violations
  become events.

Supply-chain compromises phone home from inside containers — npm, PyPI, a
hijacked maintainer. Docker users need Falco, Cilium, or a vendor to see
or stop that; ply owns each instance's network, so it does it by
construction.

### The claim

```toml
[network]
egress = ["api.stripe.com", "*.amazonaws.com", "140.82.112.0/20", "1.1.1.1"]
```

| entry | matches |
|---|---|
| `host.example` | that name exactly (case-insensitive, trailing dot ignored) |
| `*.example` | any name ending in `.example`, not `example` itself |
| `1.2.3.4` | that IPv4 address |
| `10.0.0.0/8` | that IPv4 range |
| `*` | everything — "unrestricted"; `ply up --plan` and `ply run` print `ply: <app> declares unrestricted egress` |

Anything else — a URL, a port suffix, IPv6 — is a manifest error at
`ply build`, `ply up`, or `ply check`, naming the entry and the accepted
forms:

```
egress entry `https://api.stripe.com`: not a destination — expected host.example, *.example, 1.2.3.4, 10.0.0.0/8, or *
```

`egress = []` is a valid claim — "this talks to nobody". The *absence* of
`[network]` is not a claim: `ply inspect` shows `egress: not declared`
(an empty list shows `egress: none (declared)`).

The claim is the author's word about the app itself, and it does not
cover what an operator wires up around it. The `postgres` keg declares
`egress = []` and means it — but its `BACKUP_DEST`/`BACKUP_RESTORE`
backups push and pull through rclone, which is outbound traffic that
claim does not include. An operator using them says so in the stack file,
next to where the destination is configured:

```toml
[[app]]
run    = "postgres@17"
e      = ["BACKUP_DEST=s3:my-bucket/pg"]
egress = { mode = "audit", allow = ["s3.eu-central-1.amazonaws.com"] }
```

Left undeclared, a backup shows up as undeclared traffic in `audit` and
fails in `enforce` — the contract working, not a bug in the keg.

### The policy

```toml
[[app]]
run    = "postgres@17"
egress = { mode = "enforce" }                     # enforce the keg's declared list
egress = { mode = "enforce", allow = [] }         # override: nothing at all
egress = { mode = "audit",   allow = ["*.stripe.com"] }
egress = "off"                                    # shorthand for { mode = "off" }
```

`allow`, when present, **replaces** the manifest's list for that member.
`mode` is one of `off`, `audit`, `enforce`. A standalone run takes the
same two knobs: `ply run --egress <mode> --egress-allow <entry>`
(repeatable; any occurrence replaces the manifest list).

### Effective policy

| manifest `[network] egress` | operator `egress` | effective mode | effective list |
|---|---|---|---|
| absent | absent | `off` | none |
| present | absent | `audit` | the manifest's |
| absent | `{ mode = M }` | M | none |
| present | `{ mode = M }` | M | the manifest's |
| any | `{ mode = M, allow = L }` | M | L |

The operator's word wins, the author's claim fills in. Shipping a claim
never changes what an app can do on its own — the default is `audit`,
which only makes what it does visible.

### Always allowed, in every mode

- loopback, where the forwarder listens;
- the bridge subnet: stack members, `<name>.ply`, and the host's
  `internal` published ports;
- established and related traffic;
- UDP port 53 to the forwarder's upstream resolvers, from the
  forwarder's own fixed source port. An app that queries an upstream
  resolver directly instead of going through the forwarder is treated
  like any other destination: logged, and blocked in `enforce`.

What the bridge-subnet accept costs, plainly: `10.77.0.0/16` reaches
**every other ply instance on this host**, and through `10.77.0.1`
**anything the host itself listens on `0.0.0.0` or on that gateway
address**. A member's contract is only as strong as its neighbours and as
the host's own listeners — an enforced member next to an unenforced one
that will proxy for it is not contained, and neither is one on a host
running a resolver or an HTTP proxy on `0.0.0.0`. So the claim that
`enforce` closes DNS as an exfiltration channel holds only when nothing
of that sort listens there, and (as below) only outside the subdomains of
a wildcard the policy itself declares. `ss -lntu | grep -v 127.0.0.1` on
the host is the audit.

### The three modes

- **`off`** — no thread, no table; today's behavior, unchanged.
- **`audit`** (the default once something is declared) — resolves and
  forwards every query, pins declared names, logs what's declared and
  what isn't, blocks nothing.
- **`enforce`** — refuses DNS for undeclared names (an instant `REFUSED`,
  not a 30-second hang) and drops undeclared connections at the table.

A declared name's answers are pinned into the table for their TTL (floor:
five minutes) and re-pinned on every re-resolution — delete + add in one
batch, because a plain `add` does not refresh an element's timeout on
kernels before 6.10. A declared name that resolves to a **private or
link-local address is not pinned** unless the address is also declared:
loopback, `169.254.0.0/16` (the cloud metadata service), RFC 1918, CGNAT,
multicast, broadcast, `0.0.0.0/8`, and the bridge subnet. Otherwise a
declared `*.nip.io` would be DNS rebinding through your own allow list.
The lookup is still logged; only the pin is withheld.

### Capabilities that void the contract

`enforce` refuses to launch an app that keeps `CAP_NET_RAW`,
`CAP_NET_ADMIN` or `CAP_SYS_ADMIN`, or that runs `--privileged`: with a
raw socket the app writes its own packets (and can forge the forwarder's
source port), and with either admin capability it can read, edit or flush
the very table that holds the policy.

```
egress policy: enforce needs an app without CAP_NET_RAW/CAP_NET_ADMIN — web keeps CAP_NET_RAW (capabilities = "oci" keeps CAP_NET_RAW); list capabilities without it, or run with --egress audit
```

This bites imported images: `ply import` marks them `capabilities =
"oci"`, and Docker's default set includes `CAP_NET_RAW`. **An image with
`capabilities = "oci"` cannot run under `enforce` until its capability
list drops NET_RAW** — replace the preset with the explicit list the app
actually needs (a database does not ping). `audit` runs anyway, with one
warning:

```
ply: warning: egress: web keeps CAP_NET_RAW — an app with it can bypass observation
```

### When enforcement cannot start

Installing the table needs a bind on `127.0.0.53:53` and the `nft`
binary — nftables 1.0 or newer must be on the host (`apt install
nftables`) for either mode to observe anything. The two modes disagree on
purpose about what a failure there means:

- **`enforce`** — the launch itself aborts, with the failure's reason.
  Missing `nft` prints exactly:
  ```
  egress policy: nft not found — install nftables or run with --egress audit
  ```
- **`audit`** — a warning, and the instance still starts, with the
  host's own resolver and no observation at all: `ply: warning: <reason>
  — running unobserved`, e.g.:
  ```
  ply: warning: egress policy: nft not found — install nftables or run with --egress audit — running unobserved
  ```
  Audit promises observation, never containment, so losing the thread
  never blocks the app from starting.

Rootless has no per-instance network, so a non-`off` effective policy
prints one warning and runs exactly as it did before this feature:

```
ply: egress policy needs a network per instance — rootless runs unenforced and unobserved (use a rootful host to audit or enforce)
```

A backend that cannot keep the contract (the macOS VM backend, for now)
prints its own warning and runs unobserved instead:

```
ply: egress policy is not enforced on this platform yet — running unobserved
```

### `ply egress <app>`

```sh
ply egress web [--follow] [--blocked] [--json]
```

A table over every instance's audit log — `DESTINATION`, `NAME`, `PORT`,
`PROTO`, `NEW PKTS`, `FIRST`, `LAST`, `VERDICT`. `--blocked` filters to
blocked connections and refused names; `--follow` tails new records;
`--json` prints the raw log lines instead. The log itself is
`/var/lib/ply/egress/<app>.<n>.log` (`~/.local/share/ply/egress/`
rootless), JSON lines, rotated like the log ring.

`NEW PKTS` is **packets in conntrack state `new`, not connections**: only
`ct state new` updates the counters, and a blocked TCP connect
retransmits its SYN several times, so one refused connection shows up as
several packets. On a `refused` row it is the number of queries that
record stands for — the `resolved` and `refused` records are damped to
one a minute per name, so an app that resolves in a loop cannot rotate
the connection evidence out of the log.

### Events

The first `blocked` record for a destination emits an `egress-blocked`
event, then at most once an hour per destination. In `audit` mode the
same destination emits `egress-undeclared` under the same throttle, so
the events journal shows what *would* be blocked before anyone flips the
switch.

### Limits (v1)

- No payload or URL inspection — TLS hides them, and ply never
  intercepts certificates.
- No per-package attribution inside a container; the instance is the
  unit.
- No IPv6 policy: `enforce` refuses IPv6 egress outright, `audit` logs
  nothing for it.
- No ingress policy — published ports already state exposure.
- No host-wide policy defaults yet.
- No rootless enforcement or audit (see above).

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
