# Runtime Backend Seam Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `ply run` at one trait so the Linux namespace runtime becomes one `Backend` behind `cfg(target_os = "linux")`, `ply-cli` compiles for `aarch64-apple-darwin`, and CI keeps it that way — milestone 1 of the macOS spec, with zero behaviour change on Linux.

**Architecture:** `runtime/run.rs` keeps the supervisor (manifest, layers, env, params, publish, state files, health/`after` gates, restarts, rolling deploys, control dir, log tee) and talks to the platform only through `runtime::backend::{Backend, Instance}`. Everything that clones, mounts, pivots, drops rights, or touches namespaces moves under `runtime/ns/` and implements the trait. Two supervisor helpers (`health_gate`, `stop_with_patience`) are written against the trait and unit-tested with a fake instance — the first tests the runtime's supervision logic has ever had.

**Tech Stack:** Rust 2021 workspace; `nix` 0.31 (portable subset only outside `ns/`); `caps` and `seccompiler` become Linux-only dependencies; `cargo-zigbuild` for local darwin checks; GitHub Actions `macos-latest` for the CI gate.

**Spec:** `docs/superpowers/specs/2026-09-02-macos-native-vm-design.md` — sections "The seam: `runtime::Backend`" and "Milestones" (row 1). `docs/ply-vm.md` supplies R1–R6/A5.

## Global Constraints

