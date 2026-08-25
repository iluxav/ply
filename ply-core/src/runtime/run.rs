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
    /// `--publish`, repeatable: each spec gets its own host listener and its
    /// own backend pool, all fed by the same instances. An edge needs :80 and
    /// :443 together; a service may want HTTP plus gRPC or metrics.
    pub publish: Vec<crate::runtime::publish::Publish>,
    /// `--after`: apps on this host that must be healthy before the first
    /// instance launches (waited for once, at parent start).
    pub after: Vec<String>,
    /// `--after-timeout`: how long to wait for `after` before giving up.
    pub after_timeout: std::time::Duration,
    /// `--privileged`: skip rights stripping entirely — no capability drop,
    /// no no_new_privs, no seccomp. The app runs with everything the parent
    /// had. Debugging and OCI-import triage only: imported images expect
    /// Docker's retained capabilities (`chown -R x:x /data && exec gosu x`),
    /// which ply's zero-capability default refuses.
    pub privileged: bool,
}

/// Live wiring for a published pool, threaded through instance launches.
struct PublishWiring {
    pool: crate::runtime::publish::Pool,
    /// The parsed spec: the host port and bind scope the parent claimed, plus
    /// the port instances serve on (rootful: on their bridge IPs; rootless:
    /// an allocated loopback port per instance instead).
    spec: crate::runtime::publish::Publish,
}

