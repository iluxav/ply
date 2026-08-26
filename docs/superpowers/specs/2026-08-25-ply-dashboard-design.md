# ply-dashboard — the opt-in web UI that is just an app

**Date:** 2026-08-25
**Status:** approved design, pre-implementation
**Positioning:** Coolify's dashboard IS Coolify — daemon + Postgres + Redis on
a $24 droplet before your first app deploys. ply's dashboard is an app in the
registry: distributed, deployed, TLS'd, rolled, and removed by the thing it
observes. ply is 100% functional without it. When scale-to-zero lands, it
costs zero resident processes while nobody is looking at it.

## Non-negotiables

- **No daemon added to ply.** The dashboard is a ply app like any other.
- **No database.** State is read from ply's own files; auth is two small
  files in a volume.
- **No new API surface in ply (v1).** The dashboard reads the same files the
  CLI reads. Mutations in v1 are copy-paste commands; v1.1 uses the
  control-dir protocol (below) — files, not sockets.
- **Fails open, never blocks ply.** If the dashboard is down, broken, or
  uninstalled, nothing about ply changes.

## Repo & distribution

**Own public repo: `iluxav/ply-dashboard`.** Being outside the monorepo is
itself the proof: it consumes only public, documented interfaces. Release CI
builds the static Go binary (CGO_ENABLED=0) for x64+arm64, runs the Tailwind
build, then `ply build --arch` twice, and attaches both `.img` files to the
GitHub release.

Three install paths, in order of blessing:

1. **Registry (primary):** published to `apps/dashboard` — so
   `ply run dashboard@0.1 …` just works, version-locked in stacks, etc.
2. **GitHub release direct:** the repo's own `install.sh` (curl-able) checks
   `ply` is installed, downloads the right-arch `.img` from the latest
   release, prints the run command. No registry dependency.
3. **`[sources]`:** `dash = "github:iluxav/ply-dashboard"` works for
   manifest dependencies today; `ply run --source github:…` needs version
   listing on forge sources (known limitation, noted, not v1's problem).

ply's own `install.sh` gains one closing hint line (never a prompt — it is
piped into `sh`): `want a UI? → ply run dashboard@0.1 …`.

## Stack

- **Go, stdlib-first**: `net/http`, `html/template`, `embed`. No framework.
- **htmx** (vendored, `go:embed`) for partial updates; 2–5s polling per
  panel. No SPA, no build chain for JS.
- **Tailwind** via the standalone CLI at build time; output CSS embedded.
  No Node in the build.
- One binary, all assets embedded. Target < 15 MB.

### Design language

Dark-mode-first, terminal spirit: dense tables, monospace numerals,
`ply ps`-like columns, subtle green/amber/red health dots, no cards-and-
shadows SaaS look. It should feel like a beautiful TUI that happens to be in
a browser. Light mode follows the same tokens.

## Runtime shape

Runs as a normal ply app, rootful on the droplet (that's where production
state lives):

```sh
ply run dashboard@0.1 \
  --publish internal:7070 --domain ply.example.com \
  --link /run/ply/state:/ply/host/state \
  --link /var/lib/ply/apps:/ply/host/apps \
  --link /sys/fs/cgroup:/ply/host/cgroup \
  --link /proc:/ply/host/proc
```

All links read-only in effect (v1 writes nothing); the exact flags are baked
into the docs and the repo install.sh output. Rootless dev works with the
`~/.ply` / XDG equivalents — the binary probes both prefixes.

**Why links, not magic:** the container cannot see host state by design;
the operator explicitly grants exactly these four read surfaces. The grant
is visible in `ply ps`'s command line — auditable like everything else.

### Reading state without the CLI

The container has no ply binary. The dashboard vendors small Go structs
mirroring the state JSON (unknown fields ignored, missing fields defaulted),
reads:

- `/ply/host/state/*.json` — instances: app, pid, ip, ports, image, started,
  restarts, health_port, published_addr, domains
