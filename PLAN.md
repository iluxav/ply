# PLAN.md — simplification plan

Written 2026-08-29, after a day of building the registry, stacks and a
clean-room DX test that surfaced ten bugs. The finding that matters:

> ply is not complicated because it does too much. It got complicated
> because **the same idea has two meanings depending on privilege.**

Rootful: an app has its own IP, binds its natural port, is reachable at
`<name>.ply`. Rootless: the app is on your laptop's network, binds a port
ply picked for it, and has no name. Those are not two implementations of
one idea — they are two products sharing a CLI, and every feature has to be
designed twice. Essentially every hour lost today went into that seam: the
postgres `PORT` contract, the loopback allocator (twice), the IPv4/IPv6
probe, and `stack.dev.toml` needing to exist at all.

The plan is ordered by leverage. Item 1 deletes more code than it adds.

---

## 1 · Rootless gets its own network — one namespace per stack

**Status: in progress — foundation landed (`ply-core/src/runtime/netns.rs`)**

Done: a namespace ply owns. `NetNs::create()` forks a holder that unshares
a user namespace (for `CAP_NET_ADMIN`), unshares the network, raises `lo`
with an ioctl, reports readiness on a pipe, then parks; the namespace lives
as long as that pid, and `/proc/<pid>/ns/net` is the handle everything
joins through. `enter()` is the per-thread `setns` the publish proxy needs.
**Not proven on this laptop.** The netns tests SKIP here: Ubuntu 24.04+
restricts unprivileged user namespaces and ply's AppArmor profile grants
them to exactly one path (`profile ply /usr/local/bin/ply`), which a cargo
test binary is not. `PLY_NETNS_TESTS=1` turns an unavailable namespace into
a failure — use it where the mechanism is expected to work. The real
verification is `ply up` from the INSTALLED binary.

Step 1 done: an instance can be placed in a namespace. `RunOptions.netns`
carries `/proc/<pid>/ns/net` down to `ContainerSpec.join_netns`, and the
container joins it **first**, before any mounting — the namespace decides
what "localhost" and the app's own ports mean, and nothing later depends on
which network it started in. `None` keeps today's behaviour, so no path
changes until the pieces below land. Tested: two joiners report the same
namespace inode, and the caller keeps its own.

Step 2 done: `--publish` reaches into the namespace. `Dialer` is how the
parent connects to a backend — `Direct` in its own network, `InNamespace`
otherwise, backed by `NsDialer`: a thread that enters once and stays, taking
connect requests on a channel and handing the connected socket back (an fd
moves between threads for free, so splicing stays where it was). `setns` is
per-thread and sticky, so a resident thread is the only shape that works.
`NsDialer::spawn` returns only once the thread is actually inside, so a
caller can never dial into the wrong network. Tested: a listener that exists
only inside a namespace is unreachable via `Direct` and reachable via
`InNamespace`.

**Step 3 implemented; needs the installed binary to verify.** The design
below is now in place: `ply up` makes the namespace, and each `ply run`
binds its published listeners in the caller's network FIRST (a bound socket
keeps working after its process moves), then joins the namespace, then
spawns its accept threads and clones the container — which inherits the
namespace rather than joining it. That ordering is what makes it legal:
a container that has already made its own user namespace could never join
a sibling's. `Dialer`/`NsDialer` are gone with it — a parent that is itself
inside needs no proxy to dial for it.

Verification is blocked on AppArmor, not on the code: with
`apparmor_restrict_unprivileged_userns=1`, only ply's profiled
`/usr/local/bin/ply` gets capabilities in a user namespace, so a build
directory binary fails at `unshare net: EPERM` and cargo tests skip.

Earlier note, kept because it is why the design changed: `ply up` creates the holder, but on this kernel the holder cannot map
itself (`/proc/self/setgroups: EACCES`), so it falls back to the host
network — visibly, and with the reason. Nothing regressed; the members ran
exactly as before, which is why ports were still injected.

Two things the attempt taught, both structural:

1. **Mapping.** ply's working rootless path never writes `setgroups`/`uid_map`
   from inside the child — the PARENT maps the child with the setuid helpers
   `newuidmap`/`newgidmap` over the `/etc/subuid` range (`write_id_maps`,
   which also documents why `setgroups=deny` is deliberately avoided: gosu
   and su-exec need `setgroups` on their way down to a service user). The
   holder must use that same path, not a hand-rolled self-map.

2. **Ownership, the bigger one.** Joining a network namespace needs
   CAP_SYS_ADMIN *in the user namespace that owns it*. Capabilities flow to
   descendants, never to siblings or ancestors — so a container that has
   already created its own user namespace can never join a namespace owned
   by a sibling. `setns` has to happen while the process is still in an
   ancestor of the owning user namespace, i.e. **before** the container's
   own `clone(CLONE_NEWUSER)`.

