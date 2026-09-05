//! `ply run` — the whole runtime: mount the closure and exec.
//! `--scale N` = N identical processes/cgroups/netns, one shared store.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use nix::sys::signal::{self, Signal};

use crate::env::compose_env;
use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{read_embedded, read_lockfile, read_manifest};
use crate::manifest::{Layer, Manifest};
use crate::runtime::backend::{Backend, InstanceSpec, Launched, NetworkFacts};
use crate::runtime::state::InstanceState;
use crate::runtime::{params_tree, state};
use crate::source::Source;
use crate::store::Store;

#[derive(Clone)]
pub struct RunOptions {
    pub image: PathBuf,
    /// `--name`: override the app's identity (state pool, `.ply` DNS name,
    /// `--after` target, control dir). Defaults to the image's own name.
    /// A deployment passes its file name so two deployments of one image
    /// (two postgres, say) get distinct identities instead of colliding.
    pub name: Option<String>,
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
    /// Names of the other members sharing `network`, so `<name>.ply`
    /// resolves: to loopback inside each container on Linux, to the sibling's
    /// own address on the switch in a microVM.
    pub network_peers: Vec<String>,
    /// The resolver inside `network` — the namespace's user-mode router on
    /// Linux, the switch itself on macOS — when there is one.
    pub network_dns: Option<String>,
    /// The stack's network, which every instance of this run joins. A netns
    /// path (`/proc/<pid>/ns/net`) on Linux, where members bind their own
    /// natural ports and reach each other on loopback, touching no host
    /// port; a `--vswitch` unix socket on macOS, where each member is its
    /// own machine on one userspace L2. `None` keeps the caller's network
    /// (Linux) or gives this run a private switch of its own (macOS).
    pub network: Option<PathBuf>,
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
    /// Replace the image's entrypoint (dev overlays, boot-failure debugging).
    /// The image is untouched — this is an argv swap at spawn time.
    pub entrypoint: Option<Vec<String>>,
    /// `--domain` hostnames for the edge (recorded in instance state; the
    /// proxy watcher renders them into vhost config). TLS is Caddy's job.
    pub domains: Vec<String>,
    /// `--volume`: extra container paths to back with managed, chowned
    /// volumes, added to whatever the manifest declares. For imported apps
    /// whose image doesn't declare a VOLUME but still writes a data dir as a
    /// non-root user (n8n's ~/.n8n).
    pub volumes: Vec<String>,
    /// `--egress` / `--egress-allow`, or a stack member's `egress = …`: the
    /// operator's word over whatever the image's manifest declares. `None`
    /// leaves the manifest's claim (or its absence) to decide.
    pub egress: Option<crate::egress::EgressOverride>,
}

/// Live wiring for a published pool, threaded through instance launches.
struct PublishWiring {
    pool: crate::runtime::publish::Pool,
    /// The parsed spec: the host port and bind scope the parent claimed, plus
    /// the port instances serve on (rootful: on their bridge IPs; rootless:
    /// an allocated loopback port per instance instead).
    spec: crate::runtime::publish::Publish,
}

