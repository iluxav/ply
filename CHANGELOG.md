# Changelog

Every release gets a short entry written by a person: what changed, fixes,
breaking changes, and known limitations. Write under **Unreleased** as work
lands; `make release-cli` turns that heading into the version and date, and
the release workflow publishes the entry as the GitHub release notes.

## v0.1.92 — 2026-09-08

### What changed
- Notify config moved to `<data>/config/notify.toml` (the old
  `<data>/notify.toml` is still read), and `ply setup` creates the
  `config/` dir. It holds only notify config and no secrets, so the
  dashboard can be granted it read-write without exposing `host.key` — the
  dashboard now has a **Notifications page** where you pick the events and
  add a Telegram (or Discord, webhook, command) destination, with an
  instant "send test". Config code and the settings page live in the
  separate iluxav/ply-dashboard repo.

## v0.1.91 — 2026-09-08

### What changed
- **Notifications.** A host tells you when something happens: a
  `notify.toml` names the events (`deploy-failed`, `restart-loop`,
  `snapshot-failed`, `egress-blocked`, `disk-high`, …) and the
  destinations (Telegram, Discord, any webhook, or a local command for
  email), and the reconcile beat that already runs each minute reads the
  events journal and delivers the new ones — no daemon, no metrics stack.
  `restart-loop` (3 crashes of one app in 5 minutes) and `disk-high` (the
  data filesystem past 90%) are computed and rate-limited so one problem is
  one message. A destination may be a sealed value, so a fleet repo stays
  publishable. `ply notify --test` proves delivery; `ply notify` flushes by
  hand. See the Notifications guide.

## v0.1.90 — 2026-09-08

### What changed
- An instance's state file records its declared volume names, so a reader
  can tell a stateful app from a stateless one without opening the image.
  The dashboard uses it to offer snapshot controls only where there is data
  to snapshot: a stateless app (read-only rootfs, throwaway scratch) no
  longer shows a "take snapshot" button that could only ever error.

## v0.1.89 — 2026-09-08

### What changed
- **Snapshot and restore from the dashboard.** The run parent now accepts
  two control-dir commands — `snapshot` (take a volume snapshot now) and
  `restore <name>` (roll the slot back onto one) — so the dashboard, which
  drives every action by writing a file the parent consumes, gets a
  snapshots panel with take and per-snapshot restore. `ply snapshot take`
  also writes a small JSON index per snapshot under
  `<apps>/<app>/snapshots/`, so a reader holding only the apps-dir grant
  (the dashboard) lists an app's snapshots without opening a squashfs. Both
  actions are journal events. Needs ply ≥ 0.1.89 on the host and the
  matching dashboard build.

## v0.1.88 — 2026-09-08

### What changed
- **Snapshots: a backup for any app, nothing in the image required.**
  `ply snapshot take APP` commits every declared volume as one dated image
  in the store, the way `ply craft commit` commits an overlay — with the
  app's processes held still for the seconds the copy takes, inside the
  instance, as the app's own user, so a database comes out the way a power
  cut would leave it, which it recovers from. The copy streams through
  `ply exec`, so it is the same rootful, rootless and in a macOS microVM,
  and carries the ownership a restore needs. `ply snapshot ls|rm`, and
  `ply restore APP [NAME]` rolls the slot back onto it: stopped, volumes
  moved aside (kept under `.pre-restore/`), filled from the image by the
  instance's own init before the app starts, health-gated. A `[volumes]`
  entry is the whole contract. The microVM half needs
  `ply/microvm-kernel@1.0.2`, which this ply pins.
- The Postgres dump contract from v0.1.87 keeps its verbs under
  `ply backup`, with its restore now `ply backup restore` — `ply restore`
  is the generic one.

### Fixes
- `ply deploy` could report a slot rolled without any roll: it counted a
  slot as restarted when its recorded start was within a second of the
  deploy's start, so a deploy issued in the same second an instance
  launched reported success immediately. A rolled slot is a new process
  now — the pid it had when the deploy began is what the watcher compares.

## v0.1.87 — 2026-09-08