- **Zero behaviour change on Linux.** Every existing test passes unchanged (`make check`: fmt, clippy `-D warnings`, `cargo test --workspace`). Messages printed by `ply run` keep their exact text; only their source file moves. The owner's manual verification (a rootless two-member stack, and the droplet's rootful stack) must look identical before and after.
- **The seam is the only platform boundary.** After Task 4, no file outside `ply-core/src/runtime/ns/` and `ply-core/src/craft.rs` may use `nix::sched`, `nix::mount`, `caps`, `seccompiler`, `setns`, `pivot_root`, loop-device ioctls, or `/sys/fs/cgroup`. Portable `nix` (signal, unistd, socket, wait, pty) may stay.
- **Darwin check target:** `cargo check --target aarch64-apple-darwin -p ply-cli` (CI, native on `macos-latest`); locally `make check-darwin` runs it through `cargo-zigbuild` (install: `uv tool install cargo-zigbuild`, plus a `zig` on PATH — `~/.local/bin/zig` may be a two-line shim `exec ~/.local/share/uv/tools/cargo-zigbuild/bin/python -m ziglang "$@"`). The darwin check must be clean (0 errors, 0 warnings under `-D warnings`) at the end of Task 4.
- **No new runtime behaviour for macOS in this plan.** On a non-Linux host `ply run` fails with the exact message `ply run is not available on this platform yet — the macOS runtime lands in a later release` (from `default_backend()`); `ply build`, `ply inspect`, `ply search`, `ply push`, `ply up --plan` are expected to work there but are verified in plan 2 (needs a Mac).
- **Trait names and signatures are fixed by Task 2** and used verbatim by Tasks 3–4 (see each task's Interfaces block).
- **Lockfile invariance** (spec): nothing in this plan touches `ply.lock` writing.
- **Commits:** the owner commits. Implementers do not run `git commit`; each task ends with `make check` green and the task's darwin-check expectation met. (Where a step says "commit", read: stop and report.)

---

## File structure after this plan

```
ply-core/src/runtime/
  mod.rs            pub mod after, backend, control, events, hosts, logring, params_tree, publish, run, state, supervise;
                    #[cfg(target_os = "linux")] pub mod ns;
  backend.rs        NEW  the seam: Backend, Instance, InstanceSpec, Facts, NetworkFacts, Launched, Record,
                         scrub_instance_dir(), default_backend()
  supervise.rs      NEW  health_gate(), stop_with_patience(), Health; #[cfg(test)] FakeInstance
  run.rs            supervisor only (no clone/mount/netns/cgroup/caps); Running wraps Box<dyn Instance>
  state.rs          reap_stale() calls backend::scrub_instance_dir instead of mount::unmount_detach
  hosts.rs, publish.rs, after.rs, params_tree.rs, control.rs, events.rs, logring.rs  unchanged (portable)
  ns/
    mod.rs          NEW  NsBackend, NsInstance, InstanceGuard, in_namespace, alone_in_its_network,
                         rootless_scale_guard + ScaleGuard, and their tests
    subid.rs        NEW  parse_subid, SubIdRange, subid_gap, write_id_maps, mirror_own_map, username_for, have + subid_tests
    container.rs    MOVED from runtime/container.rs (git mv)
    mount.rs        MOVED
    netns.rs        MOVED
    network.rs      MOVED
    security.rs     MOVED
    cgroup.rs       MOVED
    loopdev.rs      MOVED
    exec.rs         MOVED
    term.rs         MOVED
ply-core/src/lib.rs           #[cfg(target_os = "linux")] pub mod craft;
ply-core/Cargo.toml           caps + seccompiler under [target.'cfg(target_os = "linux")'.dependencies]
ply-cli/src/commands/
  up.rs             stack netns creation behind cfg(target_os = "linux"); host network elsewhere
  exec.rs, craft.rs, setup.rs   Linux bodies behind cfg; non-Linux bodies return a one-line error
  ps.rs             unchanged (reap_stale keeps its signature)
Makefile                      check-darwin target
.github/workflows/ci.yml      NEW  linux: make check; darwin: cargo check + clippy for aarch64-apple-darwin
```

---

### Task 1: Platform-gate the Linux-only crates and add the darwin check lane

**Files:**
- Modify: `ply-core/Cargo.toml` (the `[dependencies]` block, lines 3–20)
- Modify: `Makefile` (after the `check:` target, line 9–12)
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: nothing.
- Produces: `make check-darwin` (used by every later task to measure progress) and the CI gate the spec requires "from the first task on". Until Task 4 finishes, the darwin job is expected to fail — that is the point of a gate.

- [ ] **Step 1: Gate the two crates that cannot compile on darwin**

In `ply-core/Cargo.toml`, delete these two lines from `[dependencies]`:

```toml
caps.workspace = true
seccompiler.workspace = true
```

and add, before `[dev-dependencies]`:

```toml
# Linux-only: capability bounding sets and seccomp filters. Everything that
# uses them lives under runtime/ns/ (and craft.rs), behind the same cfg.
[target.'cfg(target_os = "linux")'.dependencies]
caps.workspace = true
seccompiler.workspace = true
```

- [ ] **Step 2: Verify Linux is untouched**

Run: `make check`
Expected: fmt, clippy and all tests pass exactly as before (290 ply-core + 72 ply-cli tests at the time of writing).

- [ ] **Step 3: Add the local darwin check target**

In `Makefile`, after the `check:` target:

```make
# The macOS seam gate, runnable on Linux. Needs cargo-zigbuild
# (`uv tool install cargo-zigbuild`) and a `zig` on PATH; CI runs the same
# check natively on macos-latest. Clean = 0 errors under -D warnings.
check-darwin:
	rustup target add aarch64-apple-darwin >/dev/null
	cargo-zigbuild check --target aarch64-apple-darwin -p ply-cli
	cargo-zigbuild clippy --target aarch64-apple-darwin -p ply-cli -- -D warnings
```

- [ ] **Step 4: Run it and record the baseline**

Run: `make check-darwin 2>&1 | grep -E '^\s+--> ply-core' | sed -E 's/:[0-9]+:[0-9]+$//' | sort | uniq -c | sort -rn`
Expected: the check FAILS, and every error location is one of `ply-core/src/runtime/{security,container,mount,netns,exec}.rs`, `ply-core/src/craft.rs`, or `ply-core/src/runtime/run.rs` (the latter two lines: `nix::sched::CloneFlags` at `run.rs:8` and `nix::sched::clone` at `run.rs:1846`). No error may come from a dependency crate. Put the per-file counts in your report (baseline measured on 2026-09-02: 104 errors; security 61, container 38, mount 21, craft 5, netns 5, exec 2, run 2).

- [ ] **Step 5: Add CI**

Create `.github/workflows/ci.yml`:

```yaml
name: ci
on:
  push:
    branches: [main]
  pull_request:

jobs:
  linux:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup component add clippy rustfmt
      - run: make check

  # The macOS seam gate (spec: "CI runs cargo check --target
  # aarch64-apple-darwin -p ply-cli on every push from the first task on").
  # Native on Apple Silicon runners, so no cross toolchain is needed.
  darwin:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - run: rustup target add aarch64-apple-darwin && rustup component add clippy
      - run: cargo check --target aarch64-apple-darwin -p ply-cli
      - run: cargo clippy --target aarch64-apple-darwin -p ply-cli -- -D warnings
```

- [ ] **Step 6: Lint the workflow file**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml')); print('yaml ok')"`
Expected: `yaml ok`

- [ ] **Step 7: Report** — `make check` green; darwin baseline counts; stop.

---

### Task 2: The seam's types and the two supervisor helpers

**Files:**
- Create: `ply-core/src/runtime/backend.rs`
- Create: `ply-core/src/runtime/supervise.rs`
- Modify: `ply-core/src/runtime/mod.rs` (add `pub mod backend; pub mod supervise;`)

**Interfaces:**
- Consumes: `crate::manifest::{Capabilities, Manifest, Resources, RunUser}`, `crate::runtime::run::RunOptions` (exists, `#[derive(Clone)]`), `nix::sys::signal::Signal`.
- Produces (used verbatim by Tasks 3 and 4):

```rust
// runtime/backend.rs
pub struct InstanceSpec { /* fields below */ }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct Facts { pub loopback: bool, pub own_addresses: bool }
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct NetworkFacts { pub facts: Facts, pub in_stack_network: bool, pub alone: bool }
pub struct Launched { pub instance: Box<dyn Instance>, pub output: Box<dyn std::io::Read + Send> }
pub type Record<'a> = &'a mut dyn FnMut(i32, std::net::Ipv4Addr) -> crate::Result<()>;
pub trait Instance { fn pid(&self) -> i32; fn child_pid(&self) -> Option<i32>; fn ip(&self) -> std::net::Ipv4Addr; fn alive(&self) -> bool; fn signal(&self, sig: Signal) -> crate::Result<()>; fn try_wait(&mut self) -> crate::Result<Option<i32>>; fn tcp_open(&self, port: u16, timeout: std::time::Duration) -> std::io::Result<()>; }
pub trait Backend { fn capability(&self) -> std::result::Result<(), String>; fn preflight(&self, opts: RunOptions) -> crate::Result<RunOptions>; fn facts(&self) -> Facts; fn admit(&self, manifest: &Manifest, opts: &RunOptions) -> crate::Result<()>; fn attach(&self, opts: &RunOptions) -> crate::Result<()>; fn network(&self, opts: &RunOptions) -> NetworkFacts; fn launch(&self, spec: &InstanceSpec, record: Record<'_>) -> crate::Result<Launched>; fn terminal(&self, app: &str, slot: u32, nonce: &str) -> crate::Result<()>; }
pub fn scrub_instance_dir(dir: &std::path::Path);      // Task 3 fills the Linux body; Task 4 adds the non-Linux no-op
pub fn default_backend() -> crate::Result<Box<dyn Backend>>;  // Task 3: NsBackend; Task 4: the non-Linux error
// runtime/supervise.rs
pub enum Health { Healthy, Died, NoAnswer(Option<std::io::Error>) }
pub fn health_gate(instance: &dyn Instance, port: Option<u16>, grace: Duration, poll: Duration) -> Health
pub fn stop_with_patience(instance: &mut dyn Instance, stop: Signal, patience: Duration, poll: Duration) -> Option<i32>
```

- [ ] **Step 1: Write `backend.rs` (types and docs only — no implementation yet)**

```rust
//! The seam between `ply run`'s platform-neutral supervisor (`run.rs`) and
//! the platform's way of running ONE instance: Linux namespaces today
//! (`runtime/ns`), a microVM on macOS later. `run.rs` never names a
//! platform; it drives whatever `default_backend()` hands it through these
//! two traits.
//!
//! Order of calls in one run, all on the main thread:
//! `preflight` → `facts` → (manifest read) → `admit` → (host ports bound)
//! → `attach` → `network` → `launch`×N … `terminal` on demand.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::signal::Signal;

use crate::error::Result;
use crate::manifest::{Capabilities, Manifest, Resources, RunUser};
use crate::runtime::run::RunOptions;

/// Everything a backend needs to start one instance, and nothing a
/// platform decides. Built by the supervisor from the manifest, the
/// lockfile, `RunOptions`, and the composed environment.
pub struct InstanceSpec {
    /// Runtime identity: `--name`, else the package name. State pool,
    /// `<app>.ply`, volumes and the params tree key on it.
    pub app: String,
    /// `[package] name` — drives the `/opt/<name>` prefix, never renamed.
    pub package: String,
    pub n: u32,
    /// `run_dir()/instances/<app>.<n>` with `rw/ work/ root/ layers/`
    /// already created. The backend owns it from `launch` on: its
    /// `Instance` removes the directory when dropped. On a `launch`
    /// error the supervisor removes it.
    pub instance_dir: PathBuf,
    /// Image files, app image first, then the lockfile's packages in
    /// lockfile order — the overlay's lower layers, top first.
    pub images: Vec<PathBuf>,
    pub entrypoint: Vec<String>,
    pub cwd: PathBuf,
    /// Fully composed: manifest `[env]`, resolved params, `-e`, `HOME`,
    /// `TERM`, and an injected `PORT` when the supervisor decided one.
    pub env: Vec<(String, String)>,
    pub hostname: String,
    /// (host path, container path): declared volumes then `--link`s.
    pub binds: Vec<(PathBuf, String)>,
    /// Container paths of the declared volumes (chowned to `run_user`).
    pub volume_targets: Vec<String>,
    pub run_user: Option<RunUser>,
    /// `[package] capabilities` as declared; the backend decides what
    /// "keep" means on its platform.
    pub capabilities: Option<Capabilities>,
    /// Some declared port is < 1024.
    pub keep_net_bind: bool,
    pub privileged: bool,
    pub resources: Option<Resources>,
    /// Resolver for the instance (`--netns-dns`), if the run has one.
    pub dns: Option<String>,
    /// Names that resolve to the instance's own loopback (`--netns-peer`).
    pub local_aliases: Vec<String>,
}

/// What the backend knows before any network is joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    /// Host listeners for `internal` scope bind loopback and the pool
    /// connects to backends on loopback (rootless namespaces; a VM).
    /// `false`: the bridge gateway (rootful namespaces).
    pub loopback: bool,
    /// Every instance has a reachable address of its own, so `{host}`,
    /// `{addr}` and `{base_url}` resolve (rootful namespaces; a VM).
    pub own_addresses: bool,
}

/// What the backend knows once `attach` has run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkFacts {
    pub facts: Facts,
    /// This process is inside the stack's own network (a rootless run
    /// that joined the `--netns` it was given).
    pub in_stack_network: bool,
    /// Nothing else contends for the instance's ports, so `PORT` is not
    /// injected: rootful (own bridge address), a VM (own network), or a
    /// rootless run alone in a namespace with `--scale 1`.
    pub alone: bool,
}

/// What `launch` hands back: the instance and its combined stdout+stderr,
/// which the supervisor tees to its own stdout and the log ring.
pub struct Launched {
    pub instance: Box<dyn Instance>,
    pub output: Box<dyn std::io::Read + Send>,
}

/// Called by `launch` exactly once, when the instance's pid and address
/// are known and BEFORE it runs: writes the state file so the address is
/// visible to concurrent runs before anything can race it.
pub type Record<'a> = &'a mut dyn FnMut(i32, Ipv4Addr) -> Result<()>;

/// One running instance, as the supervisor sees it.
pub trait Instance {
    /// The pid recorded in the state file (`ply ps`, `ply stop`).
    fn pid(&self) -> i32;
    /// The pid a signal HANDLER may `kill` directly: the child process for
    /// namespaces; `None` for a backend whose instance is not a child (a
    /// VM), which is stopped through `signal` from the main loop instead.
    fn child_pid(&self) -> Option<i32>;
    fn ip(&self) -> Ipv4Addr;
    fn alive(&self) -> bool;
    fn signal(&self, sig: Signal) -> Result<()>;
    /// Non-blocking reap: `Some(exit code)` once the instance has ended
    /// (`128 + signal` for a signal death), `None` while it runs. Sticky:
    /// after the first `Some`, every later call returns the same value.
    fn try_wait(&mut self) -> Result<Option<i32>>;
    /// TCP connect to the instance's own `port` — the health gate and the
    /// `--after` probes go through here, never through a raw address.
    fn tcp_open(&self, port: u16, timeout: Duration) -> std::io::Result<()>;
}

/// A platform's runtime. One per `ply run`, on the main thread.
pub trait Backend {
    /// Can this host run instances at all? `Err(reason)` is what
    /// `ply check` will print (plan 2); `ply run` fails with it.
    fn capability(&self) -> std::result::Result<(), String>;
    /// First thing in a run. May amend the options (a rootless namespace
    /// run without a network makes its own) and prints its banners.
    fn preflight(&self, opts: RunOptions) -> Result<RunOptions>;
    fn facts(&self) -> Facts;
    /// After the manifest is read, before any host port is claimed:
    /// refuse (`Err`) or warn about what this platform cannot do for this
    /// app (rootless scale, missing subids).
    fn admit(&self, manifest: &Manifest, opts: &RunOptions) -> Result<()>;
    /// After host listeners are bound, before any launch: enter the
    /// stack's network (best effort — a failed join prints and stays on
    /// the host's) and prepare the host side (the bridge, rootful).
    fn attach(&self, opts: &RunOptions) -> Result<()>;
    fn network(&self, opts: &RunOptions) -> NetworkFacts;
    fn launch(&self, spec: &InstanceSpec, record: Record<'_>) -> Result<Launched>;
    /// The `exec` control command: serve a terminal into `app.slot` at
    /// `term-<nonce>.sock`. Backends without one return `Err`.
    fn terminal(&self, app: &str, slot: u32, nonce: &str) -> Result<()>;
}

/// Undo what a previous instance left under `dir` that a plain remove
/// cannot (Linux: layer mounts). Used on a stale instance directory by
/// `allocate_instance` and `state::reap_stale`.
pub fn scrub_instance_dir(dir: &Path) {
    let _ = dir; // Task 3 (Linux) and Task 4 (elsewhere) fill this in
}

/// The platform's backend, or why there is none.
pub fn default_backend() -> Result<Box<dyn Backend>> {
    Err(crate::Error::Runtime(
        "ply run is not available on this platform yet — the macOS runtime lands in a later release".into(),
    )) // Task 3 replaces this on Linux
}
```

Add to `runtime/mod.rs` (keep alphabetical): `pub mod backend;` and `pub mod supervise;`.

- [ ] **Step 2: Write the failing tests for the two helpers (`supervise.rs`, with the fake)**

```rust
//! Supervision written against `Instance`, not against a pid: the health
//! gate and the patient stop. Both were inlined in `run.rs` for years and
//! untestable; the fake below is the first test double the runtime has.

use std::time::{Duration, Instant};

use nix::sys::signal::Signal;

use crate::runtime::backend::Instance;

/// The deploy health gate's verdict.
pub enum Health {
    Healthy,
    /// The instance died inside the grace window.
    Died,
    /// A `[health] port` never answered within grace; the last connect
    /// error, if any attempt was made.
    NoAnswer(Option<std::io::Error>),
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct FakeInstance {
    pub polls: std::cell::Cell<u32>,
    /// From this poll on the instance is dead (exit `exit`).
    pub dies_at_poll: Option<u32>,
    pub exit: i32,
    /// From this poll on `tcp_open` succeeds; `None` = never.
    pub answers_from_poll: Option<u32>,
    /// A signal it complies with (ends with `exit`); SIGKILL always ends it (137).
    pub obeys: Option<Signal>,
    pub signals: std::cell::RefCell<Vec<Signal>>,
    ended: std::cell::Cell<Option<i32>>,
}

#[cfg(test)]
impl FakeInstance {
    fn tick(&self) -> Option<i32> {
        self.polls.set(self.polls.get() + 1);
        if self.ended.get().is_none() {
            if let Some(at) = self.dies_at_poll {
                if self.polls.get() >= at {
                    self.ended.set(Some(self.exit));
                }
            }
        }
        self.ended.get()
    }
}

#[cfg(test)]
impl Instance for FakeInstance {
    fn pid(&self) -> i32 { 4242 }
    fn child_pid(&self) -> Option<i32> { None }
    fn ip(&self) -> std::net::Ipv4Addr { std::net::Ipv4Addr::new(10, 77, 0, 2) }
    fn alive(&self) -> bool { self.tick().is_none() }
    fn signal(&self, sig: Signal) -> crate::Result<()> {
        self.signals.borrow_mut().push(sig);
        if sig == Signal::SIGKILL {
            self.ended.set(Some(137));
        } else if self.obeys == Some(sig) {
            self.ended.set(Some(self.exit));
        }
        Ok(())
    }
    fn try_wait(&mut self) -> crate::Result<Option<i32>> { Ok(self.tick()) }
    fn tcp_open(&self, _port: u16, _timeout: Duration) -> std::io::Result<()> {
        match self.answers_from_poll {
            Some(at) if self.polls.get() >= at => Ok(()),
            _ => Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLL: Duration = Duration::from_millis(1);
    const GRACE: Duration = Duration::from_millis(200);

    #[test]
    fn dying_during_grace_is_reported_not_retried() {
        let mut fake = FakeInstance::default();
        fake.dies_at_poll = Some(2);
        fake.answers_from_poll = Some(1);
        assert!(matches!(health_gate(&fake, Some(5432), GRACE, POLL), Health::Died));
    }

    #[test]
    fn a_port_that_answers_is_healthy_as_soon_as_it_does() {
        let mut fake = FakeInstance::default();
        fake.answers_from_poll = Some(3);
        let began = Instant::now();
        assert!(matches!(health_gate(&fake, Some(5432), GRACE, POLL), Health::Healthy));
        assert!(fake.polls.get() >= 3);
        assert!(began.elapsed() < GRACE, "healthy must not wait out the grace window");
    }

    #[test]
    fn without_a_port_surviving_the_grace_window_is_the_bar() {
        let fake = FakeInstance::default();
        let began = Instant::now();
        assert!(matches!(health_gate(&fake, None, GRACE, POLL), Health::Healthy));
        assert!(began.elapsed() >= GRACE);
    }

    #[test]
    fn a_port_that_never_answers_reports_the_last_error() {
        let fake = FakeInstance::default();
        match health_gate(&fake, Some(5432), GRACE, POLL) {
            Health::NoAnswer(Some(e)) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused),
            _ => panic!("expected NoAnswer with the connect error"),
        }
    }

    #[test]
    fn a_compliant_instance_gets_only_the_stop_signal() {
        let mut fake = FakeInstance::default();
        fake.obeys = Some(Signal::SIGTERM);
        let code = stop_with_patience(&mut fake, Signal::SIGTERM, Duration::from_millis(30), POLL);
        assert_eq!(code, Some(0));
        assert_eq!(*fake.signals.borrow(), vec![Signal::SIGTERM]);
    }

    #[test]
    fn a_stubborn_instance_is_killed_once_patience_runs_out() {
        let mut fake = FakeInstance::default();
        let code = stop_with_patience(&mut fake, Signal::SIGTERM, Duration::from_millis(30), POLL);
        assert_eq!(code, Some(137));
        assert_eq!(*fake.signals.borrow(), vec![Signal::SIGTERM, Signal::SIGKILL]);
    }

    #[test]
    fn the_declared_stop_signal_is_what_is_sent() {
        let mut fake = FakeInstance::default();
        fake.obeys = Some(Signal::SIGQUIT);
        fake.exit = 3;
        let code = stop_with_patience(&mut fake, Signal::SIGQUIT, Duration::from_millis(30), POLL);
        assert_eq!(code, Some(3));
        assert_eq!(*fake.signals.borrow(), vec![Signal::SIGQUIT]);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p ply-core supervise::`
Expected: compile error — `health_gate` and `stop_with_patience` not found.

- [ ] **Step 4: Implement the two helpers (same file, above the tests)**

These transcribe today's `wait_healthy` loop (`run.rs:910–978`) and `stop_instance` (`run.rs:889–906`) onto the trait. Semantics preserved: the gate polls `alive` first, then the port; with no port, surviving the window is healthy; a stop sends the declared signal, waits `patience`, then SIGKILL and waits for the reap (bounded).

```rust
/// The deploy health gate. With a port: a TCP connect within `grace`.
/// Without: the instance just has to be alive after the window.
pub fn health_gate(instance: &dyn Instance, port: Option<u16>, grace: Duration, poll: Duration) -> Health {
    let deadline = Instant::now() + grace;
    let mut last_err: Option<std::io::Error> = None;
    loop {
        if !instance.alive() {
            return Health::Died;
        }
        if let Some(port) = port {
            match instance.tcp_open(port, Duration::from_millis(300)) {
                Ok(()) => return Health::Healthy,
                Err(e) => last_err = Some(e),
            }
        }
        if Instant::now() >= deadline {
            return match port {
                Some(_) => Health::NoAnswer(last_err),
                None => Health::Healthy,
            };
        }
        std::thread::sleep(poll);
    }
}

/// Deliberate stop: `stop`, up to `patience` to comply, then SIGKILL.
/// Returns the exit code once the instance is reaped; `None` only if even
/// SIGKILL did not end it within five seconds.
pub fn stop_with_patience(instance: &mut dyn Instance, stop: Signal, patience: Duration, poll: Duration) -> Option<i32> {
    let _ = instance.signal(stop);
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(code)) = instance.try_wait() {
            return Some(code);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(poll);
    }
    let _ = instance.signal(Signal::SIGKILL);
    let kill_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(Some(code)) = instance.try_wait() {
            return Some(code);
        }
        if Instant::now() >= kill_deadline {
            return None;
        }
        std::thread::sleep(poll);
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ply-core supervise::`
Expected: 7 passed.

- [ ] **Step 6: Whole check**

Run: `make check`
Expected: green (clippy: `FakeInstance` is `cfg(test)` and derives `Default`, so neither dead-code nor `new_without_default` fires; `Health` and both helpers are `pub` and unused until Task 3 — not a warning for `pub` items in a library).

- [ ] **Step 7: Report** — stop.

---

### Task 3: `NsBackend` — move the Linux half of `run.rs` behind the trait, and rewire the supervisor

This is the load-bearing task. It is a move, not a rewrite: every block below is named by its current line range in `ply-core/src/runtime/run.rs` (2880 lines, as of 2026-09-02) and lands in a named function. Read `run.rs:87–888` (`run`), `:889–978` (`stop_instance`, `wait_healthy`), `:1432–1486` (`Instance` + `Drop`), `:1551–1981` (`alone_in_its_network`, `in_namespace`, `launch_instance`), `:2038–2260` (subid block), `:2510–2650` (signal table, `allocate_instance`, `InstanceGuard`), `:2701–2724` (`rootless_scale_guard`) before starting.

**Files:**
- Create: `ply-core/src/runtime/ns/mod.rs`
- Create: `ply-core/src/runtime/ns/subid.rs`
- Modify: `ply-core/src/runtime/mod.rs` (add `pub mod ns;` — NOT gated yet; Task 4 gates it)
- Modify: `ply-core/src/runtime/backend.rs` (`scrub_instance_dir`, `default_backend` Linux bodies)
- Modify: `ply-core/src/runtime/run.rs` (everything listed in the mapping table)
- Modify: `ply-core/src/runtime/state.rs:101–124` (`reap_stale`)
- Modify: `ply-cli/src/commands/setup.rs:336` (`parse_subid` path)

**Interfaces:**
- Consumes: Task 2's traits and helpers verbatim.
- Produces:
  - `pub struct NsBackend` with `pub fn new() -> NsBackend` (`rootless: !crate::paths::is_root()`, `own_ns: RefCell<Option<NetNs>>`), `impl Backend for NsBackend`.
  - `pub(crate) struct NsInstance` implementing `Instance`.
  - `ply_core::runtime::ns::subid::{parse_subid, SubIdRange, subid_gap}` (pub) and `write_id_maps` (pub(crate)).
  - `runtime::backend::default_backend()` on Linux returns `Ok(Box::new(NsBackend::new()))`.
  - `runtime::backend::scrub_instance_dir(dir)` on Linux: for each entry of `dir/layers`, `mount::unmount_detach(&entry.path())`.

**Mapping table (today → destination):**

| `run.rs` lines today | Content | Destination |
|---|---|---|
| 8, 20–22 (`CloneFlags`, `Cgroup`, `child_main/ContainerSpec`, `loopdev, mount, network`) imports | Linux imports | deleted from run.rs; used in `ns/mod.rs` |
| 88–157 | own-netns creation, `--privileged` warning, rootless banner + AppArmor check | `NsBackend::preflight` |
| 181–202 | subid warning, `rootless_scale_guard` match | `NsBackend::admit` |
| 226–242 | join `--netns` (best effort) | `NsBackend::attach` (plus `network::ensure_bridge()?` when rootful, from 340–342) |
| 1551–1565 | `alone_in_its_network`, `in_namespace` | `ns/mod.rs` (private) — feed `NsBackend::network` |
| 1675–1707 | `InstanceGuard` creation + layer mount/extract loop | `NsBackend::launch` |
| 1708–1716, 1718–1733 | sync + log pipes, `keep_set` + "keeps N capabilities" line | `NsBackend::launch` |
| 1768–1790 | `ContainerSpec` construction | `NsBackend::launch` |
| 1834–1885 | clone, net lock, `write_id_maps` / cgroup + IP + veth, error unwind | `NsBackend::launch` |
| 1923–1925 | `hosts::add_entry` (rootful) | `NsBackend::launch`, after `record` |
| 1927–1929, 1951–1953 | drop `log_fd`; release the child | `NsBackend::launch` (release is the last thing before returning) |
| 1432–1441, 1455–1486 | `Instance` struct + `Drop` | split: pools/state/params-tree part → `Running` (run.rs); hosts removal + guard → `NsInstance` (ns) |
| 2038–2260 + 2433–2509 | subid block + `subid_tests` | `ns/subid.rs` |
| 2627–2650 | `InstanceGuard` | `ns/mod.rs` |
| 2701–2724 + tests `rootful_and_single_instance_pass`, `publish_lifts_the_rootless_scale_refusal`, `rootless_scale_with_declared_ports_refuses`, `rootless_scale_without_ports_warns` | `ScaleGuard`, `rootless_scale_guard` | `ns/mod.rs` (+ its `#[cfg(test)] mod tests`) |
| 889–906 | `stop_instance` | run.rs, body becomes `supervise::stop_with_patience` |
| 910–978 | `wait_healthy` | run.rs, body becomes `supervise::health_gate` + the prints/params-tree writes |
| 2587–2591 (stale dir self-heal) | `mount::unmount_detach` loop | `backend::scrub_instance_dir(&dir)` |
| everything else in `run` and `launch_instance` | supervisor | stays in run.rs, unchanged text |

- [ ] **Step 1: Create `ns/subid.rs`** by moving `run.rs:2038–2260` (from `pub struct SubIdRange` through `subid_gap` and the private helpers `mirror_own_map`, `username_for`, `have`, `write_id_maps`) and the `subid_tests` module (`:2433–2509`) verbatim. Module header:

```rust
//! Rootless id mapping: the invoking user's /etc/subuid range mapped into
//! the child's user namespace through newuidmap/newgidmap.
use std::path::Path;
use crate::error::{Error, Result};
```

Make `write_id_maps` `pub(crate)`; keep `parse_subid`, `SubIdRange`, `subid_gap` `pub`. Fix the one external caller: `ply-cli/src/commands/setup.rs:336` → `ply_core::runtime::ns::subid::parse_subid(&t, &user, id)`.

- [ ] **Step 2: Create `ns/mod.rs`** — the backend. Skeleton with the moved bodies indicated by their line ranges:

```rust
//! The Linux backend: one instance = one process in fresh mount/PID/UTS/
//! IPC (+user, rootless; +net, rootful) namespaces, layers as squashfs
//! loop mounts (root) or store extractions (rootless), rights dropped in
//! the child (see container.rs). Everything platform-specific about
//! `ply run` on Linux is in this module tree.

pub mod subid;

use std::cell::RefCell;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use nix::sched::CloneFlags;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;

use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::runtime::backend::{Backend, Facts, Instance, InstanceSpec, Launched, NetworkFacts, Record};
use crate::runtime::cgroup::Cgroup;
use crate::runtime::container::{child_main, ContainerSpec};
use crate::runtime::netns::NetNs;
use crate::runtime::run::RunOptions;
use crate::runtime::{hosts, loopdev, mount, network, state};
use crate::store::Store;

pub struct NsBackend {
    rootless: bool,
    /// A rootless run with no namespace handed to it makes its own in
    /// `preflight`; it lives as long as the backend (the run).
    own_ns: RefCell<Option<NetNs>>,
    store: Store,
}

impl NsBackend {
    pub fn new() -> Result<NsBackend> {
        Ok(NsBackend {
            rootless: !crate::paths::is_root(),
            own_ns: RefCell::new(None),
            store: Store::open_default()?,
        })
    }
}

impl Backend for NsBackend {
    fn capability(&self) -> std::result::Result<(), String> {
        Ok(()) // namespaces are a kernel feature every supported Linux has
    }

    fn preflight(&self, opts: RunOptions) -> Result<RunOptions> {
        // MOVED: run.rs:88–157. `own_ns` → `*self.own_ns.borrow_mut() = Some(ns)`;
        // `opts_owned` becomes the returned value; `rootless` is `self.rootless`.
        // The AppArmor check's `return Err(...)` stays an `Err` here.
        todo!("move run.rs:88-157 here")
    }

    fn facts(&self) -> Facts {
        Facts { loopback: self.rootless, own_addresses: !self.rootless }
    }

    fn admit(&self, manifest: &Manifest, opts: &RunOptions) -> Result<()> {
        // MOVED: run.rs:181–202 (subid warning, then the scale guard match);
        // `ctx.manifest` → `manifest`; `subid_gap` → `subid::subid_gap`.
        todo!("move run.rs:181-202 here")
    }

    fn attach(&self, opts: &RunOptions) -> Result<()> {
        // MOVED: run.rs:226–242 — the best-effort join (`_joined` is not
        // needed; the print on failure stays). Then, from run.rs:340–342:
        if !self.rootless {
            network::ensure_bridge()?;
        }
        Ok(())
    }

    fn network(&self, opts: &RunOptions) -> NetworkFacts {
        NetworkFacts {
            facts: self.facts(),
            in_stack_network: opts.netns.as_ref().is_some_and(|p| in_namespace(p)),
            alone: alone_in_its_network(self.rootless, opts),
        }
    }

    fn launch(&self, spec: &InstanceSpec, record: Record<'_>) -> Result<Launched> {
        let rootless = self.rootless;
        let app = &spec.app;
        let n = spec.n;
        let instance_dir = &spec.instance_dir;
        let guard = InstanceGuard {
            dir: instance_dir.clone(),
            mounted_layers: std::cell::RefCell::new(Vec::new()),
        };
        // MOVED: run.rs:1680–1707 — the layer loop over `spec.images`
        // (was `all_images`), producing `layers: Vec<PathBuf>`; `store` is `&self.store`.
        // MOVED: run.rs:1708–1716 — sync + log pipes.
        // MOVED: run.rs:1718–1733 — keep_caps from `spec.capabilities.as_ref()`
        //        and `spec.keep_net_bind`, and the "keeps N capability/ies" line.
        // MOVED: run.rs:1768–1790 — ContainerSpec, as a local named `container` (`spec` is the InstanceSpec); field sources:
        //   layers, instance_dir: instance_dir.clone(), hostname: spec.hostname.clone(),
        //   cwd: spec.cwd.clone(), env: spec.env.clone(), argv: spec.entrypoint.clone(),
        //   binds: spec.binds.clone(), sync_rx, volume_targets: spec.volume_targets.clone(),
        //   keep_caps, privileged: spec.privileged, rootless, dns: spec.dns.clone(),
        //   local_aliases: spec.local_aliases.clone(), run_user: spec.run_user.clone(),
        //   log_fd: Some(log_tx)
        // MOVED: run.rs:1834–1885 — clone with the same flags; net lock; the
        //        `prepared` closure (rootless: subid::write_id_maps + 127.0.0.1;
        //        rootful: Cgroup::create(&format!("{app}.{n}"), spec.resources.as_ref()),
        //        allocate_ip over state::list(), setup_instance); the error unwind
        //        (drop sync_tx, SIGKILL, waitpid, return Err).
        // NEW: record the state file while the net lock is still held and the
        // child is still parked on the sync pipe — same unwind on error:
        let hosts_entry = !rootless;
        let settled = record(child.as_raw(), ip).and_then(|()| {
            if hosts_entry {
                hosts::add_entry(app, n, ip) // MOVED: run.rs:1923–1925
            } else {
                Ok(())
            }
        });
        if let Err(e) = settled {
            drop(sync_tx); // EOF → the parked child aborts
            let _ = signal::kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            return Err(e);
        }
        drop(container.log_fd); // MOVED: run.rs:1927–1929 — our copy of the write end must close or the tee never sees EOF
        // MOVED: run.rs:1951–1953 — release the child (write 1 byte, drop sync_tx).
        Ok(Launched {
            instance: Box::new(NsInstance {
                app: app.clone(),
                n,
                child,
                ip,
                ended: None,
                hosts_entry,
                _cgroup: cgroup,
                _guard: guard,
            }),
            output: Box::new(std::fs::File::from(log_rx)),
        })
    }

    fn terminal(&self, app: &str, slot: u32, nonce: &str) -> Result<()> {
        crate::runtime::term::spawn(app, slot, nonce);
        Ok(())
    }
}

pub(crate) struct NsInstance {
    app: String,
    n: u32,
    child: Pid,
    ip: Ipv4Addr,
    ended: Option<i32>,
    hosts_entry: bool,
    _cgroup: Option<Cgroup>,
    _guard: InstanceGuard,
}

impl Instance for NsInstance {
    fn pid(&self) -> i32 { self.child.as_raw() }
    fn child_pid(&self) -> Option<i32> { Some(self.child.as_raw()) }
    fn ip(&self) -> Ipv4Addr { self.ip }
    fn alive(&self) -> bool {
        self.ended.is_none() && unsafe { nix::libc::kill(self.child.as_raw(), 0) == 0 }
    }
    fn signal(&self, sig: Signal) -> Result<()> {
        signal::kill(self.child, sig).map_err(|e| Error::Runtime(format!("kill {}: {e}", self.child)))
    }
    fn try_wait(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.ended {
            return Ok(Some(code));
        }
        loop {
            match waitpid(self.child, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => return Ok(None),
                Ok(WaitStatus::Exited(_, code)) => {
                    self.ended = Some(code);
                    return Ok(Some(code));
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    self.ended = Some(128 + sig as i32);
                    return Ok(self.ended);
                }
                Ok(_) => return Ok(None), // stop/continue notices are not deaths
                Err(nix::errno::Errno::EINTR) => continue,
                // Reaped by someone else: gone, and nothing more to learn.
                Err(nix::errno::Errno::ECHILD) => {
                    self.ended = Some(0);
                    return Ok(self.ended);
                }
                Err(e) => return Err(Error::Runtime(format!("waitpid: {e}"))),
            }
        }
    }
    fn tcp_open(&self, port: u16, timeout: std::time::Duration) -> std::io::Result<()> {
        let addr = std::net::SocketAddr::from((self.ip, port));
        crate::runtime::publish::connect_either_family(addr, timeout).map(|_| ())
    }
}

impl Drop for NsInstance {
    fn drop(&mut self) {
        if self.hosts_entry {
            let _ = hosts::remove_entry(&self.app, self.n);
        }
        // `_guard` drops next: unmounts the layers, removes the instance dir.
    }
}

// MOVED verbatim: run.rs:2627–2650 (InstanceGuard + Drop),
// run.rs:1551–1565 (alone_in_its_network, in_namespace),
// run.rs:2701–2724 (ScaleGuard, rootless_scale_guard) and, in a
// `#[cfg(test)] mod tests`, the four scale-guard tests from run.rs's `mod tests`.
```

Replace every `todo!` with the moved code before running anything — `todo!` in a runtime path is a plan failure, the markers above only say where each range lands.

- [ ] **Step 3: Fill the Linux bodies in `backend.rs`**

```rust
pub fn scrub_instance_dir(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir.join("layers")) {
        for entry in entries.filter_map(|e| e.ok()) {
            crate::runtime::mount::unmount_detach(&entry.path());
        }
    }
}

