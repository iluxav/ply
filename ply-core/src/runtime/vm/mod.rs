//! The macOS backend: one instance = one microVM on Hypervisor.framework,
//! its disks the same `.img` files the namespace backend overlays, its
//! network a userspace switch the `ply run`/`ply up` parent owns.
//!
//! Module layout is deliberate. `kernel`, `spec_disk` and `switch` are
//! PORTABLE — they are pure logic (a version pin, a byte format, a TCP/IP
//! stack) and they compile and run their tests on Linux CI, which is where
//! this project's tests actually run. Only the device models and the vCPU
//! loop touch Hypervisor.framework, and only those are gated.

pub mod kernel;
pub mod p9;
pub mod spec_disk;
pub mod switch;

#[cfg(target_os = "macos")]
mod blk;
#[cfg(target_os = "macos")]
mod console;
#[cfg(target_os = "macos")]
mod machine;
#[cfg(target_os = "macos")]
mod net;
#[cfg(target_os = "macos")]
mod p9dev;
#[cfg(target_os = "macos")]
mod pl011;
#[cfg(target_os = "macos")]
pub mod worker;

#[cfg(target_os = "macos")]
use std::net::Ipv4Addr;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(target_os = "macos")]
use std::time::Duration;

#[cfg(target_os = "macos")]
use std::io::{BufRead, Write};
#[cfg(target_os = "macos")]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(target_os = "macos")]
use nix::sys::signal::Signal;
#[cfg(target_os = "macos")]
use ply_vm_proto::{host_line, parse_guest_line, GuestLine, HostLine, PARENT_OWNED};

#[cfg(target_os = "macos")]
use super::backend::{Backend, Facts, Instance, InstanceSpec, Launched, NetworkFacts, Record};
#[cfg(target_os = "macos")]
use super::run::RunOptions;
#[cfg(target_os = "macos")]
use crate::error::{Error, Result};
#[cfg(target_os = "macos")]
use crate::manifest::Manifest;

/// Guest RAM per instance. Fixed, not derived from `[resources]`: the
/// guest's page cache is its own, so a limit that means "cap this process"
/// on Linux would mean "starve this kernel" here.
#[cfg(target_os = "macos")]
pub const MEMORY_MIB: u64 = 512;

/// Can this host run microVMs, and if not, exactly why? The message is what
/// `ply check` prints and what `ply run` fails with, so it names the remedy.
#[cfg(target_os = "macos")]
pub fn capability_report() -> std::result::Result<String, String> {
    // Apple Silicon: Hypervisor.framework on Intel is a different device
    // model and is a declared non-goal.
    if std::env::consts::ARCH != "aarch64" {
        return Err("ply's microVM runtime needs an Apple Silicon Mac (M1 or later)".into());
    }
    // The kernel's own answer, rather than a version comparison: this is the
    // bit that says whether the CPU and the OS will actually let us in.
    let supported = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "kern.hv_support"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false);
    if !supported {
        return Err(
            "Hypervisor.framework is unavailable on this machine (kern.hv_support = 0) — \
             virtualization may be disabled, or this Mac is not supported"
                .into(),
        );
    }
    // The binary must carry com.apple.security.hypervisor or hv_vm_create
    // fails at launch with a permission error nobody can read.
    match machine::probe_hypervisor() {
        Ok(()) => Ok("Hypervisor.framework ok".into()),
        Err(e) => Err(format!(
            "Hypervisor.framework refused this process ({e}) — the ply binary must be signed \
             with the com.apple.security.hypervisor entitlement; reinstall with \
             `curl -fsSL https://plybox.sh/install.sh | sh`"
        )),
    }
}

