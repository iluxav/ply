# todo.md — the road to fighting Coolify

> Spec: `AGENTS.md`. Delivered phases: `TASKS.md`. This file is the *next*
> product bet, not the backlog — everything here serves one thesis:
>
> **Coolify needs a $24 droplet to run itself. ply runs 50 apps on a $6 one.**
>
> The test for every item below: **does it add a resident process?** Socket
> activation *removes* processes. A static status page adds none. A dashboard
> with its own database adds five — and the moment ply needs Postgres to run
> ply, the pitch is dead.

---

## Part 0 — Urgent: the glibc lane

> **Status 2026-08-25:** decided — **Debian (trixie) becomes the default
> lane**, Alpine freezes (append-only). Wolfi rejected (vendor-curated free
> repo). `deb2pkg` implemented (ELF-closure walk, not Depends) + debian base
> minted + proof kegs verified rootless: redis 8.0.2 → PONG, postgres 17.10
> → `select version()`, node 24.6.0 loads a host-built glibc `.node` addon.
> Spec + amendments: `docs/superpowers/specs/2026-08-25-deb2pkg-design.md`.
>
> **Consumer DX (2026-08-25 pm):** `ply run <name[@ver]>` resolves runnable
> apps from the registry's `apps/` namespace (kegs stay in `ply/`) — newest
> matching version, fetched + cached; `docker://` demoted to escape hatch.
> First-class `postgres` + `redis` live in `services/`, speaking the
> docker-library env contract (POSTGRES_PASSWORD/USER/DB, REDIS_PASSWORD/…).
> `ply build --arch x64|arm64` cross-builds with per-arch dep resolution.

Everything ply can run today is musl, because `alpine` is the only base
package and every other package is `apk2pkg`-derived from it. For most
ecosystems that is invisible. For **Node it is a wall**: npm's prebuilt
binaries target glibc first, musl is the afterthought, and some packages ship
no musl build at all.

The failure is worse than a wall, because it is silent. A `node_modules`
built on a GitHub Actions runner (glibc) copied onto an alpine base loads
fine right up until something actually `require`s a native addon — then
`Error: … .so: not found`, with nothing pointing at libc.

**plybox.sh is living this right now**: `.next/standalone` ships
`@img/sharp-linux-x64` (glibc) onto an alpine base. It has not broken only
because Next lazily requires sharp when it optimizes an image at runtime, and
ours are prerendered. The first runtime optimization throws.

This is an adoption tax paid before anyone has formed an opinion about ply —
"my CI output doesn't run" is where they stop reading.

### 0.1 A glibc base package

`scripts/build-base.sh` already has the shape: download a verified rootfs
tarball, write a `base = true` manifest, `ply build`. The Debian variant is
that script with a different URL, a different checksum source, and
`provides_abi = "linux-x64-gnu"`.

- [x] `scripts/build-base-debian.sh` — trixie-slim debuerreotype rootfs, sha256-verified against its OCI manifest; x64 built (25.7 MiB), arm64 is the same command
- [ ] Push `debian` as a base package alongside `alpine`
- [ ] Confirm the resolver's ABI guard (`resolve.rs:287`) gives a clear message on a mixed graph — it already refuses; check the wording names libc, not just the ABI string

### 0.2 A glibc `node` package

Easier than the Alpine path: nodejs.org publishes official `linux-x64` /
`linux-arm64` tarballs, which are glibc and bundle what they need. No
`apk2pkg`, no dependency-closure walk — tarball → `/opt/node-<version>/` →
`[layer] path`.

- [x] Node keg from the official nodejs.org tarball, `provides_abi = "linux-x64-gnu"` — verified loading a glibc lightningcss addon on the debian base. (Debian's own `nodejs` deb is relocation-hostile: externalized builtins at compiled-in absolute paths — don't convert it, mint from the tarball.)
- [ ] Publish the current LTS and current lines for both arches
- [ ] `ply search node` should make the two lanes legible — musl vs glibc — rather than looking like duplicate versions

### 0.3 Stop shipping the wrong libc silently

Independent of the lane, and worth doing first because it is an afternoon:

- [ ] `ply build` warns when it packs `*-linux-x64-gnu` or a glibc `.node` onto a musl base (and the reverse). Cheap check over the packed tree; turns a runtime `.so not found` into one sentence at build time
- [ ] Same check in the `--watch`/dev path once that exists, where it will fire far more often
- [ ] Fix plybox.sh's own sharp: `npm ci --os=linux --libc=musl --cpu=x64` in `deploy-web.yml`, or move to the glibc base once 0.1/0.2 land

**Gate:** a Node app with native dependencies, built on an ordinary glibc CI
runner with an unmodified `npm ci`, runs on ply without flags or ceremony.

**Today's workaround, until then:** `npm ci --os=linux --libc=musl --cpu=x64`.
npm resolves platform-specific optional deps against those values instead of
the host's. Works for the modern prebuilt pattern (sharp, swc, lightningcss,
tailwind-oxide all publish musl twins); does nothing for packages that
compile at install time via node-gyp with no prebuilds.

---

## Part 1 — Entry ticket

Not differentiating. You cannot compete without them, and nobody will notice
that you have them. Ship them anyway.

### 1.1 Domains + TLS — `ply run --domain app.example.com`

`runtime/publish.rs:1` currently draws a hard line: *"no TLS, no HTTP, no
hostnames (that's the edge's job)."* That line is correct for the runtime and
wrong for the product: **on a single droplet, ply IS the edge.** This is
Coolify's number-one job and the first question anyone asks.