### What changed
- **Backups, driven.** The registry's Postgres already dumped itself on a
  schedule to any rclone target and restored into an empty volume; nobody
  could find it and nothing drove it. Now: `ply backup now db`, `ply
  backup ls db`, `ply restore db --to check` (beside the live data) and
  `ply restore db --replace` (over it: connections terminated, the
  database recreated, the dump loaded), all through `ply exec` with the
  instance's own environment — so the destination and its sealed
  credentials are set once, on the service. `backup.sh` is the one unit of
  work the schedule and the verb share; each run writes its outcome to
  `/run/ply/self/backup`. A Backups guide covers the S3 credentials, the
  egress allowance, the disaster path and how to prove a backup before it
  is needed. `ply/postgres@17.10.9`.
- The CLI reference now lists `ply secret` (it never did), the new
  `hostkey` and `seal`, the `current.img` behaviour of `ply systemd`, and
  what `ply ps`'s ADDRESS column means; the docs index links the Sealed
  secrets and Backups guides; the agent skill teaches both.

## v0.1.86 — 2026-09-08

### What changed
- **Sealed secrets.** A manifest, a deployment file or a stack member can
  carry `DATABASE_URL = "enc:v1:…"`, sealed for one host's key with
  `ply secret seal … --for <key>`, and commit it to a public repo. The run
  parent on that host opens it while composing the app's environment, on
  its way into the process, and says which names it opened — never the
  values. `ply secret hostkey` prints a host's key and makes it on first
  use; `sudo ply setup` makes root's. A value opens only under the name
  it was sealed for and only on the host it was sealed for; a host with
  no key refuses to launch the app rather than run it with ciphertext.
  Kamal's and SOPS's shape, not Vault's: no leases, no rotation, no
  identity — those stay Vault's job. See the [Sealed secrets] guide.

## v0.1.85 — 2026-09-08

Everything below came out of a first-hour audit on a fresh Ubuntu 24.04
server, following the docs literally.

### Fixes
- **A restart no longer reverts a deploy.** `ply systemd` ran the app from
  the image's own path, so after `ply deploy` rolled to a new version a
  unit restart or a reboot came back on the old one, silently. The unit
  now runs a `current.img` link beside the image, and `ply deploy`
  re-points it once the roll succeeds; it also notes when an app was
  started from a plain path that a restart would revert to.
- **Rootless `--scale N` with a pinned instance port is refused** with the
  fix spelled out. The instances share one network namespace, so
  `--publish 8081:8000 --scale 2` made the second bind the same port,
  exit 98 and restart forever, while `ply deploy` reported "1 instance(s)"
  with no hint why.
- `ply reconcile` run by hand says that it was one pass, and that nothing
  keeps converging until the watcher is installed (`sudo ply setup
  --edge`). The docs promised "delete the file and the app stops"; on a
  plain install it kept serving.
- `ply setup` installs `passt` itself when apt or dnf is there, instead
  of telling you to, and every rootless run nagging until you did.
- `ply ps` shows the address callers can dial: the published address when
  there is one, a dash for a rootless instance nobody can reach. It showed
  `127.0.0.1` next to `db:5432` for one, which a newcomer connects to and
  cannot. As a user it also says that root's instances need `sudo ply ps`.
- `ply exec postgres psql` works: the app image sets `PGHOST=/tmp`, where
  its socket is (`ply/postgres@17.10.8`).
- The publishing line says when a port is public, every interface, and
  how to keep it private. A bare `--publish 8080` still binds `0.0.0.0` —
  changing that default would turn every existing unit's public port
  private on upgrade.
- A Rust project the registry cannot serve is told the two ways that work
  today (ship a static binary; `ply import docker://`).
- The quickstart's install paragraph matches the installer; its Postgres
  line publishes the port so an app can reach it; the installer warns when
  a root-owned copy earlier on PATH will shadow a user install.

## v0.1.84 — 2026-09-08

### Fixes
- **Apps started with SIGPIPE ignored, since v0.1.79, on both backends.**
  The run parent ignores SIGPIPE so a dropped probe connection cannot kill
  it, and on Linux that disposition survived the clone and the execve into
  the app; in a microVM the guest's init is a Rust program, which ignores
  SIGPIPE at startup, with the same result. An entrypoint like
  `producer | head -1` got EPIPE errors where a Unix program expects to
  exit quietly, which breaks `set -e` scripts and C tools that never check
  `write()`. Every app, and every `ply exec` command, now starts with the
  default disposition, as under Docker. The microVM half needs
  `ply/microvm-kernel@1.0.1`, which this ply pins.