/// The macOS backend: every instance is a microVM.
///
/// Owns the run's kernel — resolved once, in `new`, so every instance of the
/// run boots the same one and a missing keg is reported before any host port
/// is claimed rather than N times mid-launch.
#[cfg(target_os = "macos")]
pub struct VmBackend {
    kernel: kernel::Kernel,
    /// The run's network, joined or made by `attach`.
    ///
    /// It lives in a PROCESS and not in a daemon because macOS gives no tap
    /// device without `com.apple.vm.networking`, which is restricted: the
    /// whole network is userspace and dies with the process that owns it —
    /// the same lifetime the stack's netns has on Linux, for the same
    /// reason. Which process owns it is what `--vswitch` decides: `ply up`'s
    /// parent for a stack, this one for a standalone run.
    ///
    /// `OnceLock` rather than a field set in `new`, because `new` also runs
    /// for `ply check`, which needs the kernel pin and no threads.
    net: OnceLock<switch::Net>,
}

#[cfg(target_os = "macos")]
impl VmBackend {
    pub fn new() -> Result<VmBackend> {
        Ok(VmBackend {
            kernel: kernel::resolve()?,
            net: OnceLock::new(),
        })
    }

    /// The run's network, or `None` when this host could not start one.
    ///
    /// Everything downstream treats `None` as "this instance has no network
    /// card": it boots, it runs, and it cannot be published to. That is
    /// worse than the alternative in exactly one way and better in another —
    /// refusing to launch would turn a network problem into a total outage
    /// for apps that need no network at all.
    fn net(&self) -> Option<&switch::Net> {
        self.net.get()
    }
}

/// Where a standalone run's own switch listens.
///
/// Named for the process, under `run_dir()`, for two reasons. It is in the
/// same tmpfs-ish directory as the state files that point at it, so the two
/// disappear together; and the pid makes it unique per run, so two `ply run`
/// parents never fight over one path and a socket left by a `kill -9` is
/// never mistaken for a live one.
#[cfg(target_os = "macos")]
pub fn switch_socket_path() -> PathBuf {
    crate::paths::run_dir()
        .join("switch")
        .join(format!("{}.sock", std::process::id()))
}

#[cfg(target_os = "macos")]
impl Backend for VmBackend {
    fn capability(&self) -> std::result::Result<(), String> {
        capability_report().map(|_| ())
    }

    fn preflight(&self, opts: RunOptions) -> Result<RunOptions> {
        eprintln!(
            "ply: microVM runtime ({}) — {} MiB per instance",
            self.kernel.origin, MEMORY_MIB
        );
        Ok(opts)
    }

    fn facts(&self) -> Facts {
        // Every instance is its own machine with its own address on the
        // switch, so `{host}`/`{addr}`/`{base_url}` resolve — like a rootful
        // namespace run, not a rootless one. Published listeners bind
        // loopback: on a Mac there is no bridge gateway to bind instead.
        Facts {
            loopback: true,
            own_addresses: true,
        }
    }

    fn admit(&self, manifest: &Manifest, _opts: &RunOptions) -> Result<()> {
        // A feature the VM backend does not have. Saying so beats silently
        // running something different from what the manifest asked for.
        if manifest.resources.is_some() {
            eprintln!(
                "ply: warning: [resources] limits are ignored by the microVM runtime \
                 (each instance gets a fixed {MEMORY_MIB} MiB)"
            );
        }
        Ok(())
    }