Keep the data plane Caddy — TLS/ACME, h2/h3, websockets and drain semantics
are a decade of someone else's work, and the README's "no proxy" claim is
load-bearing. ply stays a config emitter, which is what `ply lb` / `ply proxy`
already are.

- [ ] `--domain <host>` on `ply run` (repeatable), recorded in instance state next to the published port
- [ ] `ply proxy --watch`: inotify on `/run/ply/state` → regenerate Caddyfile → `POST /load` to Caddy's admin API. Already in the TASKS.md backlog; `--domain` is what makes it worth building
- [ ] `ply setup --edge` — install Caddy + a ply-managed Caddyfile include, so a fresh droplet is one command from HTTPS
- [ ] Docs: apex + wildcard, `demos/edge` promoted from demo to the documented path

**Gate:** fresh droplet → `ply run app.img --domain app.example.com` → valid cert, HTTPS serving, no manual Caddy editing.

### 1.2 `ply push user@host <img>` — deploy from a laptop

Kamal parity. Already scoped in TASKS.md under "CI-driven deployment"; it
belongs here because without it the story stops at "ssh in and run it
yourself."

- [ ] `ply push` = scp + remote `ply deploy`, reusing the existing health-gate/roll/revert machinery
- [ ] `ply status user@host [app] --json` so CI can gate waves
- [ ] `ply deploy-shell` forced-command target for `authorized_keys`

**Gate:** a GitHub Action that builds and pushes to a droplet, with a failed health gate reverting.

### 1.3 `ply ui` — a status page, not a dashboard

Coolify's real moat is the UI, and matching it feature-for-feature means
becoming it. The cheap 80% is a **single self-contained HTML file** emitted
from `ply ps --json` + `ply stats --json`: apps, instances, versions, health,
memory. No server, no database, no websocket.

- [ ] `ply ui > status.html` — one file, inlined CSS/JS, reads a JSON blob embedded at emit time
- [ ] `ply ui --watch` — rewrite on state change so a browser reload is current (still no server; serve it with the edge Caddy)
- [ ] Explicitly NOT: deploy buttons, env editing, log streaming, accounts

**Gate:** a screenshot that makes ply look real, produced by a command that leaves nothing running.

---

## Part 2 — Scale-to-zero (the wedge)

The one feature Coolify structurally cannot copy. **systemd holds the
listening socket; zero ply processes exist until someone connects.** First TCP
connection wakes ply, it mounts the squashfs (~40 ms) and splices the
connection systemd was holding in the backlog. Idle timeout stops it again.

Both halves already exist in embryo: `runtime/publish.rs:101` binds a port and
`:112` splices to a pool; `lifecycle.rs:333` emits systemd `[Service]` units.

### 2.1 Accept an inherited listener

- [ ] Honour `LISTEN_FDS` / `LISTEN_PID`: when systemd passes a socket, take fd 3 instead of calling `publish::bind()`. Small change, contained to `publish.rs`
- [ ] `ply run --publish` learns to prefer an inherited fd; everything downstream (pool, health gate, roll) is untouched

### 2.2 Emit socket units

- [ ] `ply systemd --socket` emits a `.socket` + `.service` pair:
      `ply-<app>.socket` with `ListenStream=<port>`, `ply-<app>.service` with `Type=notify` (or `exec`) and no `WantedBy` — systemd starts it on demand
- [ ] `--domain` composes: the socket listens on a high port, Caddy routes the hostname to it

### 2.3 Idle stop

- [ ] `--idle <duration>` — the run parent exits once no instance has had a connection for that long. systemd resumes holding the socket; the next connection starts it again
- [ ] Connection accounting already lives in the splice loop; this is a timer plus a clean shutdown path
- [ ] Interaction with `[restart]` policy must be explicit: idle-exit is **not** a failure and must not trigger a restart

### 2.4 The proof

- [ ] Benchmark script: N apps installed, M running, RSS and cold-start latency measured
- [ ] **The screenshot**: `ply ps` listing 50 apps with 3 running, `free -m` showing the box mostly idle, `curl` against a cold URL returning in < 100 ms
- [ ] Measure the real numbers before publishing any of them

**Gate:** 50 socket-activated apps on a 1 GB droplet; a cold request served in under 100 ms; idle RSS dominated by the kernel, not by ply.

**Why Coolify can't follow:** dockerd is always resident, images must be
extracted on disk, and container start is 300 ms–2 s. Bolting socket
activation onto that makes the first request feel broken. The feature only
works at ply's latency — which is a consequence of mounting instead of
extracting, and therefore not something a Docker-based platform can retrofit.

---

## Part 3 — What scale-to-zero unlocks

- [ ] **Preview environments.** Every PR gets `pr-42.app.example.com`, costs nothing while nobody is looking, wakes in 40 ms on click. Vercel's shape on a $6 VPS; falls out of §1.1 + §2 almost for free
- [ ] **Per-app idle policy** in the manifest (`[scale] idle = "5m"`) so it is a property of the app, not of the invocation

---

## Explicitly not doing

- A web application with its own database. That is Coolify's shape; competing there means inheriting its weight and forfeiting the only structural advantage ply has
- Log aggregation, metrics storage, alerting — journald and the edge already do this, and each would add a resident process
- A marketplace of one-click service templates. `ply import docker://` already runs 21/21 of the mainstream images (see TASKS.md Phase 11); a curated `ply.toml` gallery in docs is the small-spirit version
