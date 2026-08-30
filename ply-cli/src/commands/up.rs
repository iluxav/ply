//! `ply up` — start a stack: registry apps fetched, local dirs built, every
//! member its own `ply run` parent, wired with `--after` and env.
//!
//! Process model: up spawns one `ply run` child per member (the same
//! supervision parents `systemd` would run in production) and stays in the
//! foreground. Each member runs under its own `--name` (the member name), so
//! `--after`, `--publish` and the `<member>.ply` bridge name all key on that
//! identity — two members may even run the same image. `after` ordering rides
//! the children's own `--after` gates — up starts everything and the gates
//! serialize startup. Ctrl-C reaches the whole process group; a member dying
//! takes the stack down in reverse order so nothing keeps serving against a
//! dead dependency.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use ply_core::stack::{self, Member, MemberSource, StackLock};

use crate::cli::UpArgs;

static STOPPING: AtomicBool = AtomicBool::new(false);

extern "C" fn note_stop(_: i32) {
    STOPPING.store(true, Ordering::SeqCst);
}

struct Prepared {
    /// The member identity — the `--name` the child runs under.
    member: String,
    /// What the member's `ply run` child gets: a fetched image path for
    /// `run =` registry members, the app DIR for local `run = "./dir"`
    /// members (so the child owns the build and the ply.dev.toml overlay),
    /// or the URL for `run = "https://…"` members.
    target: String,
    /// Launch-ready env (every `$VAR` already expanded).
    env: Vec<(String, String)>,
    after: Vec<String>,
    publish: Vec<String>,
    volume: Vec<String>,
    domain: Vec<String>,
    scale: Option<u32>,
}