That points at the rootlesskit shape rather than the one attempted here:
**one user namespace for the whole fleet**, created by `ply up`, owning the
network namespace, with every member's container nested inside it. The
`--publish` listener still binds in the host's network first — a bound
socket keeps working after its process changes namespace — so ingress is
unaffected. `NetNs`, `Dialer`/`NsDialer` and `join_netns` all stay; what
changes is who creates the user namespace and when the join happens.

Next, in order — each step keeps the workspace green:
3. `ply up`: one namespace per stack, handed to every member; `<name>.ply`
   becomes an `/etc/hosts` entry on 127.0.0.1.
4. egress: attach `pasta` at the namespace edge; ship `passt` as a keg.
5. delete the workarounds — `PORT` injection, `allocate_loopback_port`,
   `instance_port_explicit`, `connect_either_family`, the scale guard.

Today rootless shares the host's network namespace ("host network (no .ply
names), no cgroup limits"). Everything below exists only to work around
that, and all of it goes away:

- `PORT` injection (the rootless-only branch in the run path)
- `allocate_loopback_port` — and both bugs fixed in it today
- `instance_port_explicit` as a special case in the publish grammar
- `connect_either_family` (the IPv4/IPv6 probe fallback)
- the rootless scale guard
- the address half of `stack.dev.toml`

**The design.** One network namespace per `ply up` (and per `ply run`):

- members bind their **natural** ports inside it — postgres 5432, an API
  3001, a web app 3000. No injection, no allocation, no negotiation.
- they reach each other on loopback; `<name>.ply` is an `/etc/hosts` entry
  pointing at 127.0.0.1, so **the stack file is identical in dev and prod**
- nothing binds a host port, so the machine's own postgres is irrelevant
- egress: `pasta` (Debian `passt`, what podman uses rootless) attached once
  at the namespace edge
- ingress: `--publish` keeps its meaning; the proxy connects from inside
  via a `setns` helper

**Deliberate constraints.** Rootless runs one of each — scale is a
production concern, and the guard already half-says so. Two members of one
stack cannot share a port (the Kubernetes pod constraint); the dev overlay
covers the rare case.

**Rootful does not change.** It is already the target model, and becomes
the reference the rootless path converges to.

**Stage 2, only if local scale ever matters:** put a bridge and veths
*inside the namespace already created here*. The outer plumbing (pasta, the
setns relay) is unchanged, so nothing built now is wasted.

## 2 · Collapse the deployment lanes, 5 → 2

`app`, `image`, `url`, `github` are four spellings of "a built artifact,
somewhere". One `from =` taking a registry ref, a path, a URL or
`github:org/repo` leaves the two real concepts: **fetch it**, or **build it
here** (`repo`). Adding the `url` lane today is what made the redundancy
obvious.

## 3 · Shrink the magic in wiring

`--after db` injects `DB_HOST`; the app read `POSTGRES_HOST`; the site
served happily and every login answered "nodb". Explicit
`DATABASE_URL=…@db.ply:5432/…` in the stack is more typing and far more
legible — the connection is readable off the file. Keep injection as a
convenience; stop treating it as the wiring mechanism.

## 4 · Scope the registry's identity system; gate version deletion

Users, tokens, namespaces, grants, reserved names, admin logins — a second
product, built in a day. Two concrete follow-ups:

- **the grant bug**: `RESERVED` is used both to *block* squatting and to
  *grant* admins, so an admin now holds `api`, `www`, `docs`, `account`…
  Split the lists (`RESERVED` to block, `OFFICIAL = {ply, apps}` to grant)
  and clean the stray rows.
- **version deletion must be gated on nothing pinning it.** Today's
  registry rebuild orphaned `node 24.6.0` (broke plybox-web in production —
  the pin is unrecoverable, since rebuilding does not reproduce the digest)
  and `postgres 17.10.3` (still pinned in ply-labs' lock).

## 5 · Ops correctness (all found today)

- the rootless extraction cache grows unbounded and `gc` will not reclaim
  it while an app record exists — 17 GB on this laptop
- `ply volume rm` cannot delete the volumes it just listed (rootless
  volumes are subuid-owned; it prints the `sudo rm -rf` escape hatch)
- secrets land in `ExecStart` in plain text, so `systemctl cat` and
  journald see them — pass an env file to the unit instead of expanded `-e`
- instances leak when `ply up` dies abruptly (no cgroup rootless), and the
  leaked listeners then poison the next run's ports
- `i.plybox.sh` 404s without `/install.sh` — add a redirect

## 6 · Protect the core

An app is a package. An image is a resolved lockfile. A deployment is a
file. No daemon. `ply run` is one process. **None of today's ten bugs came
from any of these** — they came from the seams. Rootful networking is the
reference; make everything converge to it rather than adding modes.