- `/ply/host/proc/<pid>` existence — aliveness (host procfs, bind-mounted)
- `/ply/host/cgroup/…` — cpu.stat, memory.current, pids.current per instance
- `/ply/host/apps/<app>/` — deploy pointer leftovers, GC roots

This makes the state-file schema a **semi-public contract**: additive
changes only, documented in ply's `docs/architecture.md`. (The dashboard is
version-tolerant by construction; a field it doesn't know is ignored.)

## v1 feature set (observe + advise)

- **Overview**: one row per app — health dot, instances, restarts, uptime,
  image version+digest, published addr, domains (linked). Sorted, dense,
  auto-refreshing.
- **App detail**: per-instance table (pid, ip, uptime, restarts, health);
  CPU/mem sparklines from cgroup deltas (in-memory ring buffer, no
  persistence); volumes with sizes; current image digest and lock summary
  (read from the image if the store is linked — else omitted).
- **Host panel**: ply version, store size, instance totals, disk usage.
- **Command panel (the mutation story in v1)**: every action renders the
  exact command instead of a button — scale, deploy, restart, logs
  (`journalctl -u ply-<app> -f`), rm. One click copies. Honest, useful, and
  it teaches the CLI.
- **Auth**: below.

Explicitly out of v1: log viewing (journald from a container is a cgo/format
swamp — the command panel covers it), metrics history (ring buffer only),
multi-host, users/roles, and any mutation.

## Auth (no database, race-closed)

1. First boot, no auth file → generate a random **setup token**, print it to
   stdout (`ply logs`-visible / journalctl). The create-account page requires
   it. Closes the first-visitor-owns-the-box race.
2. Account creation writes `auth.json` into the app's **`data` volume**
   (survives rolls): `{user, argon2id_hash, session_key}`. argon2id via
   `golang.org/x/crypto` — the one non-stdlib dependency besides htmx.
3. Sessions: HMAC-signed cookie (key from `auth.json`), 7-day expiry,
   `Secure` + `HttpOnly` + `SameSite=Lax`.
4. Login rate limit: 5 attempts / minute / IP, in-memory.
5. Password change = re-create with current password; forgot = delete
   `auth.json` from the volume (documented — filesystem is the admin API).

## v1.1 — the control dir (mutations, still no daemon)

Promotes ply's deploy mechanism (pointer file + SIGHUP) into the general
control plane. In ply:

- Each app's dir gains `control/`; the run parent polls it every 2s (same
  cadence as `ply proxy --watch`) in its existing supervision loop —
  SIGHUP remains the zero-latency path for the CLI.
- Commands are files, atomic-renamed in, consumed (deleted) by the parent:
  `scale` (content: N), `restart` (empty; rolling restart), `next-image`
  (existing deploy pointer, now also picked up by poll). Parent writes
  `last-result` (JSON: command, outcome, timestamp) for the dashboard to
  show.
- **Permissions are the ACL**: who may write the control dir may command the
  app. The dashboard gets that via an additional rw link, granted explicitly
  by the operator — and until it is granted, the dashboard stays observe-only
  on that host. Graceful, visible, auditable.

Then dashboard buttons write files, and the audit story is `ls -l` — the
Linux answer ("everything is a file"; daemontools/runit did supervision this
way decades ago, and `/proc/sys` is the kernel saying the same thing).

## Division of labor

iluxa scaffolds the repo (their preference): `go mod init`, layout, Tailwind
setup, CI skeleton. Claude then fills in: state readers, cgroup sampling,
auth, templates/partials, install.sh, release workflow, and the ply-side
control-dir protocol when v1.1 starts.

## Open questions (deliberately deferred)

- Name collision risk: registry app name `dashboard` is generic — fine while
  the registry is curated; revisit if namespaces open up.
- A `--link-ro` flag in ply (read-only binds) would make the grants tighter —
  today links are rw; worth a small ply issue regardless of the dashboard.
- Whether `ply setup --dashboard` (systemd unit à la `--edge`) is wanted for
  boot persistence, or `ply systemd dashboard.img …` documented is enough.
