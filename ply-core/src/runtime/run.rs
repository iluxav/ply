//! `ply run` — the whole runtime: mount the closure and exec.
//! `--scale N` = N identical processes/cgroups/netns, one shared store.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use nix::sched::CloneFlags;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;

use crate::env::compose_env;
use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{read_embedded, read_lockfile, read_manifest};
use crate::manifest::{Layer, Manifest};
use crate::runtime::cgroup::Cgroup;
use crate::runtime::container::{child_main, ContainerSpec};
use crate::runtime::state::InstanceState;
use crate::runtime::{hosts, loopdev, mount, network, state};
use crate::source::Source;
use crate::store::Store;

pub struct RunOptions {
    pub image: PathBuf,
    /// CLI -e KEY=VALUE overrides (highest precedence).
    pub cli_env: Vec<(String, String)>,
    pub allow_insecure: bool,
    pub scale: u32,
    /// Dev-mode bind mounts: (host path, container path). Same mechanism
    /// as volumes.
    pub links: Vec<(PathBuf, String)>,
}

pub fn run(opts: &RunOptions) -> Result<i32> {
    let rootless = !crate::paths::is_root();
    if rootless {
        eprintln!(
            "ply: rootless mode — extracted layers, host network (no .ply names), no cgroup limits"
        );
        // Ubuntu >= 24.04 strips capabilities from unprivileged user
        // namespaces unless an AppArmor profile grants `userns`.
        let restricted =
            std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                .map(|v| v.trim() == "1")
                .unwrap_or(false);
        if restricted && !Path::new("/etc/apparmor.d/ply").exists() {
            return Err(Error::Runtime(
                "this kernel restricts unprivileged user namespaces — one-time fix:\n  \
                 sudo ply setup\n  \
                 (or run with sudo instead)"
                    .into(),
            ));
        }
    }

    let store = Store::open_default()?;
    let mut ctx = prepare_app(&opts.image, &opts.cli_env, opts.allow_insecure, &store)?;

    if !rootless {
        network::ensure_bridge()?;
    }
    let _ = state::reap_stale(); // free IPs/dirs leaked by killed runs

    // Same app already running? That's legal (canary: old + new side by
    // side) — but say so, and point at deploy for the replace case.
    let already_running = state::list()?
        .iter()
        .filter(|s| s.app == ctx.manifest.package.name && s.alive())
        .count();
    if already_running > 0 {
        eprintln!(
            "ply: note: {} already has {already_running} running instance(s) — this run ADDS instances (canary).\n\
             ply:       to replace the running version instead: ply deploy {}",
            ctx.manifest.package.name,
            opts.image.display()
        );
    }

    // Forward SIGTERM/SIGINT to every instance. SIGHUP is overloaded:
    // user-sent (ply deploy / kill -HUP) = rolling reload; kernel-sent
    // (terminal hangup) = stop, like any daemonless foreground process.
    unsafe {
        signal::signal(Signal::SIGTERM, signal::SigHandler::Handler(forward_signal)).ok();
        signal::signal(Signal::SIGINT, signal::SigHandler::Handler(forward_signal)).ok();
        let hup = signal::SigAction::new(
            signal::SigHandler::SigAction(hup_dispatch),
            signal::SaFlags::SA_SIGINFO,
            nix::sys::signal::SigSet::empty(),
        );
        signal::sigaction(Signal::SIGHUP, &hup).ok();
    }

    // [restart] policy: the parent respawns instances it started. If the
    // parent itself dies, that's systemd's layer.
    let restart_policy = ctx.manifest.restart.clone();
    let initial_backoff = restart_policy
        .as_ref()
        .map(|r| crate::manifest::parse_duration(&r.backoff))
        .transpose()?
        .unwrap_or(std::time::Duration::from_secs(2));
    let max_backoff = restart_policy
        .as_ref()
        .map(|r| crate::manifest::parse_duration(&r.max_backoff))
        .transpose()?
        .unwrap_or(std::time::Duration::from_secs(30));

    struct SlotInfo {
        backoff: std::time::Duration,
        restarts: u32,
        started: std::time::Instant,
        sig_idx: usize,
    }
    let mut slots: std::collections::BTreeMap<u32, SlotInfo> = Default::default();
    let mut pending: Vec<(u32, std::time::Instant)> = Vec::new(); // (slot, due)

    let mut instances: Vec<Instance> = Vec::new();
    for _ in 0..opts.scale.max(1) {
        let instance = launch_instance(&ctx, &opts.links, &store, rootless, None, 0)?;
        let sig_idx = register_child(instance.child.as_raw());
        slots.insert(
            instance.n,
            SlotInfo {
                backoff: initial_backoff,
                restarts: 0,
                started: std::time::Instant::now(),
                sig_idx,
            },
        );
        instances.push(instance);
    }

    // Rolling deploy state (SIGHUP): slots still to roll + the previous
    // context to fall back to if a new instance flunks its health gate.
    let mut roll_queue: Vec<u32> = Vec::new();
    let mut old_ctx: Option<AppContext> = None;

    // Reap, respawn per policy, exit when nothing is left to wait for.
    let mut exit_code = 0;
    loop {
        // Collect every child death that's already happened.
        let mut deaths: Vec<(Pid, i32, bool)> = Vec::new(); // (pid, code, failed)
        loop {
            use nix::sys::wait::WaitPidFlag;
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => break,
                Ok(WaitStatus::Exited(pid, code)) => deaths.push((pid, code, code != 0)),
                Ok(WaitStatus::Signaled(pid, sig, _)) => deaths.push((pid, 128 + sig as i32, true)),
                Ok(_) => continue,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::ECHILD) => break,
                Err(e) => return Err(Error::Runtime(format!("waitpid: {e}"))),
            }
        }

        let shutting_down = SHUTTING_DOWN.load(Ordering::SeqCst);

        for (pid, code, failed) in deaths {
            let Some(pos) = instances.iter().position(|i| i.child == pid) else {
                continue;
            };
            let slot = instances[pos].n;
            let info = slots.get_mut(&slot).expect("live instance has a slot");
            update_child(info.sig_idx, 0);
            drop(instances.remove(pos)); // unmount, remove state + hosts now

            let respawn = !shutting_down
                && match restart_policy.as_ref().map(|r| r.policy.as_str()) {
                    Some("always") => true,
                    Some("on-failure") => failed,
                    _ => false,
                };
            if respawn {
                // A healthy stretch resets the backoff ladder.
                if info.started.elapsed() > std::time::Duration::from_secs(60) {
                    info.backoff = initial_backoff;
                }
                pending.push((slot, std::time::Instant::now() + info.backoff));
                info.backoff = (info.backoff * 2).min(max_backoff);
            } else if exit_code == 0 && code != 0 {
                exit_code = code;
            }
        }
        if shutting_down {
            pending.clear();
        }

        // Launch respawns that are due.
        let now = std::time::Instant::now();
        let due: Vec<u32> = pending
            .iter()
            .filter(|(_, at)| *at <= now)
            .map(|(n, _)| *n)
            .collect();
        pending.retain(|(_, at)| *at > now);
        for slot in due {
            let info = slots.get_mut(&slot).expect("pending slot is tracked");
            info.restarts += 1;
            eprintln!(
                "ply: restarting {}.{slot} (restart #{}, policy {})",
                ctx.manifest.package.name,
                info.restarts,
                restart_policy
                    .as_ref()
                    .map(|r| r.policy.as_str())
                    .unwrap_or("?"),
            );
            match launch_instance(
                &ctx,
                &opts.links,
                &store,
                rootless,
                Some(slot),
                info.restarts,
            ) {
                Ok(instance) => {
                    update_child(info.sig_idx, instance.child.as_raw());
                    info.started = std::time::Instant::now();
                    instances.push(instance);
                }
                Err(e) => {
                    // Relaunch itself failed (fetch/mount error): retry with
                    // doubled backoff rather than crash-looping the parent.
                    eprintln!(
                        "ply: restart of {}.{slot} failed: {e}",
                        ctx.manifest.package.name
                    );
                    info.backoff = (info.backoff * 2).min(max_backoff);
                    pending.push((slot, std::time::Instant::now() + info.backoff));
                }
            }
        }

        // Deploy requested: consume the pointer file, prepare the new
        // version while the old one keeps serving.
        if RELOAD_REQUESTED.swap(false, Ordering::SeqCst) && !shutting_down {
            let pointer = crate::paths::apps_dir()
                .join(&ctx.manifest.package.name)
                .join("next-image");
            match std::fs::read_to_string(&pointer) {
                Ok(text) => {
                    let _ = std::fs::remove_file(&pointer); // consumed either way
                    let new_image = PathBuf::from(text.trim());
                    eprintln!("ply: deploy -> {}", new_image.display());
                    match prepare_app(&new_image, &opts.cli_env, opts.allow_insecure, &store) {
                        Ok(new_ctx) => {
                            let mut queue: Vec<u32> = instances.iter().map(|i| i.n).collect();
                            queue.sort_unstable();
                            roll_queue = queue;
                            old_ctx = Some(std::mem::replace(&mut ctx, new_ctx));
                        }
                        Err(e) => {
                            eprintln!("ply: deploy aborted — new image unusable: {e}");
                        }
                    }
                }
                Err(_) => eprintln!(
                    "ply: SIGHUP received but no deploy pointer at {} — ignoring",
                    pointer.display()
                ),
            }
        }

        // One roll step per iteration: stop old instance, start new, gate.
        if !roll_queue.is_empty() && !shutting_down {
            let slot = roll_queue.remove(0);
            if let Some(pos) = instances.iter().position(|i| i.n == slot) {
                let old_instance = instances.remove(pos);
                if let Some(info) = slots.get(&slot) {
                    update_child(info.sig_idx, 0);
                }
                stop_instance(old_instance);

                let restarts = slots.get(&slot).map(|s| s.restarts).unwrap_or(0);
                let outcome =
                    launch_instance(&ctx, &opts.links, &store, rootless, Some(slot), restarts)
                        .and_then(|instance| {
                            if wait_healthy(&ctx, &instance) {
                                Ok(instance)
                            } else {
                                let app = instance.app.clone();
                                stop_instance(instance);
                                Err(Error::Runtime(format!(
                                    "{app}.{slot} failed its health gate"
                                )))
                            }
                        });
                match outcome {
                    Ok(instance) => {
                        if let Some(info) = slots.get_mut(&slot) {
                            update_child(info.sig_idx, instance.child.as_raw());
                            info.started = std::time::Instant::now();
                        }
                        eprintln!(
                            "ply: {}.{slot} now on {}",
                            ctx.manifest.package.name,
                            ctx.image.display()
                        );
                        instances.push(instance);
                        if roll_queue.is_empty() {
                            old_ctx = None;
                            eprintln!(
                                "ply: deploy complete — all instances on {}",
                                ctx.image.display()
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("ply: deploy aborted: {e}");
                        roll_queue.clear();
                        if let Some(prev) = old_ctx.take() {
                            ctx = prev; // untouched + reverted slots stay on the old version
                        }
                        eprintln!(
                            "ply: reverting {}.{slot} to {}",
                            ctx.manifest.package.name,
                            ctx.image.display()
                        );
                        match launch_instance(
                            &ctx,
                            &opts.links,
                            &store,
                            rootless,
                            Some(slot),
                            restarts,
                        ) {
                            Ok(instance) => {
                                if let Some(info) = slots.get_mut(&slot) {
                                    update_child(info.sig_idx, instance.child.as_raw());
                                    info.started = std::time::Instant::now();
                                }
                                instances.push(instance);
                            }
                            Err(e) => {
                                eprintln!(
                                    "ply: revert launch failed ({e}) — retrying via restart path"
                                );
                                pending.push((slot, std::time::Instant::now() + initial_backoff));
                            }
                        }
                    }
                }
            }
        }

        if instances.is_empty() && pending.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    drop(instances); // unmount, remove state + hosts entries
    Ok(exit_code)
}

/// Deliberate stop: TERM, up to 10s to comply, then KILL. Reaps the child so
/// its death never reaches the policy loop.
fn stop_instance(instance: Instance) {
    use nix::sys::wait::WaitPidFlag;
    let child = instance.child;
    let _ = signal::kill(child, Signal::SIGTERM);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while let Ok(WaitStatus::StillAlive) = waitpid(child, Some(WaitPidFlag::WNOHANG)) {
        if std::time::Instant::now() >= deadline {
            let _ = signal::kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    drop(instance); // unmount, remove state + hosts
}

/// The deploy health gate. With [health] port: TCP connect within grace.
/// Without: the process just has to be alive after a short settle.
fn wait_healthy(ctx: &AppContext, instance: &Instance) -> bool {
    let (port, grace) = match &ctx.manifest.health {
        Some(health) => (
            health.port,
            crate::manifest::parse_duration(&health.grace)
                .unwrap_or(std::time::Duration::from_secs(10)),
        ),
        None => (None, std::time::Duration::from_secs(1)),
    };
    let ip = InstanceState::path(&instance.app, instance.n)
        .exists()
        .then(|| std::fs::read_to_string(InstanceState::path(&instance.app, instance.n)).ok())
        .flatten()
        .and_then(|text| serde_json::from_str::<InstanceState>(&text).ok())
        .map(|s| s.ip);

    let deadline = std::time::Instant::now() + grace;
    let mut last_err: Option<std::io::Error> = None;
    loop {
        let alive = unsafe { nix::libc::kill(instance.child.as_raw(), 0) == 0 };
        if !alive {
            eprintln!(
                "ply: health gate: {}.{} died during grace",
                instance.app, instance.n
            );
            return false;
        }
        if let (Some(port), Some(ip)) = (port, ip) {
            let addr = std::net::SocketAddr::from((ip, port));
            match std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300))
            {
                Ok(_) => return true,
                Err(e) => last_err = Some(e),
            }
        }
        if std::time::Instant::now() >= deadline {
            if let Some(port) = port {
                eprintln!(
                    "ply: health gate: no answer on {}:{port} within {grace:?} — last error: {}",
                    ip.map(|i| i.to_string())
                        .unwrap_or_else(|| "<no ip in state!>".into()),
                    last_err
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "none".into()),
                );
                return false;
            }
            return true; // no port to probe: surviving the grace window is the bar
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

static RELOAD_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// SIGHUP with SA_SIGINFO: si_code tells apart a process-sent signal
/// (SI_USER/SI_QUEUE, <= 0 — a deploy) from a kernel-generated hangup
/// (terminal closed — treat exactly like SIGTERM).
extern "C" fn hup_dispatch(
    sig: i32,
    info: *mut nix::libc::siginfo_t,
    _ctx: *mut nix::libc::c_void,
) {
    let user_sent = unsafe { !info.is_null() && (*info).si_code <= 0 };
    if user_sent {
        RELOAD_REQUESTED.store(true, Ordering::SeqCst);
    } else {
        forward_signal(sig); // sets SHUTTING_DOWN + forwards (as SIGTERM-ish)
    }
}

/// Everything needed to launch instances of one app version. Rebuilt from a
/// new image on deploy (SIGHUP) — respawns then use the new context.
pub struct AppContext {
    pub manifest: Manifest,
    pub entrypoint: Vec<String>,
    pub env: Vec<(String, String)>,
    pub image: PathBuf,
    pub dep_images: Vec<PathBuf>,
}

/// The pre-launch phase: read manifest + lockfile, fetch missing store
/// digests, enforce host policy, compose env, record the app (GC roots).
fn prepare_app(
    image: &Path,
    cli_env: &[(String, String)],
    allow_insecure: bool,
    store: &Store,
) -> Result<AppContext> {
    let manifest = read_manifest(image)?;
    let entrypoint = manifest.package.entrypoint.clone().ok_or_else(|| {
        Error::Runtime(format!(
            "{} is a library/runtime package (no entrypoint) — only app images run",
            image.display()
        ))
    })?;
    if manifest.package.isolation == "vm" {
        return Err(Error::Runtime(
            "isolation = \"vm\" is not implemented yet — only \"ns\" runs today".into(),
        ));
    }
    let lockfile = read_lockfile(image)?;

    let mut dep_images: Vec<PathBuf> = Vec::new();
    let mut dep_layers: Vec<Layer> = Vec::new();
    if let Some(lockfile) = &lockfile {
        for pkg in &lockfile.packages {
            let path = match store.image_path(&pkg.sha256) {
                Some(path) => path,
                None => {
                    let source = Source::parse(&pkg.source, allow_insecure)?;
                    let img =
                        ImageName::new(&pkg.name, pkg.version.clone(), Os::Linux, Arch::host())?;
                    eprintln!("ply: fetching {img} ({})", pkg.sha256);
                    source.fetch(&img, Some(&pkg.sha256), store)?.1
                }
            };
            if let Some(bytes) = read_embedded(&path, "/.layer.toml")? {
                dep_layers.push(
                    toml::from_str(&String::from_utf8_lossy(&bytes)).map_err(|e| {
                        Error::Runtime(format!("{}: bad /.layer.toml: {e}", pkg.name))
                    })?,
                );
            }
            dep_images.push(path);
        }
    }

    // Host policy: refused runtimes don't run; deprecated ones warn.
    if let Some(lockfile) = &lockfile {
        if let Some(policy) = crate::policy::Policy::load_default()? {
            for finding in policy.check_lockfile(lockfile) {
                match finding.severity {
                    crate::policy::Severity::Error => {
                        return Err(Error::Runtime(format!("host policy: {}", finding.message)))
                    }
                    crate::policy::Severity::Warning => {
                        eprintln!("ply: warning: {}", finding.message)
                    }
                }
            }
        }
    }

    // Record the app as installed — the GC root set.
    let record = crate::apps::AppRecord {
        name: manifest.package.name.clone(),
        image: std::path::absolute(image).unwrap_or_else(|_| image.to_path_buf()),
        digests: lockfile
            .as_ref()
            .map(|l| l.packages.iter().map(|p| p.sha256.clone()).collect())
            .unwrap_or_default(),
        updated: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    record.save()?;

    let layer_refs: Vec<&Layer> = dep_layers.iter().collect();
    let mut env = compose_env(&layer_refs, &manifest.env, cli_env);
    env.entry("HOME".into()).or_insert("/root".into());
    if let Ok(term) = std::env::var("TERM") {
        env.entry("TERM".into()).or_insert(term);
    }

    Ok(AppContext {
        entrypoint,
        env: env.into_iter().collect(),
        image: image.to_path_buf(),
        dep_images,
        manifest,
    })
}

struct Instance {
    app: String,
    n: u32,
    child: Pid,
    _cgroup: Option<Cgroup>,
    guard: InstanceGuard,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = hosts::remove_entry(&self.app, self.n);
        InstanceState::remove(&self.app, self.n);
        let _ = &self.guard; // unmounts layers, removes instance dir
    }
}

fn launch_instance(
    ctx: &AppContext,
    links: &[(PathBuf, String)],
    store: &Store,
    rootless: bool,
    slot: Option<u32>,
    restarts: u32,
) -> Result<Instance> {
    let manifest = &ctx.manifest;
    let entrypoint = &ctx.entrypoint;
    let env = &ctx.env;
    let app_image = &ctx.image;
    let dep_images = &ctx.dep_images;
    let app = &manifest.package.name;
    let (instance_dir, n) = allocate_instance(app, slot)?;

    // Named volumes: per-instance by default (scaling can never silently
    // corrupt single-writer state); `scope = "shared"` is the explicit opt-in.
    let mut binds: Vec<(PathBuf, String)> = Vec::new();
    for (name, volume) in &manifest.volumes {
        let suffix = if volume.scope == "shared" {
            "shared".to_string()
        } else {
            n.to_string()
        };
        let host_dir = crate::paths::volumes_dir()
            .join(app)
            .join(format!("{name}.{suffix}"));
        std::fs::create_dir_all(&host_dir).map_err(|source| Error::Io {
            path: host_dir.clone(),
            source,
        })?;
        binds.push((host_dir, volume.path.clone()));
    }
    for (host, container) in links {
        let host = std::path::absolute(host).map_err(|source| Error::Io {
            path: host.clone(),
            source,
        })?;
        if !host.exists() {
            return Err(Error::Runtime(format!(
                "--link source {} does not exist",
                host.display()
            )));
        }
        binds.push((host, container.clone()));
    }
    let guard = InstanceGuard {
        dir: instance_dir.clone(),
        mounted_layers: std::cell::RefCell::new(Vec::new()),
    };

    // Layers: root loop-mounts squashfs; rootless uses store-cached
    // extractions (unprivileged kernels can't mount squashfs).
    let mut layers: Vec<PathBuf> = Vec::new();
    let all_images: Vec<PathBuf> = std::iter::once(app_image.to_path_buf())
        .chain(dep_images.iter().cloned())
        .collect();
    for (i, img) in all_images.iter().enumerate() {
        if rootless {
            let digest = crate::digest::sha256_file(img)?;
            let rootfs = store.extracted_rootfs(img, &digest)?;
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

    let keep_net_bind = manifest.ports.values().any(|p| *p < 1024);
    let spec = ContainerSpec {
        layers,
        instance_dir: instance_dir.clone(),
        hostname: app.clone(),
        cwd: PathBuf::from(format!("/opt/{app}")),
        env: env.to_vec(),
        argv: entrypoint.to_vec(),
        binds,
        sync_rx,
        keep_net_bind,
        privileged: false,
        rootless,
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
            Box::new(|| child_main(&spec)),
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
            write_id_maps(child.as_raw())?;
            return Ok((None, Ipv4Addr::new(127, 0, 0, 1)));
        }
        let cgroup = Cgroup::create(&format!("{app}.{n}"), manifest.resources.as_ref())?;
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

    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let state = InstanceState {
        app: app.clone(),
        n,
        pid: child.as_raw(),
        ip,
        ports: manifest.ports.clone(),
        image: app_image.display().to_string(),
        started,
        restarts,
    };
    state.save()?;
    if !rootless {
        hosts::add_entry(app, n, ip)?; // /etc/hosts needs root
    }

    // Release the child.
    let _ = nix::unistd::write(&sync_tx, &[1u8]);
    drop(sync_tx);

    Ok(Instance {
        app: app.clone(),
        n,
        child,
        _cgroup: cgroup,
        guard,
    })
}

/// Map the invoking user to root inside the child's user namespace.
fn write_id_maps(pid: i32) -> Result<()> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let write = |file: &str, contents: String| -> Result<()> {
        let path = format!("/proc/{pid}/{file}");
        std::fs::write(&path, contents).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    };
    write("setgroups", "deny".into())?;
    write("gid_map", format!("0 {gid} 1"))?;
    write("uid_map", format!("0 {uid} 1"))?;
    Ok(())
}

const MAX_CHILDREN: usize = 256;
static CHILD_PIDS: [AtomicI32; MAX_CHILDREN] = [const { AtomicI32::new(0) }; MAX_CHILDREN];
static CHILD_COUNT: AtomicUsize = AtomicUsize::new(0);

fn register_child(pid: i32) -> usize {
    let idx = CHILD_COUNT.fetch_add(1, Ordering::SeqCst);
    if idx < MAX_CHILDREN {
        CHILD_PIDS[idx].store(pid, Ordering::SeqCst);
    }
    idx
}

/// Respawns reuse their slot's signal-table entry (0 = empty slot); the
/// table never accumulates stale pids that a later process could inherit.
fn update_child(idx: usize, pid: i32) {
    if idx < MAX_CHILDREN {
        CHILD_PIDS[idx].store(pid, Ordering::SeqCst);
    }
}

static SIGNALS_SEEN: AtomicUsize = AtomicUsize::new(0);
static SHUTTING_DOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn forward_signal(sig: i32) {
    // A forwarded stop means intent: no respawns after this point.
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    // A container's entrypoint is PID 1 in its pid ns: the kernel drops
    // default-action signals to init. First signal forwards as-is (apps
    // with handlers stop gracefully); repeated signals escalate to SIGKILL,
    // which init cannot ignore.
    let escalate = SIGNALS_SEEN.fetch_add(1, Ordering::SeqCst) >= 1;
    let sig = if escalate { nix::libc::SIGKILL } else { sig };
    let count = CHILD_COUNT.load(Ordering::SeqCst).min(MAX_CHILDREN);
    for slot in CHILD_PIDS.iter().take(count) {
        let pid = slot.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe { nix::libc::kill(pid, sig) };
        }
    }
}

/// `/run/ply/instances/<app>.<n>` with rw/, work/, root/, layers/.
/// `slot` pins the instance number (respawns keep their identity — and
/// their per-instance volume).
fn allocate_instance(app: &str, slot: Option<u32>) -> Result<(PathBuf, u32)> {
    let instances = crate::paths::run_dir().join("instances");
    std::fs::create_dir_all(&instances).map_err(|source| Error::Io {
        path: instances.clone(),
        source,
    })?;
    if let Some(n) = slot {
        let dir = instances.join(format!("{app}.{n}"));
        if dir.exists() {
            // stale leftover from a crashed predecessor — self-heal
            let _ = crate::paths::force_remove_dir_all(&dir);
        }
        std::fs::create_dir(&dir).map_err(|source| Error::Io {
            path: dir.clone(),
            source,
        })?;
        for sub in ["rw", "work", "root", "layers"] {
            std::fs::create_dir_all(dir.join(sub)).map_err(|source| Error::Io {
                path: dir.join(sub),
                source,
            })?;
        }
        return Ok((dir, n));
    }
    for n in 1..10_000u32 {
        let dir = instances.join(format!("{app}.{n}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                for sub in ["rw", "work", "root", "layers"] {
                    std::fs::create_dir_all(dir.join(sub)).map_err(|source| Error::Io {
                        path: dir.join(sub),
                        source,
                    })?;
                }
                return Ok((dir, n));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(Error::Io { path: dir, source }),
        }
    }
    Err(Error::Runtime(format!("no free instance slot for {app}")))
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

/// Parse an --env-file: KEY=VALUE lines, `#` comments, blanks ignored.
pub fn parse_env_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut pairs = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| {
            Error::Runtime(format!(
                "{}:{}: expected KEY=VALUE",
                path.display(),
                lineno + 1
            ))
        })?;
        pairs.push((k.trim().to_string(), v.to_string()));
    }
    Ok(pairs)
}
