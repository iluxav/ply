//! The seam between `ply run`'s platform-neutral supervisor (`run.rs`) and
//! the platform's way of running ONE instance: Linux namespaces today
//! (`runtime/ns`), a microVM on macOS later. `run.rs` never names a
//! platform; it drives whatever `default_backend()` hands it through these
//! two traits.
//!
//! Order of calls in one run, all on the main thread:
//! `preflight` → `facts` → (manifest read) → `admit` → (host ports bound)
//! → `attach` → `network` → `launch`×N … `terminal` on demand.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    /// The effective egress contract for this instance, when there is one to
    /// keep: `None` means "nothing to do" — the mode is `off`, or the host
    /// cannot give an instance a network of its own (rootless), and the
    /// supervisor already said so.
    pub egress: Option<crate::egress::Policy>,
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
    /// How a published pool reaches this instance's `port`.
    ///
    /// The default is the address `ip()` names, which is what every
    /// namespace instance wants: the parent is in the same network and can
    /// dial it. A microVM overrides it, because its address exists only on
    /// the parent's userspace switch and `TcpStream::connect` to it from
    /// the host finds nothing at all — the failure `--publish` used to have
    /// on macOS, where the parent accepted a connection, found no reachable
    /// backend, and reset it.
    fn connector(&self, port: u16) -> Arc<dyn crate::runtime::publish::Connector> {
        crate::runtime::publish::connector_for(SocketAddr::from((self.ip(), port)))
    }
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
    /// The unix socket an instance's address can be reached on from ANOTHER
    /// process, when the address is not one the host can dial.
    ///
    /// `None` — every namespace instance — means "the address in the state
    /// file is the whole answer". A microVM's is not: `10.77.0.2` names a
    /// machine on a userspace switch, and a `--after` port probe running in
    /// a different `ply run` parent has no way to reach it without being
    /// told which switch to ask. Recorded in the instance state file, which
    /// is the only place a reader ever looks.
    ///
    /// Called once, after `attach`.
    fn reach_via(&self) -> Option<PathBuf> {
        None
    }
    /// Why this backend cannot keep an egress contract, or `None` when it
    /// can. Enforcement capability is the backend's word, not a fact the
    /// supervisor infers: whether a policy can be installed depends on how
    /// the platform gives an instance a network, which only the platform
    /// knows. The reason is printed once, verbatim, and the run continues
    /// with no policy.
    fn egress_support(&self) -> Option<&'static str> {
        Some("egress policy is not enforced on this platform yet — running unobserved")
    }
    /// The kernel's share of `--publish` for `spec`, when this platform has
    /// one: a mirror the pool keeps in sync so new connections are DNATed by
    /// the kernel instead of relayed by the parent. `None` — the default,
    /// rootless, the VM switch, a loopback address — means the relay does
    /// all the work, as before. Called once per published port, before any
    /// instance launches.
    fn kernel_publish(
        &self,
        _spec: &crate::runtime::publish::Publish,
    ) -> Option<Arc<dyn crate::runtime::publish::PoolMirror>> {
        None
    }
    fn launch(&self, spec: &InstanceSpec, record: Record<'_>) -> Result<Launched>;
    /// The `exec` control command: serve a terminal into `app.slot` at
    /// `term-<nonce>.sock`. Backends without one return `Err`.
    fn terminal(&self, app: &str, slot: u32, nonce: &str) -> Result<()>;
}

/// Undo what a previous instance left under `dir` that a plain remove
/// cannot (Linux: layer mounts). Used on a stale instance directory by
/// `allocate_instance` and `state::reap_stale`.
#[cfg(target_os = "linux")]
pub fn scrub_instance_dir(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir.join("layers")) {
        for entry in entries.filter_map(|e| e.ok()) {
            crate::runtime::ns::mount::unmount_detach(&entry.path());
        }
    }
}

/// No backend has ever run here, so there is nothing to undo.
#[cfg(not(target_os = "linux"))]
pub fn scrub_instance_dir(_dir: &Path) {}

/// The platform's backend, or why there is none.
#[cfg(target_os = "linux")]
pub fn default_backend() -> Result<Box<dyn Backend>> {
    Ok(Box::new(crate::runtime::ns::NsBackend::new()?))
}

/// One microVM per instance on Hypervisor.framework.
#[cfg(target_os = "macos")]
pub fn default_backend() -> Result<Box<dyn Backend>> {
    Ok(Box::new(crate::runtime::vm::VmBackend::new()?))
}

/// Neither Linux namespaces nor macOS microVMs: nothing to run instances
/// with.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn default_backend() -> Result<Box<dyn Backend>> {
    Err(crate::Error::Runtime(
        "ply run has no runtime on this platform — Linux (namespaces) and macOS on Apple Silicon (microVMs) are supported".into(),
    ))
}
