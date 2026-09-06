# Changelog

Every release gets a short entry written by a person: what changed, fixes,
breaking changes, and known limitations. Write under **Unreleased** as work
lands; `make release-cli` turns that heading into the version and date, and
the release workflow publishes the entry as the GitHub release notes.

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
