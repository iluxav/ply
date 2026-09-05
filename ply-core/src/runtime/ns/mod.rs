//! The Linux backend: one instance = one process in fresh mount/PID/UTS/
//! IPC (+user, rootless; +net, rootful) namespaces, layers as squashfs
//! loop mounts (root) or store extractions (rootless), rights dropped in
//! the child (see container.rs). Everything platform-specific about
//! `ply run` on Linux is in this module tree.

pub mod cgroup;
pub mod container;
pub mod egress;
pub mod exec;
pub mod loopdev;
pub mod mount;
pub mod netns;
pub mod network;
pub mod security;
pub mod subid;
pub mod term;

use std::cell::RefCell;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use nix::sched::CloneFlags;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;

use self::cgroup::Cgroup;
use self::container::{child_main, ContainerSpec};
use self::netns::NetNs;
use super::backend::{Backend, Facts, Instance, InstanceSpec, Launched, NetworkFacts, Record};
use super::run::RunOptions;
use super::{hosts, state};
use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::store::Store;

/// The Linux backend: namespaces, overlayfs layers and (rootful) a bridge.
///
/// Owns the run's rootless-ness decision (taken once, from `paths::is_root`,
/// so every instance of the run agrees), the network namespace it made in
/// `preflight` when a rootless run was handed none — it lives as long as the
/// backend and is torn down with it — and the `Store` handle the layer mounts
/// are resolved through. Owns nothing per-instance: that is `NsInstance`.
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

    /// Rootful is the only shape that can keep a contract: the policy is a
    /// table in the instance's OWN network namespace, and a rootless run
    /// shares one network with everything else on the box.
    fn egress_support(&self) -> Option<&'static str> {
        if self.rootless {
            Some("egress policy needs a network per instance — rootless runs unenforced and unobserved (use a rootful host to audit or enforce)")
        } else {
            None
        }
    }

    fn preflight(&self, opts: RunOptions) -> Result<RunOptions> {
        let mut opts = opts;
        // A rootless run with no namespace handed to it makes its own, so that
        // ONE app is as isolated as a stack's members are: its own ports, no
        // collision with whatever the machine already runs, a way out through a
        // user-mode router. `ply up` passes its stack namespace instead, and
        // rootful already gives every instance a bridge address.
        if self.rootless && opts.network.is_none() {
            match NetNs::create().and_then(|ns| ns.enter_user().map(|()| ns)) {
                Ok(mut ns) => {
                    let dns = match ns.attach_egress() {
                        Ok(_) => Some(netns::EGRESS_DNS.to_string()),
                        Err(e) => {
                            eprintln!("ply: no outbound network for this app — {e}");
                            None
                        }
                    };
                    opts.network = Some(ns.path());
                    opts.network_dns = dns;
                    // the namespace lives as long as this backend (the run)
                    *self.own_ns.borrow_mut() = Some(ns);
                }
                Err(e) => {
                    eprintln!("ply: staying on the host network — {e}");
                }
            }
        }
        let rootless = self.rootless;
        if opts.privileged {
            // Never quiet about this: the whole point of the runtime is that the
            // app ends up with nothing, and --privileged undoes all three layers.
            eprintln!(
                "ply: WARNING: --privileged — capabilities kept, no_new_privs off, seccomp off.{}",
                if rootless {
                    " Rootless, so this is still bounded by your user namespace."
                } else {
                    " Running as root: the app gets REAL root on this host."
                }
            );
        }
        if rootless {
            // Say which network this actually got: with one of its own the app
            // binds its declared ports and answers to `<name>.ply`, which is the
            // opposite of what the old banner promised.
            let net = match &opts.network {
                Some(_) => "own network",
                None => "host network (no .ply names)",
            };
            eprintln!("ply: rootless mode — extracted layers, {net}, no cgroup limits");
            // Ubuntu >= 24.04 strips capabilities from unprivileged user
            // namespaces unless an AppArmor profile grants `userns`.
            let restricted =
                std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                    .map(|v| v.trim() == "1")
                    .unwrap_or(false);
            if restricted && !std::path::Path::new("/etc/apparmor.d/ply").exists() {
                return Err(Error::Runtime(
                    "this kernel restricts unprivileged user namespaces — one-time fix:\n  \
                     sudo ply setup\n  \
                     (or run with sudo instead)"
                        .into(),
                ));
            }
        }
        Ok(opts)
    }

    fn facts(&self) -> Facts {
        Facts {
            loopback: self.rootless,
            own_addresses: !self.rootless,
        }
    }

    fn admit(&self, manifest: &Manifest, opts: &RunOptions) -> Result<()> {
        // Only apps that need a second uid to exist care about the subid range:
        // a declared [package] user, or an import whose entrypoint will gosu down.
        if self.rootless {
            let needs_ids =
                manifest.package.user.is_some() || manifest.package.capabilities.is_some();
            if needs_ids {
                if let Some(gap) = subid::subid_gap() {
                    eprintln!("ply: warning: {gap}");
                }
            }
        }

        match rootless_scale_guard(
            self.rootless,
            opts.scale,
            !manifest.ports.is_empty(),
            !opts.publish.is_empty(),
        ) {
            ScaleGuard::Refuse(msg) => return Err(Error::Runtime(msg.into())),
            ScaleGuard::Warn(msg) => eprintln!("ply: warning: {msg}"),
            ScaleGuard::Ok => {}
        }
        Ok(())
    }

    fn attach(&self, opts: &RunOptions) -> Result<()> {
        // Joining is best-effort: a stack that cannot get its own network should
        // still run on the host's, exactly as it did before any of this existed.
        // What must NOT happen is believing we joined when we did not — the
        // instance's address is derived from it, and on the host network an
        // un-injected port makes the pool's backend the proxy's own listener,
        // which then accepts its own connections until it runs out of threads.
        if let Some(path) = &opts.network {
            if let Err(e) = std::fs::File::open(path)
                .map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })
                .and_then(|ns| netns::enter(&std::os::fd::OwnedFd::from(ns)))
            {
                eprintln!("ply: staying on the host network — {e}");
            }
        }
        if !self.rootless {
            network::ensure_bridge()?;
        }
        Ok(())
    }

    fn network(&self, opts: &RunOptions) -> NetworkFacts {
        NetworkFacts {
            facts: self.facts(),
            in_stack_network: opts.network.as_ref().is_some_and(|p| in_namespace(p)),
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

        // Layers: root loop-mounts squashfs; rootless uses store-cached
        // extractions (unprivileged kernels can't mount squashfs).
        let mut layers: Vec<PathBuf> = Vec::new();
        for (i, img) in spec.images.iter().enumerate() {
            if rootless {
                let digest = crate::digest::sha256_file(img)?;
                let rootfs = self.store.extracted_rootfs(img, &digest)?;
                // overlayfs splits lowerdir at `:` — store paths contain
                // `sha256:`, so hand the kernel a colon-free symlink instead
                let link = instance_dir.join("layers").join(i.to_string());
                std::os::unix::fs::symlink(&rootfs, &link).map_err(|source| Error::Io {
                    path: link.clone(),
                    source,
                })?;
                layers.push(link);
            } else {
                let target = instance_dir.join("layers").join(i.to_string());
                let (device, dev_fd) = loopdev::attach_ro(img)?;
                mount::mount_squashfs_ro(&device, &target)?;
                drop(dev_fd); // mount holds the device now; autoclear arms for unmount
                guard.mounted_layers.borrow_mut().push(target.clone());
                layers.push(target);
            }
        }

        // Sync pipe: the child waits for cgroup + network before setup.
        let (sync_rx, sync_tx) =
            nix::unistd::pipe().map_err(|e| Error::Runtime(format!("pipe: {e}")))?;
        // Log tee: the child's stdout+stderr become this pipe; a copier thread
        // passes the stream through to the parent's stdout (journald/terminal
        // behavior unchanged) while also feeding the bounded log ring that
        // `ply logs` and the dashboard read.
        let (log_rx, log_tx) =
            nix::unistd::pipe().map_err(|e| Error::Runtime(format!("log pipe: {e}")))?;

        let keep_caps = security::keep_set(spec.capabilities.as_ref(), spec.keep_net_bind)?;
        if !keep_caps.is_empty() {
            // Anything above the empty default is worth one line of output — the
            // whole promise of the runtime is that the app ends up with nothing.
            eprintln!(
                "ply: {app} keeps {} capability/ies: {}",
                keep_caps.len(),
                keep_caps
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }

        let container = ContainerSpec {
            layers,
            instance_dir: instance_dir.clone(),
            hostname: spec.hostname.clone(),
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            argv: spec.entrypoint.clone(),
            binds: spec.binds.clone(),
            sync_rx,
            volume_targets: spec.volume_targets.clone(),
            keep_caps,
            privileged: spec.privileged,
            rootless,
            dns: spec.dns.clone(),
            local_aliases: spec.local_aliases.clone(),
            run_user: spec.run_user.clone(),
            log_fd: Some(log_tx),
            egress: spec.egress.is_some(),
        };

        let mut stack = vec![0u8; 1024 * 1024];
        let mut flags = CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWPID
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWIPC;
        if rootless {
            // user ns grants the mount rights; host netns (no veth privileges)
            flags |= CloneFlags::CLONE_NEWUSER;
        } else {
            flags |= CloneFlags::CLONE_NEWNET;
        }
        let child = unsafe {
            nix::sched::clone(
                Box::new(|| child_main(&container)),
                &mut stack,
                flags,
                Some(nix::libc::SIGCHLD),
            )
        }
        .map_err(|e| Error::Runtime(format!("clone: {e}")))?;

        // cgroup + veth (root) / uid maps (rootless) while the child is parked
        // on the pipe. The network lock serializes IP pick + veth setup across
        // concurrent `ply run`s (two parents reading state simultaneously would
        // pick the same IP and collide on the derived veth name) and is held
        // until this instance's state file makes the IP visible to others.
        let _net_lock = if rootless {
            None
        } else {
            Some(network::lock()?)
        };
        let prepared = (|| -> Result<(Option<Cgroup>, Ipv4Addr)> {
            if rootless {
                subid::write_id_maps(child.as_raw())?;
                return Ok((None, Ipv4Addr::new(127, 0, 0, 1)));
            }
            let cgroup = Cgroup::create(&format!("{app}.{n}"), spec.resources.as_ref())?;
            cgroup.add_pid(child.as_raw())?;
            let used: Vec<Ipv4Addr> = state::list()?.iter().map(|s| s.ip).collect();
            let ip = network::allocate_ip(&used)?;
            network::setup_instance(child.as_raw(), ip)?;
            Ok((Some(cgroup), ip))
        })();
        let (cgroup, ip) = match prepared {
            Ok(ok) => ok,
            Err(e) => {
                drop(sync_tx); // EOF → child aborts
                let _ = signal::kill(child, Signal::SIGKILL);
                let _ = waitpid(child, None);
                return Err(e);
            }
        };

        // The egress contract, once the network exists and before the child
        // is released: the table has to be in place before the app can send
        // its first packet, and the thread needs nothing but the pid. In
        // `enforce` a failure here is the launch's failure — an app that
        // would run without its contract is not the app that was asked for;
        // in `audit` it is a warning, because audit promises observation and
        // never containment.
        let egress = match &spec.egress {
            None => None,
            Some(policy) => {
                // An app that keeps CAP_NET_RAW can build its own packets
                // and speak straight to the wire, and one with
                // CAP_NET_ADMIN or CAP_SYS_ADMIN can edit or flush the
                // table itself: with any of them the contract is theatre.
                // `capabilities = "oci"` — every imported Docker image —
                // keeps CAP_NET_RAW, so this is the common case, not a
                // corner one.
                if let Some(cap) = egress_blocking_caps(&container.keep_caps, spec.privileged) {
                    let how = if spec.privileged {
                        format!("{app} runs --privileged")
                    } else {
                        format!("{app} keeps {cap} (capabilities = \"oci\" keeps CAP_NET_RAW)")
                    };
                    if policy.mode == crate::egress::Mode::Enforce {
                        drop(sync_tx); // EOF → the parked child aborts
                        let _ = signal::kill(child, Signal::SIGKILL);
                        let _ = waitpid(child, None);
                        return Err(Error::Runtime(format!(
                            "egress policy: enforce needs an app without CAP_NET_RAW/CAP_NET_ADMIN — {how}; list capabilities without it, or run with --egress audit"
                        )));
                    }
                    eprintln!("ply: warning: egress: {app} keeps {cap} — an app with it can bypass observation");
                }
                let upstreams = container::upstream_resolvers(rootless);
                match egress::spawn(app, n, child.as_raw(), policy, upstreams) {
                    Ok(handle) => Some(handle),
                    Err(e) if policy.mode == crate::egress::Mode::Enforce => {
                        drop(sync_tx); // EOF → the parked child aborts
                        let _ = signal::kill(child, Signal::SIGKILL);
                        let _ = waitpid(child, None);
                        return Err(e);
                    }
                    Err(e) => {
                        eprintln!("ply: warning: {e} — running unobserved");
                        None
                    }
                }
            }
        };

        // Record the state file while the net lock is still held and the child
        // is still parked on the sync pipe, so the address is visible to
        // concurrent runs before anything can race it.
        let hosts_entry = !rootless;
        let settled = record(child.as_raw(), ip).and_then(|()| {
            if hosts_entry {
                hosts::add_entry(app, n, ip) // /etc/hosts needs root
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

        // Parent half of the log tee: our copy of the write end must close or
        // the copier would never see EOF when the instance dies.
        drop(container.log_fd);

        // Release the child — telling it whether the forwarder it was going
        // to resolve through is actually there. Only an `audit` contract can
        // reach the release with no thread; `enforce` returned above.
        let release = if spec.egress.is_some() && egress.is_none() {
            container::RELEASE_UNOBSERVED
        } else {
            container::RELEASE
        };
        let _ = nix::unistd::write(&sync_tx, &[release]);
        drop(sync_tx);

        Ok(Launched {
            instance: Box::new(NsInstance {
                app: app.clone(),
                n,
                child,
                ip,
                ended: None,
                hosts_entry,
                _egress: egress,
                _cgroup: cgroup,
                _guard: guard,
            }),
            output: Box::new(std::fs::File::from(log_rx)),
        })
    }

    fn terminal(&self, app: &str, slot: u32, nonce: &str) -> Result<()> {
        term::spawn(app, slot, nonce);
        Ok(())
    }
}

/// One instance as the Linux backend runs it: a cloned child in its own
/// namespaces.
///
/// Owns the child pid (`ply ps`, `ply stop` and the health gate all go
/// through it), the instance's cgroup, and — via `InstanceGuard` — the
/// overlay layer mounts and the instance directory itself.
///
/// Drop releases all of it: the `/etc/hosts` entry when the run is rootful,
/// then the cgroup, then the layer unmounts and the instance directory. The
/// child is NOT killed on drop; the supervisor stops it first (the state file
/// is left to `state::reap_stale`, as it always was).
pub(crate) struct NsInstance {
    app: String,
    n: u32,
    child: Pid,
    ip: Ipv4Addr,
    ended: Option<i32>,
    hosts_entry: bool,
    /// The egress thread, declared FIRST so it stops before the cgroup goes:
    /// drop order is declaration order, and a thread still talking to nft in
    /// a namespace whose cgroup has been removed has nothing useful to say.
    _egress: Option<egress::EgressHandle>,
    _cgroup: Option<Cgroup>,
    _guard: InstanceGuard,
}

impl Instance for NsInstance {
    fn pid(&self) -> i32 {
        self.child.as_raw()
    }
    fn child_pid(&self) -> Option<i32> {
        Some(self.child.as_raw())
    }
    fn ip(&self) -> Ipv4Addr {
        self.ip
    }
    fn alive(&self) -> bool {
        self.ended.is_none() && unsafe { nix::libc::kill(self.child.as_raw(), 0) == 0 }
    }
    fn signal(&self, sig: Signal) -> Result<()> {
        signal::kill(self.child, sig)
            .map_err(|e| Error::Runtime(format!("kill {}: {e}", self.child)))
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
        super::publish::connect_either_family(addr, timeout).map(|_| ())
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

/// Unmounts layer mounts and removes the instance dir on drop — including
/// error paths.
struct InstanceGuard {
    dir: PathBuf,
    mounted_layers: std::cell::RefCell<Vec<PathBuf>>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        for target in self.mounted_layers.borrow().iter() {
            mount::unmount_detach(target);
        }
        let _ = crate::paths::force_remove_dir_all(&self.dir);
    }
}

/// Is this instance the only thing in its network? Rootful, always (its own
/// bridge address). Rootless, only when the run has a namespace it is really
/// in AND there is one instance — every instance of a run shares that one
/// namespace, so past the first they contend for the same ports.
pub(crate) fn alone_in_its_network(rootless: bool, opts: &RunOptions) -> bool {
    !rootless || (opts.network.as_ref().is_some_and(|p| in_namespace(p)) && opts.scale <= 1)
}

/// Is this process inside the namespace at `path`? Compared by identity,
/// not by whether a join was attempted — believing a failed join is what
/// turns a proxy into its own backend.
///
/// Namespace membership is asked of the kernel rather than remembered — a
/// failed join must never look like a successful one, or the pool ends up
/// pointing at the proxy itself.
fn in_namespace(path: &std::path::Path) -> bool {
    let ino = |p: &std::path::Path| {
        std::fs::metadata(p)
            .ok()
            .map(|m| std::os::unix::fs::MetadataExt::ino(&m))
    };
    match (ino(path), ino(std::path::Path::new("/proc/self/ns/net"))) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Can `--scale N` work in this mode? Rootless instances share the host
/// network namespace (no per-instance IPs without root), so N > 1 instances
/// of a port-binding app all race for the same port and N-1 crash with
/// EADDRINUSE. Refuse up front when the manifest declares ports; warn when
/// it doesn't (the app may still bind something undeclared).
enum ScaleGuard {
    Ok,
    Warn(&'static str),
    Refuse(&'static str),
}

fn rootless_scale_guard(rootless: bool, scale: u32, has_ports: bool, publish: bool) -> ScaleGuard {
    match (rootless, scale, has_ports, publish) {
        // --publish makes the parent the listener; past one instance each
        // gets its own injected PORT, so nothing collides.
        (_, _, _, true) | (false, _, _, _) | (true, 0 | 1, _, _) => ScaleGuard::Ok,
        (true, _, true, false) => ScaleGuard::Refuse(
            "rootless instances share one network, so every instance would bind the same declared port (EADDRINUSE for all but the first).\n\
             publish the pool through the parent:  ply run --publish <port> --scale N …\n\
             or run it rootful for per-instance IPs:  sudo ply run --scale N …\n\
             or stay rootless with --scale 1",
        ),
        (true, _, false, false) => ScaleGuard::Warn(
            "rootless instances share one network — if these instances bind the same port they will collide (per-instance IPs need root, or use --publish)",
        ),
    }
}

/// The capability, if any, that makes an egress contract unenforceable.
///
/// `CAP_NET_RAW` lets the app build its own packets (a raw socket is not
/// subject to the connect() path the contract is written against, and the
/// app can spoof the forwarder's source port); `CAP_NET_ADMIN` and
/// `CAP_SYS_ADMIN` let it read, edit or flush the very table that holds
/// the policy. `--privileged` keeps everything, so it fails on the first
/// of them.
///
/// Names the FIRST offender it finds, in the order above — the message
/// tells an operator which one to take out of the list.
fn egress_blocking_caps(keep: &[caps::Capability], privileged: bool) -> Option<String> {
    use caps::Capability::{CAP_NET_ADMIN, CAP_NET_RAW, CAP_SYS_ADMIN};
    let blocking = [CAP_NET_RAW, CAP_NET_ADMIN, CAP_SYS_ADMIN];
    if privileged {
        return Some(CAP_NET_RAW.to_string());
    }
    blocking
        .iter()
        .find(|c| keep.contains(c))
        .map(|c| c.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enforce is void when the app can make its own packets or edit the
    /// table. `capabilities = "oci"` keeps CAP_NET_RAW, so every imported
    /// Docker image lands here.
    #[test]
    fn the_caps_that_make_an_egress_contract_theatre_are_named() {
        use caps::Capability::*;
        assert_eq!(egress_blocking_caps(&[], false), None);
        assert_eq!(
            egress_blocking_caps(&[CAP_CHOWN, CAP_SETUID, CAP_NET_BIND_SERVICE], false),
            None,
            "the capabilities a service user actually needs are fine"
        );
        assert_eq!(
            egress_blocking_caps(&security::OCI_DEFAULT_CAPABILITIES, false).as_deref(),
            Some("CAP_NET_RAW"),
            "the oci preset is Docker's set, and Docker's set has NET_RAW"
        );
        assert_eq!(
            egress_blocking_caps(&[CAP_NET_ADMIN], false).as_deref(),
            Some("CAP_NET_ADMIN")
        );
        assert_eq!(
            egress_blocking_caps(&[CAP_SYS_ADMIN], false).as_deref(),
            Some("CAP_SYS_ADMIN")
        );
        // --privileged keeps the lot, whatever the list says
        assert_eq!(
            egress_blocking_caps(&[], true).as_deref(),
            Some("CAP_NET_RAW")
        );
    }

    #[test]
    fn rootful_and_single_instance_pass() {
        assert!(matches!(
            rootless_scale_guard(false, 8, true, false),
            ScaleGuard::Ok
        ));
        assert!(matches!(
            rootless_scale_guard(true, 1, true, false),
            ScaleGuard::Ok
        ));
    }

    #[test]
    fn publish_lifts_the_rootless_scale_refusal() {
        assert!(matches!(
            rootless_scale_guard(true, 4, true, true),
            ScaleGuard::Ok
        ));
        assert!(matches!(
            rootless_scale_guard(true, 4, false, true),
            ScaleGuard::Ok
        ));
    }

    #[test]
    fn rootless_scale_with_declared_ports_refuses() {
        let ScaleGuard::Refuse(msg) = rootless_scale_guard(true, 4, true, false) else {
            panic!("expected refusal");
        };
        assert!(msg.contains("EADDRINUSE"));
        assert!(msg.contains("sudo ply run"));
        assert!(msg.contains("--publish"));
    }

    #[test]
    fn rootless_scale_without_ports_warns() {
        assert!(matches!(
            rootless_scale_guard(true, 4, false, false),
            ScaleGuard::Warn(_)
        ));
    }
}
