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

**DONE — verified 2026-08-30 on this laptop.** A rootless stack runs in its
own network namespace: postgres binds 5432, the API 3001, Next 3000, members
reach each other as `<name>.ply`, and the machine's own postgres on 5432 is
untouched. The dev overlay no longer overrides a single address — only a
password, one host-side port, and local checkouts.

How it fits together, in the order the kernel forces:

1. `ply up` forks a holder that unshares a user namespace and waits; the
   parent maps it with `newuidmap` (a child cannot map itself under
   `apparmor_restrict_unprivileged_userns`), then the holder unshares the
   network namespace and raises `lo`.
2. `ply up` joins the USER namespace only. That is the key: joining a
   network namespace needs CAP_SYS_ADMIN in the namespace that owns it AND
   in the caller's own, and inside the one it owns it has both. The network
   stays the host's, so members can still resolve names and fetch images.
3. Each member inherits that user namespace — keeping capabilities across
   `execve` because inside it they are uid 0 — fetches what it needs, binds
   its published listener OUT HERE, then joins the network namespace and
   launches. A bound socket keeps working after its process moves, so the
   proxy accepts on the host and connects from inside.

Three traps this uncovered, all fixed and each worth remembering:

- `geteuid() == 0` is not "am I root". Inside a user namespace it is true
  while none of root's reach exists — the store picked `/var/lib/ply/store`
  and could not write it. Every root check now routes through
  `paths::is_root`, which also requires the initial user namespace.
- Capabilities are dropped by `execve` unless euid is 0, so the fleet
  namespace must map root-is-you; an identity map left members powerless.
- A guard comparing socket ADDRESSES must know whether two sockets share a
  network: `127.0.0.1:3001` inside and outside are different sockets.