    /// Join the run's network, or make one.
    ///
    /// The namespace backend's `attach` joins a namespace someone else made
    /// and this one does the same when it can: `ply up` runs the stack's
    /// switch in its own process and passes the socket on `--vswitch`, so
    /// every member lands on ONE L2 and `<peer>.ply` means something. With
    /// no socket — a standalone `ply run` — this process makes its own,
    /// listening, so that one app is as isolated as a stack member and is
    /// still reachable by a `--after` gate in another `ply run`.
    ///
    /// # A `--vswitch` that does not answer is fatal, and that is the point
    ///
    /// Falling back to a private switch would leave a stack member booted,
    /// healthy-looking and alone: `<peer>.ply` would resolve to nothing, no
    /// error would be printed, and the first failure a person saw would be a
    /// connection refused inside their own app. A run told to join a network
    /// that is not there has to say so.
    fn attach(&self, opts: &RunOptions) -> Result<()> {
        if let Some(socket) = &opts.network {
            let client = switch::unix::Client::connect(socket).map_err(|e| {
                Error::Runtime(format!(
                    "the stack's switch at {} is not answering ({e}) — this member could \
                     not join the network its peers are on",
                    socket.display()
                ))
            })?;
            let _ = self.net.set(switch::Net::Joined(client));
            return Ok(());
        }
        let path = switch_socket_path();
        match switch::unix::Server::start(&path) {
            Ok((server, warning)) => {
                if let Some(warning) = warning {
                    // The switch runs; only the way IN from another process
                    // is missing. `--after` from a second `ply run` is what
                    // that costs, so name it rather than leaving a gate to
                    // time out for no stated reason.
                    eprintln!(
                        "ply: warning: this run's network has no socket ({warning}) — \
                         another `ply run --after` will not be able to probe it"
                    );
                }
                let _ = self.net.set(switch::Net::Own(Arc::new(server)));
            }
            Err(e) => {
                // Not fatal, and deliberately so: an app that needs no
                // network should not fail to start because the network did.
                eprintln!(
                    "ply: warning: the microVM network could not start ({e}) — instances \
                     will boot with no network card, so --publish and health checks \
                     will not reach them"
                );
            }
        }
        Ok(())
    }

    /// # What `in_stack_network` decides, and why joining a switch sets it
    ///
    /// It is what `discovery_env` consults to choose between handing a
    /// dependant `<dep>.ply:<port>` and handing it the published host
    /// address. On Linux it reads "this process is inside the stack's
    /// namespace", which is the same question because the instances are
    /// this process's children in that namespace.
    ///
    /// Here the process is not on the network at all — it reaches guests
    /// only through the switch — but the environment it composes is CONSUMED
    /// inside a guest that is, so the guest's view is the one that has to be
    /// answered. A member of a `ply up` stack really does reach its
    /// dependency at `<dep>.ply`, on the dependency's own port; the
    /// published pair is the Mac's side of a proxy and means nothing in
    /// there.
    ///
    /// A standalone run has no peers on its switch to name, so it keeps the
    /// published address — which is the only address that is true for it.
    fn network(&self, opts: &RunOptions) -> NetworkFacts {
        NetworkFacts {
            facts: self.facts(),
            in_stack_network: opts.network.is_some() && self.net().is_some(),
            // Every instance is its own machine with its own address, so
            // nothing contends for its ports and `PORT` is not injected.
            alone: true,
        }
    }

    fn reach_via(&self) -> Option<PathBuf> {
        self.net()?.socket().map(|p| p.to_path_buf())
    }

