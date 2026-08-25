//! `ply up` — start a [stack]: registry apps fetched, local dirs built,
//! every member its own `ply run` parent, wired with `--after` and env.
//!
//! Process model: up spawns one `ply run` child per member (the same
//! supervision parents `systemd` would run in production) and stays in the
//! foreground. `after` ordering rides the children's own `--after` gates —
//! up starts everything and the gates serialize startup. Ctrl-C reaches the
//! whole process group; a member dying takes the stack down in reverse
//! order so nothing keeps serving against a dead dependency.

use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use ply_core::stack::{self, MemberSource, StackLock};

use crate::cli::UpArgs;

static STOPPING: AtomicBool = AtomicBool::new(false);

extern "C" fn note_stop(_: i32) {
    STOPPING.store(true, Ordering::SeqCst);
}

struct Prepared {
    member: String,
    /// What the member's `ply run` child gets: a fetched image path for
    /// `run =` members, the app DIR for `path =` members — the dir form so
    /// the child owns the build (up-to-date skip) and the ply.dev.toml
    /// overlay, exactly like a hand-typed `ply run ./server`.
    target: PathBuf,
    app: String,
    env: Vec<(String, String)>,
    after_apps: Vec<String>,
}

pub fn exec(args: UpArgs) -> Result<()> {
    let Some(stack) = stack::load(&args.dir)? else {
        bail!(
            "no [stack] in {} — a stack ply.toml lists members, e.g.\n  [stack]\n  db     = {{ run = \"postgres@17\" }}\n  server = {{ path = \"./server\", after = \"db\" }}",
            args.dir.join("ply.toml").display()
        );
    };
    let selected = stack::select(&stack, &args.members)?;

    // --- prepare every member first: fetch or build, learn app names --------
    let mut lock = StackLock::load(&args.dir);
    let mut prepared: Vec<Prepared> = Vec::new();
    for member in &selected {
        let (target, app) = match &member.source {
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
                let (path, resolved) = match &pin {
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
                        (path, resolved)
                    }
                    _ => {
                        let want = pin
                            .as_ref()
                            .map(|p| p.version.clone())
                            .or_else(|| version.clone());
                        let (path, resolved, digest) = ply_core::catalog::fetch_app_image(
                            name,
                            want.as_deref(),
                            &args.source,
                        )?;
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
                        (path, resolved)
                    }
                };
                let _ = resolved;
                let app = ply_core::image::read::read_manifest(&path)?.package.name;
                (path, app)
            }
            MemberSource::Path(rel) => {
                // The child (`ply run DIR`) owns building — with the
                // up-to-date skip — and the ply.dev.toml overlay. Here we
                // only need the app name for wiring.
                let dir = args.dir.join(rel);
                let manifest_path = dir.join("ply.toml");
                let manifest =
                    ply_core::manifest::Manifest::load(&manifest_path).with_context(|| {
                        format!("stack member `{}` ({})", member.name, dir.display())
                    })?;
                if !manifest.is_app() {
                    bail!(
                        "stack member `{}` ({}) has no entrypoint — only apps run",
                        member.name,
                        dir.display()
                    );
                }
                let dir = std::path::absolute(&dir)
                    .with_context(|| format!("resolving {}", dir.display()))?;
                (dir, manifest.package.name)
            }
        };
        prepared.push(Prepared {
            member: member.name.clone(),
            target,
            app,
            env: member.env.clone(),
            after_apps: Vec::new(), // filled below, needs every app name known
        });
    }

    // Two members resolving to one app name would fight over instance state.
    for (i, a) in prepared.iter().enumerate() {
        if let Some(b) = prepared[..i].iter().find(|b| b.app == a.app) {
            bail!(
                "members `{}` and `{}` both run app `{}` — instance state is per app name, they cannot coexist",
                b.member,
                a.member,
                a.app
            );
        }
    }

    // Translate `after` member keys into the app names `--after` gates on.
    let member_apps: Vec<(String, String)> = prepared
        .iter()
        .map(|p| (p.member.clone(), p.app.clone()))
        .collect();
    for (i, member) in selected.iter().enumerate() {
        let mut after_apps = Vec::new();
        for dep in &member.after {
            match member_apps.iter().find(|(m, _)| m == dep) {
                Some((_, app)) => after_apps.push(app.clone()),
                // selection always pulls the after-closure in, so this is
                // unreachable — but a silent skip would be a debugging trap
                None => bail!(
                    "member `{}`: dependency `{dep}` was not prepared",
                    member.name
                ),
            }
        }
        prepared[i].after_apps = after_apps;
    }

    if prepared
        .iter()
        .zip(selected.iter())
        .any(|(_, m)| matches!(m.source, MemberSource::Run { .. }))
    {
        lock.save(&args.dir)?;
    }

    // --- spawn: one `ply run` parent per member ------------------------------
    unsafe {
        signal::signal(Signal::SIGINT, signal::SigHandler::Handler(note_stop)).ok();
        signal::signal(Signal::SIGTERM, signal::SigHandler::Handler(note_stop)).ok();
    }
    let exe = std::env::current_exe().context("locating the ply binary")?;
    let mut children: Vec<(String, Child)> = Vec::new();
    for p in &prepared {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("run").arg(&p.target);
        for (k, v) in &p.env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        for app in &p.after_apps {
            cmd.arg("--after").arg(app);
        }
        cmd.arg("--after-timeout").arg(&args.after_timeout);
        eprintln!("ply up: starting {} ({})", p.member, p.app);
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