pub fn default_backend() -> Result<Box<dyn Backend>> {
    Ok(Box::new(crate::runtime::ns::NsBackend::new()?))
}
```

(Task 4 puts `#[cfg(target_os = "linux")]` on both and adds the non-Linux twins.)

- [ ] **Step 4: Point `state::reap_stale` and `allocate_instance` at the scrub**

`state.rs:111–116` becomes `crate::runtime::backend::scrub_instance_dir(&instance_dir);`. `run.rs:2587–2591` (inside `allocate_instance`'s stale-dir branch) becomes `crate::runtime::backend::scrub_instance_dir(&dir);`.

- [ ] **Step 5: Rewire `run()`** — the supervisor. Replace the regions named in the table; the rest of the function's text stays byte-identical. The new head of `run` up to the `--after` block:

```rust
pub fn run(opts: &RunOptions) -> Result<i32> {
    let backend = crate::runtime::backend::default_backend()?;
    if let Err(reason) = backend.capability() {
        return Err(Error::Runtime(reason));
    }
    let opts = &backend.preflight(opts.clone())?;
    let facts = backend.facts();

    let store = Store::open_default()?;
    let mut ctx = prepare_app(
        &opts.image,
        &opts.cli_env,
        opts.allow_insecure,
        opts.entrypoint.as_deref(),
        &store,
        RunFacts {
            name_override: opts.name.as_deref(),
            host_available: facts.own_addresses,
            port: opts.publish.first().map(|p| p.instance_port),
            scale: opts.scale,
        },
    )?;
    let identity = opts
        .name
        .clone()
        .unwrap_or_else(|| ctx.manifest.package.name.clone());
    backend.admit(&ctx.manifest, opts)?;

    let listeners: Vec<(crate::runtime::publish::Publish, std::net::TcpListener)> = opts
        .publish
        .iter()
        .map(|spec| crate::runtime::publish::bind(*spec, facts.loopback).map(|l| (*spec, l)))
        .collect::<Result<Vec<_>>>()?;
    backend.attach(opts)?;
    let net = backend.network(opts);
    // … pools loop unchanged, except `spec.scope.bind_addr(rootless)` → `bind_addr(facts.loopback)`
```

Then, in order:
- The multi-publish scale warning (`run.rs:267–276`): condition becomes `if !net.alone && opts.publish.len() > 1 && opts.scale > 1`.
- The PORT-override warning (`:282–297`): `let injecting = !net.alone && !first.instance_port_explicit;`.
- The `--after` block (`:329`): `let in_stack_network = net.in_stack_network;` (drop the `in_namespace` call).
- Delete `:340–342` (`ensure_bridge` — now in `attach`). Keep `state::reap_stale()`.
- Both `prepare_app` calls (`:159–171` shown above, and the deploy path `:583–595`) use `host_available: facts.own_addresses`.
- Every `launch_instance(&ctx, opts, &store, rootless, …)` call (5 sites: `:413`, `:545`, `:636`, `:786`, `:837`) becomes `launch_instance(backend.as_ref(), &ctx, opts, &net, …)` with the same slot/restarts/publishing arguments.
- `register_child(instance.child.as_raw())` (`:414`, `:646`) → `register_child(instance.inner.child_pid().unwrap_or(0))`; `update_child(info.sig_idx, instance.child.as_raw())` (`:555`, `:809`, `:848`) → `update_child(info.sig_idx, instance.inner.child_pid().unwrap_or(0))`.
- The death collection (`:438–451`) becomes:

```rust
        let mut deaths: Vec<(u32, i32, bool)> = Vec::new(); // (slot, code, failed)
        for running in instances.iter_mut() {
            match running.inner.try_wait() {
                Ok(Some(code)) => deaths.push((running.n, code, code != 0)),
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
```

  and the consumer (`:476–480`) matches on the slot: `for (slot, code, failed) in deaths { let Some(pos) = instances.iter().position(|i| i.n == slot) else { continue; };` (drop the now-redundant `let slot = instances[pos].n;`).
- SIGKILL escalation (`:471`): `let _ = instance.inner.signal(Signal::SIGKILL);`.
- The `Exec` control command (`:748–768`) becomes:

```rust
                    crate::runtime::control::Command::Exec { slot, nonce } => {
                        if !slots.contains_key(&slot) {
                            crate::runtime::control::write_result(&app_name, "exec", false, &format!("no instance .{slot}"));
                        } else {
                            match backend.terminal(&app_name, slot, &nonce) {
                                Ok(()) => {
                                    crate::runtime::events::emit(&app_name, "terminal", &format!("shell opened into {app_name}.{slot}"));
                                    crate::runtime::control::write_result(&app_name, "exec", true, &format!("terminal serving at term-{nonce}.sock"));
                                }
                                Err(e) => crate::runtime::control::write_result(&app_name, "exec", false, &e.to_string()),
                            }
                        }
                    }
```

- Delete the `waitpid`/`WaitStatus`/`Pid` imports that are no longer used; keep `signal::{self, Signal}` (handlers) and add `use crate::runtime::backend::{Backend, InstanceSpec, NetworkFacts};`.

- [ ] **Step 6: Replace `Instance` with `Running`, and the two helpers**

```rust
/// One instance as the supervisor holds it: the backend's handle plus the
/// host-side bookkeeping that is the same on every platform.
struct Running {
    app: String,
    n: u32,
    inner: Box<dyn crate::runtime::backend::Instance>,
    /// Published pools this instance is registered in (removed on Drop, so
    /// every stop path — death, roll, shutdown — also stops traffic).
    pools: Vec<crate::runtime::publish::Pool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        for pool in &self.pools {
            pool.remove(self.n);
        }
        InstanceState::remove(&self.app, self.n);
        // (the two params-tree writes from run.rs:1462–1483, verbatim)
        // `inner` drops after this body: the backend's own teardown —
        // hosts entry, layer unmounts, the instance directory.
    }
}

/// Deliberate stop: the declared signal, up to 10s to comply, then KILL.
/// Reaps the instance so its death never reaches the policy loop.
fn stop_instance(mut instance: Running, stop_signal: Signal) {
    crate::runtime::supervise::stop_with_patience(
        instance.inner.as_mut(),
        stop_signal,
        std::time::Duration::from_secs(10),
        std::time::Duration::from_millis(100),
    );
    drop(instance); // pools, state, params tree, then the backend's teardown
}

/// The deploy health gate. With [health] port: TCP connect within grace.
/// Without: the process just has to be alive after a short settle.
fn wait_healthy(ctx: &AppContext, instance: &Running) -> bool {
    let (port, grace) = match &ctx.manifest.health {
        Some(health) => (
            health.port,
            crate::manifest::parse_duration(&health.grace)
                .unwrap_or(std::time::Duration::from_secs(10)),
        ),
        None => (None, std::time::Duration::from_secs(1)),
    };
    let publish_state = |state: &str| {
        if let Err(e) = params_tree::publish(&instance.app, "state", state) {
            eprintln!("ply: warning: params tree {}/state: {e}", instance.app);
        }
    };
    use crate::runtime::supervise::Health;
    match crate::runtime::supervise::health_gate(
        instance.inner.as_ref(),
        port,
        grace,
        std::time::Duration::from_millis(200),
    ) {
        Health::Healthy => {
            publish_state("healthy");
            true
        }
        Health::Died => {
            eprintln!("ply: health gate: {}.{} died during grace", instance.app, instance.n);
            publish_state("unhealthy");
            false
        }
        Health::NoAnswer(last_err) => {
            eprintln!(
                "ply: health gate: no answer on {}:{} within {grace:?} — last error: {}",
                instance.inner.ip(),
                port.unwrap_or(0),
                last_err.map(|e| e.to_string()).unwrap_or_else(|| "none".into()),
            );
            publish_state("unhealthy");
            false
        }
    }
}
```

(`NoAnswer` only occurs with a port, so `port.unwrap_or(0)` never prints 0; the old `<no ip in state!>` branch is gone because the address now comes from the instance itself.)

- [ ] **Step 7: Split `launch_instance`** into the supervisor half (stays) and the spec hand-off:

```rust
fn launch_instance(
    backend: &dyn Backend,
    ctx: &AppContext,
    opts: &RunOptions,
    net: &NetworkFacts,
    slot: Option<u32>,
    restarts: u32,
    publish: &[PublishWiring],
) -> Result<Running> {
    let manifest = &ctx.manifest;
    // UNCHANGED: run.rs:1576–1674 (identity, allocate_instance, run_user,
    // volumes → binds + volume_targets with the empty-volume warning, --link binds).
    // UNCHANGED: run.rs:1734–1741 (HOME for run_user) and 1742–1767 (PORT
    // injection) — with `rootless && !alone_in_its_network` replaced by `!net.alone`:
    let injected_port = match (publish.first(), !net.alone) {
        (Some(w), true) if !w.spec.instance_port_explicit => { /* as today */ }
        _ => None,
    };
    let spec = InstanceSpec {
        app: app.clone(),
        package: manifest.package.name.clone(),
        n,
        instance_dir: instance_dir.clone(),
        images: std::iter::once(ctx.image.clone()).chain(ctx.dep_images.iter().cloned()).collect(),
        entrypoint: ctx.entrypoint.clone(),
        cwd: manifest.package.workdir.clone().map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/opt/{}", manifest.package.name))),
        env: spec_env,
        hostname: app.clone(),
        binds,
        volume_targets,
        run_user: run_user.clone(),
        capabilities: manifest.package.capabilities.clone(),
        keep_net_bind: manifest.ports.values().any(|p| *p < 1024),
        privileged: opts.privileged,
        resources: manifest.resources.clone(),
        dns: opts.netns_dns.clone(),
        local_aliases: opts.netns_peers.clone(),
    };
    // UNCHANGED: run.rs:1792–1832 (params-tree facts, published BEFORE launch —
    // the child re-binds them read-only inside /run/ply/self).
    let mut ring = crate::runtime::logring::RingWriter::create(&app, n)?;
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let loopback = net.facts.loopback;
    let mut record = |pid: i32, ip: Ipv4Addr| -> Result<()> {
        InstanceState {
            app: app.clone(),
            n,
            pid,
            ip,
            ports: manifest.ports.clone(),
            image: ctx.image.display().to_string(),
            started,
            restarts,
            health_port: match (injected_port, manifest.health.as_ref().and_then(|h| h.port)) {
                (Some(injected), Some(_)) => Some(injected),
                (_, declared) => declared,
            },
            published_port: publish.first().map(|w| w.spec.host_port),
            instance_port: publish.first().map(|w| w.spec.instance_port),
            published_addr: publish.first().map(|w| {
                format!("{}:{}", w.spec.scope.connect_addr(loopback), w.spec.host_port)
            }),
            domains: opts.domains.clone(),
        }
        .save()
    };
    let launched = backend.launch(&spec, &mut record).map_err(|e| {
        let _ = crate::paths::force_remove_dir_all(&instance_dir);
        e
    })?;
    // The log tee (run.rs:1930–1949, verbatim) reading from `launched.output`
    // instead of `File::from(log_rx)`:
    {
        let mut output = launched.output;
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            loop {
                match output.read(&mut buf) {
                    Ok(0) | Err(_) => break, // instance ended
                    Ok(size) => {
                        let chunk = &buf[..size];
                        let _ = std::io::stdout().write_all(chunk);
                        let _ = std::io::stdout().flush();
                        ring.append(chunk);
                    }
                }
            }
        });
    }
    let ip = launched.instance.ip();
    // UNCHANGED: run.rs:1955–1969 (pool join) using `ip` and `injected_port`.
    Ok(Running { app, n, inner: launched.instance, pools })
}
```

Keep the existing comments with the code they belong to. The existing tests `health_port_tests`, `multi_publish_tests`, `discovery_tests`, and the three stop/volume tests in `mod tests` stay in run.rs (their subjects did not move); the four scale-guard tests and `subid_tests` move with their subjects (Step 1–2).

- [ ] **Step 8: Build and test on Linux**

Run: `make check`
Expected: green; the same test names as before this task (some now under `runtime::ns::`), plus Task 2's seven. If clippy flags `too_many_arguments` on `launch_instance` (7 is the limit and it has 7), you counted wrong — do not add an `allow`.

- [ ] **Step 9: Measure the darwin check**

Run: `make check-darwin 2>&1 | grep -E '^\s+--> ply-core' | sed -E 's/:[0-9]+:[0-9]+$//' | sort | uniq -c | sort -rn`
Expected: `run.rs` no longer appears (its two errors moved into `ns/mod.rs`); the other files are the same set as the Task 1 baseline. Not clean yet — Task 4 gates them.

- [ ] **Step 10: Owner verification (hand-over, not automated)** — after this task the owner runs, in their own terminal, the same checks as before the change and compares output line by line:

```sh
cargo build -p ply-cli
./target/debug/ply run postgres@17 -e POSTGRES_PASSWORD=dev -p 5433      # rootless: banner, "publishing", healthy, Ctrl-C stops within 10s
cd ~/projects/ply-labs && ~/projects/ply/target/debug/ply up               # two members, {db.url} wiring, `ply ps`, `ply logs db`, `ply stop`
```

and on the droplet (rootful) a `ply restart` of the plybox stack after `ply self-update` once released. Report these as pending in the task report; they are not a gate for Task 4.

---

### Task 4: Fence the Linux modules and make `ply-cli` compile for darwin

**Files:**
- Move (`git mv`): `ply-core/src/runtime/{container,mount,netns,network,security,cgroup,loopdev,exec,term}.rs` → `ply-core/src/runtime/ns/`
- Modify: `ply-core/src/runtime/mod.rs`, `ply-core/src/runtime/ns/mod.rs` (module declarations and `use` paths), `ply-core/src/runtime/backend.rs` (cfg twins), `ply-core/src/lib.rs` (`craft` gate), `ply-core/src/runtime/state.rs` (nothing further — it calls `backend::scrub_instance_dir`)
- Modify: `ply-cli/src/commands/{up,exec,craft,setup,mod}.rs`
- Modify: every `use crate::runtime::{container,mount,netns,network,security,cgroup,loopdev,exec,term}` path in ply-core (`grep -rn 'runtime::\(container\|mount\|netns\|network\|security\|cgroup\|loopdev\|exec\|term\)' ply-core/src ply-cli/src`)

**Interfaces:**
- Consumes: Task 3's `NsBackend`, `backend::{scrub_instance_dir, default_backend}`.
- Produces: `ply_core::runtime::ns::{container, mount, netns, network, security, cgroup, loopdev, exec, term, subid}` (Linux only); `runtime::backend::default_backend()` non-Linux error; a clean `make check-darwin`.

- [ ] **Step 1: Move the files**

```sh
cd ply-core/src/runtime && for f in container mount netns network security cgroup loopdev exec term; do git mv $f.rs ns/$f.rs; done
```

In `runtime/mod.rs` delete those nine `pub mod` lines and add:

```rust
/// The Linux backend and everything only it may use: namespaces, mounts,
/// loop devices, cgroups, capabilities, seccomp, the bridge, `ply exec`.
#[cfg(target_os = "linux")]
pub mod ns;
```

In `ns/mod.rs` add, at the top: `pub mod cgroup; pub mod container; pub mod exec; pub mod loopdev; pub mod mount; pub mod netns; pub mod network; pub mod security; pub mod subid; pub mod term;` and change its own imports from `crate::runtime::{cgroup, …}` to `self::{cgroup, …}` / `super::hosts` etc.

- [ ] **Step 2: Fix every path** — inside the moved files, `crate::runtime::mount` → `crate::runtime::ns::mount` (and likewise for the other eight); `crate::runtime::hosts`, `params_tree`, `state`, `publish` are unchanged. Run `cargo check -p ply-core` until clean; then `cargo check -p ply-cli` and fix `ply-cli/src/commands/up.rs:343,355` (`runtime::netns` → `runtime::ns::netns`), `exec.rs:6` (`runtime::exec` → `runtime::ns::exec`).

- [ ] **Step 3: Gate `craft`** — `ply-core/src/lib.rs`: `#[cfg(target_os = "linux")] pub mod craft;`.

- [ ] **Step 4: The cfg twins in `backend.rs`**

```rust
#[cfg(target_os = "linux")]
pub fn scrub_instance_dir(dir: &Path) { /* Task 3 body */ }

#[cfg(not(target_os = "linux"))]
pub fn scrub_instance_dir(_dir: &Path) {}

#[cfg(target_os = "linux")]
pub fn default_backend() -> Result<Box<dyn Backend>> {
    Ok(Box::new(crate::runtime::ns::NsBackend::new()?))
}

#[cfg(not(target_os = "linux"))]
pub fn default_backend() -> Result<Box<dyn Backend>> {
    Err(crate::Error::Runtime(
        "ply run is not available on this platform yet — the macOS runtime lands in a later release".into(),
    ))
}
```

- [ ] **Step 5: Gate the CLI's Linux-only commands** — keep the clap surface identical on every platform (the `Command` enum and args structs are not gated); only the bodies are:

`ply-cli/src/commands/exec.rs`:

```rust
use anyhow::Result;

use crate::cli::ExecArgs;

#[cfg(target_os = "linux")]
pub fn exec(args: ExecArgs) -> Result<()> {
    let code = ply_core::runtime::ns::exec::exec(&args.app, &args.cmd)?;
    std::process::exit(code);
}

#[cfg(not(target_os = "linux"))]
pub fn exec(_args: ExecArgs) -> Result<()> {
    anyhow::bail!("ply exec is not available on this platform yet")
}
```

`ply-cli/src/commands/craft.rs`: wrap the existing file body (its `use ply_core::craft…` and `dispatch`) in `#[cfg(target_os = "linux")] mod linux { … }` re-exported as `pub use linux::dispatch;`, and add:

```rust
#[cfg(not(target_os = "linux"))]
pub fn dispatch(_command: crate::cli::CraftCommand) -> anyhow::Result<()> {
    anyhow::bail!("ply craft needs Linux — it builds inside a namespace sandbox")
}
```

`ply-cli/src/commands/setup.rs`: same shape — the whole existing body under `#[cfg(target_os = "linux")]` with `pub use` of `exec`, and a non-Linux `exec(_args: SetupArgs)` that bails with `ply setup is Linux-only (subuid ranges, AppArmor, privileged ports); nothing to set up here`.

`ply-cli/src/commands/up.rs:340–366`: move the `netns` match (and the `egress_dns` it fills) into a Linux-only helper, with a non-Linux twin whose `Option` is always `None` but still has a `.path()` so the later `if let Some(ns) = &netns { cmd.arg("--netns").arg(ns.path()); … }` block (`:375–383`) compiles unchanged on both sides and nothing is left unused:

```rust
/// One network for the stack (rootless: a namespace this process owns;
/// rootful: none needed — the bridge). Returns it and the resolver its
/// members should use.
#[cfg(target_os = "linux")]
fn stack_network() -> (Option<ply_core::runtime::ns::netns::NetNs>, Option<String>) {
    let mut egress_dns: Option<String> = None;
    let netns = match ply_core::paths::is_root() {
        // … the existing arms from :341–366, verbatim, with
        // `ply_core::runtime::ns::netns::` paths …
    };
    (netns, egress_dns)
}

/// No stack network on this platform yet (plan 2's switch); members run
/// on the host's network, as a rootless stack does when the namespace fails.
#[cfg(not(target_os = "linux"))]
struct StackNet;
#[cfg(not(target_os = "linux"))]
impl StackNet {
    fn path(&self) -> std::path::PathBuf {
        unreachable!("stack_network never returns Some on this platform")
    }
}
#[cfg(not(target_os = "linux"))]
fn stack_network() -> (Option<StackNet>, Option<String>) {
    (None, None)
}
```

and at the old site: `let (netns, egress_dns) = stack_network();` (the comment block at `:334–339` moves onto the Linux helper). Lines `21–22` (`nix::sys::signal`, `nix::unistd::Pid`) are portable and stay.

- [ ] **Step 6: Linux still green**

Run: `make check`
Expected: green, same test totals as Task 3.

- [ ] **Step 7: The gate**

Run: `make check-darwin`
Expected: clean — `cargo-zigbuild check` and `clippy -D warnings` both exit 0 for `-p ply-cli`. Any remaining error is a Linux-only use outside `ns/` (the Global Constraints list what may not appear): move it into `ns/` or gate it; do not `allow` it.

- [ ] **Step 8: The boundary is real** — run the grep from Global Constraints:

```sh
grep -rnE 'nix::sched|nix::mount|caps::|seccompiler|setns|pivot_root|LOOP_(CTL|SET)|/sys/fs/cgroup' ply-core/src ply-cli/src | grep -vE '^ply-core/src/(runtime/ns/|craft\.rs)'
```

Expected: no output.

- [ ] **Step 9: Report** — `make check` green, `make check-darwin` clean, the grep empty; stop. After the owner pushes, the `darwin` CI job must be green on the first run.

---

## Owner hand-over (after Task 4)

1. Review, commit (the seam is one logical commit; the file moves keep history via `git mv`), push: the new `ci.yml` runs both jobs.
2. Verify on your terminal (rootless) and on the droplet (rootful) per Task 3 Step 10; the expected outputs are the ones from before this branch.
3. Plan 2 (on the Mac) starts from this tree: the libkrun spike, then `runtime/vm/`.

## Self-review

- **Spec coverage (milestone 1):** trait in ply-core (Task 2); `ns/` behind `cfg(linux)` (Task 4); `run.rs` neutral (Task 3); darwin `cargo check` gate in CI (Task 1) and clean (Task 4); "Linux tests unchanged" (every task's `make check`); the Mac smoke (`ply build/inspect/push/search/up --plan` natively) is deferred to plan 2 because it needs a Mac — stated in Global Constraints. `Backend::capability` is in the trait (spec) with `ply check` deferred to plan 2 (no macOS backend yet to report on). `Instance::tcp_open` (spec) is on the trait and used by the health gate; the `--after` port probe still goes through `after::check` on the host — it reads state files, not instances, and stays portable; plan 2 routes it through the switch.
- **Placeholders:** the only `todo!` markers are in Task 3 Step 2's skeleton, each naming the exact line range that replaces it, with an explicit instruction that none may survive.
- **Type consistency:** `Facts`/`NetworkFacts` (Task 2) are what Task 3's `run()` reads (`facts.loopback`, `facts.own_addresses`, `net.alone`, `net.in_stack_network`, `net.facts.loopback`); `Record<'_>` is the closure type `launch_instance` builds; `Launched { instance, output }` is what the tee and pool join consume; `Instance::child_pid()` feeds `register_child`/`update_child`; `supervise::{health_gate, stop_with_patience, Health}` are called with the same argument order they are defined with.