    /// Boot one microVM and hand the supervisor an instance.
    ///
    /// The order below is the order the guest depends on, and every step of
    /// it is load-bearing:
    ///
    /// 1. the disks, **in attach order** — layers, then volumes, then the
    ///    spec disk. `spec_disk::volume_devs` names those devices for the
    ///    guest and `machine::build_dtb` makes the names true; neither can be
    ///    checked against the other by anything but this list.
    /// 2. the spec disk, written last because it carries the device names
    ///    computed from the two lists above.
    /// 3. `Machine::boot`, which returns as soon as the vCPU thread is
    ///    running — it never blocks and never calls `exit()`.
    /// 4. `{"ready":true}` on the control channel, bounded. A VM that never
    ///    says ready is a FAILED launch, not a hung one: the supervisor gets
    ///    an error it can print instead of a machine it believes in.
    /// 5. only then the state file, so nothing observes an instance that is
    ///    not there.
    fn launch(&self, spec: &InstanceSpec, record: Record<'_>) -> Result<Launched> {
        let guard = InstanceGuard {
            dir: spec.instance_dir.clone(),
        };

        // --- the disk list: the guest's whole world, in order -------------
        let mut disks: Vec<machine::DiskSpec> = spec
            .images
            .iter()
            .map(|path| machine::DiskSpec {
                path: path.clone(),
                read_only: true,
            })
            .collect();
        // Declared volumes are disks; links are shares of the host directory
        // itself, over 9p, so an edit on the Mac is what the guest reads.
        let (volumes, links) = spec_disk::partition(spec);
        for (i, (host_dir, _target)) in volumes.iter().enumerate() {
            disks.push(machine::DiskSpec {
                path: volume_disk(&spec.app, i, host_dir)?,
                read_only: false,
            });
        }
        let (uid, gid) = spec
            .run_user
            .as_ref()
            .map(|u| (u.uid, u.gid))
            .unwrap_or((0, 0));
        let shares: Vec<machine::ShareSpec> = links
            .iter()
            .enumerate()
            .map(|(i, (host_dir, _target))| machine::ShareSpec {
                tag: spec_disk::share_tag(i),
                root: host_dir.clone(),
                uid,
                gid,
            })
            .collect();
        // The names the guest will be told, from the contract's own function
        // rather than composed here: two spellings of `/dev/vdX` that can
        // drift is exactly the failure mode `volume_devs` exists to prevent.
        let devs = spec_disk::volume_devs(spec.images.len(), volumes.len());

        // --- the worker: one process, one VM ------------------------------
        // Hypervisor.framework allows one VM per process, so every instance
        // gets a process of its own (`worker`). What follows is the link:
        // the parent listens, the worker connects and joins the switch, and
        // only then does the parent know the address to put in the spec
        // disk — after which it tells the worker to boot.
        let socket_path = spec.instance_dir.join(worker::CONTROL_SOCKET);
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).map_err(|e| {
            Error::Runtime(format!(
                "{}: listening for the instance's worker at {}: {e}",
                spec.app,
                socket_path.display()
            ))
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|e| Error::Runtime(format!("{}: the worker listener: {e}", spec.app)))?;

        let spec_img = spec.instance_dir.join("spec.img");
        let mut worker_disks: Vec<(PathBuf, bool)> = disks
            .iter()
            .map(|d| (d.path.clone(), d.read_only))
            .collect();
        worker_disks.push((spec_img.clone(), true));
        let worker_spec = worker::WorkerSpec {
            app: spec.app.clone(),
            kernel: self.kernel.image.clone(),
            initramfs: self.kernel.initramfs.clone(),
            disks: worker_disks,
            mem_mib: MEMORY_MIB,
            // Addressed by SLOT — `<app>.<n>` — because two instances of one
            // app are two machines and cannot share an address; slots start
            // at 1 and an instance restarted into the same slot comes back
            // on the address its peers already know. `<app>` is then an
            // alias onto the first of them, so `<app>.ply` answers.
            net: self.net().and_then(|net| {
                net.socket().map(|socket| worker::WorkerNet {
                    socket: socket.to_path_buf(),
                    slot: format!("{}.{}", spec.app, spec.n),
                    alias: spec.app.clone(),
                })
            }),
            shares: shares
                .iter()
                .map(|s| worker::WorkerShare {
                    tag: s.tag.clone(),
                    root: s.root.clone(),
                    uid: s.uid,
                    gid: s.gid,
                })
                .collect(),
        };
        let worker_json = spec.instance_dir.join(worker::SPEC_FILE);
        std::fs::write(
            &worker_json,
            serde_json::to_vec_pretty(&worker_spec).map_err(|e| {
                Error::Runtime(format!("{}: encoding the worker spec: {e}", spec.app))
            })?,
        )
        .map_err(|source| Error::Io {
            path: worker_json.clone(),
            source,
        })?;

        let exe = std::env::current_exe().map_err(|e| {
            Error::Runtime(format!(
                "{}: locating the ply binary for the worker: {e}",
                spec.app
            ))
        })?;
        let mut child = std::process::Command::new(&exe)
            .arg(worker::SUBCOMMAND)
            .arg(&spec.instance_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| {
                Error::Runtime(format!("{}: spawning the microVM worker: {e}", spec.app))
            })?;

        // Everything from here to `record` kills the worker on failure: an
        // instance that never got its state file must not outlive the
        // error that describes it.
        let launched = (|| -> Result<(UnixStream, Option<Ipv4Addr>)> {
            let deadline = std::time::Instant::now() + READY_TIMEOUT;
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if let Ok(Some(status)) = child.try_wait() {
                            return Err(Error::Runtime(format!(
                                "{}: the microVM worker exited ({status}) before it connected",
                                spec.app
                            )));
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err(Error::Runtime(format!(
                                "{}: the microVM worker did not connect within {}s",
                                spec.app,
                                READY_TIMEOUT.as_secs()
                            )));
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        return Err(Error::Runtime(format!(
                            "{}: accepting the worker's connection: {e}",
                            spec.app
                        )))
                    }
                }
            };
            stream
                .set_nonblocking(false)
                .map_err(|e| Error::Runtime(format!("{}: the worker link: {e}", spec.app)))?;
            let mut line = String::new();
            std::io::BufReader::new(
                stream
                    .try_clone()
                    .map_err(|e| Error::Runtime(format!("{}: the worker link: {e}", spec.app)))?,
            )
            .read_line(&mut line)
            .map_err(|e| {
                Error::Runtime(format!("{}: reading the worker's address: {e}", spec.app))
            })?;
            let ip = match line.trim().strip_prefix("attached ") {
                Some("none") => None,
                Some(addr) => Some(addr.parse::<Ipv4Addr>().map_err(|_| {
                    Error::Runtime(format!(
                        "{}: the worker reported an address that is not IPv4: {line:?}",
                        spec.app
                    ))
                })?),
                None => {
                    return Err(Error::Runtime(format!(
                        "{}: the worker said {line:?} where `attached <ip>` was expected",
                        spec.app
                    )))
                }
            };
            Ok((stream, ip))
        })();
        let (mut stream, ip) = match launched {
            Ok(v) => v,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        };
        drop(listener);
        let _ = std::fs::remove_file(&socket_path);

        // The guest's resolver is the switch, always: it is the only thing
        // on that network that can reach one.
        let dns = ip.map(|_| switch::GATEWAY.to_string());

        // Where this instance's siblings are, so its `/etc/hosts` can name
        // them. A peer with no address on the switch gets NO LINE — never a
        // loopback placeholder: `/etc/hosts` beats DNS, so a `127.0.0.1
        // db.ply` would shadow the switch's correct answer for the life of
        // this guest and point every cross-member connection back into
        // itself. `ply up` reserves every member's address before it spawns
        // anything, so the peer that has not started yet is still named
        // correctly here.
        let peers: Vec<(String, Ipv4Addr)> = match self.net() {
            Some(net) => spec
                .local_aliases
                .iter()
                .filter_map(|alias| Some((alias.clone(), net.lookup(alias)?)))
                .collect(),
            None => Vec::new(),
        };
        for alias in &spec.local_aliases {
            if !peers.iter().any(|(name, _)| name == alias) {
                eprintln!(
                    "ply: warning: {}: no address for `{alias}` on this run's network — \
                     {alias}.ply is left to the switch's resolver",
                    spec.app
                );
            }
        }

        // --- the spec disk, then the go-ahead -----------------------------
        let booted = (|| -> Result<()> {
            spec_disk::write(&spec_img, &spec_disk::build(spec, &devs, dns, &peers, ip))?;
            stream
                .write_all(b"boot\n")
                .and_then(|_| stream.flush())
                .map_err(|e| {
                    Error::Runtime(format!("{}: telling the worker to boot: {e}", spec.app))
                })
        })();
        if let Err(e) = booted {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }

        // Guest lines arrive over the link as text; a reader turns them back
        // into `GuestLine`s so the ready wait and the pump are the same code
        // they were when the VM lived in this process.
        let (tx, lines) = std::sync::mpsc::channel::<GuestLine>();
        {
            let reader = stream
                .try_clone()
                .map_err(|e| Error::Runtime(format!("{}: the worker link: {e}", spec.app)))?;
            std::thread::Builder::new()
                .name("ply-vm-link".into())
                .spawn(move || {
                    let mut reader = std::io::BufReader::new(reader);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                if let Some(guest) = parse_guest_line(&line) {
                                    if tx.send(guest).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                })
                .map_err(|e| Error::Runtime(format!("spawning the link reader: {e}")))?;
        }

        // --- wait for the guest to say it is up ---------------------------
        let exit = Arc::new(Mutex::new(None));
        if let Err(e) = wait_for_ready(&spec.app, &lines, &exit) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }

        // --- the state file, while nothing can race it --------------------
        // The WORKER's pid: it is a real child, the process `ply stop`
        // signals and `ply deploy` walks up from to find this parent. The
        // address is the instance's own on the switch — loopback only for
        // an instance that has no network card, where it is as true as
        // anything else would be.
        let ip = ip.unwrap_or(Ipv4Addr::LOCALHOST);
        record(child.id() as i32, ip)?;

        let app = spec.app.clone();
        let pump_exit = exit.clone();
        std::thread::Builder::new()
            .name("ply-vm-control".into())
            .spawn(move || pump_control(&app, lines, &pump_exit))
            .map_err(|e| Error::Runtime(format!("spawning the control reader: {e}")))?;

        let output = child
            .stdout
            .take()
            .ok_or_else(|| Error::Runtime("the worker has no stdout".into()))?;
        Ok(Launched {
            instance: Box::new(VmInstance {
                ip,
                net: self.net().cloned(),
                child,
                control: Mutex::new(stream),
                ended: None,
                _guard: guard,
            }),
            output: Box::new(output),
        })
    }

    fn terminal(&self, _app: &str, _slot: u32, _nonce: &str) -> Result<()> {
        Err(Error::Runtime(
            "`ply exec` into a microVM is not available yet (a virtio-console shell channel is v2)"
                .into(),
        ))
    }
}