- An instance's state file is written whole and renamed into place. It was
  rewritten in place when the run parent seated the instance in its
  published pool, and a `ply exec` or `ply ps` reading it in that instant
  could see a torn file and report no running instance.
- `ply craft commit` names what it leaves out of `/tmp` and the shell
  history, one path at a time, instead of folding them into a total
  labelled "package indexes and session logs" — a tool unpacked into
  `/tmp` and meant to be kept used to vanish behind that line. Package-
  manager caches are still one total.
- `ply run .` on a directory with no `ply.toml` reuses its image when
  nothing in the directory changed, as it already did with a manifest.
  It repacked every file on every run.

## v0.1.83 — 2026-09-08

### What changed
- **The microVM kernel keg has its own version**, starting at
  `ply/microvm-kernel@1.0.0`, and names the Linux release it carries in its
  description. It used to be versioned as the kernel, which meant a change
  to the guest init — which ships in the keg — had no version to publish
  under until kernel.org happened to release one. The `6.12.x` kegs stay
  published and resolvable; a registry withdraws nothing.
- **`ply exec` works on macOS.** `ply exec <app> <cmd>` runs a command
  inside a running microVM — as the app's user, with the app's environment
  and workdir — and streams its output back with stdout and stderr kept
  apart, byte for byte, ending with its exit code. Standard input is
  forwarded when it is not a terminal, so `echo x | ply exec app cat`
  works, and several commands can run at once.

  There is no namespace to enter, so this is not the Linux path: the
  request crosses the control channel the machine already has, and the
  guest's init forks the command beside the app. An interactive shell is
  still missing — that needs a pseudo-terminal, which the guest kernel is
  built without — and `ply exec app sh` therefore has no prompt; `sh -c
  '…'` is the form to use.

  A guest now says what it can do when it reports ready, and the host asks
  before relying on it: an instance booted from an older microVM kernel
  refuses a command with a sentence instead of waiting forever on a
  message nothing will answer. The ready line an older guest sends is
  unchanged, and reads correctly as "no capabilities".
- `ply craft commit` leaves out what a package manager regenerates: apt's
  and apk's package lists, their download caches, and the session's own
  logs. A session that ran `apt-get install jq` packed to 16 MiB and now
  packs to 564 KiB, and the commit line says what it left out. The dpkg
  database still ships, so a session resumed with `craft edit` knows what
  is installed and only needs `apt-get update` before installing more —
  the same convention a Dockerfile follows.

### Fixes
- `ply deploy` could report "deploy complete" while the app's published
  port still had no backend, so the next request after a deploy — a CI
  smoke test, say — could be answered by nothing at all. Being healthy and
  being seated in the published pool are different moments, and only the
  run parent can see the second, so it records it and the watcher waits
  for it. Probing the published port from outside cannot substitute: the
  parent binds it at startup and accepts on it either way.
- `ply craft new` demanded `--source` although its own help said the
  official registry was the default, so the command the packages guide
  prints failed at the first step. The default is now the default.
- The packages guide's craft example named a `--base` flag that does not
  exist (it is `--from`), and did not say that craft needs root.

## v0.1.82 — 2026-09-08

### Fixes
- The detector told Bun from Node only by a lockfile, so a Bun project
  with a package.json ran on Node, and a one-file Bun script with no
  dependencies was not recognised at all. It now reads the package.json
  (a start script that invokes `bun`, or `packageManager = bun@…`), and a
  lone `index.ts`, `main.ts` or `server.ts` with no package.json and no
  deno.json runs on Bun, which needs no setup for TypeScript — the printed
  manifest says so, so a Deno project learns to carry its deno.json.

## v0.1.81 — 2026-09-08

### What changed
- **The registry carries `ruby` 3.3.8, `deno` 2.9.6 and `bun` 1.4.2**, on
  both architectures, so a directory with a Gemfile, a deno.json or a bun
  lockfile runs with `ply run .` like a Node, Python or Go one. Ruby is
  converted from Debian trixie; Deno and Bun are the official Linux builds,
  checksum-verified, in a keg each. Rust is detected but not carried: a
  usable toolchain is 132 MiB before a C linker, over the registry's cap.