/// The egress contract: the author's claim, the operator's word.
///
/// Order: off → unsupported (one line, nothing else) → unrestricted warning +
/// start line → start line. `None` means there is nothing for the backend to
/// install — either the effective mode is `off`, or this backend cannot keep
/// a contract on this host, which the operator has just been told in the
/// backend's own words.
///
/// Called once per manifest: at start, and again for the new manifest a
/// deploy brings in (with the same override, which is the operator's word
/// for this run and does not change under it).
fn effective_egress(
    manifest: &Manifest,
    over: Option<&crate::egress::EgressOverride>,
    identity: &str,
    backend: &dyn Backend,
) -> Result<Option<crate::egress::Policy>> {
    let declared = manifest.egress_entries()?;
    let policy = crate::egress::effective(declared.as_deref(), over);
    if policy.mode == crate::egress::Mode::Off {
        return Ok(None);
    }
    if let Some(reason) = backend.egress_support() {
        eprintln!("ply: {reason}");
        return Ok(None);
    }
    if policy.unrestricted() {
        eprintln!("ply: {identity} declares unrestricted egress");
    }
    eprintln!("ply: egress {}", policy.describe());
    Ok(Some(policy))
}

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
    // The app's runtime identity: what its state pool, `.ply` name, control
    // dir, and `--after` matching key on. Defaults to the image's name;
    // `--name` overrides it (so two runs of one image get distinct
    // identities) WITHOUT changing the filesystem prefix `/opt/<name>`.
    let identity = opts
        .name
        .clone()
        .unwrap_or_else(|| ctx.manifest.package.name.clone());

    // What this platform refuses or warns about for this app, before any
    // host port is claimed.
    backend.admit(&ctx.manifest, opts)?;

    // The egress contract: the author's claim, the operator's word. Decided
    // once the manifest is in hand, before anything is launched.
    ctx.egress = effective_egress(
        &ctx.manifest,
        opts.egress.as_ref(),
        &identity,
        backend.as_ref(),
    )?;

    // --publish: claim the host port BEFORE anything starts (fail fast on a
    // taken port), then serve the pool from a dedicated accept thread. The
    // pool map follows the instance lifecycle via launch/Drop.
    // Every listener is claimed before anything starts, so a taken port fails
    // fast rather than half-way through a launch.
    // Order matters. The listeners are claimed HERE, in the caller's
    // network, because that is where the world reaches them — a bound
    // socket keeps working after its process moves. Only then does this
    // process join the stack's namespace, so everything spawned below (the
    // accept threads, and every instance) is already inside it.
    let listeners: Vec<(crate::runtime::publish::Publish, std::net::TcpListener)> = opts
        .publish
        .iter()
        .map(|spec| crate::runtime::publish::bind(*spec, facts.loopback).map(|l| (*spec, l)))
        .collect::<Result<Vec<_>>>()?;
    backend.attach(opts)?;
    let net = backend.network(opts);

    let mut publishing: Vec<PublishWiring> = Vec::new();
    for (spec, listener) in listeners {
        let spec = &spec;
        let pool = crate::runtime::publish::Pool::new();
        let serve_pool = pool.clone();
        // The listener was bound out there; the instances live in here.
        let same_network = opts.network.is_none();
        std::thread::spawn(move || {
            crate::runtime::publish::serve(listener, serve_pool, same_network)
        });
        // Rootful Linux hands the byte-moving to the kernel (DNAT that follows
        // the pool); the listener above then only sees traffic the kernel did
        // not claim — loopback, and an empty pool. Everywhere else it relays.
        let kernel = same_network
            .then(|| backend.kernel_publish(spec))
            .flatten()
            .map(|mirror| pool.mirror(mirror))
            .is_some();
        eprintln!(
            "ply: publishing {}:{} → {} pool{}",
            spec.scope.bind_addr(facts.loopback),
            spec.host_port,
            ctx.manifest.package.name,
            if kernel { " (kernel dnat)" } else { "" },
        );
        publishing.push(PublishWiring { pool, spec: *spec });
    }
    // Every instance of a rootless run shares that run's ONE namespace, so
    // they cannot all bind the same port — ply hands each one its own loopback
    // port as PORT. PORT is a single variable, so only the FIRST spec can be
    // satisfied that way; the
    // rest expect the app to bind their instance_port itself (an edge reads
    // its ports from its own config, not from PORT).
    if !net.alone && opts.publish.len() > 1 && opts.scale > 1 {
        eprintln!(
            "ply: warning: rootless --scale {} with {} published ports — only the first \
             ({}) is injected as PORT; the app must bind the rest itself, and instances \
             will collide if it does",
            opts.scale,
            opts.publish.len(),
            opts.publish[0].instance_port,
        );
    }
    // Say it when a computed PORT is about to beat one the operator SET.
    // Everywhere else in this runtime an explicit value wins (`HOME`/`TERM`
    // use or_insert; `--after` discovery skips keys already present), so the
    // one place that overrides has to be loud about it. Scale is exactly when
    // nobody is watching a single instance's environment.
    if let Some(first) = opts.publish.first() {
        let injecting = !net.alone && !first.instance_port_explicit;
        if injecting {
            if let Some((_, set)) = ctx.env.iter().find(|(k, _)| k == "PORT") {
                eprintln!(
                    "ply: warning: PORT={set} is being overridden — {} instance(s) share this \
                     run's namespace and cannot all bind {set}, so each gets its own loopback \
                     port. To keep {set}, publish it explicitly: --publish {}:{set}",
                    opts.scale, first.host_port,
                );
            }
        }
    }

    // --after: block until the named conditions hold. Placed after
    // --publish so a taken port still fails fast, before any launch so the
    // wait happens exactly once per parent. Parsed up front so a malformed
    // condition fails fast, before the WaitingMarker (and any waiting) — a
    // bad --after should never look like a hang.
    if !opts.after.is_empty() {
        let me = &identity;
        let waits: Vec<crate::runtime::after::Wait> = opts
            .after
            .iter()
            .map(|s| crate::runtime::after::parse_wait(s))
            .collect::<Result<_>>()?;
        // Distinct app names, first-seen order: a bare `a` and a conditional
        // `a.x == 'y'` on the same app are two gates on one WaitingMarker
        // entry / one discovery lookup, not two.
        let mut wait_apps: Vec<String> = Vec::new();
        for w in &waits {
            if !wait_apps.contains(&w.app) {
                wait_apps.push(w.app.clone());
            }
        }
        if wait_apps.iter().any(|a| a == me) {
            return Err(Error::Runtime(format!(
                "--after {me}: an app cannot wait for itself"
            )));
        }
        let _waiting = crate::runtime::after::WaitingMarker::write(me, &wait_apps)?;
        crate::runtime::after::wait_for(&waits, opts.after_timeout)?;

        // Resolved only now: the dependency is up, so its parent has recorded
        // where to reach it. Never overrides a value the author set — an
        // explicit [env] or -e wins, so this can only add.
        let in_stack_network = net.in_stack_network;
        for (key, value) in discovery_env(&wait_apps, in_stack_network) {
            if ctx.env.iter().any(|(k, _)| *k == key) {
                continue;
            }
            eprintln!("ply: {key}={value}");
            ctx.env.push((key, value));
        }
        eprintln!("ply: starting {me}");
    }

    let _ = state::reap_stale(); // free IPs/dirs leaked by killed runs

    // Same app already running? That's legal (canary: old + new side by
    // side) — but say so, and point at deploy for the replace case.
    let already_running = state::list()?
        .iter()
        .filter(|s| s.app == identity && s.alive())
        .count();
    if already_running > 0 {
        eprintln!(
            "ply: note: {identity} already has {already_running} running instance(s) — this run ADDS instances (canary).\n\
             ply:       to replace the running version instead: ply deploy {}",
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
    // The handler installed above stops instances too — it must ask for the
    // same thing a rolling deploy asks for, or `systemctl stop` means
    // something the app never agreed to.
    STOP_SIGNAL.store(stop_signal as i32, Ordering::SeqCst);

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

    // Shutdown bookkeeping — see SHUTDOWN_GRACE.
    let mut shutdown_began: Option<std::time::Instant> = None;
    let mut escalated = false;

    let mut instances: Vec<Running> = Vec::new();
    for _ in 0..opts.scale.max(1) {
        let instance = launch_instance(
            backend.as_ref(),
            &ctx,
            opts,
            &net,
            None,
            0,
            publishing.as_ref(),
        )?;
        let sig_idx = register_child(instance.inner.child_pid().unwrap_or(0));
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
    // Control dir (commands as files): polled on a 2s cadence.
    let mut last_control_poll = std::time::Instant::now();

    // Reap, respawn per policy, exit when nothing is left to wait for.
    let mut exit_code = 0;
    loop {
        // Collect every instance death that's already happened.
        let mut deaths: Vec<(u32, i32, bool)> = Vec::new(); // (slot, code, failed)
        for running in instances.iter_mut() {
            match running.inner.try_wait() {
                Ok(Some(code)) => deaths.push((running.n, code, code != 0)),
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }

        let shutting_down = SHUTTING_DOWN.load(Ordering::SeqCst);

        // Reach the instances the signal HANDLER could not — a microVM has
        // no pid to `kill`, so this loop is the only way in. See
        // `supervise::request_stop` for what that costs when it is missing.
        //
        // Once, on the first observation of the shutdown: `shutdown_began` is
        // set by the `get_or_insert_with` just below, so `is_none()` is true
        // exactly one lap. Sending the stop signal twice to an app that asked
        // for one is not what `ply stop` promises.
        if shutting_down && shutdown_began.is_none() && !instances.is_empty() {
            let waiting: Vec<&dyn crate::runtime::backend::Instance> =
                instances.iter().map(|r| r.inner.as_ref()).collect();
            crate::runtime::supervise::request_stop(&waiting, stop_signal);
        }

        // A stop has to END. The container's PID 1 is the app's own
        // entrypoint, and the kernel drops default-action signals to PID 1 —
        // a plain `sh run.sh` with no SIGTERM handler ignores the polite
        // request completely. Waiting for it forever is not patience: it
        // hands the decision to systemd, which kills only the supervisor
        // (the instance is in its own cgroup) and leaves the instance
        // running, holding its slot, so the replacement takes the next slot
        // and a different volume.
        if shutting_down && !escalated && !instances.is_empty() {
            let began = *shutdown_began.get_or_insert_with(std::time::Instant::now);
            if began.elapsed() >= SHUTDOWN_GRACE {
                escalated = true;
                eprintln!(
                    "ply: {identity} did not stop within {}s — SIGKILL",
                    SHUTDOWN_GRACE.as_secs(),
                );
                for instance in &instances {
                    let _ = instance.inner.signal(Signal::SIGKILL);
                }
            }
        }

        for (slot, code, failed) in deaths {
            let Some(pos) = instances.iter().position(|i| i.n == slot) else {
                continue;
            };
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
            // Live params tree: read-modify-write so the published count
            // stays independently correct even if this call site's own
            // bookkeeping ever drifts from what launch last published.
            let tree_restarts: u32 = params_tree::read(&identity, "restarts")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if let Err(e) =
                params_tree::publish(&identity, "restarts", &(tree_restarts + 1).to_string())
            {
                eprintln!("ply: warning: params tree {identity}/restarts: {e}");
            }
            eprintln!(
                "ply: restarting {}.{slot} (restart #{}, policy {})",
                ctx.manifest.package.name,
                info.restarts,
                restart_policy
                    .as_ref()
                    .map(|r| r.policy.as_str())
                    .unwrap_or("?"),
            );
            crate::runtime::events::emit(
                &ctx.manifest.package.name,
                "instance-restart",
                &format!(
                    "{}.{slot} respawned (restart #{})",
                    ctx.manifest.package.name, info.restarts
                ),
            );
            match launch_instance(
                backend.as_ref(),
                &ctx,
                opts,
                &net,
                Some(slot),
                info.restarts,
                &publishing,
            ) {
                Ok(instance) => {
                    update_child(info.sig_idx, instance.inner.child_pid().unwrap_or(0));
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
                    match prepare_app(
                        &new_image,
                        &opts.cli_env,
                        opts.allow_insecure,
                        opts.entrypoint.as_deref(),
                        &store,
                        RunFacts {
                            name_override: Some(identity.as_str()),
                            host_available: facts.own_addresses,
                            port: opts.publish.first().map(|p| p.instance_port),
                            scale: opts.scale,
                        },
                    ) {
                        // The new version's manifest may declare a different
                        // list; the operator's override is this run's word
                        // and does not change under it.
                        Ok(mut new_ctx) => match effective_egress(
                            &new_ctx.manifest,
                            opts.egress.as_ref(),
                            &identity,
                            backend.as_ref(),
                        ) {
                            Ok(policy) => {
                                new_ctx.egress = policy;
                                let mut queue: Vec<u32> = instances.iter().map(|i| i.n).collect();
                                queue.sort_unstable();
                                roll_queue = queue;
                                old_ctx = Some(std::mem::replace(&mut ctx, new_ctx));
                            }
                            Err(e) => {
                                eprintln!("ply: deploy aborted — new image unusable: {e}")
                            }
                        },
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

        // Control dir: consume command files (scale, restart) and notice a
        // deploy pointer written without a SIGHUP. Same 2s cadence as the
        // proxy watcher; the parent is already awake every 150ms.
        if !shutting_down && last_control_poll.elapsed() >= std::time::Duration::from_secs(2) {
            last_control_poll = std::time::Instant::now();
            let app_name = identity.clone();
            // a pointer file alone is a deploy request (file-only protocol)
            if crate::paths::apps_dir()
                .join(&app_name)
                .join("next-image")
                .exists()
            {
                RELOAD_REQUESTED.store(true, Ordering::SeqCst);
            }
            for command in crate::runtime::control::poll(&app_name) {
                match command {
                    crate::runtime::control::Command::Scale(target) => {
                        let target = target as usize;
                        let current = slots.len();
                        if target > current {
                            let mut grown = 0;
                            for _ in current..target {
                                match launch_instance(
                                    backend.as_ref(),
                                    &ctx,
                                    opts,
                                    &net,
                                    None,
                                    0,
                                    &publishing,
                                ) {
                                    Ok(instance) => {
                                        let sig_idx =
                                            register_child(instance.inner.child_pid().unwrap_or(0));
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
                                        grown += 1;
                                    }
                                    Err(e) => {
                                        eprintln!("ply: scale up failed: {e}");
                                        crate::runtime::control::write_result(
                                            &app_name,
                                            "scale",
                                            false,
                                            &format!("{e}"),
                                        );
                                        break;
                                    }
                                }
                            }
                            if grown == target - current {
                                eprintln!("ply: scaled {app_name} to {target}");
                                crate::runtime::control::write_result(
                                    &app_name,
                                    "scale",
                                    true,
                                    &format!("{current} -> {target}"),
                                );
                                crate::runtime::events::emit(
                                    &app_name,
                                    "scale",
                                    &format!("{current} -> {target}"),
                                );
                            }
                        } else if target < current {
                            // stop the highest slots; forget them so nothing respawns
                            let mut extras: Vec<u32> = slots.keys().copied().collect();
                            extras.sort_unstable();
                            for slot in extras.into_iter().rev().take(current - target) {
                                if let Some(pos) = instances.iter().position(|i| i.n == slot) {
                                    let instance = instances.remove(pos);
                                    if let Some(info) = slots.get(&slot) {
                                        update_child(info.sig_idx, 0);
                                    }
                                    stop_instance(instance, stop_signal);
                                }
                                slots.remove(&slot);
                                pending.retain(|(n, _)| *n != slot);
                                roll_queue.retain(|n| *n != slot);
                            }
                            eprintln!("ply: scaled {app_name} to {target}");
                            crate::runtime::control::write_result(
                                &app_name,
                                "scale",
                                true,
                                &format!("{current} -> {target}"),
                            );
                            crate::runtime::events::emit(
                                &app_name,
                                "scale",
                                &format!("{current} -> {target}"),
                            );
                        } else {
                            crate::runtime::control::write_result(
                                &app_name,
                                "scale",
                                true,
                                &format!("already at {target}"),
                            );
                        }
                    }
                    crate::runtime::control::Command::Restart => {
                        if roll_queue.is_empty() {
                            let mut queue: Vec<u32> = instances.iter().map(|i| i.n).collect();
                            queue.sort_unstable();
                            eprintln!("ply: rolling restart of {app_name} ({} slots)", queue.len());
                            crate::runtime::events::emit(
                                &app_name,
                                "restart",
                                &format!("rolling restart ({} slots)", queue.len()),
                            );
                            roll_queue = queue;
                            crate::runtime::control::write_result(
                                &app_name,
                                "restart",
                                true,
                                "rolling restart started",
                            );
                        } else {
                            crate::runtime::control::write_result(
                                &app_name,
                                "restart",
                                false,
                                "a roll is already in progress",
                            );
                        }
                    }
                    crate::runtime::control::Command::Exec { slot, nonce } => {
                        if !slots.contains_key(&slot) {
                            crate::runtime::control::write_result(
                                &app_name,
                                "exec",
                                false,
                                &format!("no instance .{slot}"),
                            );
                        } else {
                            match backend.terminal(&app_name, slot, &nonce) {
                                Ok(()) => {
                                    crate::runtime::events::emit(
                                        &app_name,
                                        "terminal",
                                        &format!("shell opened into {app_name}.{slot}"),
                                    );
                                    crate::runtime::control::write_result(
                                        &app_name,
                                        "exec",
                                        true,
                                        &format!("terminal serving at term-{nonce}.sock"),
                                    );
                                }
                                Err(e) => crate::runtime::control::write_result(
                                    &app_name,
                                    "exec",
                                    false,
                                    &e.to_string(),
                                ),
                            }
                        }
                    }
                }
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
                    backend.as_ref(),
                    &ctx,
                    opts,
                    &net,
                    Some(slot),
                    restarts,
                    &publishing,
                )
                .and_then(|mut instance| {
                    if wait_healthy(&ctx, &instance) {
                        // Healthy means accepting: seat it now rather than a
                        // loop turn later, so the roll never runs one short.
                        instance.membership.join(instance.n);
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
                            update_child(info.sig_idx, instance.inner.child_pid().unwrap_or(0));
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
                            backend.as_ref(),
                            &ctx,
                            opts,
                            &net,
                            Some(slot),
                            restarts,
                            &publishing,
                        ) {
                            Ok(instance) => {
                                if let Some(info) = slots.get_mut(&slot) {
                                    update_child(
                                        info.sig_idx,
                                        instance.inner.child_pid().unwrap_or(0),
                                    );
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

        // Seat every instance that has started accepting connections since
        // the last turn; until then it takes no traffic.
        for instance in instances.iter_mut() {
            if !instance.membership.joined()
                && instance
                    .membership
                    .ready(std::time::Duration::from_millis(100))
            {
                instance.membership.join(instance.n);
            }
        }

        if instances.is_empty() && pending.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    drop(instances); // unmount, remove state + hosts entries
    for wiring in &publishing {
        wiring.pool.teardown_mirror(); // the kernel's DNAT chains go with the parent
    }

    // The app's final stop: this parent's last instance is gone (the loop
    // above only ever exits once `instances` is empty and staying empty).
    // A canary — another `ply run` of this same identity — may still be
    // alive though, sharing this app's params tree node; only tear it down
    // once no instance of `identity` is left anywhere on the host.
    if !state::list()
        .unwrap_or_default()
        .iter()
        .any(|s| s.app == identity && s.alive())
    {
        params_tree::remove_app(&identity);
    }
    Ok(exit_code)
}

/// Deliberate stop: the declared signal, up to 10s to comply, then KILL.
/// Reaps the instance so its death never reaches the policy loop.
/// How long a retiring instance keeps serving the connections it already
/// has before it is told to stop — new ones stopped arriving at the start of
/// the window.
const DRAIN: std::time::Duration = std::time::Duration::from_secs(1);

fn stop_instance(mut instance: Running, stop_signal: Signal) {
    let Running {
        membership,
        n,
        inner,
        ..
    } = &mut instance;
    // Out of every pool first — relay and kernel alike stop feeding it —
    // then a moment for in-flight requests, then the signal. Without this
    // order a roll drops the requests that land between signal and Drop.
    let pools = membership.pools();
    crate::runtime::publish::drain_then(&pools, *n, DRAIN, || {
        // The in-container init forwards whatever it receives, so an image
        // that wants SIGQUIT (nginx) or SIGWINCH (httpd) gets to drain
        // instead of being SIGKILLed when the 10s patience runs out.
        crate::runtime::supervise::stop_with_patience(
            inner.as_mut(),
            stop_signal,
            std::time::Duration::from_secs(10),
            std::time::Duration::from_millis(100),
        );
    });
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

    // Live params tree: state=healthy on pass, state=unhealthy on fail. A
    // write failure here must not turn a real health result into a launch
    // failure, so it's logged and swallowed rather than propagated.
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
            eprintln!(
                "ply: health gate: {}.{} died during grace",
                instance.app, instance.n
            );
            publish_state("unhealthy");
            false
        }
        // Only reachable with a declared port, so `unwrap_or(0)` never prints.
        Health::NoAnswer(last_err) => {
            eprintln!(
                "ply: health gate: no answer on {}:{} within {grace:?} — last error: {}",
                instance.inner.ip(),
                port.unwrap_or(0),
                last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "none".into()),
            );
            publish_state("unhealthy");
            false
        }
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
    /// The effective egress contract for this version of the app, decided by
    /// the supervisor right after the manifest is read (see
    /// [`effective_egress`]) and handed to every instance it launches. Rides
    /// here rather than as another `launch_instance` argument because it
    /// changes with the manifest: a deploy re-reads it with the same
    /// override.
    pub egress: Option<crate::egress::Policy>,
}

/// Resolve a standalone app's own manifest `[env]` holes — the one thing
/// `ply up`/`ply reconcile` already do (via `stack::resolve_stack`, handed
/// to the child as `-e` overrides) that a bare `ply run` used to skip
/// entirely: `compose_env` copies `manifest.env` verbatim, so without this
/// the child saw the literal string `{user}` instead of a keg's declared
/// default.
///
/// Pure: no I/O, no minting, no root. `manifest.params.is_none()`
/// short-circuits to an empty map before touching the resolver at all — the
/// additive guarantee (a manifest without `[params]` is never even looked
/// at here). Otherwise builds a single-member `stack::MemberInput` (no
/// stack, no cross-member refs — `env`/`params`/`after` empty) and resolves
/// it through `stack::resolve_stack_for_run` with `secrets: None,
/// plan_only: true`, so a declared secret resolves to the tainted
/// `"(will mint)"` placeholder rather than erroring or minting.
///
/// For each of the resolver's self-config entries: a key `cli_env` already
/// sets is skipped — the operator's `-e` wins and the hole is never seen,
/// not even to check its taint. Otherwise a secret-tainted value is a hard
/// error naming the remedy (never minted, never written to disk, the value
/// itself never appears in the message) — a bare `ply run` has nowhere
/// durable to keep a secret the way `ply up` does (`secrets/` in the stack
/// dir). Anything else is returned for the caller to merge over the
/// manifest's own `[env]` (non-hole values are never produced here, so they
/// stay untouched either way).
fn resolve_manifest_env(
    name: &str,
    manifest: &Manifest,
    host_available: bool,
    port: Option<u16>,
    scale: u32,
    image: Option<String>,
    cli_env: &[(String, String)],
) -> Result<std::collections::BTreeMap<String, String>> {
    if manifest.params.is_none() {
        return Ok(std::collections::BTreeMap::new());
    }
    // `RunFacts.port` is the container-side port of the first `--publish`,
    // decided before the manifest was read. With nothing published, the
    // `{port}` fact falls back to the app's own single labelled port — the
    // same fallback the stack resolver applies (`stack::manifest_port`), so
    // a keg reads the same standalone as it does in a stack.
    let port = port.or_else(|| crate::stack::manifest_port(manifest));
    let input = crate::stack::MemberInput {
        name: name.to_string(),
        version: Some(manifest.package.version.to_string()),
        manifest: Some(manifest.clone()),
        env: Vec::new(),
        params: std::collections::BTreeMap::new(),
        after: Vec::new(),
        publish: Vec::new(),
        domain: Vec::new(),
        port,
        scale: Some(scale),
        image,
    };
    let resolution = crate::stack::resolve_stack_for_run(&input, host_available)?;

    let cli_keys: std::collections::BTreeSet<&str> =
        cli_env.iter().map(|(k, _)| k.as_str()).collect();
    let mut out = std::collections::BTreeMap::new();
    for entry in resolution.env.get(name).into_iter().flatten() {
        if cli_keys.contains(entry.key.as_str()) {
            continue; // the operator's -e wins; the hole is never seen
        }
        if entry.secret {
            let key = &entry.key;
            return Err(Error::Runtime(format!(
                "{name}: [env] {key} reads a secret param — pass -e {key}=… , or run it \
                 from a stack (ply up mints secrets)"
            )));
        }
        out.insert(entry.key.clone(), entry.value.clone());
    }
    Ok(out)
}

/// The facts a standalone run's own `[params]`/`[env]` resolution needs
/// that a stack member already carries elsewhere (see
/// `resolve_manifest_env`): the run's identity (`--name`, else the
/// manifest's own name — resolved once the manifest is loaded), whether
/// `{host}`/`{addr}`/`{base_url}` can resolve at all (rootful only), the
/// container-side port of the first `--publish`, and `--scale`. Grouped so
/// `prepare_app` stays under clippy's argument-count lint.
struct RunFacts<'a> {
    name_override: Option<&'a str>,
    host_available: bool,
    port: Option<u16>,
    scale: u32,
}

/// The pre-launch phase: read manifest + lockfile, fetch missing store
/// digests, enforce host policy, compose env, record the app (GC roots).
fn prepare_app(
    image: &Path,
    cli_env: &[(String, String)],
    allow_insecure: bool,
    entrypoint_override: Option<&[String]>,
    store: &Store,
    facts: RunFacts,
) -> Result<AppContext> {
    let manifest = read_manifest(image)?;
    let entrypoint = match entrypoint_override {
        Some(argv) => argv.to_vec(),
        None => manifest.package.entrypoint.clone().ok_or_else(|| {
            Error::Runtime(format!(
                "{} is a library/runtime package (no entrypoint) — only app images run",
                image.display()
            ))
        })?,
    };
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
    // A keg's own hole-y [env] (e.g. `POSTGRES_USER = "{user}"`) is only
    // resolved here for a STANDALONE run — `ply up`/`ply reconcile` already
    // did it and hand every self-config key through as a `-e`, which
    // `resolve_manifest_env` sees in `cli_env` and skips re-resolving.
    // `self_env.is_empty()` whenever there's nothing to add (no [params], or
    // [params] but no hole-y [env] value) — take the exact old call in that
    // case so an untouched manifest composes byte-for-byte as before.
    let name = facts
        .name_override
        .unwrap_or(manifest.package.name.as_str());
    let self_env = resolve_manifest_env(
        name,
        &manifest,
        facts.host_available,
        facts.port,
        facts.scale,
        Some(image.display().to_string()),
        cli_env,
    )?;
    let mut env = if self_env.is_empty() {
        compose_env(&layer_refs, &manifest.env, cli_env)
    } else {
        let mut manifest_env = manifest.env.clone();
        manifest_env.extend(self_env);
        compose_env(&layer_refs, &manifest_env, cli_env)
    };
    env.entry("HOME".into()).or_insert("/root".into());
    if let Ok(term) = std::env::var("TERM") {
        env.entry("TERM".into()).or_insert(term);
    }

    Ok(AppContext {
        entrypoint,
        env: env.into_iter().collect(),
        // symlinks resolved: instance state must record the file that is
        // actually running, not a pointer that may move under it
        image: std::fs::canonicalize(image).unwrap_or_else(|_| image.to_path_buf()),
        dep_images,
        manifest,
        // Decided by the caller, which knows the override and the host's
        // facts; `prepare_app` only reads the image.
        egress: None,
    })
}

#[cfg(test)]
mod resolve_manifest_env_tests {
    use super::*;
    use std::collections::BTreeMap;

    const DEFAULTED_MANIFEST: &str = r#"
[package]
name = "postgres"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
user = "postgres"

[env]
POSTGRES_USER = "{user}"
"#;

    const SECRET_MANIFEST: &str = r#"
[package]
name = "postgres"
version = "17.0.0"
entrypoint = ["postgres"]

[params]
password = { secret = true }

[env]
POSTGRES_PASSWORD = "{password}"
"#;

    const NO_PARAMS_MANIFEST: &str = r#"
[package]
name = "app"
version = "1.0.0"
entrypoint = ["app"]

[env]
X = "{not a hole}"
"#;

    fn manifest(text: &str) -> Manifest {
        Manifest::parse(text).unwrap()
    }

    /// Mirrors what `prepare_app` does right before `compose_env`: resolve
    /// the manifest's own self-config holes, merge them over the manifest's
    /// raw `[env]` (non-hole values untouched), and compose — all pure, no
    /// layers, no store, no container.
    fn composed(
        manifest: &Manifest,
        cli_env: &[(String, String)],
    ) -> Result<BTreeMap<String, String>> {
        composed_with(manifest, true, cli_env)
    }

    /// [`composed`], with the run mode spelled out: `host_available: false`
    /// is a rootless run, where `<name>.ply` resolves nowhere.
    fn composed_with(
        manifest: &Manifest,
        host_available: bool,
        cli_env: &[(String, String)],
    ) -> Result<BTreeMap<String, String>> {
        let self_env = resolve_manifest_env(
            &manifest.package.name.clone(),
            manifest,
            host_available,
            None,
            1,
            None,
            cli_env,
        )?;
        let mut merged = manifest.env.clone();
        merged.extend(self_env);
        Ok(compose_env(&[], &merged, cli_env))
    }

    #[test]
    fn standalone_run_resolves_manifest_env_holes_from_declared_defaults() {
        let m = manifest(DEFAULTED_MANIFEST);
        let env = composed(&m, &[]).unwrap();
        assert_eq!(env["POSTGRES_USER"], "postgres");
    }

    #[test]
    fn a_cli_override_wins_and_skips_resolution() {
        let m = manifest(DEFAULTED_MANIFEST);
        let cli = vec![("POSTGRES_USER".to_string(), "admin".to_string())];
        let env = composed(&m, &cli).unwrap();
        assert_eq!(env["POSTGRES_USER"], "admin");
    }

    #[test]
    fn an_unoverridden_secret_backed_env_value_is_a_hard_error() {
        let m = manifest(SECRET_MANIFEST);
        let err = composed(&m, &[]).unwrap_err().to_string();
        assert!(err.contains("pass -e POSTGRES_PASSWORD="), "{err}");
        assert!(!err.contains("(will mint)"), "never print a secret: {err}");

        let cli = vec![("POSTGRES_PASSWORD".to_string(), "dev".to_string())];
        let env = composed(&m, &cli).unwrap();
        assert_eq!(env["POSTGRES_PASSWORD"], "dev");
    }

    #[test]
    fn a_manifest_without_params_is_untouched() {
        let m = manifest(NO_PARAMS_MANIFEST);
        let env = composed(&m, &[]).unwrap();
        assert_eq!(env["X"], "{not a hole}");
    }

    /// Ruling: `{host}` is `Some("<name>.ply")` for a rootful standalone run
    /// but `None` for a rootless one (no stack netns, no `/etc/hosts`
    /// entry) — so a manifest that references it resolves under root and
    /// names the gap otherwise, the same "publishes no port" shape the
    /// resolver already uses for a missing `{port}`.
    #[test]
    fn the_host_fact_depends_on_whether_the_run_is_rootful() {
        let m = manifest(
            r#"
[package]
name = "app"
version = "1.0.0"
entrypoint = ["app"]

[params]
hostname = "{host}"

[env]
SELF_HOST = "{hostname}"
"#,
        );
        let rootful = resolve_manifest_env("app", &m, true, None, 1, None, &[]).unwrap();
        assert_eq!(rootful["SELF_HOST"], "app.ply");

        let err = resolve_manifest_env("app", &m, false, None, 1, None, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `app.ply` address"), "{err}");
    }

    /// C1: `ply run redis@8` shape — a keg whose `[params]` declares a
    /// computed `url` reading `{host}`/`{port}`, run rootless with no
    /// `--publish`. Nothing in `[env]` reads `url`, so it must not fail;
    /// `{port}` falls back to the manifest's single `[ports]` entry.
    #[test]
    fn a_computed_param_no_env_value_reads_never_blocks_a_rootless_run() {
        let m = manifest(
            r#"
[package]
name = "redis"
version = "8.0.0"
entrypoint = ["redis-server"]

[ports]
db = 6379

[params]
listen = "6379"
url = "x://{host}:{port}"

[env]
REDIS_PORT = "{listen}"
PORT_FACT = "{port}"
"#,
        );
        let env = resolve_manifest_env("redis", &m, false, None, 1, None, &[]).unwrap();
        assert_eq!(env["REDIS_PORT"], "6379");
        assert_eq!(env["PORT_FACT"], "6379", "{{port}} falls back to [ports]");
    }

    /// C1: `ply run postgres@17 -e POSTGRES_PASSWORD=dev` shape — the keg's
    /// own manifest, rootless, nothing published. `url` (which reads
    /// `{host}`) is never referenced by `[env]`, so the defaults resolve and
    /// the operator's `-e` covers the one secret-backed key.
    #[test]
    fn a_postgres_shaped_keg_runs_rootless_with_only_the_password_supplied() {
        let m = manifest(
            r#"
[package]
name = "postgres"
version = "17.0.0"
entrypoint = ["postgres"]

[ports]
db = 5432

[params]
user = "postgres"
database = "postgres"
password = { secret = true }
url = "postgres://{user}:{password}@{host}:{port}/{database}"

[env]
POSTGRES_USER = "{user}"
POSTGRES_DB = "{database}"
POSTGRES_PASSWORD = "{password}"
"#,
        );
        let cli = vec![("POSTGRES_PASSWORD".to_string(), "dev".to_string())];
        let env = composed_with(&m, false, &cli).unwrap();
        assert_eq!(env["POSTGRES_USER"], "postgres");
        assert_eq!(env["POSTGRES_DB"], "postgres");
        assert_eq!(env["POSTGRES_PASSWORD"], "dev");
    }
}

/// One instance as the supervisor holds it: the backend's handle plus the
/// host-side bookkeeping that is the same on every platform.
struct Running {
    app: String,
    n: u32,
    inner: Box<dyn crate::runtime::backend::Instance>,
    /// This instance's seat in the published pools: taken by the run loop
    /// once the instance accepts connections, given back on Drop, so every
    /// stop path — death, roll, shutdown — also stops traffic.
    membership: crate::runtime::publish::Membership,
}

/// How many instances of `app` are alive right now, straight from the state
/// pool — the `instances` live fact, and the "is anything of this app left?"
/// gate `Drop for Running` reads. `alive()` filtered: a state file outlives
/// the process it describes until its parent reaps it.
fn live_instance_count(app: &str) -> usize {
    state::list()
        .unwrap_or_default()
        .iter()
        .filter(|s| s.app == app && s.alive())
        .count()
}

impl Drop for Running {
    fn drop(&mut self) {
        self.membership.leave(self.n);
        InstanceState::remove(&self.app, self.n);
        // Live params tree: state=stopped once nothing of this app is left
        // running. InstanceState::remove just above already dropped this
        // instance's own state file, so state::list() here reflects every
        // OTHER instance — the same idiom as the final-stop `remove_app`
        // guard at the end of `run()`. Without this gate, scaling down N
        // of N+1 instances (or a crashed instance under `restart: no`
        // while siblings still serve) would stomp a still-healthy app's
        // published state to "stopped" permanently, since nothing
        // downstream would ever republish it. Best-effort either way.
        let alive = live_instance_count(&self.app);
        if alive == 0 {
            if let Err(e) = params_tree::publish(&self.app, "state", "stopped") {
                eprintln!("ply: warning: params tree {}/state: {e}", self.app);
            }
        }
        // `instances` is published at launch as "alive + this one"; nothing
        // else decrements it, so a scale-down (or a crash) would leave a
        // stale high-water mark forever. Republish the remaining count here
        // — best-effort, same guard idiom as the state write above.
        if let Err(e) = params_tree::publish(&self.app, "instances", &alive.to_string()) {
            eprintln!("ply: warning: params tree {}/instances: {e}", self.app);
        }
        // `inner` drops after this body: the backend's own teardown —
        // the hosts entry, the layer unmounts, the instance directory.
    }
}

/// A stable, filesystem-safe volume name from a container path
/// (`/home/node/.n8n` -> `n8n`), for CLI-added volumes.
fn volume_name_from_path(path: &str) -> String {
    let base = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_start_matches('.');
    let name: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() {
        "data".into()
    } else {
        name
    }
}

/// Other instances' copies of this volume that already hold data.
///
/// A per-instance volume belongs to its instance number, so a second run of
/// an app gets `data.2` while `data.1` keeps the bytes. That is right for a
/// scaled app and catastrophic for a single database: it comes up empty,
/// serves fine, and the only symptom is that everything it knew is gone.
/// Nothing here changes what happens — it changes whether anyone is told.
fn populated_siblings(app_dir: &Path, name: &str, suffix: &str) -> Vec<String> {
    let mut found = Vec::new();
    let prefix = format!("{name}.");
    let Ok(entries) = std::fs::read_dir(app_dir) else {
        return found;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let file_name = entry.file_name();
        let Some(dir_name) = file_name.to_str() else {
            continue;
        };
        let Some(rest) = dir_name.strip_prefix(&prefix) else {
            continue;
        };
        if rest == suffix {
            continue;
        }
        let has_data = std::fs::read_dir(entry.path())
            .map(|mut e| e.next().is_some())
            .unwrap_or(false);
        if has_data {
            found.push(dir_name.to_string());
        }
    }
    found.sort();
    found
}

/// Removes the instance directory unless disarmed.
///
/// `allocate_instance` has already claimed `run_dir()/instances/<app>.<n>`,
/// and a claimed slot is not free: `allocate_instance` skips a directory
/// that exists, so the next run of this app lands on n+1 — a different,
/// EMPTY per-instance volume, which is the outage documented on
/// `populated_siblings`. Every error exit between `allocate_instance` and a
/// successful `backend.launch` therefore has to undo the claim. A launch
/// that succeeds hands the directory to the backend's `Instance`, which
/// owns it from then on, so the guard is disarmed there.
struct PendingDir(Option<PathBuf>);

impl PendingDir {
    /// The backend owns the directory now — leave it alone.
    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for PendingDir {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = crate::paths::force_remove_dir_all(&dir);
        }
    }
}

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
    let env = &ctx.env;
    // Identity (state pool, .ply name, volumes, control) comes from --name
    // when given; the image's own name still drives the /opt/<name> prefix.
    let app: String = opts
        .name
        .clone()
        .unwrap_or_else(|| manifest.package.name.clone());
    let (instance_dir, n) = allocate_instance(&app, slot)?;
    // From here to a successful launch, every `?` gives the slot back.
    let mut pending_dir = PendingDir(Some(instance_dir.clone()));
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
    // Manifest volumes plus any added with --volume: a deployment can give an
    // imported app a writable data dir its image never declared.
    let mut all_volumes = manifest.volumes.clone();
    for path in &opts.volumes {
        let vname = volume_name_from_path(path);
        all_volumes
            .entry(vname)
            .or_insert_with(|| crate::manifest::Volume {
                path: path.clone(),
                scope: "instance".into(),
                ephemeral: false,
            });
    }
    for (name, volume) in &all_volumes {
        let suffix = if volume.scope == "shared" {
            "shared".to_string()
        } else {
            n.to_string()
        };
        let app_volumes = crate::paths::volumes_dir().join(&app);
        let host_dir = app_volumes.join(format!("{name}.{suffix}"));
        std::fs::create_dir_all(&host_dir).map_err(|source| Error::Io {
            path: host_dir.clone(),
            source,
        })?;
        // Starting empty next to a volume that is full is almost never what
        // the operator meant, and a database that does it looks healthy.
        //
        // The test is EMPTY, not newly-created: the second time an app lands
        // on the wrong slot, that slot's directory is already there from the
        // first time — which is exactly when the operator most needs telling.
        //
        // Scaled apps are exempt: asking for N instances means asking for N
        // empty data dirs, and saying so N times is noise. Asking for ONE
        // and getting an empty one is the accident.
        let empty = std::fs::read_dir(&host_dir)
            .map(|mut e| e.next().is_none())
            .unwrap_or(false);
        if empty && opts.scale <= 1 && volume.scope != "shared" && !volume.ephemeral {
            let siblings = populated_siblings(&app_volumes, name, &suffix);
            if !siblings.is_empty() {
                eprintln!(
                    "ply: warning: {app}.{n} starts on an EMPTY volume {name}.{suffix} — {} already holds data.\n\
                     ply:          Instances do not share a per-instance volume. If you meant to REPLACE\n\
                     ply:          the running instance rather than add one beside it, stop it first.",
                    siblings.join(", "),
                );
            }
        }
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
    let mut spec_env = env.to_vec();
    if let Some(user) = &run_user {
        for pair in spec_env.iter_mut() {
            if pair.0 == "HOME" {
                pair.1 = format!("/home/{}", user.name);
            }
        }
    }
    // A published port has to reach ONE listener per instance, and rootless
    // instances share a network — the host's, or the one ply made. Sharing
    // is fine while there is a single instance: it binds the port it
    // declares, exactly as it does on a bridge, and that is now the common
    // case. Scale past one and they would fight over the same number, so
    // each is handed its own via PORT and the parent balances across them.
    //
    // Skipped when the spec named the instance port: that is the author
    // saying where the app listens, and an imported image cannot be talked
    // out of it.
    let injected_port = match (publish.first(), !net.alone) {
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
    let spec = InstanceSpec {
        app: app.clone(),
        package: manifest.package.name.clone(),
        n,
        instance_dir: instance_dir.clone(),
        images: std::iter::once(ctx.image.clone())
            .chain(ctx.dep_images.iter().cloned())
            .collect(),
        entrypoint: ctx.entrypoint.clone(),
        cwd: manifest
            .package
            .workdir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/opt/{}", manifest.package.name))),
        env: spec_env,
        hostname: app.clone(),
        binds,
        volume_targets,
        run_user,
        capabilities: manifest.package.capabilities.clone(),
        keep_net_bind: manifest.ports.values().any(|p| *p < 1024),
        privileged: opts.privileged,
        resources: manifest.resources.clone(),
        dns: opts.network_dns.clone(),
        local_aliases: opts.network_peers.clone(),
        egress: ctx.egress.clone(),
    };

    // Live params tree: publish before spawn. The child's own mount
    // sequence (runtime/container.rs) re-binds each PARENT_OWNED file
    // read-only over itself inside /run/ply/self and needs the target to
    // already exist — this has to land before the backend clones the child,
    // not later at the InstanceState save. Best-effort: a write failure here must not
    // block the app from starting.
    {
        let launched_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Instances of this app already ALIVE, plus this one about to
        // launch — the "fact the parent already knows" version of a count.
        // `alive()` matters: a state file outlives a dead instance until
        // its parent reaps it, and counting those would publish an
        // `instances` that only ever grows (`Drop for Running` republishes
        // the remaining count on the way down).
        let instance_count = live_instance_count(&app) + 1;
        let publish_fact = |key: &str, value: &str| {
            if let Err(e) = params_tree::publish(&app, key, value) {
                eprintln!("ply: warning: params tree {app}/{key}: {e}");
            }
        };
        publish_fact("state", "starting");
        publish_fact("started_at", &launched_at.to_string());
        publish_fact("instances", &instance_count.to_string());
        publish_fact("restarts", &restarts.to_string());
        // Facts only — never a secret, never a declared [params] value.
        publish_fact("name", &app);
        publish_fact("host", &format!("{app}.ply"));
        // Same `{port}` fact the resolver uses: the first publish entry,
        // else the app's own single labelled port.
        if let Some(port) = publish
            .first()
            .map(|w| w.spec.instance_port)
            .or_else(|| crate::stack::manifest_port(manifest))
        {
            publish_fact("port", &port.to_string());
        }
        publish_fact("version", &manifest.package.version.to_string());
    }

    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let loopback = net.facts.loopback;
    // How another process reaches `ip`, when `ip` alone is not enough — a
    // microVM's address lives on this parent's switch and nowhere else, and
    // an `--after` gate in a different `ply run` has no other way to find
    // it. `None` for every namespace instance, which is the whole of Linux.
    let reach_via = backend.reach_via();
    // Handed to the backend, which calls it once the instance's pid and
    // address are known and BEFORE the instance runs.
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
            instance_port: publish.first().map(|w| w.spec.instance_port),
            published_addr: publish.first().map(|w| {
                format!(
                    "{}:{}",
                    w.spec.scope.connect_addr(loopback),
                    w.spec.host_port
                )
            }),
            domains: opts.domains.clone(),
            network: reach_via.clone(),
        }
        .save()
    };
    let Launched {
        mut instance,
        output,
    } = backend.launch(&spec, &mut record)?;
    pending_dir.disarm(); // the instance owns its directory from here

    // The ring is created only once the instance is actually running.
    // `RingWriter::create` is not idempotent: it renames `<app>.<n>.log` to
    // `.log.1` and starts an empty live file, and there is exactly one
    // rotation slot. Created before the launch, any backend failure (loop
    // devices, the squashfs mounts, clone, the cgroup, the veth) would push
    // the last good generation into `.1`, and the restart's retry would push
    // the empty ring over it — `ply logs` would be blank exactly when the
    // owner needs it.
    let mut ring = match crate::runtime::logring::RingWriter::create(&app, n) {
        Ok(ring) => ring,
        Err(e) => {
            // The instance is already running; take it down before reporting.
            // `instance` drops on return: hosts entry, unmounts, instance dir.
            // The state file falls to `reap_stale`, as a killed launch always did.
            crate::runtime::supervise::stop_with_patience(
                instance.as_mut(),
                Signal::SIGKILL,
                std::time::Duration::ZERO,
                std::time::Duration::from_millis(50),
            );
            return Err(e);
        }
    };

    // The log tee: a copier thread passes the instance's combined output
    // through to the parent's stdout (journald/terminal behavior unchanged)
    // while also feeding the bounded log ring that `ply logs` and the
    // dashboard read.
    {
        let mut output = output;
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

    // Where the published pools will reach this instance: rootful backends
    // live on the bridge at the shared instance port; rootless ones on their
    // injected loopback port; a microVM has no host-dialable address at all
    // and hands over a connector that goes through the switch
    // (`Instance::connector`). It JOINS the pools later, once that address
    // accepts a connection — the run loop checks each turn.
    let membership = crate::runtime::publish::Membership::new(
        publish
            .iter()
            .enumerate()
            .map(|(i, wiring)| {
                let backend = match injected_port {
                    // only the first spec gets the injected loopback port, and
                    // that one really is a host address: ply forwarded it there.
                    Some(port) if i == 0 => crate::runtime::publish::connector_for(
                        std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    ),
                    _ => instance.connector(wiring.spec.instance_port),
                };
                (wiring.pool.clone(), backend)
            })
            .collect(),
    );

    Ok(Running {
        app,
        n,
        inner: instance,
        membership,
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
pub fn discovery_env(after: &[String], in_stack_network: bool) -> Vec<(String, String)> {
    let states = state::list().unwrap_or_default();
    let mut out = Vec::new();
    for dep in after {
        let Some(found) = states
            .iter()
            .find(|s| &s.app == dep && s.alive() && s.published_addr.is_some())
        else {
            continue;
        };
        // Sharing a stack's network, the dependency answers on its own port
        // at its own name; the published pair is the HOST's side of a proxy
        // and reaches nothing from in here.
        let addr = match (in_stack_network, found.instance_port) {
            (true, Some(port)) => format!("{dep}.ply:{port}"),
            _ => found.published_addr.clone().expect("filtered above"),
        };
        let stem = env_stem(dep);
        let host = addr.rsplit_once(':').map(|(h, _)| h.to_string());
        let port = match (in_stack_network, found.instance_port) {
            (true, Some(port)) => Some(port),
            _ => found.published_port,
        };
        out.push((format!("{stem}_ADDR"), addr));
        if let (Some(host), Some(port)) = (host, port) {
            out.push((format!("{stem}_HOST"), host));
            out.push((format!("{stem}_PORT"), port.to_string()));
        }
    }
    out
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
    fn env_file_strips_quotes_trims_and_refuses_shellisms() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("secrets.env");

        std::fs::write(
            &f,
            "# a comment\n\
             \n\
             PLAIN=abc\n\
             TRAILING=abc   \n\
             DQ=\"s3cret\"\n\
             SQ='s3cret'\n\
             KEEPS_SPACE=\"  padded  \"\n\
             INNER=a\"b\n\
             URL=postgres://u:p@host:5432/db?x=1\n\
             HASH=pa#ss\n",
        )
        .unwrap();
        let got: std::collections::BTreeMap<String, String> =
            parse_env_file(&f).unwrap().into_iter().collect();

        assert_eq!(got["PLAIN"], "abc");
        assert_eq!(
            got["TRAILING"], "abc",
            "value must be trimmed, not just the key"
        );
        assert_eq!(got["DQ"], "s3cret", "double quotes are stripped");
        assert_eq!(got["SQ"], "s3cret", "single quotes are stripped");
        assert_eq!(got["KEEPS_SPACE"], "  padded  ", "quoting preserves spaces");
        assert_eq!(got["INNER"], "a\"b", "an unmatched inner quote is literal");
        assert_eq!(
            got["URL"], "postgres://u:p@host:5432/db?x=1",
            "splits on the FIRST ="
        );
        assert_eq!(got["HASH"], "pa#ss", "# is only a comment at line start");

        // shell-isms are refused, not silently turned into weird keys
        std::fs::write(&f, "export FOO=1\n").unwrap();
        let err = parse_env_file(&f).unwrap_err().to_string();
        assert!(err.contains("export"), "{err}");
        assert!(err.contains("FOO=…"), "{err}");

        std::fs::write(&f, "no equals here\n").unwrap();
        assert!(parse_env_file(&f)
            .unwrap_err()
            .to_string()
            .contains("expected KEY=VALUE"));
    }

    #[test]
    // `alone_in_its_network` is the namespace backend's own rule for when an
    // instance may keep its declared port instead of having PORT injected.
    // The predicate itself is portable — a truth table over three scalars.
    // The gate is only about where it LIVES: it moved into `runtime::ns`
    // with `launch_instance`'s use of it. Worth revisiting if a second
    // backend ever needs the same rule, at which point `target_os = "linux"`
    // stops meaning "namespaces".
    #[cfg(target_os = "linux")]
    fn alone_in_its_network_matches_when_port_is_injected() {
        use crate::runtime::ns::alone_in_its_network;
        // The PORT-override WARNING and the injection itself read this one
        // predicate, so they cannot drift apart. Rootful is always alone (its
        // own bridge address); rootless with no namespace never is — which is
        // why the host-network fallback injects even at scale 1.
        let opts = |scale: u32, netns: Option<&str>| RunOptions {
            image: std::path::PathBuf::from("x.img"),
            name: None,
            cli_env: vec![],
            allow_insecure: false,
            scale,
            links: vec![],
            publish: vec![],
            network_peers: vec![],
            network_dns: None,
            network: netns.map(std::path::PathBuf::from),
            after: vec![],
            after_timeout: std::time::Duration::from_secs(60),
            privileged: false,
            entrypoint: None,
            domains: vec![],
            volumes: vec![],
            egress: None,
        };
        // rootful: alone at any scale
        assert!(alone_in_its_network(false, &opts(1, None)));
        assert!(alone_in_its_network(false, &opts(8, None)));
        // rootless, no namespace (the fallback): never alone, so PORT is
        // injected and the warning fires — at scale 1 too.
        assert!(!alone_in_its_network(true, &opts(1, None)));
        assert!(!alone_in_its_network(true, &opts(4, None)));
        // rootless WITH a namespace still loses it past one instance: they
        // all share that one namespace.
        assert!(!alone_in_its_network(
            true,
            &opts(2, Some("/proc/1/ns/net"))
        ));
    }

    #[test]
    fn env_stems_are_shell_safe() {
        assert_eq!(env_stem("api-server"), "API_SERVER");
        assert_eq!(env_stem("postgres"), "POSTGRES");
        assert_eq!(env_stem("my.app_1"), "MY_APP_1");
    }
}

/// How long a stop waits for instances to go quietly before SIGKILL.
///
/// Strictly inside `lifecycle::SYSTEMD_STOP_TIMEOUT_SECS`: the supervisor
/// must be the one that ends the shutdown, never systemd. If systemd's
/// timeout fires first it kills the supervisor alone — the instance lives in
/// its own cgroup, survives, and keeps its slot, and the unit systemd starts
/// next lands on a fresh slot with an empty volume. Two production databases
/// were swapped for empty ones that way (2026-08-30).
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

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
/// `[package] stop_signal`, readable from a signal handler. A rolling deploy
/// always honoured it; a plain `systemctl stop` used to forward SIGTERM
/// verbatim, which is a *different request* for anything that reads the two
/// differently — to postgres SIGTERM means "smart shutdown: wait for every
/// client to disconnect first", so a database with a live connection pool
/// never exited at all.
static STOP_SIGNAL: AtomicI32 = AtomicI32::new(nix::libc::SIGTERM);

/// What to send a container on the `seen`-th stop request: what the app
/// declared it wants, and SIGKILL once it has ignored that. The signal we
/// RECEIVED is deliberately not passed on — SIGTERM from systemd means
/// "stop", not "smart shutdown", and only the app knows which signal spells
/// that for it.
fn signal_for_stop(seen: usize, declared: i32) -> i32 {
    if seen >= 1 {
        nix::libc::SIGKILL
    } else {
        declared
    }
}

extern "C" fn forward_signal(_sig: i32) {
    // A forwarded stop means intent: no respawns after this point.
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
    // A container's entrypoint is PID 1 in its pid ns: the kernel drops
    // default-action signals to init. First signal sends the app's declared
    // stop signal (apps with handlers stop gracefully); repeated signals
    // escalate to SIGKILL, which init cannot ignore.
    let seen = SIGNALS_SEEN.fetch_add(1, Ordering::SeqCst);
    let sig = signal_for_stop(seen, STOP_SIGNAL.load(Ordering::SeqCst));
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
            crate::runtime::backend::scrub_instance_dir(&dir);
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

/// Parse an --env-file: KEY=VALUE lines, `#` comments, blanks ignored.
///
/// The value is trimmed, and a matched pair of surrounding quotes is removed
/// (`PW="s3cret"` → `s3cret`). Both matter because these files hold
/// credentials: an untrimmed trailing space or a literal `"` in a password
/// produces an auth failure far from its cause, and every other tool people
/// arrive from — dotenv, docker-compose — strips them. `account.rs` already
/// parsed its own KEY=VALUE file this way.
///
/// A `#` only starts a comment at the beginning of a line. Inline comments
/// are genuinely ambiguous in a file where `#` can be part of a password, so
/// they stay part of the value — quote the value to make that explicit.
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
        let at = |msg: String| Error::Runtime(format!("{}:{}: {msg}", path.display(), lineno + 1));
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| at("expected KEY=VALUE".into()))?;
        let key = k.trim();
        if let Some(rest) = key.strip_prefix("export ") {
            return Err(at(format!(
                "`{line}` — this is an env file, not a shell script; drop the `export ` (just `{}=…`)",
                rest.trim()
            )));
        }
        if key.is_empty() || key.split_whitespace().count() != 1 {
            return Err(at(format!("`{k}` is not a usable key")));
        }
        pairs.push((key.to_string(), unquote(v.trim()).to_string()));
    }
    Ok(pairs)
}

/// Strip one matched pair of surrounding quotes; anything else is returned
/// as is. The inside is never trimmed — quoting is how a value KEEPS its
/// leading or trailing spaces.
fn unquote(v: &str) -> &str {
    let bytes = v.as_bytes();
    match (bytes.first(), bytes.last()) {
        (Some(a), Some(b)) if a == b && (*a == b'"' || *a == b'\'') && v.len() >= 2 => {
            &v[1..v.len() - 1]
        }
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `systemctl stop` must ask the app for the shutdown IT defined. ply
    /// used to forward the received SIGTERM verbatim, which to a postmaster
    /// means "smart shutdown — wait for every client to disconnect": with a
    /// web app holding a connection pool, the database never exited, systemd
    /// timed out, and the orphaned instance kept its slot.
    #[test]
    fn a_stop_asks_for_the_signal_the_app_declared() {
        let sigint = Signal::SIGINT as i32;
        assert_eq!(
            signal_for_stop(0, sigint),
            sigint,
            "the first stop must send the declared signal, not the one we were sent",
        );
        assert_ne!(signal_for_stop(0, sigint), Signal::SIGTERM as i32);
        // An app that ignores its own stop signal does not get to hang here.
        assert_eq!(signal_for_stop(1, sigint), nix::libc::SIGKILL);
        // Undeclared still means SIGTERM — the default is unchanged.
        assert_eq!(
            signal_for_stop(0, Signal::SIGTERM as i32),
            Signal::SIGTERM as i32
        );
    }

    /// The shipped postgres must ask for a FAST shutdown. Losing this line
    /// re-creates the outage: a database that cannot stop while anything is
    /// connected to it.
    #[test]
    fn the_shipped_postgres_asks_for_a_fast_shutdown() {
        let toml =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../services/postgres/ply.toml");
        let text =
            std::fs::read_to_string(&toml).unwrap_or_else(|e| panic!("{}: {e}", toml.display()));
        let manifest: crate::manifest::Manifest =
            toml::from_str(&text).expect("postgres manifest parses");
        let declared = manifest
            .package
            .stop_signal
            .expect("postgres must declare stop_signal — SIGTERM is a SMART shutdown");
        assert_eq!(
            crate::manifest::parse_stop_signal(&declared).expect("valid signal"),
            Signal::SIGINT,
        );
    }

    /// The supervisor must always be the one that ends a shutdown. If
    /// systemd's stop timeout fires first it kills the supervisor and NOT
    /// the instance (different cgroups), the instance keeps its slot, and
    /// the replacement unit starts on the next slot — a different volume.
    #[test]
    fn the_stop_grace_finishes_inside_systemds_patience() {
        let systemd = std::time::Duration::from_secs(crate::lifecycle::SYSTEMD_STOP_TIMEOUT_SECS);
        assert!(
            SHUTDOWN_GRACE < systemd,
            "SHUTDOWN_GRACE ({:?}) must stay under TimeoutStopSec ({:?}) — otherwise systemd \
             kills the supervisor first and orphans the instance, which is how a restart \
             silently swaps a database for an empty one",
            SHUTDOWN_GRACE,
            systemd,
        );
        // And the unit really carries that number, so the two cannot drift.
        let unit = crate::lifecycle::render_unit(
            "db",
            "/usr/local/bin/ply",
            "db.img",
            "/var/lib/ply/db.img",
            &[],
            &[],
            false,
        );
        assert!(
            unit.contains(&format!(
                "TimeoutStopSec={}",
                crate::lifecycle::SYSTEMD_STOP_TIMEOUT_SECS
            )),
            "generated unit lost its stop timeout:\n{unit}",
        );
    }

    /// A launch that fails after `allocate_instance` must give the slot back.
    /// Leaving it behind is not cosmetic: `allocate_instance` skips an
    /// existing directory, so the next run of the app takes n+1 and comes up
    /// on a fresh, EMPTY per-instance volume — the plybox-db outage.
    #[test]
    fn an_unfinished_launch_gives_its_instance_slot_back() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let armed = tmp.path().join("app.1");
        std::fs::create_dir(&armed).expect("slot dir");
        drop(PendingDir(Some(armed.clone())));
        assert!(
            !armed.exists(),
            "an armed guard must remove the slot it holds",
        );

        // Disarmed: the launch succeeded and the backend's Instance owns the
        // directory; removing it here would unmount a live instance's layers.
        let kept = tmp.path().join("app.2");
        std::fs::create_dir(&kept).expect("slot dir");
        let mut guard = PendingDir(Some(kept.clone()));
        guard.disarm();
        drop(guard);
        assert!(
            kept.exists(),
            "a disarmed guard must leave the directory to the backend",
        );
    }

    /// Why the log ring is created only AFTER `backend.launch` succeeds:
    /// `RingWriter::create` rotates, and there is exactly one rotation slot.
    /// A ring created before a launch that then failed would spend that slot
    /// on nothing, and the restart's retry would push the empty ring over the
    /// last good generation — `ply logs` blank exactly when it is needed.
    #[test]
    fn creating_a_ring_twice_spends_the_one_rotation_slot() {
        // XDG_RUNTIME_DIR is process-global; paths.rs owns the lock.
        let _guard = crate::paths::ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", tmp.path());

        let live = crate::runtime::logring::path("web", 1);
        let rotated = live.with_extension("log.1");

        let mut ring = crate::runtime::logring::RingWriter::create("web", 1).expect("ring");
        ring.append(b"the generation the owner needs\n");
        drop(ring);
        assert!(!rotated.exists(), "nothing rotated by the first create");

        crate::runtime::logring::RingWriter::create("web", 1).expect("ring");
        assert!(
            rotated.exists(),
            "a second create rotates the live ring into the only slot there is",
        );
        assert_eq!(
            std::fs::read_to_string(&live).expect("live ring"),
            "",
            "and leaves an empty live ring behind",
        );

        match previous {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    /// The plybox-db outage: a second instance came up on data.2 while
    /// data.1 held the registry, and nothing said so until the site had been
    /// serving an empty database for hours.
    ///
    /// data.2 ALREADY EXISTS here, left behind by the first time this
    /// happened — the recurrence is the case that matters, and keying the
    /// warning on "directory was just created" missed it in production.
    #[test]
    fn a_full_volume_from_another_instance_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = dir.path().join("plybox-db");
        std::fs::create_dir_all(app.join("data.1")).expect("data.1");
        std::fs::write(app.join("data.1/PG_VERSION"), "17").expect("write");
        std::fs::create_dir_all(app.join("data.2")).expect("data.2");
        assert!(app.join("data.2").exists(), "the empty slot is not new");

        assert_eq!(
            populated_siblings(&app, "data", "2"),
            vec!["data.1".to_string()],
            "instance 2 must be told that instance 1 holds the data",
        );
        // The instance that owns the data is not warned about itself, and an
        // empty sibling is not worth a word.
        assert!(populated_siblings(&app, "data", "1").is_empty());
        // A different volume of the same app is not a sibling of this one.
        std::fs::create_dir_all(app.join("backups.1")).expect("backups.1");
        std::fs::write(app.join("backups.1/dump"), "x").expect("write");
        assert_eq!(
            populated_siblings(&app, "data", "2"),
            vec!["data.1".to_string()]
        );
    }
}