pub fn exec(args: UpArgs) -> Result<()> {
    let (stack, members, lock_dir) = load_up_stack(&args)?;
    let selected = stack::select(&stack, &members)?;

    // --- resolve $VAR holes from --env-file + the process environment --------
    let overrides: BTreeMap<String, String> = match &args.env_file {
        Some(f) => ply_core::runtime::run::parse_env_file(f)?
            .into_iter()
            .collect(),
        None => BTreeMap::new(),
    };
    let lookup = |k: &str| overrides.get(k).cloned().or_else(|| std::env::var(k).ok());

    // Expand every $VAR hole up front — a missing one fails instantly, before
    // any download, and names the member and key.
    let envs: Vec<Vec<(String, String)>> = selected
        .iter()
        .map(|m| stack::expand_member_env(m, &lookup))
        .collect::<ply_core::Result<_>>()?;

    // --- prepare every member: fetch (registry) or resolve (local dir/URL) ---
    // A stack fetched from the registry (or a bare file) has no dir to pin
    // in, so it resolves fresh; a `-C dir` project stack uses its ply.lock.
    let mut lock = lock_dir.as_deref().map(StackLock::load).unwrap_or_default();
    let mut prepared: Vec<Prepared> = Vec::new();
    for (member, env) in selected.iter().zip(envs) {
        let target = prepare_target(member, &args, &mut lock)?;
        prepared.push(Prepared {
            member: member.name.clone(),
            target,
            env,
            after: member.after.clone(),
            publish: member.publish.clone(),
            volume: member.volume.clone(),
            domain: member.domain.clone(),
            scale: member.scale,
        });
    }

    if let Some(dir) = &lock_dir {
        if selected
            .iter()
            .any(|m| matches!(m.source, MemberSource::Run { .. }))
        {
            lock.save(dir)?;
        }
    }

    // --- spawn: one `ply run` parent per member ------------------------------
    unsafe {
        signal::signal(Signal::SIGINT, signal::SigHandler::Handler(note_stop)).ok();
        signal::signal(Signal::SIGTERM, signal::SigHandler::Handler(note_stop)).ok();
    }
    let exe = std::env::current_exe().context("locating the ply binary")?;

    // One network for the stack. Rootful already gives every instance its
    // own address on the bridge; rootless cannot attach a veth to the host,
    // so ply makes a namespace it owns and puts the members inside it.
    // There they bind their own declared ports, reach each other on
    // loopback as `<name>.ply`, and touch nothing on the machine — which is
    // what lets one stack file mean the same thing here and on a droplet.
    let netns = match ply_core::paths::is_root() {
        true => None,
        false => match ply_core::runtime::netns::NetNs::create() {
            Ok(ns) => Some(ns),
            Err(e) => {
                eprintln!("ply up: {e}");
                eprintln!("ply up: falling back to the host's network for this run");
                None
            }
        },
    };
    let peers: Vec<String> = prepared.iter().map(|p| p.member.clone()).collect();

    let mut children: Vec<(String, Child)> = Vec::new();
    for p in &prepared {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("run").arg(&p.target);
        cmd.arg("--name").arg(&p.member);
        if let Some(ns) = &netns {
            cmd.arg("--netns").arg(ns.path());
            for peer in peers.iter().filter(|n| *n != &p.member) {
                cmd.arg("--netns-peer").arg(peer);
            }
        }
        for (k, v) in &p.env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        for dep in &p.after {
            cmd.arg("--after").arg(dep);
        }
        for publish in &p.publish {
            cmd.arg("--publish").arg(publish);
        }
        for volume in &p.volume {
            cmd.arg("--volume").arg(volume);
        }
        for domain in &p.domain {
            cmd.arg("--domain").arg(domain);
        }
        if let Some(scale) = p.scale {
            cmd.arg("--scale").arg(scale.to_string());
        }
        cmd.arg("--after-timeout").arg(&args.after_timeout);
        eprintln!("ply up: starting {}", p.member);
        let child = cmd
            .spawn()
            .with_context(|| format!("spawning `ply run` for member `{}`", p.member))?;
        children.push((p.member.clone(), child));
    }

    // --- supervise -----------------------------------------------------------
    let code = loop {
        if STOPPING.load(Ordering::SeqCst) {
            eprintln!("ply up: stopping (signal)");
            break teardown(&mut children, 130);
        }
        let mut exited: Option<(usize, std::process::ExitStatus)> = None;
        for (i, (_, child)) in children.iter_mut().enumerate() {
            if let Some(status) = child.try_wait()? {
                exited = Some((i, status));
                break;
            }
        }
        if let Some((i, status)) = exited {
            let (name, _) = children.remove(i);
            let code = status.code().unwrap_or(130);
            if children.is_empty() {
                eprintln!("ply up: {name} exited ({code})");
                break code;
            }
            eprintln!("ply up: {name} exited ({code}) — stopping the stack");
            break teardown(&mut children, if code == 0 { 1 } else { code });
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    std::process::exit(code);
}

/// Determine what stack `ply up` should run and where its lock lives:
/// - `ply up` (no source arg) → the stack in `-C dir`; positionals are member
///   selections; the lock is that dir's ply.lock.
/// - `ply up <ref|file> [MEMBER…]` → a registry stack (`namespace/name`) or a
///   stack file; the rest of the positionals are member selections. No lock —
///   a fetched stack resolves fresh each run.
fn load_up_stack(args: &UpArgs) -> Result<(stack::Stack, Vec<String>, Option<PathBuf>)> {
    if let Some(first) = args.members.first() {
        if is_stack_source(first) {
            let members = args.members[1..].to_vec();
            return Ok((resolve_stack_source(first, &args.source)?, members, None));
        }
    }
    let (mut stack, file) = stack::discover(&args.dir)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no stack in {} — expected `[[app]]` blocks in stack.toml or ply.toml, each one a `ply run`:\n\n  [[app]]\n  run = \"postgres@17\"\n  name = \"db\"\n\n  [[app]]\n  run   = \"./server\"\n  after = [\"db\"]\n\nor bring up a published stack: `ply up <namespace>/<name>`",
            args.dir.display()
        )
    })?;
    apply_dev_overlay(&mut stack, &file);
    Ok((stack, args.members.clone(), Some(args.dir.clone())))
}

/// `<stack>.dev.toml` beside the stack file: local truths (loopback
/// addresses, a port that dodges what this machine already runs) kept out of
/// the committed, publishable stack. `ply up` only — a host never applies it.
fn apply_dev_overlay(stack: &mut stack::Stack, file: &std::path::Path) {
    match stack::apply_dev_overlay(stack, file) {
        Ok(Some(what)) => eprintln!(
            "ply up: applying {} ({what})",
            stack::dev_overlay_path(file).display()
        ),
        Ok(None) => {}
        // a broken overlay must not silently ship production values
        Err(e) => eprintln!("ply up: ignoring dev overlay — {e}"),
    }
}