- **A keg can set environment variables**, `[layer] env = { … }`, beside
  the `PATH` and `LD_LIBRARY_PATH` it already contributes; they compose
  before the app's own `[env]`, dependents over dependencies. Debian's Ruby
  has its load path compiled in as `/usr/lib/ruby`, so the `ruby` keg sets
  `RUBYLIB` to its own prefix and `require "socket"` works. A keg carrying
  `[layer] env` needs this ply or newer: an older one refuses the field.

## v0.1.80 — 2026-09-08

### What changed
- **`ply run .` needs no ply.toml.** A directory without a manifest is run
  with the one `ply init -y` would have written: ply says what it inferred
  it from (a package.json, a go.mod, a manage.py), prints the whole manifest,
  builds and runs it, and writes nothing into the directory. `ply init -y`
  keeps it. A directory nothing is recognised in says so and points at
  `ply init` (or `ply import` when there is a Dockerfile); a project whose
  runtime the registry does not carry yet is refused up front, naming the
  package, instead of failing at build time.
- **The detector knows more projects.** Go (`go.mod` → `go run .`, with the
  toolchain as a dependency and the build directory inside the app's own
  prefix, since `/tmp` is noexec), Django (`manage.py runserver` on 8000),
  Flask (port 5000), Rust, Ruby, Deno and Bun. `ply init` prefills the same.
- **The registry carries `python3` 3.13 and `go` 1.24**, on both
  architectures, plus `python3-psycopg2` 2.9. The python-postgres example
  now resolves them from the registry instead of a local source.

## v0.1.79 — 2026-09-08

### What changed
- **macOS: every microVM runs in a process of its own.** `ply run` spawns
  one worker per instance and keeps the switch, the published ports and
  the state for itself. That is what lets `--scale N` boot N microVMs
  (Hypervisor.framework allows one VM per process; the second used to die
  at `hv_vm_create`), gives `ply deploy` a real child pid to find the run
  parent from (it used to answer "no running instances" on a Mac), and
  makes `ply ps` show a pid that `kill` reaches.
- **macOS: `ply.dev.toml` links are shared live.** A linked host directory
  is served into the guest over virtio-9p, so an edit on the Mac is what
  the next read in the guest sees. Before, a link became an empty disk
  with a warning.
- **macOS: the guest clock is the host's.** The spec disk carries the
  host's wall clock and the guest init sets it before anything else. Until
  now a microVM started in 1970 and every HTTPS connection failed with
  "certificate not yet valid".
- The kernel keg is `ply/microvm-kernel@6.12.109`: Linux 6.12.109 with
  9p over virtio, and the guest init that sets the clock and mounts shares.
  The binary pins it; the first `ply run` fetches it.
- `ply deploy` reports a slot as rolled only once the new instance answers
  on its `[health] port`, not when its state file appears. "deploy
  complete" used to print while the new instance was still booting, and
  the next request found nothing listening.

### Fixes
- macOS: a `ply run` parent died of SIGPIPE (exit 141) the moment its
  readiness probe reached an app that speaks first on a connection — a
  greeting on accept, a database banner: the app answered into a probe
  connection the parent had already dropped, and the switch's write raised
  the signal the CLI leaves at its Unix default for `ply … | head`. The
  run supervisor, `ply up` and the microVM worker now ignore SIGPIPE, so a
  hung-up socket is the `EPIPE` they already handle.
- A stop signal that reached a `ply run` parent during a rolling deploy's
  health gate never reached the new instance: its pid was registered with
  the signal handler only after the gate, so it was SIGKILLed ten seconds
  later without its exit code. The pid is registered at launch.
- The stacks guide named the overlay for a stack `stack.dev.toml`; it is
  `<stack file>.dev.toml`, so a `ply.toml` stack reads `ply.dev.toml`.

### Known limitations
- macOS: `ply exec`, egress enforcement, `[resources]` limits and
  autoscale samples are still missing in the microVM backend.

## v0.1.78 — 2026-09-07

### What changed
- ply installs on Apple Silicon Macs: `curl -fsSL https://plybox.sh/install.sh | sh`
  installs the release binary (`ply-darwin-arm64`, signed with the
  hypervisor entitlement, so `ply run` can create VMs), skips the
  server-only `ply setup` and wizard, and checks the entitlement and
  Hypervisor.framework after installing. An Intel Mac is told to use Lima.
  The first `ply run` fetches the kernel keg `ply/microvm-kernel@6.12.0`
  from the registry. The backend remains experimental; see the macOS guide
  for what it does not do yet (`ply exec`, egress enforcement, resource
  limits).