**Egress DONE — verified 2026-08-30.** `ply up` attaches a user-mode router
to the namespace: a process that reads its packets off a tap device and
re-sends them as ordinary sockets, so it needs no privilege. `pasta`
preferred (podman's default, splices sockets), `slirp4netns` as fallback,
started with `--ready-fd` so members never launch into a half-configured
network, `--disable-host-loopback` so the machine's own services stay
private, and killed with the stack. Containers get `nameserver 10.0.2.3`,
because a host loopback stub is unreachable in there. With neither router
installed the stack still runs and says outbound is missing.

`ply exec` needed the same lesson from the other side: it joined the
instance's user namespace first and then could not join a network namespace
owned by an ANCESTOR (the stack's). It now asks the kernel who owns that
network (`NS_GET_USERNS`), enters the owner, the network, and only then the
instance's own user namespace.

Discovery env and `ply exec` DONE (2026-08-30). Discovery advertises
`<dep>.ply:<its own port>` inside a stack network — the published pair is
the host's side of a proxy and reaches nothing from in there. `ply exec`
reads the app's environment from the process the container's init started,
not from the init itself, which is ply and carries ply's own PATH; when it
cannot read it, it says so instead of handing over a default PATH.

**The promised deletions were overstated — corrected here.** A rootless
`ply run` now makes its own namespace too (verified: the app binds its
declared 3001, invisible on the host, only the published 4001 exposed). But
writing the deletions out changed the answer:

`PORT` injection is not a HOST-network artifact, it is a SHARED-network one.
Instances of a scaled app share one namespace, so at `--scale 2` they fight
over the same port exactly as they did on the host. Injection, the
allocator, the scale guard and `instance_port_explicit` therefore all stay;
so does `connect_either_family`, because an app binding `[::]` is
unreachable over IPv4 loopback inside a namespace just as it was outside.

What DID change is when they apply: injection now happens only when
instances actually share a network — scale > 1, or the fallback where no
namespace could be created. The common case (one instance, its own
namespace) binds its declared port, which is the whole user-visible win.
Deleting the rest needs a namespace PER INSTANCE, which is the nested-bridge
design in stage 2.

Original text, kept because the reasoning still holds for stage 2:

**The deletions need one more step first.** `PORT` injection, the loopback
allocator, `instance_port_explicit`, `connect_either_family` and the scale
guard all exist for rootless-on-the-host-network — which is still exactly
what a plain `ply run` does. `ply up` makes a namespace; `ply run` does not.
Until it does, every one of those remains load-bearing.

So: give a rootless `ply run` its own namespace too. The machinery is built
(`NetNs::create`, `enter_user`, `attach_egress`, and the join after
`prepare_app`), but it lands in the most-used code path, so it wants a
session where it can be exercised properly — and on this kernel only the
AppArmor-profiled `/usr/local/bin/ply` can create a namespace at all, so
every iteration costs an install.

Superseded note, kept because it is why the design changed: The design
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

## 2 · Collapse the deployment lanes, 5 → 2 — DONE (2026-08-30)

`from =` takes a registry ref (`redis@8.0`, `ply/plybox-web`), a path, a
URL, or `github:org/repo`, and its SHAPE picks the lane — the same reading
`ply run` gives its argument, so knowing one is knowing the other. With
`repo` (build it here) that leaves the two real concepts.

`from` normalizes into the old keys inside `Spec::parse`, so `app`, `image`,
`url` and `github` keep working and every deployment already on a host is
untouched. Naming `from` AND one of them is refused rather than merged: it
is a mistake, and folding them would hide it.

## 3 · Shrink the magic in wiring — DONE (2026-08-30)

Docs and examples now teach explicit wiring: `after` orders the start and
waits for health, and the address is `<member>.ply`, written down in the
file. The injected `<APP>_ADDR`/`_HOST`/`_PORT` are documented as a
convenience with their failure mode named — an app reading other names sees
nothing, concludes it has no database, and serves happily, which is exactly
what cost an afternoon. Changed in `running.md` (the discovery section
rewritten), `stacks.md` (the canonical example wires `DATABASE_URL` and
`API_ORIGIN`), `deployments.md`, `docker.md`, `agents.md`, `cli.md`.

Injection itself stays: removing it would break apps that do read those
names, and it is printed at launch, so it is visible rather than secret.

Original reasoning:

## 3 · (original) Shrink the magic in wiring

`--after db` injects `DB_HOST`; the app read `POSTGRES_HOST`; the site
served happily and every login answered "nodb". Explicit
`DATABASE_URL=…@db.ply:5432/…` in the stack is more typing and far more
legible — the connection is readable off the file. Keep injection as a
convenience; stop treating it as the wiring mechanism.

## 4 · Scope the registry's identity system; gate version deletion

Users, tokens, namespaces, grants, reserved names, admin logins — a second
product, built in a day. Two concrete follow-ups:

- **the grant bug** — DONE (2026-08-30). Not by splitting the list but by
  shortening it: `RESERVED = {ply, apps}`, the two official shelves. Every
  other name was only ever a guess about words we might want, and each one
  cost a grant row per admin login and made that name unclaimable. The stray
  rows still need deleting once the database is back:
  `DELETE FROM namespace_grants WHERE namespace IN
   ('api','www','admin','registry','docs','account','login','new');`
- **version deletion must be gated on nothing pinning it.** Today's
  registry rebuild orphaned `node 24.6.0` (broke plybox-web in production —
  the pin is unrecoverable, since rebuilding does not reproduce the digest)
  and `postgres 17.10.3` (still pinned in ply-labs' lock).

## 5 · Ops correctness (all found today)

**The plybox outage (2026-08-30) — two of these, in series.** The self-update
timer fired at 00:05. It brought up a SECOND `plybox-db` instance beside the
running one: slot 2, and therefore volume `data.2`, freshly initialised and
empty. The registry — users, tokens, packages, versions, grants — stayed in
`data.1`, which instance 1 kept writing until it stopped at 00:48. Nothing
warned. Then `plybox-web`, holding a start-time COPY of /etc/hosts, went on
dialling instance 1's dead address (EHOSTUNREACH 10.77.0.3) until every
DB-backed route returned 500: login, `ply push`, the whole API.

Both causes are now fixed in the runtime:

- a container's /etc/hosts is a **bind-mounted file**, not a copy, and
  `hosts::add_entry`/`remove_entry` refresh every running instance's copy —
  a peer that comes back on a new address is reachable without restarting
  its siblings (`runtime/hosts.rs`, `runtime/container.rs`)
- starting an instance on an **empty per-instance volume while another
  instance's copy holds data** now says so, loudly (`runtime/run.rs`). It
  does not change what happens — a second instance genuinely does not share
  the first's data — only whether anyone is told. Worth deciding: should a
  volume-declaring app refuse a second instance outright unless scaled?

Still open, and the reason the catalog needs rebuilding at all: an
`apps/postgres` push regenerated `index.json` from a database that had just
lost its rows, unlisting `17.10.3` whose bytes are still in R2. **A catalog
regenerated from an empty database silently unpublishes everything.** The
regeneration should refuse to shrink a namespace it cannot account for.

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
