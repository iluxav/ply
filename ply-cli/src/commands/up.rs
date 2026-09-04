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
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use ply_core::params;
use ply_core::secrets::SecretStore;
use ply_core::stack::{self, Member, MemberSource, Resolution, StackLock};

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
    /// The member's `run =` spec exactly as written in the stack — e.g.
    /// `postgres@17`, `./server`, `https://…` — never the resolved store
    /// path `target` carries; `--plan`'s header shows this.
    run_spec: String,
    /// The member's `run =` kind — tells [`resolve_members`] whether
    /// `target` is a source dir (unbuilt) or an already-resolved image.
    source: MemberSource,
    /// The member's stack `e = [...]` env, `$VAR`-expanded; `{}` holes stay
    /// raw here — [`resolve_members`] resolves those into `Resolution.env`,
    /// the single source of truth the spawn loop reads, without mutating
    /// this field.
    env: Vec<(String, String)>,
    /// `params = {...}` overrides, `$VAR`-expanded (`{}` holes stay raw —
    /// [`params::namespace`] resolves those against this member's own
    /// namespace).
    params: BTreeMap<String, String>,
    after: Vec<String>,
    publish: Vec<String>,
    volume: Vec<String>,
    domain: Vec<String>,
    scale: Option<u32>,
}

/// The `{image}` built-in fact: the store digest for an already-fetched
/// image (its containing store dir name IS the digest), the raw target for
/// a not-yet-fetched URL, or `None` for a `run = "./dir"` member — its
/// image doesn't exist until the child builds it.
fn image_fact(source: &MemberSource, target: &str) -> Option<String> {
    match source {
        MemberSource::Path(_) => None,
        MemberSource::Run { .. } => Path::new(target)
            .parent()
            .and_then(|d| d.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .or_else(|| Some(target.to_string())),
        MemberSource::Url(_) => Some(target.to_string()),
    }
}

/// `ply up`'s side of the shared resolver: read every prepared member's
/// manifest (which file that is depends on its `run =` kind) and hand the
/// whole stack to [`stack::resolve_stack`], along with the built-in facts
/// only this process can see — the fetched image's digest, the container
/// port of the member's first `--publish`.
///
/// `prepared` is never mutated: the returned [`Resolution`] is the single
/// source of truth for what a member's spawn `-e`s become.
fn resolve_members(
    prepared: &[Prepared],
    stack_dir: Option<&Path>,
    plan_only: bool,
) -> Result<Resolution> {
    let mut inputs: Vec<stack::MemberInput> = Vec::new();
    for p in prepared {
        // No manifest is readable for a `run = "https://…"` member before
        // the child fetches it — `resolve_stack` treats `None` as "declares
        // no params".
        let manifest = match &p.source {
            MemberSource::Url(_) => None,
            MemberSource::Path(_) => Some(
                stack::member_manifest(&p.target, Some(Path::new(p.target.as_str())))
                    .with_context(|| format!("member `{}`: reading its manifest", p.member))?,
            ),
            MemberSource::Run { .. } => Some(
                stack::member_manifest(&p.target, None)
                    .with_context(|| format!("member `{}`: reading its manifest", p.member))?,
            ),
        };
        inputs.push(stack::MemberInput {
            name: p.member.clone(),
            version: manifest.as_ref().map(|m| m.package.version.to_string()),
            manifest,
            env: p.env.clone(),
            params: p.params.clone(),
            after: p.after.clone(),
            publish: p.publish.clone(),
            domain: p.domain.clone(),
            port: stack::container_port(&p.publish),
            scale: p.scale,
            image: image_fact(&p.source, &p.target),
        });
    }
    let secrets = stack_dir.map(SecretStore::for_stack);
    Ok(stack::resolve_stack(&inputs, secrets.as_ref(), plan_only)?)
}

/// The secret mask: every leaf secret substring, and a whole tainted value
/// that matches no leaf and holds no `"(will mint)"`, renders as this.
const MASK: &str = "********";

/// `resolution.secret_values`, longest first (so a shorter secret that
/// happens to be a substring of a longer one never leaves a visible
/// remainder) — the substrings [`mask_value`] blots out.
fn ordered_secrets(resolution: &Resolution) -> Vec<&str> {
    let mut secrets: Vec<&str> = resolution
        .secret_values
        .iter()
        .map(String::as_str)
        .collect();
    secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    secrets
}

/// Render one env value for `--plan`: verbatim if not tainted; with every
/// occurrence of every leaf secret blotted out to [`MASK`] if tainted
/// (masking substrings, not the whole value, so a composed string like a
/// connection URL still shows its non-secret shape — and a `"(will mint)"`
/// substring inside one, being neither a leaf secret nor yet a real value,
/// stays visible right where it sits); a whole-value [`MASK`] for a tainted
/// value that matches no leaf and holds no `"(will mint)"` either (safety
/// fallback — should not arise from `resolve_members`'s own output, where
/// every tainted value is built from namespace values `secret_values`
/// already covers).
fn mask_value(value: &str, secret: bool, secrets: &[&str]) -> String {
    if !secret {
        return value.to_string();
    }
    let mut masked = value.to_string();
    let mut hit = false;
    for s in secrets {
        // Guard against an empty leaf (shouldn't occur — `resolve_members`
        // never inserts one — but `str::replace("", …)` would otherwise
        // splice `MASK` between every character of every value).
        if !s.is_empty() && masked.contains(s) {
            masked = masked.replace(s, MASK);
            hit = true;
        }
    }
    if hit || value.contains("(will mint)") {
        masked
    } else {
        MASK.to_string()
    }
}

/// The rendering of one wait entry in a member's header: bare if it's one of
/// the member's own explicit `after = [...]` entries (a plain name or a
/// `a.finish_boot == 'ok'` condition, verbatim either way); `"x (via
/// {x.param})"` if it's a derived edge — annotated with the first `{x.*}`
/// reference in the member's env, `publish`, or `domain` values, in that
/// order (mirrors `stack::derived_after`'s own edge scan).
fn render_wait(p: &Prepared, wait: &str) -> String {
    if p.after.iter().any(|a| a == wait) {
        return wait.to_string();
    }
    let values = p
        .env
        .iter()
        .map(|(_, v)| v)
        .chain(p.publish.iter())
        .chain(p.domain.iter());
    for raw in values {
        for r in params::refs(raw) {
            if r.app.as_deref() == Some(wait) {
                return format!("{wait} (via {{{wait}.{}}})", r.param);
            }
        }
    }
    wait.to_string()
}

/// `name (target)  publish  after: x (via {x.param}), y` — the header line
/// `render_plan` prints once per member, ahead of its env lines.
fn render_header(p: &Prepared, resolution: &Resolution) -> String {
    let mut parts = vec![format!("{} ({})", p.member, p.run_spec)];
    if !p.publish.is_empty() {
        parts.push(p.publish.join(" "));
    }
    let waits = resolution
        .waits
        .get(&p.member)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !waits.is_empty() {
        let rendered: Vec<String> = waits.iter().map(|w| render_wait(p, w)).collect();
        parts.push(format!("after: {}", rendered.join(", ")));
    }
    parts.join("  ")
}

/// Render the composed plan `ply up --plan` prints: per member, in launch
/// order, a header line followed by one `  KEY = VALUE  source` line per
/// resolved env entry (keys left-aligned, values padded to a common column,
/// within that member); a member with no env entries gets just the header.
/// Pure — no I/O, no minting: everything here was already decided by
/// `resolve_members(plan_only: true)`.
fn render_plan(resolution: &Resolution, prepared: &[Prepared]) -> String {
    let secrets = ordered_secrets(resolution);
    let mut out = String::new();
    for p in prepared {
        out.push_str(&render_header(p, resolution));
        out.push('\n');

        let entries = resolution
            .env
            .get(&p.member)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if entries.is_empty() {
            continue;
        }
        let masked: Vec<String> = entries
            .iter()
            .map(|e| mask_value(&e.value, e.secret, &secrets))
            .collect();
        let key_w = entries.iter().map(|e| e.key.len()).max().unwrap_or(0);
        let val_w = masked.iter().map(|v| v.len()).max().unwrap_or(0);
        for (e, val) in entries.iter().zip(masked.iter()) {
            let key = &e.key;
            let source = &e.source;
            out.push_str(&format!("  {key:key_w$} = {val:val_w$}  {source}\n"));
        }
    }
    out
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
            run_spec: describe_run_spec(&member.source),
            source: member.source.clone(),
            env,
            params: stack::expand_member_params(member, &lookup)?,
            after: member.after.clone(),
            // publish and domain carry holes too: a stack published for
            // other people cannot know their hostname or which ports are
            // already taken on their box.
            publish: stack::expand_member_list(&member.publish, &member.name, "publish", &lookup)?,
            volume: member.volume.clone(),
            domain: stack::expand_member_list(&member.domain, &member.name, "domain", &lookup)?,
            scale: member.scale,
        });
    }

    // `--plan` resolves everything but writes nothing: skip the lock write
    // so a plan run leaves `ply.lock` byte-identical.
    if let Some(dir) = &lock_dir {
        if !args.plan
            && selected
                .iter()
                .any(|m| matches!(m.source, MemberSource::Run { .. }))
        {
            lock.save(dir)?;
        }
    }

    // --- resolve params: namespaces, {} interpolation, provider self-config --
    let resolution = resolve_members(&prepared, lock_dir.as_deref(), args.plan)?;

    if args.plan {
        // Plan is the validator: resolution above already ran everything
        // (and any error already propagated via `?`) — print the composed
        // result and stop, before any minting, spawn, or netns setup below.
        print!("{}", render_plan(&resolution, &prepared));
        return Ok(());
    }

    // --- spawn: one `ply run` parent per member ------------------------------
    unsafe {
        signal::signal(Signal::SIGINT, signal::SigHandler::Handler(note_stop)).ok();
        signal::signal(Signal::SIGTERM, signal::SigHandler::Handler(note_stop)).ok();
    }
    let exe = std::env::current_exe().context("locating the ply binary")?;

    // One network for the stack (Linux rootless: a namespace this process
    // owns; Linux rootful: none needed — the bridge; macOS: a userspace
    // switch this process runs, which members join over a unix socket).
    let peers: Vec<String> = prepared.iter().map(|p| p.member.clone()).collect();
    let (netns, egress_dns) = stack_network(&peers);

    let mut children: Vec<(String, Child)> = Vec::new();
    for p in &prepared {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("run").arg(&p.target);
        cmd.arg("--name").arg(&p.member);
        if let Some(ns) = &netns {
            cmd.arg(NETWORK_FLAG).arg(ns.path());
            if let Some(dns) = &egress_dns {
                cmd.arg("--netns-dns").arg(dns);
            }
            for peer in peers.iter().filter(|n| *n != &p.member) {
                cmd.arg("--netns-peer").arg(peer);
            }
        }
        for entry in resolution.env.get(&p.member).into_iter().flatten() {
            if entry.secret {
                // Value stays out of argv (and /proc/*/cmdline): set on the
                // child's own environment, then tell it the bare name.
                cmd.env(&entry.key, &entry.value);
                cmd.arg("-e").arg(&entry.key);
            } else {
                cmd.arg("-e").arg(format!("{}={}", entry.key, entry.value));
            }
        }
        // after ∪ derived_after ({app.param} refs are themselves an edge —
        // the reference is the wait, whether or not `after =` names it too).
        for dep in resolution.waits.get(&p.member).into_iter().flatten() {
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

/// What a member is told to join the stack's network with. Two spellings of
/// one idea: a namespace has a path in `/proc` and a switch has a unix
/// socket, and only the platform knows which of those the stack has.
#[cfg(target_os = "linux")]
const NETWORK_FLAG: &str = "--netns";
#[cfg(not(target_os = "linux"))]
const NETWORK_FLAG: &str = "--vswitch";

/// One network for the stack (rootless: a namespace this process owns;
/// rootful: none needed — the bridge). Returns it and the resolver its
/// members should use.
///
/// Rootful already gives every instance its own address on the bridge;
/// rootless cannot attach a veth to the host, so ply makes a namespace it
/// owns and puts the members inside it. There they bind their own declared
/// ports, reach each other on loopback as `<name>.ply`, and touch nothing
/// on the machine — which is what lets one stack file mean the same thing
/// here and on a droplet.
///
/// `members` is unused here: a namespace has no address table to seed, and
/// each member allocates its own inside it.
#[cfg(target_os = "linux")]
fn stack_network(
    _members: &[String],
) -> (Option<ply_core::runtime::ns::netns::NetNs>, Option<String>) {
    let mut egress_dns: Option<String> = None;
    let netns = match ply_core::paths::is_root() {
        true => None,
        false => match ply_core::runtime::ns::netns::NetNs::create()
            .and_then(|ns| ns.enter_user().map(|()| ns))
        {
            // This process now owns the namespaces; the network is still the
            // host's, so members can fetch what they need. Each joins the
            // stack's network itself, once it is ready to launch.
            Ok(mut ns) => {
                // A namespace with no way out is worse than none for an app
                // that calls anything: say what happened either way.
                match ns.attach_egress() {
                    Ok(router) => {
                        eprintln!("ply up: stack network ({router})");
                        egress_dns = Some(ply_core::runtime::ns::netns::EGRESS_DNS.to_string());
                    }
                    Err(e) => eprintln!("ply up: stack network, no outbound — {e}"),
                }
                Some(ns)
            }
            Err(e) => {
                eprintln!("ply up: {e}");
                eprintln!("ply up: falling back to the host's network for this run");
                None
            }
        },
    };
    (netns, egress_dns)
}

/// One network for the stack: an L2 switch inside THIS process, listening
/// on a unix socket under `run_dir()`, which every member's `ply run` joins
/// with `--vswitch`.
///
/// It is a process and not a daemon for the same reason the Linux one is a
/// namespace this process owns: it must die when the stack does. macOS
/// grants no tap device without `com.apple.vm.networking`, which is
/// restricted, so the whole network is userspace — and userspace in
/// somebody's address space is userspace with an owner.
///
/// # Every member's address is reserved BEFORE anything starts
///
/// A guest's `/etc/hosts` is a copy taken when it boots, so a peer that has
/// no address yet would get no line and stay unnamed in that guest for the
/// rest of its life. Reserving here — in stack order, so the addresses are
/// the same on every run of one stack file — means `web` boots knowing where
/// `db` will be even if `db` has not been spawned yet, and a member that
/// restarts comes back on the address its peers already wrote down.
///
/// The reservation is made under `<name>.1` with `<name>` aliased onto it,
/// which is exactly what `VmBackend::launch` does for slot 1 — allocating a
/// bare `<name>` here instead would hand the alias an address no machine
/// ever takes.
#[cfg(not(target_os = "linux"))]
struct StackNet {
    /// Held, never read: dropping it stops the switch thread and unlinks
    /// the socket, which is the whole of this type's job.
    _server: ply_core::runtime::vm::switch::unix::Server,
    path: std::path::PathBuf,
}

#[cfg(not(target_os = "linux"))]
impl StackNet {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(not(target_os = "linux"))]
fn stack_network(members: &[String]) -> (Option<StackNet>, Option<String>) {
    use ply_core::runtime::vm::switch;

    let path = ply_core::paths::run_dir()
        .join("switch")
        .join(format!("up-{}.sock", std::process::id()));
    let (server, warning) = match switch::unix::Server::start(&path) {
        Ok(started) => started,
        Err(e) => {
            eprintln!("ply up: no stack network — {e}");
            eprintln!("ply up: each member will run on a private network of its own");
            return (None, None);
        }
    };
    if let Some(warning) = warning {
        // Without the socket there is a switch and no way for a member to
        // reach it, which is the same as having none. Say so and fall back
        // to what a stack did before this existed: a private switch per
        // member, and no `<name>.ply` between them.
        eprintln!("ply up: no stack network — {warning}");
        eprintln!("ply up: each member will run on a private network of its own");
        return (None, None);
    }
    for member in members {
        // `allocate`, not `attach`: a reservation is an entry in the name
        // table and nothing else. Attaching here would put a member on the
        // fabric that no guest is behind, and the real one arriving later
        // would be the SECOND holder of that MAC.
        let ip = server.switch().allocate(&format!("{member}.1"));
        server.switch().alias(member, ip);
    }
    eprintln!(
        "ply up: stack network (userspace switch, {}/{})",
        switch::GATEWAY,
        switch::PREFIX_LEN
    );
    (
        Some(StackNet {
            _server: server,
            path,
        }),
        Some(switch::GATEWAY.to_string()),
    )
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

/// The member's `run =` spec exactly as written in the stack — `postgres@17`,
/// `./server`, `https://…` — for `Prepared.run_spec`/`--plan`'s header.
/// Never what `prepare_target` resolves it to (a store path, a built dir).
fn describe_run_spec(source: &MemberSource) -> String {
    match source {
        MemberSource::Run { name, version } => match version {
            Some(v) => format!("{name}@{v}"),
            None => name.clone(),
        },
        MemberSource::Path(p) => p.display().to_string(),
        MemberSource::Url(u) => u.clone(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ply_core::params::Resolved;
    use ply_core::stack::{EnvSource, ResolvedEnv};
    use std::collections::BTreeSet;

    // --- image_fact (pure helper) -------------------------------------------

    #[test]
    fn image_fact_by_source_kind() {
        let run = MemberSource::Run {
            name: "postgres".into(),
            version: Some("17".into()),
        };
        assert_eq!(
            image_fact(&run, "/store/sha256:deadbeef/pkg.img"),
            Some("sha256:deadbeef".to_string()),
            "the digest is the store dir the image file sits in"
        );
        let path = MemberSource::Path(PathBuf::from("./server"));
        assert_eq!(
            image_fact(&path, "/abs/server"),
            None,
            "unbuilt local dir has no image yet"
        );
        let url = MemberSource::Url("https://example.com/app.img".into());
        assert_eq!(
            image_fact(&url, "https://example.com/app.img"),
            Some("https://example.com/app.img".to_string())
        );
    }

    // --- render_plan -----------------------------------------------------------

    #[test]
    fn render_plan_masks_secrets_and_annotates_derived_waits() {
        let db = Prepared {
            member: "db".to_string(),
            target: "unused-by-render-plan".to_string(),
            run_spec: "postgres@17".to_string(),
            source: MemberSource::Run {
                name: "postgres".to_string(),
                version: Some("17".to_string()),
            },
            env: Vec::new(),
            params: BTreeMap::new(),
            after: Vec::new(),
            publish: vec!["internal:5432".to_string()],
            volume: Vec::new(),
            domain: Vec::new(),
            scale: None,
        };
        let server = Prepared {
            member: "server".to_string(),
            target: "unused-by-render-plan".to_string(),
            run_spec: "./server".to_string(),
            source: MemberSource::Path(PathBuf::from("./server")),
            env: vec![("DATABASE_URL".to_string(), "{db.url}".to_string())],
            params: BTreeMap::new(),
            after: Vec::new(),
            publish: vec!["internal:3001".to_string()],
            volume: Vec::new(),
            domain: Vec::new(),
            scale: None,
        };
        let prepared = vec![db, server];

        let mut db_ns = BTreeMap::new();
        db_ns.insert(
            "password".to_string(),
            Ok(Resolved {
                value: "s3cr3t-pw".to_string(),
                secret: true,
            }),
        );
        db_ns.insert(
            "api_key".to_string(),
            Ok(Resolved {
                value: "(will mint)".to_string(),
                secret: true,
            }),
        );
        let namespaces = BTreeMap::from([
            ("db".to_string(), db_ns),
            ("server".to_string(), BTreeMap::new()),
        ]);

        let env = BTreeMap::from([
            (
                "db".to_string(),
                vec![
                    ResolvedEnv {
                        key: "POSTGRES_DB".to_string(),
                        value: "todos".to_string(),
                        secret: false,
                        source: EnvSource::Override,
                    },
                    ResolvedEnv {
                        key: "POSTGRES_PASSWORD".to_string(),
                        value: "s3cr3t-pw".to_string(),
                        secret: true,
                        source: EnvSource::Minted("secrets/db.password".to_string()),
                    },
                    ResolvedEnv {
                        key: "API_KEY".to_string(),
                        value: "(will mint)".to_string(),
                        secret: true,
                        source: EnvSource::Minted("secrets/db.api_key".to_string()),
                    },
                ],
            ),
            (
                "server".to_string(),
                vec![
                    ResolvedEnv {
                        key: "DATABASE_URL".to_string(),
                        value: "postgres://postgres:s3cr3t-pw@db.ply:5432/todos".to_string(),
                        secret: true,
                        source: EnvSource::ParamRef("{db.url}".to_string()),
                    },
                    ResolvedEnv {
                        key: "NODE_ENV".to_string(),
                        value: "production".to_string(),
                        secret: false,
                        source: EnvSource::SelfEnv,
                    },
                ],
            ),
        ]);

        let waits = BTreeMap::from([
            ("db".to_string(), Vec::new()),
            ("server".to_string(), vec!["db".to_string()]),
        ]);

        // Only the LEAF secret, never the computed `url` it taints — that's
        // the whole fix findings 1/2 asked for (a hand-built `namespaces`
        // above still carries `password` as `secret: true`, matching what
        // `resolve_members` would produce, but `--plan` no longer scrapes
        // `namespaces` for maskable substrings).
        let secret_values = BTreeSet::from(["s3cr3t-pw".to_string()]);

        let resolution = Resolution {
            namespaces,
            env,
            waits,
            secret_values,
        };

        let plan = render_plan(&resolution, &prepared);

        // masked url line: the secret substring is gone, the composed shape
        // (scheme, user, host, port, database) still reads.
        assert!(
            plan.contains("postgres://postgres:********@db.ply:5432/todos"),
            "{plan}"
        );
        assert!(!plan.contains("s3cr3t-pw"), "{plan}");

        // (will mint): not masked — it names a future secret, not today's.
        assert!(plan.contains("(will mint)"), "{plan}");

        // derived edge (no explicit `after` on server), annotated with the
        // first {db.*} ref in server's env.
        assert!(plan.contains("after: db (via {db.url})"), "{plan}");

        // a `secret: false` value prints verbatim, with its source label.
        let node_env_line = plan.lines().find(|l| l.contains("NODE_ENV")).unwrap();
        assert!(node_env_line.contains("= production"), "{node_env_line}");
        assert!(
            node_env_line.trim_end().ends_with("manifest [env]"),
            "{node_env_line}"
        );
    }

    #[test]
    fn mask_value_ignores_an_empty_secret_in_its_own_list() {
        // Without the `!s.is_empty()` guard, `"...".replace("", MASK)`
        // splices `MASK` between every character instead of leaving the
        // value alone.
        let secrets = ["", "S3CR3T"];
        assert_eq!(
            mask_value("user:S3CR3T@host", true, &secrets),
            "user:********@host"
        );
    }

    /// A pure-function test: `render_wait` scans publish/domain, and these
    /// `Prepared` values are built by hand. Stack PARSE now rejects a `{}`
    /// hole in `publish`/`domain` (v1 never interpolated them), so this
    /// input no longer arrives from a real stack file — the scan itself is
    /// kept, mirroring `stack::derived_after`, and so is its test.
    #[test]
    fn render_wait_scans_publish_and_domain_too_not_only_env() {
        fn web(publish: Vec<String>, domain: Vec<String>) -> Prepared {
            Prepared {
                member: "web".to_string(),
                target: "unused-by-render-wait".to_string(),
                run_spec: "./web".to_string(),
                source: MemberSource::Path(PathBuf::from("./web")),
                env: Vec::new(),
                params: BTreeMap::new(),
                after: Vec::new(),
                publish,
                volume: Vec::new(),
                domain,
                scale: None,
            }
        }

        // A ref that lives only in `publish` (no explicit `after`, no env
        // hole) still gets the `via` annotation — mirrors
        // `stack::derived_after`'s own edge scan, which unions env with
        // publish and domain.
        let via_publish = web(vec!["internal:{db.port}".to_string()], Vec::new());
        assert_eq!(render_wait(&via_publish, "db"), "db (via {db.port})");

        let via_domain = web(Vec::new(), vec!["{cdn.hostname}".to_string()]);
        assert_eq!(render_wait(&via_domain, "cdn"), "cdn (via {cdn.hostname})");

        // still bare when nothing at all — env, publish, or domain — names
        // the ref (e.g. an explicit `after` with no corresponding `{}`).
        let nothing = web(Vec::new(), Vec::new());
        assert_eq!(render_wait(&nothing, "queue"), "queue");
    }

    // --- the spec's canonical three-member todos plan, exactly -------------

    /// `db` (a leaf secret `password`, minted; `database` stack-overridden),
    /// a computed `db.url` that embeds the leaf, `server`'s `DATABASE_URL`
    /// built from `{db.url}`, `web`'s `SERVER_URL` built from
    /// `{server.base_url}`. `extra_leaf_secrets` lets a test adversarially
    /// widen `secret_values` (e.g. with an empty string) without touching
    /// anything else, to prove `mask_value`'s own filtering doesn't lean on
    /// `resolve_members` alone to keep that set clean.
    fn todos_fixture(extra_leaf_secrets: &[&str]) -> (Vec<Prepared>, Resolution) {
        let db = Prepared {
            member: "db".to_string(),
            target: "unused-by-render-plan".to_string(),
            run_spec: "postgres@17".to_string(),
            source: MemberSource::Run {
                name: "postgres".to_string(),
                version: Some("17".to_string()),
            },
            env: Vec::new(),
            params: BTreeMap::new(),
            after: Vec::new(),
            publish: vec!["internal:5432".to_string()],
            volume: Vec::new(),
            domain: Vec::new(),
            scale: None,
        };
        let server = Prepared {
            member: "server".to_string(),
            target: "unused-by-render-plan".to_string(),
            run_spec: "./server".to_string(),
            source: MemberSource::Path(PathBuf::from("./server")),
            env: vec![("DATABASE_URL".to_string(), "{db.url}".to_string())],
            params: BTreeMap::new(),
            after: Vec::new(),
            publish: vec!["internal:3001".to_string()],
            volume: Vec::new(),
            domain: Vec::new(),
            scale: None,
        };
        let web = Prepared {
            member: "web".to_string(),
            target: "unused-by-render-plan".to_string(),
            run_spec: "./web".to_string(),
            source: MemberSource::Path(PathBuf::from("./web")),
            env: vec![("SERVER_URL".to_string(), "{server.base_url}".to_string())],
            params: BTreeMap::new(),
            after: Vec::new(),
            publish: vec!["3000".to_string()],
            volume: Vec::new(),
            domain: Vec::new(),
            scale: None,
        };
        let prepared = vec![db, server, web];

        let leaf = "S3CR3T".to_string();
        let composed_url = format!("postgres://postgres:{leaf}@db.ply:5432/todos");

        let db_ns = BTreeMap::from([
            (
                "password".to_string(),
                Ok(Resolved {
                    value: leaf.clone(),
                    secret: true,
                }),
            ),
            (
                // A computed param tainted BY the leaf, not a leaf itself —
                // this is exactly the value finding 1 was about: it must
                // never be mistaken for a second maskable substring (it
                // already contains the real leaf, which IS in
                // `secret_values` below).
                "url".to_string(),
                Ok(Resolved {
                    value: composed_url.clone(),
                    secret: true,
                }),
            ),
        ]);
        let namespaces = BTreeMap::from([
            ("db".to_string(), db_ns),
            ("server".to_string(), BTreeMap::new()),
            ("web".to_string(), BTreeMap::new()),
        ]);

        let env = BTreeMap::from([
            (
                "db".to_string(),
                vec![
                    ResolvedEnv {
                        key: "POSTGRES_DB".to_string(),
                        value: "todos".to_string(),
                        secret: false,
                        source: EnvSource::Override,
                    },
                    ResolvedEnv {
                        key: "POSTGRES_PASSWORD".to_string(),
                        value: leaf.clone(),
                        secret: true,
                        source: EnvSource::Minted("secrets/db.password".to_string()),
                    },
                ],
            ),
            (
                "server".to_string(),
                vec![ResolvedEnv {
                    key: "DATABASE_URL".to_string(),
                    value: composed_url,
                    secret: true,
                    source: EnvSource::ParamRef("{db.url}".to_string()),
                }],
            ),
            (
                "web".to_string(),
                vec![ResolvedEnv {
                    key: "SERVER_URL".to_string(),
                    value: "http://server.ply:3001".to_string(),
                    secret: false,
                    source: EnvSource::ParamRef("{server.base_url}".to_string()),
                }],
            ),
        ]);

        let waits = BTreeMap::from([
            ("db".to_string(), Vec::new()),
            ("server".to_string(), vec!["db".to_string()]),
            ("web".to_string(), vec!["server".to_string()]),
        ]);

        let mut secret_values: BTreeSet<String> = BTreeSet::from([leaf]);
        secret_values.extend(extra_leaf_secrets.iter().map(|s| s.to_string()));

        (
            prepared,
            Resolution {
                namespaces,
                env,
                waits,
                secret_values,
            },
        )
    }

    /// The expected plan, verbatim — spec-shaped headers (`(postgres@17)`,
    /// `(./server)`, `(./web)`), two-space part separators, a derived
    /// `after: db (via {db.url})`/`after: server (via {server.base_url})`,
    /// and the password masked IN PLACE inside `DATABASE_URL`'s composed
    /// value — never masking that whole value (finding 1's regression).
    const EXPECTED_TODOS_PLAN: &str = "\
db (postgres@17)  internal:5432
  POSTGRES_DB       = todos     params (stack override)
  POSTGRES_PASSWORD = ********  minted  secrets/db.password
server (./server)  internal:3001  after: db (via {db.url})
  DATABASE_URL = postgres://postgres:********@db.ply:5432/todos  {db.url}
web (./web)  3000  after: server (via {server.base_url})
  SERVER_URL = http://server.ply:3001  {server.base_url}
";

    #[test]
    fn render_plan_renders_the_full_composed_todos_plan_exactly() {
        let (prepared, resolution) = todos_fixture(&[]);
        let plan = render_plan(&resolution, &prepared);
        assert_eq!(plan, EXPECTED_TODOS_PLAN);
    }

    #[test]
    fn an_empty_leaf_secret_in_resolution_never_shreds_any_line() {
        let (prepared, resolution) = todos_fixture(&[""]);
        let plan = render_plan(&resolution, &prepared);
        assert_eq!(
            plan, EXPECTED_TODOS_PLAN,
            "an empty leaf secret in `secret_values` must be a no-op"
        );
    }
}