/// How long a guest gets to reach `{"ready":true}`.
///
/// The whole boot is a kernel, an initramfs, an overlay and an exec, and it
/// takes about a second. Ten is generous enough that a cold page cache or a
/// volume being formatted for the first time cannot trip it, and short
/// enough that a guest which will never answer is reported rather than
/// waited on.
#[cfg(target_os = "macos")]
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Block until the guest says it is running, or say why it never did.
///
/// `{"exit":N}` before `{"ready":true}` is not a failure: a fast entrypoint
/// can be gone before this thread is scheduled. It is recorded and the
/// launch succeeds, so the supervisor reports the app's own exit code rather
/// than a launch error about a VM that did exactly what it was asked.
#[cfg(target_os = "macos")]
fn wait_for_ready(
    app: &str,
    lines: &std::sync::mpsc::Receiver<GuestLine>,
    exit: &Arc<Mutex<Option<i32>>>,
) -> Result<()> {
    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(Error::Runtime(format!(
                "{app}: the microVM booted but never reported ready within {}s — the guest \
                 init did not reach the entrypoint (its own diagnostics are on stderr above)",
                READY_TIMEOUT.as_secs()
            )));
        }
        match lines.recv_timeout(left) {
            Ok(GuestLine::Ready) => return Ok(()),
            Ok(GuestLine::Exit { code }) => {
                *exit.lock().map_err(poisoned)? = Some(code);
                return Ok(());
            }
            Ok(GuestLine::Publish { publish }) => apply_publish(app, &publish, &mut false),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Runtime(format!(
                    "{app}: the microVM stopped before it reported ready"
                )))
            }
        }
    }
}