- `ply self-update` on a Mac downloads the macOS binary.

### Fixes
- The installer accepts `arm64` as a name for aarch64 on Linux.

## v0.1.77 — 2026-09-07

### Fixes
- Rootless: `ply ps`, `ply why` and `ply deploy` could not see an app started
  from a session without `XDG_RUNTIME_DIR` (a bare `su`, cron, some CI): the
  parent, inside its user namespace, built its state path from uid 0 and
  wrote to `/tmp/ply-0`. Paths now use the uid the host sees.
- The installer tried `sudo` for any user on a host that has `sudo`, and
  stopped with an error for a user not allowed to use it. Such a user now
  gets `~/.local/bin`, as documented; a sudoer with a password is asked once.
- `ply init` reads `scripts.start` from `package.json` (`node index.js`)
  before falling back to `main` or `server.js`.
- Rootless apps can bind ports below 1024. The privileged-port floor is a
  property of the network namespace and ply owns the one it creates, so it
  lowers the floor there; an imported `nginx` now serves on :80 rootless
  with no change to the host. On the host's network (no user-mode router)
  the floor is the host's, and ply says so before the app's own bind error.
- `ply setup` reports a missing user-mode router (`passt`/`slirp4netns`),
  without which rootless instances have no outbound network.
- `ply push` of a version the registry already holds, byte for byte, says
  "already published — unchanged" instead of "published".

## v0.1.76 — 2026-09-06

### What changed
- The repository is ready for visitors: `CONTRIBUTING.md` (setup, the
  required checks, how changes are reviewed, releases, AI assistance),
  issue templates for bug reports and for feedback from trying ply, and a
  layout table naming every top-level directory.
- Release notes are now the matching `CHANGELOG.md` entry, written by a
  person; `make release-cli` refuses to tag without one.

### Fixes
- `/docs/model/` was linked from the stacks guide but never rendered; the
  page has its title now.
- Stray build artifacts are ignored (`*.img.tmp`, the compiled `notify`).

### Breaking changes
- None.

### Known limitations
- Nothing in the binary changed; this release carries the documentation and
  the repository files.

## v0.1.75 — 2026-09-06

### What changed
- **Scale to zero.** `[scale] min = 0` with `idle = "10m"` lets an app sleep:
  after `idle` with no connections on its published port the last instance
  stops and the run parent keeps only the port; the next connection is held
  while an instance starts. `ply scale APP 0` sleeps an app now, `ply ps`
  shows an asleep row, `ply why` lists `sleep`/`wake` events, `ply deploy`
  on a sleeping app swaps the image for the next wake. `signal`/`target` in
  `[scale]` are now only required when `max > 1`.
- The README and quickstart example now use Node, which the public registry
  serves. `examples/hello-http` holds the runnable example.

### Fixes
- The quickstart depended on a `python3` package that the public registry
  does not have; a first `ply build` failed. The example uses `node = "22"`.

### Breaking changes
- None.

### Known limitations
- The dashboard builds its app list from instance state and does not show a
  sleeping app. `ply ps --json` has no row for one either.
- A client that has waited 60 s for a sleeping app to wake is dropped; the
  timeout is not configurable yet.

## v0.1.74 — 2026-09-06

### What changed
- `ply why APP`: why an app is in the state it is in — exits with their
  codes and log tails, blocked egress, and recent changes (deploys, scaling,
  restarts) with evidence from the events journal.

## v0.1.73 — 2026-09-05

### What changed
- `ply up` runs Docker images as stack members (`run = "docker://…"`), with
  published ports made explicit.

## v0.1.72 — 2026-09-05

### What changed
- Autoscaling in the run parent: `[scale]` grows and shrinks the instance
  count on cpu, memory, net or a custom metric; `[resources]` ranges resize
  memory and cpu limits live. `ply scale APP auto` resumes the policy after
  a pin.
- Egress contract: `[egress]` declares what an app may reach; audit and
  enforce modes, `ply egress APP` shows the log.
- Kernel-level publish on rootful Linux: `--publish` ports are balanced by
  an nftables DNAT rule instead of a userspace relay.
- Experimental native macOS backend (Apple Silicon microVMs); build from
  source, see the macOS guide.

### Known limitations
- macOS backend: kernel package not yet published; see `docs/macos.md`.