/// A first positional is a stack SOURCE (not a member name) when it names a
/// registry ref or a path (`namespace/name`, `./dir`, `services/x`) or a stack
/// toml (`…​.toml`). A bare `[a-z0-9-]` word is always a member selection — so
/// `ply up web` stays member-selection even when a `./web` member dir exists.
fn is_stack_source(first: &str) -> bool {
    first.contains('/') || first.ends_with(".toml")
}

/// Resolve a stack source to a parsed Stack: an on-disk dir/file, else a
/// registry reference fetched by name.
fn resolve_stack_source(first: &str, source: &str) -> Result<stack::Stack> {
    let path = std::path::Path::new(first);
    if path.is_dir() {
        let (mut stack, file) = stack::discover(path)?
            .ok_or_else(|| anyhow::anyhow!("{first} has no [[app]] stack"))?;
        apply_dev_overlay(&mut stack, &file);
        return Ok(stack);
    }
    if path.is_file() {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {first}"))?;
        let mut stack =
            stack::parse(&text, path)?.ok_or_else(|| anyhow::anyhow!("{first} has no [[app]]"))?;
        apply_dev_overlay(&mut stack, path);
        return Ok(stack);
    }
    if first.ends_with(".toml") {
        bail!("no such stack file: {first}");
    }
    eprintln!("ply up: fetching stack {first} …");
    Ok(ply_core::catalog::fetch_stack(first, source)?)
}

/// Fetch (registry), resolve (local dir/img), or pass through (URL) a
/// member's `run` target, returning what `ply run` should receive.
fn prepare_target(member: &Member, args: &UpArgs, lock: &mut StackLock) -> Result<String> {
    match &member.source {
        MemberSource::Run { name, version } => {
            let reference = match version {
                Some(v) => format!("{name}@{v}"),
                None => name.clone(),
            };
            let arch = ply_core::image::name::Arch::host().as_str();
            let pin = if args.refresh {
                None
            } else {
                lock.pinned(&member.name, &reference).cloned()
            };
            // A pinned digest starts straight from the store — no index
            // fetch, no download, works offline.
            let path = match &pin {
                Some(p) if p.digests.contains_key(arch) => {
                    let digest = &p.digests[arch];
                    let (path, resolved) = ply_core::catalog::fetch_app_image_pinned(
                        name,
                        &p.version,
                        digest,
                        &args.source,
                    )?;
                    eprintln!("ply up: {} -> {resolved} (locked)", member.name);
                    lock.record(&member.name, &reference, &p.version, arch, digest);
                    path
                }
                _ => {
                    let want = pin
                        .as_ref()
                        .map(|p| p.version.clone())
                        .or_else(|| version.clone());
                    let (path, resolved, digest) =
                        ply_core::catalog::fetch_app_image(name, want.as_deref(), &args.source)?;
                    eprintln!(
                        "ply up: {} -> {resolved}{}",
                        member.name,
                        if pin.is_some() { " (locked)" } else { "" }
                    );
                    lock.record(
                        &member.name,
                        &reference,
                        &resolved.version.to_string(),
                        arch,
                        &digest,
                    );
                    path
                }
            };
            Ok(path.display().to_string())
        }
        MemberSource::Path(rel) => {
            // The child (`ply run DIR`) owns building — with the up-to-date
            // skip — and the ply.dev.toml overlay.
            let dir = args.dir.join(rel);
            let dir = std::path::absolute(&dir)
                .with_context(|| format!("resolving {}", dir.display()))?;
            if !dir.exists() {
                bail!(
                    "stack member `{}`: {} does not exist",
                    member.name,
                    dir.display()
                );
            }
            Ok(dir.display().to_string())
        }
        MemberSource::Url(url) => Ok(url.clone()),
    }
}

/// SIGTERM the remaining members in reverse start order, waiting for each;
/// SIGKILL stragglers after 15 s. Returns the exit code to propagate.
fn teardown(children: &mut Vec<(String, Child)>, code: i32) -> i32 {
    while let Some((name, mut child)) = children.pop() {
        if matches!(child.try_wait(), Ok(Some(_))) {
            continue;
        }
        let pid = Pid::from_raw(child.id() as i32);
        let _ = signal::kill(pid, Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    eprintln!("ply up: {name} ignored SIGTERM for 15s — killing");
                    let _ = signal::kill(pid, Signal::SIGKILL);
                    let _ = child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
        eprintln!("ply up: {name} stopped");
    }
    code
}