/// Guest → host, for the life of the instance.
#[cfg(target_os = "macos")]
fn pump_control(
    app: &str,
    lines: std::sync::mpsc::Receiver<GuestLine>,
    exit: &Arc<Mutex<Option<i32>>>,
) {
    let mut warned = false;
    for line in lines {
        match line {
            GuestLine::Ready => {}
            GuestLine::Exit { code } => {
                if let Ok(mut slot) = exit.lock() {
                    // First answer wins: an exit code is a fact about one
                    // run of one entrypoint, and it does not change.
                    slot.get_or_insert(code);
                }
            }
            GuestLine::Publish { publish } => apply_publish(app, &publish, &mut warned),
        }
    }
}

/// Fold one `{"publish":…}` into the host's params tree — unless it names a
/// fact the parent owns.
///
/// # Why this filter exists twice
///
/// The guest already refuses to forward `state`, `instances`, `started_at`
/// and `restarts` out of its own `/run/ply/self` watch, and that seal is
/// what stops an app forging its own health for every `--after` dependant
/// downstream. But the guest is the wrong place to enforce it ALONE: unlike
/// a namespace instance, a VM app can run as uid 0 with full capabilities in
/// the guest's initial user namespace, and `PLY_MICROVM_KERNEL` is a
/// documented override — the entire guest sits inside a boundary the user
/// can replace. Defence in depth belongs on the host side of a replaceable
/// boundary, so the parent drops these here too.
///
/// Logged once per instance: an app writing in a loop must not be able to
/// turn a rejected key into a flood of parent output.
#[cfg(target_os = "macos")]
fn apply_publish(app: &str, publish: &ply_vm_proto::Publish, warned: &mut bool) {
    if PARENT_OWNED.contains(&publish.key.as_str()) {
        if !*warned {
            *warned = true;
            eprintln!(
                "ply: {app}: ignoring a published `{}` from inside the instance — \
                 {} are the parent's to set",
                publish.key,
                PARENT_OWNED.join(", ")
            );
        }
        return;
    }
    let _ = crate::runtime::params_tree::publish(app, &publish.key, &publish.value);
}