pub fn run(opts: &RunOptions) -> Result<i32> {
    let rootless = !crate::paths::is_root();
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

    // Only apps that need a second uid to exist care about the subid range:
    // a declared [package] user, or an import whose entrypoint will gosu down.
    if rootless {
        let needs_ids =
            ctx.manifest.package.user.is_some() || ctx.manifest.package.capabilities.is_some();
        if needs_ids {
            if let Some(gap) = subid_gap() {
                eprintln!("ply: warning: {gap}");
            }
        }
    }

    match rootless_scale_guard(
        rootless,
        opts.scale,
        !ctx.manifest.ports.is_empty(),
        !opts.publish.is_empty(),
    ) {
        ScaleGuard::Refuse(msg) => return Err(Error::Runtime(msg.into())),
        ScaleGuard::Warn(msg) => eprintln!("ply: warning: {msg}"),
        ScaleGuard::Ok => {}
    }

    // --publish: claim the host port BEFORE anything starts (fail fast on a
    // taken port), then serve the pool from a dedicated accept thread. The
    // pool map follows the instance lifecycle via launch/Drop.
    // Every listener is claimed before anything starts, so a taken port fails
    // fast rather than half-way through a launch.
    let mut publishing: Vec<PublishWiring> = Vec::new();
    for spec in &opts.publish {
        let listener = crate::runtime::publish::bind(*spec, rootless)?;
        let pool = crate::runtime::publish::Pool::new();
        let serve_pool = pool.clone();
        std::thread::spawn(move || crate::runtime::publish::serve(listener, serve_pool));
        eprintln!(
            "ply: publishing {}:{} → {} pool",
            spec.scope.bind_addr(rootless),
            spec.host_port,
            ctx.manifest.package.name,
        );
        publishing.push(PublishWiring { pool, spec: *spec });
    }
    // Rootless instances share the host netns, so they cannot all bind the
    // same port — ply hands each one its own loopback port as PORT. PORT is a
    // single variable, so only the FIRST spec can be satisfied that way; the
    // rest expect the app to bind their instance_port itself (an edge reads
    // its ports from its own config, not from PORT).
    if rootless && opts.publish.len() > 1 && opts.scale > 1 {
        eprintln!(
            "ply: warning: rootless --scale {} with {} published ports — only the first \
             ({}) is injected as PORT; the app must bind the rest itself, and instances \
             will collide if it does",
            opts.scale,
            opts.publish.len(),
            opts.publish[0].instance_port,
        );
    }

    // --after: block until the named apps pass their health gates. Placed
    // after --publish so a taken port still fails fast, before any launch so
    // the wait happens exactly once per parent.
    if !opts.after.is_empty() {
        let me = &ctx.manifest.package.name;
        if opts.after.iter().any(|a| a == me) {
            return Err(Error::Runtime(format!(
                "--after {me}: an app cannot wait for itself"
            )));
        }
        let _waiting = crate::runtime::after::WaitingMarker::write(me, &opts.after)?;
        crate::runtime::after::wait_for(&opts.after, opts.after_timeout)?;

        // Resolved only now: the dependency is up, so its parent has recorded
        // where to reach it. Never overrides a value the author set — an
        // explicit [env] or -e wins, so this can only add.
        for (key, value) in discovery_env(&opts.after) {
            if ctx.env.iter().any(|(k, _)| *k == key) {
                continue;
            }
            eprintln!("ply: {key}={value}");
            ctx.env.push((key, value));
        }
        eprintln!("ply: starting {me}");
    }

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

    // How this app asks to be stopped. Imported images carry their own
    // (nginx SIGQUIT, httpd SIGWINCH); everything else means SIGTERM.
    let stop_signal = match &ctx.manifest.package.stop_signal {
        Some(name) => crate::manifest::parse_stop_signal(name)?,
        None => Signal::SIGTERM,
    };

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
        let instance = launch_instance(&ctx, opts, &store, rootless, None, 0, publishing.as_ref())?;
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
                opts,
                &store,
                rootless,
                Some(slot),
                info.restarts,
                &publishing,
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
                stop_instance(old_instance, stop_signal);

                let restarts = slots.get(&slot).map(|s| s.restarts).unwrap_or(0);
                let outcome = launch_instance(
                    &ctx,
                    opts,
                    &store,
                    rootless,
                    Some(slot),
                    restarts,
                    &publishing,
                )
                .and_then(|instance| {
                    if wait_healthy(&ctx, &instance) {
                        Ok(instance)
                    } else {
                        let app = instance.app.clone();
                        stop_instance(instance, stop_signal);
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
                            opts,
                            &store,
                            rootless,
                            Some(slot),
                            restarts,
                            &publishing,
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
fn stop_instance(instance: Instance, stop_signal: Signal) {
    use nix::sys::wait::WaitPidFlag;
    let child = instance.child;
    // The in-container init forwards whatever it receives, so an image that
    // wants SIGQUIT (nginx) or SIGWINCH (httpd) gets to drain instead of
    // being SIGKILLed when the 10s patience below runs out.
    let _ = signal::kill(child, stop_signal);
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
    /// Published pools this instance is registered in (removed on Drop, so
    /// every stop path — death, roll, shutdown — also stops traffic).
    pools: Vec<crate::runtime::publish::Pool>,
}

impl Drop for Instance {
    fn drop(&mut self) {
        for pool in &self.pools {
            pool.remove(self.n);
        }
        let _ = hosts::remove_entry(&self.app, self.n);
        InstanceState::remove(&self.app, self.n);
        let _ = &self.guard; // unmounts layers, removes instance dir
    }
}

fn launch_instance(
    ctx: &AppContext,
    opts: &RunOptions,
    store: &Store,
    rootless: bool,
    slot: Option<u32>,
    restarts: u32,
    publish: &[PublishWiring],
) -> Result<Instance> {
    let manifest = &ctx.manifest;
    let entrypoint = &ctx.entrypoint;
    let env = &ctx.env;
    let app_image = &ctx.image;
    let dep_images = &ctx.dep_images;
    let app = &manifest.package.name;
    let (instance_dir, n) = allocate_instance(app, slot)?;
    let run_user = manifest
        .package
        .user
        .as_deref()
        .map(crate::manifest::parse_user)
        .transpose()?;

    // Named volumes: per-instance by default (scaling can never silently
    // corrupt single-writer state); `scope = "shared"` is the explicit opt-in.
    let mut binds: Vec<(PathBuf, String)> = Vec::new();
    // Declared volumes only — never --link, whose source is the user's own
    // working tree and must not have its ownership rewritten.
    let mut volume_targets: Vec<String> = Vec::new();
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
        // The app's user must be able to write its own volumes. Rootful this
        // succeeds here; rootless it cannot — an unprivileged parent may not
        // chown to another uid, that needs CAP_CHOWN in the INITIAL namespace.
        // The child repeats it after entering the user namespace, where it is
        // namespace-root over the mapped range, so a failure here is expected
        // and not worth reporting.
        if let Some(user) = &run_user {
            let _ = std::os::unix::fs::chown(&host_dir, Some(user.uid), Some(user.gid));
        }
        volume_targets.push(volume.path.clone());
        binds.push((host_dir, volume.path.clone()));
    }
    for (host, container) in &opts.links {
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
    let keep_caps =
        crate::runtime::security::keep_set(manifest.package.capabilities.as_ref(), keep_net_bind)?;
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
    let mut spec_env = env.to_vec();
    if let Some(user) = &run_user {
        for pair in spec_env.iter_mut() {
            if pair.0 == "HOME" {
                pair.1 = format!("/home/{}", user.name);
            }
        }
    }
    // Published rootless pool: instances share the host netns, so each one
    // gets its own loopback port, injected as PORT (the parent LBs across
    // them). Overrides any manifest/CLI PORT — with --publish the parent
    // owns the externally visible port.
    // Skipped when the spec named the instance port: that is the author saying
    // where the app listens, and an imported image cannot be talked out of it.
    let injected_port = match (publish.first(), rootless) {
        (Some(w), true) if !w.spec.instance_port_explicit => {
            let port = crate::runtime::publish::allocate_loopback_port()?;
            match spec_env.iter_mut().find(|(k, _)| k == "PORT") {
                Some(pair) => pair.1 = port.to_string(),
                None => spec_env.push(("PORT".into(), port.to_string())),
            }
            Some(port)
        }
        _ => None,
    };
    let spec = ContainerSpec {
        layers,
        instance_dir: instance_dir.clone(),
        hostname: app.clone(),
        cwd: manifest
            .package
            .workdir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/opt/{app}"))),
        env: spec_env,
        argv: entrypoint.to_vec(),
        binds,
        sync_rx,
        volume_targets,
        keep_caps,
        privileged: opts.privileged,
        rootless,
        run_user,
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
        // Rootless --publish moves the app: ply injects a per-instance
        // loopback port as PORT, so that — not the manifest's — is where the
        // app actually listens. Probing the manifest's port would knock on a
        // door nobody opened, the instance would never report healthy, and
        // every `--after` dependant would time out.
        health_port: match (injected_port, manifest.health.as_ref().and_then(|h| h.port)) {
            (Some(injected), Some(_)) => Some(injected),
            (_, declared) => declared,
        },
        // The first spec is the app's canonical address — what `--after`
        // hands to dependants and what `ply lb` emits.
        published_port: publish.first().map(|w| w.spec.host_port),
        published_addr: publish.first().map(|w| {
            format!(
                "{}:{}",
                w.spec.scope.connect_addr(rootless),
                w.spec.host_port
            )
        }),
    };
    state.save()?;
    if !rootless {
        hosts::add_entry(app, n, ip)?; // /etc/hosts needs root
    }

    // Release the child.
    let _ = nix::unistd::write(&sync_tx, &[1u8]);
    drop(sync_tx);

    // Join the published pool: rootful backends live on the bridge at the
    // shared instance port; rootless ones on their injected loopback port.
    let pools: Vec<crate::runtime::publish::Pool> = publish
        .iter()
        .enumerate()
        .map(|(i, wiring)| {
            let backend = match injected_port {
                // only the first spec gets the injected loopback port
                Some(port) if i == 0 => std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                _ => std::net::SocketAddr::from((ip, wiring.spec.instance_port)),
            };
            wiring.pool.insert(n, backend);
            wiring.pool.clone()
        })
        .collect();

    Ok(Instance {
        app: app.clone(),
        n,
        child,
        _cgroup: cgroup,
        guard,
        pools,
    })
}

/// `api-server` -> `API_SERVER`: the env-var stem for a dependency's address.
pub fn env_stem(app: &str) -> String {
    app.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Where a depending app should dial each of its `--after` apps.
///
/// `--after` already declares the edge and gates on health, so it is also the
/// honest place to answer "and where is it?". The address comes from the
/// dependency's own run parent (recorded in its instance state), so it is
/// correct per mode — loopback rootless, bridge gateway rootful — instead of
/// being guessed by the author and wrong in the other mode.
///
/// Nothing is injected for an unpublished dependency: it has no address to
/// give, and inventing one would fail later and further away.
pub fn discovery_env(after: &[String]) -> Vec<(String, String)> {
    let states = state::list().unwrap_or_default();
    let mut out = Vec::new();
    for dep in after {
        let Some(found) = states
            .iter()
            .find(|s| &s.app == dep && s.alive() && s.published_addr.is_some())
        else {
            continue;
        };
        let addr = found.published_addr.clone().expect("filtered above");
        let stem = env_stem(dep);
        let host = addr.rsplit_once(':').map(|(h, _)| h.to_string());
        out.push((format!("{stem}_ADDR"), addr));
        if let (Some(host), Some(port)) = (host, found.published_port) {
            out.push((format!("{stem}_HOST"), host));
            out.push((format!("{stem}_PORT"), port.to_string()));
        }
    }
    out
}

/// A `/etc/subuid` or `/etc/subgid` delegation: `<name>:<start>:<count>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubIdRange {
    pub start: u32,
    pub count: u32,
}

/// This user's line in a subid file. Entries are keyed by name, but the
/// numeric id is accepted too — both spellings appear in the wild.
pub fn parse_subid(text: &str, user: &str, id: u32) -> Option<SubIdRange> {
    let id = id.to_string();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split(':').collect();
        if f.len() < 3 || (f[0] != user && f[0] != id) {
            continue;
        }
        if let (Ok(start), Ok(count)) = (f[1].parse::<u32>(), f[2].parse::<u32>()) {
            if count > 0 {
                return Some(SubIdRange { start, count });
            }
        }
    }
    None
}

/// The invoking user's name, read from the host's passwd file.
fn username_for(uid: u32) -> Option<String> {
    let text = std::fs::read_to_string("/etc/passwd").ok()?;
    text.lines()
        .map(|l| l.split(':').collect::<Vec<_>>())
        .find(|f| f.len() > 2 && f[2].parse::<u32>() == Ok(uid))
        .map(|f| f[0].to_string())
}

fn have(tool: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|d| !d.is_empty() && Path::new(d).join(tool).exists())
}

/// Map the invoking user into the child's user namespace.
///
/// Without a delegated subuid range only ONE id exists inside (root = you),
/// and every other uid is unmapped: `chown redis .` and `setuid(70)` both
/// fail with EINVAL, which breaks `[package] user` and every imported image
/// that drops privileges. A `/etc/subuid` range fixes that, but the kernel
/// only lets an unprivileged process write a single-entry map — writing a
/// range needs CAP_SETUID in the parent namespace, which is exactly what the
/// setuid-root `newuidmap`/`newgidmap` helpers are for. Same mechanism
/// rootless podman and docker use.
fn write_id_maps(pid: i32) -> Result<()> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let user = username_for(uid).unwrap_or_default();

    let sub = |file: &str, id: u32| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| parse_subid(&t, &user, id))
    };
    let ranges = match (sub("/etc/subuid", uid), sub("/etc/subgid", gid)) {
        (Some(u), Some(g)) if have("newuidmap") && have("newgidmap") => Some((u, g)),
        _ => None,
    };

    if let Some((u, g)) = ranges {
        // NOT setgroups=deny here: that is irreversible, and gosu/su-exec
        // call setgroups() on their way down to a service user. The helpers
        // are setuid-root, so they can write gid_map without it.
        let helper = |tool: &str, args: [String; 7]| -> Result<()> {
            let out = std::process::Command::new(tool)
                .args(&args)
                .output()
                .map_err(|e| Error::Runtime(format!("{tool}: {e}")))?;
            if !out.status.success() {
                return Err(Error::Runtime(format!(
                    "{tool} {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(())
        };
        // inside 0 -> your uid (1 id), then inside 1.. -> the delegated range
        helper(
            "newuidmap",
            [
                pid.to_string(),
                "0".into(),
                uid.to_string(),
                "1".into(),
                "1".into(),
                u.start.to_string(),
                u.count.to_string(),
            ],
        )?;
        helper(
            "newgidmap",
            [
                pid.to_string(),
                "0".into(),
                gid.to_string(),
                "1".into(),
                "1".into(),
                g.start.to_string(),
                g.count.to_string(),
            ],
        )?;
        return Ok(());
    }

    // Fallback: the single-id map. Everything still runs except apps that
    // need a second uid to exist — say so once, with the fix.
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

/// Why the single-id map is in play, for the one-line warning at startup.
/// None when a delegated range is usable.
pub fn subid_gap() -> Option<&'static str> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let user = username_for(uid).unwrap_or_default();
    let has_range = |file: &str, id: u32| {
        std::fs::read_to_string(file)
            .ok()
            .and_then(|t| parse_subid(&t, &user, id))
            .is_some()
    };
    if !has_range("/etc/subuid", uid) || !has_range("/etc/subgid", gid) {
        return Some(
            "no /etc/subuid+/etc/subgid range for this user — only uid 0 exists inside, so \
             `[package] user` and imported images that drop privileges will fail with EINVAL.\n\
             ply:          fix: sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 $USER",
        );
    }
    if !have("newuidmap") || !have("newgidmap") {
        return Some(
            "newuidmap/newgidmap not installed — a delegated subuid range exists but the kernel \
             will not let an unprivileged process apply it, so only uid 0 exists inside.\n\
             ply:          fix: sudo apt install uidmap   (or: dnf install shadow-utils)",
        );
    }
    None
}

#[cfg(test)]
mod health_port_tests {
    /// The rule `launch_instance` applies when recording an instance's health
    /// endpoint. Rootless `--publish` injects a per-instance loopback port as
    /// PORT, so the app moves; the gate has to move with it.
    fn recorded(injected: Option<u16>, declared: Option<u16>) -> Option<u16> {
        match (injected, declared) {
            (Some(injected), Some(_)) => Some(injected),
            (_, declared) => declared,
        }
    }

    #[test]
    fn the_gate_follows_the_injected_port() {
        // app declares [health] port = 3000, rootless publish moves it to 49001
        assert_eq!(recorded(Some(49001), Some(3000)), Some(49001));
    }

    #[test]
    fn rootful_keeps_the_declared_port() {
        // own netns, no injection — the app really is on 3000
        assert_eq!(recorded(None, Some(3000)), Some(3000));
    }

    #[test]
    fn an_app_without_a_health_gate_still_has_none() {
        // injecting a port must not invent a gate the author never asked for;
        // `alive` remains the bar
        assert_eq!(recorded(Some(49001), None), None);
        assert_eq!(recorded(None, None), None);
    }
}

#[cfg(test)]
mod multi_publish_tests {
    use crate::runtime::publish::{parse_publish, BindScope};

    #[test]
    fn an_edge_can_hold_both_web_ports() {
        let specs: Vec<_> = ["80:80", "443:443"]
            .iter()
            .map(|s| parse_publish(s).unwrap())
            .collect();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].host_port, 80);
        assert_eq!(specs[1].host_port, 443);
        // both public by default: an edge is the one thing that should be
        assert!(specs.iter().all(|s| s.scope == BindScope::Public));
    }

    #[test]
    fn scopes_are_per_spec() {
        // a public web port and a loopback-only admin port on one app
        let public = parse_publish("443:443").unwrap();
        let admin = parse_publish("internal:9090").unwrap();
        assert_eq!(public.scope, BindScope::Public);
        assert_eq!(admin.scope, BindScope::Internal);
    }

    #[test]
    fn the_first_spec_is_the_canonical_address() {
        // what --after hands to dependants and what ply lb emits; adding a
        // metrics port must not silently change where callers are pointed
        let specs: Vec<_> = ["internal:3000", "internal:9090"]
            .iter()
            .map(|s| parse_publish(s).unwrap())
            .collect();
        assert_eq!(specs.first().unwrap().host_port, 3000);
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn env_stems_are_shell_safe() {
        assert_eq!(env_stem("api-server"), "API_SERVER");
        assert_eq!(env_stem("postgres"), "POSTGRES");
        assert_eq!(env_stem("my.app_1"), "MY_APP_1");
    }
}

#[cfg(test)]
mod subid_tests {
    use super::*;

    const SUBUID: &str = "# a comment
iluxa:100000:65536
someoneelse:200000:65536
";

    #[test]
    fn finds_the_range_for_this_user() {
        assert_eq!(
            parse_subid(SUBUID, "iluxa", 1000),
            Some(SubIdRange {
                start: 100000,
                count: 65536
            })
        );
    }

    #[test]
    fn matches_by_numeric_id_too() {
        // some distros write the uid instead of the name
        assert_eq!(
            parse_subid("1000:100000:65536\n", "", 1000),
            Some(SubIdRange {
                start: 100000,
                count: 65536
            })
        );
    }

    #[test]
    fn another_users_delegation_is_not_ours() {
        assert_eq!(parse_subid(SUBUID, "nobody", 65534), None);
    }

    #[test]
    fn junk_lines_are_skipped_not_fatal() {
        let text = "broken\n# comment\nnocolons\na:b:c\niluxa:100000:65536\n";
        assert_eq!(
            parse_subid(text, "iluxa", 1000),
            Some(SubIdRange {
                start: 100000,
                count: 65536
            })
        );
    }

    #[test]
    fn a_zero_width_delegation_is_no_delegation() {
        // a range of 0 ids maps nothing — treat it as absent rather than
        // handing newuidmap an argument it will reject
        assert_eq!(parse_subid("iluxa:100000:0\n", "iluxa", 1000), None);
    }

    #[test]
    fn the_range_covers_the_service_uids_that_matter() {
        let r = parse_subid(SUBUID, "iluxa", 1000).unwrap();
        // redis 999, nginx 101, postgres 70, memcached 11211 all land inside
        for uid in [70u32, 101, 999, 11211] {
            assert!(
                uid >= 1 && uid <= r.count,
                "uid {uid} outside 1..={}",
                r.count
            );
        }
    }
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
            // (unmount any layer mounts first or removal fails on EBUSY)
            if let Ok(entries) = std::fs::read_dir(dir.join("layers")) {
                for entry in entries.filter_map(|e| e.ok()) {
                    mount::unmount_detach(&entry.path());
                }
            }
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
        // --publish makes the parent the listener and gives every rootless
        // instance its own injected loopback PORT — no collision left.
        (_, _, _, true) | (false, _, _, _) | (true, 0 | 1, _, _) => ScaleGuard::Ok,
        (true, _, true, false) => ScaleGuard::Refuse(
            "rootless instances share the host network, so every instance would bind the same declared port (EADDRINUSE for all but the first).\n\
             publish the pool through the parent:  ply run --publish <port> --scale N …\n\
             or run it rootful for per-instance IPs:  sudo ply run --scale N …\n\
             or stay rootless with --scale 1",
        ),
        (true, _, false, false) => ScaleGuard::Warn(
            "rootless instances share the host network — if these instances bind the same port they will collide (per-instance IPs need root, or use --publish)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
