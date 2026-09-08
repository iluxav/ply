# Changelog

Every release gets a short entry written by a person: what changed, fixes,
breaking changes, and known limitations. Write under **Unreleased** as work
lands; `make release-cli` turns that heading into the version and date, and
the release workflow publishes the entry as the GitHub release notes.

## Unreleased

### What changed
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