#[cfg(target_os = "macos")]
fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::Runtime("the instance's exit state was left poisoned by a panic".into())
}

/// The backing file for one volume, created 8 GiB sparse on first use.
///
/// A microVM has no shared-directory transport, so a bind is a DISK: the
/// guest formats it ext4 the first time and mounts it every time. The file
/// lives inside the same per-volume directory the namespace backend
/// bind-mounts, so `ply run`'s volume bookkeeping — the naming, the
/// per-instance/shared split, the empty-volume warning — is shared between
/// the two backends rather than reinvented here.
#[cfg(target_os = "macos")]
fn volume_disk(app: &str, index: usize, host_dir: &std::path::Path) -> Result<PathBuf> {
    /// Sparse, so the file costs what the guest actually writes. A volume
    /// cannot grow under a running guest, so this is also the ceiling.
    const VOLUME_BYTES: u64 = 8 * 1024 * 1024 * 1024;

    // Only declared volumes reach here (links are 9p shares), and a declared
    // volume's host directory is ply's own. A host directory anywhere else
    // would mean a disk image written into a user's tree, so it gets one
    // under the volumes directory instead.
    let dir = if host_dir.starts_with(crate::paths::volumes_dir()) {
        host_dir.to_path_buf()
    } else {
        crate::paths::volumes_dir()
            .join(app)
            .join(format!("volume.{index}"))
    };
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join("disk.ext4");
    if !path.exists() {
        let file = std::fs::File::create(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        file.set_len(VOLUME_BYTES).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
    }
    Ok(path)
}

/// One instance as the macOS backend runs it: a microVM in a worker
/// process of its own.
///
/// `pid()` and `child_pid()` are both the worker's: it is a real child, so
/// the supervisor's signal handler can `kill` it (the worker forwards the
/// signal into the guest by name), `ply stop` can signal it, and
/// `ply deploy` can walk up from it to this parent. SIGKILL alone takes the
/// machine down with the process, which is what SIGKILL means.
///
/// Field order is the drop order and it matters: the child goes first
/// (killed if it is somehow still there — no VM outlives its instance),
/// then the guard removes the instance directory and the spec disk with
/// its composed secrets.
#[cfg(target_os = "macos")]
pub(crate) struct VmInstance {
    ip: Ipv4Addr,
    /// How anything on the Mac reaches this instance. `None` is an instance
    /// with no network card.
    net: Option<switch::Net>,
    child: std::process::Child,
    /// The link to the worker: host control lines go down it.
    control: Mutex<UnixStream>,
    /// Sticky, like `NsInstance::ended`: once known, always the same answer.
    ended: Option<i32>,
    _guard: InstanceGuard,
}

/// The name the guest init turns back into a signal number.
///
/// Names, not numbers, on purpose: the two sides are a macOS host and an
/// arm64 Linux guest, and although the common signals happen to share
/// numbers there, relying on that would be a trap the first time they did
/// not.
#[cfg(target_os = "macos")]
fn signal_name(sig: Signal) -> String {
    sig.as_str().trim_start_matches("SIG").to_string()
}

#[cfg(target_os = "macos")]
impl VmInstance {
    fn worker(&self) -> nix::unistd::Pid {
        nix::unistd::Pid::from_raw(self.child.id() as i32)
    }
}

#[cfg(target_os = "macos")]
impl Instance for VmInstance {
    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn child_pid(&self) -> Option<i32> {
        Some(self.child.id() as i32)
    }

    fn ip(&self) -> Ipv4Addr {
        self.ip
    }

    fn alive(&self) -> bool {
        self.ended.is_none() && nix::sys::signal::kill(self.worker(), None).is_ok()
    }

    /// A polite signal goes down the link as `{"signal":"TERM"}`, which the
    /// guest init forwards to the app; if the link is gone the worker gets
    /// the signal itself and forwards it the same way. SIGKILL is the one
    /// thing that must not be polite: it ends the worker, and the VM with it.
    fn signal(&self, sig: Signal) -> Result<()> {
        if sig == Signal::SIGKILL {
            let _ = nix::sys::signal::kill(self.worker(), Signal::SIGKILL);
            return Ok(());
        }
        let line = host_line(&HostLine::Signal {
            name: signal_name(sig),
        });
        let sent = match self.control.lock() {
            Ok(mut stream) => stream
                .write_all(line.as_bytes())
                .and_then(|_| stream.flush())
                .is_ok(),
            Err(_) => false,
        };
        if !sent {
            let _ = nix::sys::signal::kill(self.worker(), sig);
        }
        Ok(())
    }

    fn try_wait(&mut self) -> Result<Option<i32>> {
        if let Some(code) = self.ended {
            return Ok(Some(code));
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                use std::os::unix::process::ExitStatusExt;
                // The worker exits with the guest's own code; a worker that
                // died of a signal is `128 + n`, as a namespace child would
                // be; anything else is 255, what a supervisor calls "gone".
                let code = status
                    .code()
                    .or_else(|| status.signal().map(|n| 128 + n))
                    .unwrap_or(255);
                self.ended = Some(code);
                Ok(self.ended)
            }
            Ok(None) => Ok(None),
            Err(e) => Err(Error::Runtime(format!(
                "waiting for the microVM worker: {e}"
            ))),
        }
    }

    /// The health gate and the `--after` probes, through the switch.
    ///
    /// The connection is opened and dropped: what these callers ask is "is
    /// anything listening", and the switch's answer to that is a real
    /// handshake with the guest, not a guess.
    fn tcp_open(&self, port: u16, timeout: Duration) -> std::io::Result<()> {
        let Some(net) = &self.net else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "this instance has no network card (the run's switch did not start)",
            ));
        };
        net.connect(self.ip, port, timeout).map(|_| ())
    }

    /// How a published port reaches this instance.
    ///
    /// NOT the default address-based connector: `10.77.0.2` exists on the
    /// parent's switch and nowhere else, so `TcpStream::connect` to it from
    /// the host finds nothing. That was the bug — the parent bound the host
    /// port, accepted the connection, found no reachable backend and reset
    /// it.
    fn connector(&self, port: u16) -> Arc<dyn crate::runtime::publish::Connector> {
        match &self.net {
            Some(net) => net.connector(self.ip, port),
            // No network: the pool will find nothing, which is the truth.
            None => {
                crate::runtime::publish::connector_for(std::net::SocketAddr::from((self.ip, port)))
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for VmInstance {
    fn drop(&mut self) {
        // A worker that is still there when its instance is dropped is a VM
        // nobody will ever stop. Not the polite path — that is
        // `stop_with_patience`'s, and it has already run by now.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Removes the instance directory — and with it the spec disk, which holds
/// the run's composed environment — on drop, including error paths.
#[cfg(target_os = "macos")]
struct InstanceGuard {
    dir: PathBuf,
}

#[cfg(target_os = "macos")]
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = crate::paths::force_remove_dir_all(&self.dir);
    }
}
