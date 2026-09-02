//! `ply reconcile` — make systemd agree with /var/lib/ply/deployments/.
//!
//! Runs as a oneshot from ply-deployments.path (kernel inotify on the dir),
//! or by hand. Idempotent by construction: it converges, never accumulates.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ply_core::deployments::{self, Spec, UNIT_MARKER};

const UNIT_DIR: &str = "/etc/systemd/system";

pub fn exec(args: crate::cli::ReconcileArgs) -> Result<()> {
    let force = args.force;
    if !ply_core::paths::is_root() {
        bail!("ply reconcile writes systemd units — run as root");
    }
    fleet_sync(); // fleet hosts: pull the repo first, then converge on it
    let files = deployments::list_files()?;
    let mut desired: BTreeSet<String> = BTreeSet::new();
    let mut app_names: BTreeSet<String> = BTreeSet::new();
    let mut changed_units = false;

    for (name, path) in files {
        if !valid_name(&name) {
            deployments::write_status(&name, false, "deployment names are [a-z0-9-]");
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                deployments::write_status(&name, false, &format!("read: {e}"));
                continue;
            }
        };
        // A file with [[app]] is a stack — expand into one unit per member;
        // otherwise it's a single-app deployment.
        match ply_core::stack::parse(&text, &path) {
            Ok(Some(stack)) => {
                // Cadence at the stack-file granularity; held → keep the
                // member units alive but don't re-converge this beat.
                if held(&name, true, force) {
                    for m in &stack.members {
                        desired.insert(m.name.clone());
                    }
                    continue;
                }
                converge_stack(
                    &name,
                    stack,
                    &mut desired,
                    &mut app_names,
                    &mut changed_units,
                );
            }
            Ok(None) => {
                // …or it NAMES a published stack: `stack = "<ns>/<name>"`.
                if let Some(r) = ply_core::stack::parse_ref(&text) {
                    converge_stack_ref(
                        &name,
                        &r,
                        force,
                        &mut desired,
                        &mut app_names,
                        &mut changed_units,
                    );
                    continue;
                }
                let mut spec = match Spec::parse(&text) {
                    Ok(spec) => spec,
                    Err(e) => {
                        // A file that no longer parses is NOT a removed file.
                        // The unit it owns stays desired, or a one-character
                        // typo saved into a live deployment's spec would have
                        // the sweep below disable and delete it.
                        desired.insert(name.clone());
                        deployments::write_status(&name, false, &format!("spec: {e}"));
                        continue;
                    }
                };
                // Cadence discipline FIRST — before anything that can write
                // a status. A spec file touched at/after its last status is
                // an explicit order; untouched, auto = false holds and a
                // recent failure backs off. The env_file read below used to
                // run ahead of this and stamp a fresh failure every beat,
                // which meant the back-off never expired and `touch` could
                // not break it.
                if held(&name, spec.auto, force) {
                    desired.insert(name.clone());
                    continue;
                }
                // `.env/<name>.env` by convention when the spec names no
                // env_file. docs/deployments.md already TELLS people the file
                // is named after the deployment; restating it in the spec was
                // pure redundancy. Applied before the lookup so the
                // conventional file fills `$VAR` holes too.
                if spec.env_file.is_none() {
                    let rel = format!(".env/{name}.env");
                    if std::path::Path::new(&resolve_secret(&rel)).is_file() {
                        // Announced, not silent: with the binding implicit,
                        // reading the spec no longer tells you the app takes
                        // secrets, so the reconcile beat has to.
                        eprintln!("ply: {name}: using {rel} (by convention)");
                        spec.env_file = Some(rel);
                    }
                }
                // An unreadable or malformed env_file is this deployment's
                // failure — its own message, not a downstream "$X is not set"
                // that hides the cause.
                let lookup = match spec_env_lookup(&spec) {
                    Ok(l) => l,
                    Err(e) => {
                        desired.insert(name.clone());
                        deployments::write_status(&name, false, &e);
                        eprintln!("ply: reconcile {name}: {e}");
                        continue;
                    }
                };
                // `$VAR` holes, filled exactly as a stack member's are: from
                // this deployment's own env_file, then the process
                // environment. Without this a standalone deployment had no
                // way to say "I need SUPER_SECRET" — an unexpanded `$VAR`
                // was injected as the literal string, silently.
                if let Err(e) = expand_spec_holes(&lookup, &mut spec) {
                    deployments::write_status(&name, false, &format!("{e:#}"));
                    desired.insert(name.clone());
                    continue;
                }
                match apply(&name, &spec, &mut app_names, None) {
                    Ok(applied) => {
                        changed_units |= applied.changed;
                        desired.insert(name.clone());
                        deployments::write_status(&name, true, &applied.detail);
                        // journal state changes only — "unchanged" is silence
                        if !applied.detail.starts_with("unchanged") {
                            ply_core::runtime::events::emit(&name, "deploy", &applied.detail);
                        }
                    }
                    Err(e) => {
                        // A failed attempt is NOT a deleted spec: the
                        // deployment stays desired, its unit untouched. Only a
                        // removed FILE may orphan a unit.
                        desired.insert(name.clone());
                        deployments::write_status(&name, false, &format!("{e:#}"));
                        ply_core::runtime::events::emit(&name, "deploy-failed", &format!("{e:#}"));
                        eprintln!("ply: reconcile {name}: {e:#}");
                    }
                }
            }
            Err(e) => {
                // has [[app]] but malformed, or the file is not valid TOML.
                // Same rule: a broken file is not a deleted one. We do not
                // know which units it owned, so keep whatever the stack
                // remembered last time it converged (as the stack-ref lane
                // does) — and at minimum the unit under this file's own name.
                desired.insert(name.clone());
                let remembered = deployments::status_dir().join(format!("{name}.members"));
                if let Ok(text) = std::fs::read_to_string(&remembered) {
                    for m in text.lines().map(str::trim).filter(|m| !m.is_empty()) {
                        desired.insert(m.to_string());
                    }
                }
                deployments::write_status(&name, false, &format!("{e:#}"));
                continue;
            }
        }
    }

    warn_orphaned_env_files(&app_names);

    // Managed units whose spec is gone: stop and remove. Only ours — the
    // marker keeps hand-written ply-*.service units untouchable.
    for entry in std::fs::read_dir(UNIT_DIR).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = file
            .strip_prefix("ply-")
            .and_then(|s| s.strip_suffix(".service"))
        else {
            continue;
        };
        if desired.contains(stem) {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        if !text.starts_with(UNIT_MARKER) {
            continue;
        }
        println!("removing ply-{stem} (deployment file deleted)");
        let _ = run("systemctl", &["disable", "--now", &format!("ply-{stem}")]);
        let _ = std::fs::remove_file(entry.path());
        let _ = std::fs::remove_file(deployments::status_path(stem));
        changed_units = true;
    }

    if changed_units {
        run("systemctl", &["daemon-reload"])?;
    }
    Ok(())
}

/// Every store path ends in pkg.img; give the unit a path whose basename
/// is the image's real name so state (and any UI) can read the version.
/// Hardlink into a per-deployment dir, older versions swept.
fn named_image(name: &str, path: &std::path::Path, shown: &str) -> Result<PathBuf> {
    let dir = PathBuf::from("/var/lib/ply/deploys").join(name);
    std::fs::create_dir_all(&dir)?;
    let named = dir.join(shown);
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        // sweep superseded images only — dot-files (the releases ETag
        // cache lives here) are not ours to clean
        if entry.path() != named && !entry.file_name().to_string_lossy().starts_with('.') {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    if !named.exists() {
        std::fs::hard_link(path, &named)
            .or_else(|_| std::fs::copy(path, &named).map(|_| ()))
            .with_context(|| format!("linking {shown} into {}", dir.display()))?;
    }
    Ok(named)
}

const FAIL_BACKOFF_SECS: u64 = 600;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Was the spec file modified at/after its last status write? No status
/// at all also counts as touched — a first deploy must converge.
fn touched_since_status(name: &str) -> bool {
    let Some((_, ts)) = deployments::read_status(name) else {
        return true;
    };
    let mtime = std::fs::metadata(deployments::spec_path(name))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(u64::MAX);
    mtime >= ts
}

struct Applied {
    changed: bool,
    detail: String,
}

/// Cadence: hold at the current artifact when the file wasn't touched since
/// its last status and either it's manual (`auto = false`) or a recent
/// failure is still backing off. A stack file is always `auto = true` (its
/// members are registry apps that follow the latest, like an `app=` spec).
fn held(name: &str, auto: bool, force: bool) -> bool {
    // `ply reconcile --force`: a person is at the keyboard saying "now".
    // Skips the failure back-off (and an auto = false pin) for this beat
    // only — the status a failed attempt writes is unchanged, so the next
    // timer beat backs off again as it should.
    if force || touched_since_status(name) {
        return false;
    }
    match deployments::read_status(name) {
        Some((ok, ts)) => !auto || (!ok && now_unix().saturating_sub(ts) < FAIL_BACKOFF_SECS),
        None => false,
    }
}

/// Expand a stack file into one managed unit per member. Each member runs
/// under `--name <member>` (its `.ply` name, its `--after` target), so the
/// members wire to each other exactly as they do under `ply up`. `$VAR`
/// holes fill from the stack's `env_file` (root-owned, resolved against the
/// deployments dir) plus the process environment. A member that fails leaves
/// its unit in place (desired), like any single deployment.
/// A deployment that names a published stack (`stack = "<ns>/<name>"`).
/// Fetched every beat: the reference is unversioned, so republishing the
/// stack converges its new SHAPE exactly as an unpinned member converges a
/// new image.
///
/// The member names of the last good fetch are remembered in `.status/`.
/// Without that, one unreachable registry would look like "this stack has no
/// members" and retire every unit it owns — a network blip must never be
/// indistinguishable from a deletion.
fn converge_stack_ref(
    name: &str,
    r: &ply_core::stack::StackRef,
    force: bool,
    desired: &mut BTreeSet<String>,
    app_names: &mut BTreeSet<String>,
    changed_units: &mut bool,
) {
    let remembered = deployments::status_dir().join(format!("{name}.members"));
    let keep_known_members = |desired: &mut BTreeSet<String>| {
        if let Ok(text) = std::fs::read_to_string(&remembered) {
            for m in text.lines().map(str::trim).filter(|m| !m.is_empty()) {
                desired.insert(m.to_string());
            }
        }
    };

    let reference = r.reference.as_str();
    if held(name, r.auto, force) {
        keep_known_members(desired);
        return;
    }

    let mut stack =
        match ply_core::catalog::fetch_stack(reference, ply_core::catalog::OFFICIAL_RUN_SOURCE) {
            Ok(stack) => stack,
            Err(e) => {
                keep_known_members(desired);
                deployments::write_status(name, false, &format!("stack {reference}: {e:#}"));
                return;
            }
        };
    // The published stack is the PUBLISHER's template; its `$VAR` holes are
    // this host's business. A deployer's `env_file` on the reference is how
    // they get filled — it replaces whatever `[stack] env_file` the publisher
    // baked in, which named a path on THEIR machine. Before this the key was
    // silently dropped and every hole failed as "not set".
    if let Some(ef) = &r.env_file {
        stack.env_file = Some(ef.clone());
    }

    let members: Vec<String> = stack.members.iter().map(|m| m.name.clone()).collect();
    converge_stack(name, stack, desired, app_names, changed_units);

    // Written only after a converge: the file answers "what did this stack
    // own when we last actually knew?", which is the question the failure
    // path above asks.
    let dir = deployments::status_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        let tmp = dir.join(format!(".{name}.members.tmp"));
        if std::fs::write(&tmp, members.join("\n") + "\n").is_ok() {
            let _ = std::fs::rename(&tmp, &remembered);
        }
    }
}

fn converge_stack(
    name: &str,
    stack: ply_core::stack::Stack,
    desired: &mut BTreeSet<String>,
    app_names: &mut BTreeSet<String>,
    changed_units: &mut bool,
) {
    let mut file_env: std::collections::BTreeMap<String, String> = Default::default();
    if let Some(ef) = &stack.env_file {
        let path = resolve_secret(ef);
        match ply_core::runtime::run::parse_env_file(std::path::Path::new(&path)) {
            Ok(pairs) => file_env = pairs.into_iter().collect(),
            Err(e) => {
                deployments::write_status(name, false, &format!("env_file {path}: {e:#}"));
                // keep the members' units alive despite the missing secrets
                for m in &stack.members {
                    desired.insert(m.name.clone());
                }
                return;
            }
        }
    }
    let lookup = |k: &str| file_env.get(k).cloned().or_else(|| std::env::var(k).ok());
    let stack_label = stack.name.as_deref();

    let mut oks = 0usize;
    let mut errs: Vec<String> = Vec::new();

    // --- fetch every member first -------------------------------------------
    // A member's `{db.url}` resolves out of `db`'s manifest, and that
    // manifest lives inside `db`'s image — so nothing can be applied until
    // every image is on this host. A member that fails here is reported
    // exactly as an apply failure is, and the rest still converge; what it
    // cannot do is disappear quietly, because a member referencing its
    // namespace will fail below naming it.
    let mut pending: Vec<(String, Spec, Fetched)> = Vec::new();
    let mut inputs: Vec<ply_core::stack::MemberInput> = Vec::new();
    // Who depends on whom, for every member — including the ones that fail
    // below, since it is precisely their dependants that must not converge.
    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut failed: BTreeSet<String> = BTreeSet::new();
    for member in &stack.members {
        // reserve the unit first: a failed beat must not orphan it
        desired.insert(member.name.clone());
        edges.insert(member.name.clone(), member_edges(member));
        let spec = match Spec::from_stack_member(member, stack_label, &lookup) {
            Ok(s) => s,
            Err(e) => {
                deployments::write_status(&member.name, false, &format!("{e:#}"));
                errs.push(format!("{}: {e}", member.name));
                failed.insert(member.name.clone());
                continue;
            }
        };
        let prepared = (|| -> Result<(Fetched, ply_core::stack::MemberInput)> {
            let fetched = fetch_image(&member.name, &spec)?;
            let image = fetched.image.display().to_string();
            let manifest = ply_core::stack::member_manifest(&image, None)
                .with_context(|| format!("reading the manifest inside {}", fetched.shown))?;
            let input = ply_core::stack::MemberInput {
                name: member.name.clone(),
                version: Some(manifest.package.version.to_string()),
                manifest: Some(manifest),
                // the member's own written order, `$VAR` expanded and `{}`
                // holes still raw — `spec.env` is a map, and the resolver
                // reads these in order
                env: ply_core::stack::expand_member_env(member, &lookup)?,
                params: ply_core::stack::expand_member_params(member, &lookup)?,
                after: member.after.clone(),
                publish: spec.publish.clone(),
                domain: spec.domain.clone(),
                port: ply_core::stack::container_port(&spec.publish),
                scale: member.scale,
                image: fetched.image_fact.clone(),
            };
            Ok((fetched, input))
        })();
        match prepared {
            Ok((fetched, input)) => {
                pending.push((member.name.clone(), spec, fetched));
                inputs.push(input);
            }
            Err(e) => {
                deployments::write_status(&member.name, false, &format!("{e:#}"));
                ply_core::runtime::events::emit(&member.name, "deploy-failed", &format!("{e:#}"));
                eprintln!("ply: reconcile {}: {e:#}", member.name);
                errs.push(format!("{}: {e}", member.name));
                failed.insert(member.name.clone());
            }
        }
    }

    // --- take the failures and their dependants out of the beat -------------
    // A member whose peer is missing cannot be resolved (its `{db.url}` has
    // no namespace to read) and must not be started ahead of it either — so
    // it sits this beat out, keeping the unit it already has. A member that
    // depends on nothing broken is unaffected: it resolves and converges as
    // usual, and never carries a diagnostic about someone else.
    if !failed.is_empty() {
        let blocked = blocked_by_failures(&failed, &edges);
        let mut keep_pending = Vec::with_capacity(pending.len());
        let mut keep_inputs = Vec::with_capacity(inputs.len());
        for ((member, spec, fetched), input) in pending.into_iter().zip(inputs) {
            // `Some(None)` — a member's own failure — never reaches here:
            // one that failed above was never pushed to `pending`.
            if let Some(Some(dep)) = blocked.get(&member) {
                let detail = format!(
                    "waiting: `{dep}` did not converge this beat, and this member depends on it"
                );
                deployments::write_status(&member, false, &detail);
                eprintln!("ply: reconcile {member}: {detail}");
                errs.push(format!("{member}: depends on `{dep}`"));
                continue;
            }
            keep_pending.push((member, spec, fetched));
            keep_inputs.push(input);
        }
        pending = keep_pending;
        inputs = keep_inputs;
    }

    // --- resolve the whole stack --------------------------------------------
    // Same resolver `ply up` runs, so a `{}` ref means the same thing here;
    // secrets come from this host's own store (`.secrets/<stack>/`), minted
    // on first converge.
    let secrets = ply_core::secrets::SecretStore::for_deployments(name);
    let resolution = match ply_core::stack::resolve_stack(&inputs, Some(&secrets), false) {
        Ok(resolution) => resolution,
        Err(e) => {
            // What is left resolves as a whole or not at all — writing
            // units for the members that happen to resolve, while their
            // peers keep yesterday's values, is a half-wired stack nobody
            // asked for. A MISSING member is not this arm's case any more
            // (the exclusion above already took those out with their
            // dependants); reaching here means the stack file itself is
            // wrong — a `{db.typo}`, an undeclared param — which is every
            // member's problem and nobody's to route around. Every unit
            // stays exactly as it is; the failure names the member and the
            // ref.
            let detail = format!("resolving stack params: {e}");
            for (member, _, _) in &pending {
                deployments::write_status(member, false, &detail);
            }
            eprintln!("ply: reconcile {name}: {detail}");
            errs.push(detail);
            write_stack_status(name, oks, &errs);
            return;
        }
    };

    // --- apply, one unit per member -----------------------------------------
    for (member, mut spec, fetched) in pending {
        let entries = resolution
            .env
            .get(&member)
            .map(Vec::as_slice)
            .unwrap_or_default();
        // Secret-tainted values never reach the unit: units live
        // world-readable under /etc/systemd/system, the env file is 0600.
        let (flags, file) = split_env(entries);
        spec.env = flags.into_iter().collect();
        let had_secrets = !file.is_empty();
        match write_member_secrets_file(name, &member, &file) {
            Ok(path) => spec.env_file = path,
            Err(e) => {
                deployments::write_status(&member, false, &format!("{e:#}"));
                ply_core::runtime::events::emit(&member, "deploy-failed", &format!("{e:#}"));
                eprintln!("ply: reconcile {member}: {e:#}");
                errs.push(format!("{member}: {e}"));
                continue;
            }
        }
        // after ∪ derived_after: a `{db.url}` reference IS a wait, whether
        // or not `after =` names it too — the same list `ply up` gates on.
        if let Some(waits) = resolution.waits.get(&member) {
            spec.after = waits.clone();
        }
        match apply(&member, &spec, app_names, Some(fetched)) {
            Ok(applied) => {
                // The unit on disk no longer names the file: only now is
                // dropping a stale one from an earlier beat safe.
                if !had_secrets {
                    remove_member_secrets_file(name, &member);
                }
                *changed_units |= applied.changed;
                deployments::write_status(&member, true, &applied.detail);
                if !applied.detail.starts_with("unchanged") {
                    ply_core::runtime::events::emit(&member, "deploy", &applied.detail);
                }
                oks += 1;
            }
            Err(e) => {
                deployments::write_status(&member, false, &format!("{e:#}"));
                ply_core::runtime::events::emit(&member, "deploy-failed", &format!("{e:#}"));
                eprintln!("ply: reconcile {member}: {e:#}");
                errs.push(format!("{member}: {e}"));
            }
        }
    }
    write_stack_status(name, oks, &errs);
}

/// The aggregate status on the stack FILE itself — the deploy screen's row
/// for the stack, above its members'.
fn write_stack_status(name: &str, oks: usize, errs: &[String]) {
    if errs.is_empty() {
        deployments::write_status(name, true, &format!("stack: {oks} member(s) ok"));
    } else {
        deployments::write_status(
            name,
            false,
            &format!(
                "stack: {oks} ok, {} failed — {}",
                errs.len(),
                errs.join("; ")
            ),
        );
    }
}

/// The members one stack member depends on: its explicit `after` entries
/// (each parsed down to the app it names — `db`, `db.finish_boot == 'ok'`
/// and friends all order on `db`) unioned with the edges its `{app.param}`
/// references imply. The same union `stack::topo_sort` orders on and
/// `resolve_stack` returns as `waits`; here it answers "who does this
/// member need in order to converge at all?".
fn member_edges(member: &ply_core::stack::Member) -> BTreeSet<String> {
    let mut edges: BTreeSet<String> = member
        .after
        .iter()
        .map(|dep| {
            ply_core::runtime::after::parse_wait(dep)
                .map(|w| w.app)
                .unwrap_or_else(|_| dep.clone())
        })
        .collect();
    edges.extend(ply_core::stack::derived_after(member));
    edges
}

/// Every member that cannot converge this beat, and the peer to name for
/// it: the members in `failed` (whose own error is already recorded, so
/// their value is `None`), plus — transitively — every member that names an
/// excluded one in its `after` or through a `{app.param}` reference, whose
/// value is the excluded dependency it waits on.
///
/// This is what keeps one member's fetch failure from freezing the whole
/// stack. `resolve_stack` is deliberately all-or-nothing (a half-resolved
/// stack is worse than an unchanged one), so a member whose namespace is
/// gone has to be taken out of the resolve set together with everything
/// that reads it — and only that. A member that depends on nothing broken
/// keeps converging, and never gets stamped with somebody else's error.
///
/// `edges` maps each member to the members it depends on (explicit `after`
/// apps ∪ `stack::derived_after`). Pure: names and edges only, no fetching.
fn blocked_by_failures(
    failed: &BTreeSet<String>,
    edges: &BTreeMap<String, BTreeSet<String>>,
) -> BTreeMap<String, Option<String>> {
    let mut blocked: BTreeMap<String, Option<String>> =
        failed.iter().map(|m| (m.clone(), None)).collect();
    // Fixpoint rather than a graph walk: a stack is a handful of members,
    // and this way a cycle (which `stack::parse` already rejects) could not
    // loop forever here either.
    loop {
        let mut grew = false;
        for (member, deps) in edges {
            if blocked.contains_key(member) {
                continue;
            }
            // sorted (BTreeSet): the peer named in the status is stable
            // across beats, not whichever edge happened to be first.
            if let Some(dep) = deps.iter().find(|d| blocked.contains_key(*d)) {
                blocked.insert(member.clone(), Some(dep.clone()));
                grew = true;
            }
        }
        if !grew {
            return blocked;
        }
    }
}

/// What [`split_env`] answers: the plain pairs the unit carries as `-e`
/// flags, and the secret-tainted pairs that go to the 0600 env file.
type EnvSplit = (Vec<(String, String)>, Vec<(String, String)>);

/// Split a member's resolved env into what the unit may carry and what it
/// may not: plain values become `-e KEY=VALUE` flags in the (world-readable)
/// unit, secret-tainted ones become lines in a 0600 env file. A repeated key
/// keeps its LAST entry only — the order `resolve_stack` produces already
/// means "explicit stack `e =` beats provider self-config", and collapsing
/// it here keeps that true across the two delivery channels.
fn split_env(entries: &[ply_core::stack::ResolvedEnv]) -> EnvSplit {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut last: Vec<&ply_core::stack::ResolvedEnv> = Vec::new();
    for entry in entries.iter().rev() {
        if seen.insert(entry.key.as_str()) {
            last.push(entry);
        }
    }
    last.reverse();
    let mut flags = Vec::new();
    let mut file = Vec::new();
    for entry in last {
        let pair = (entry.key.clone(), entry.value.clone());
        if entry.secret {
            file.push(pair);
        } else {
            flags.push(pair);
        }
    }
    (flags, file)
}

/// Where a stack member's secret env lives:
/// `<deployments>/.secrets/<stack>/env/<member>.env`. Inside the same
/// systemd-watch-invisible `.secrets/` directory `SecretStore` uses on a
/// host — a write there must not retrigger the path unit, and nothing but
/// root may read it — but in an `env/` SUBDIRECTORY of it, never beside the
/// secret files themselves. `SecretStore` owns `<member>.<param>` in that
/// directory, so a flat `<member>.env` here would BE the file for a param
/// named `env`: this writer would overwrite an operator's secret and the
/// stale-file sweep would delete it, and `ply secret ls` (which lists every
/// regular file) would report one bogus `<member>.env` secret per member.
/// A subdirectory collides with nothing and `list`'s `is_file()` skips it.
fn member_secrets_path(stack: &str, member: &str) -> PathBuf {
    deployments::dir()
        .join(".secrets")
        .join(stack)
        .join("env")
        .join(format!("{member}.env"))
}

/// Write a member's secret-tainted env as a 0600 file and return its path
/// for the unit's `--env-file`. No secrets → `None`, and NO removal: see
/// [`remove_member_secrets_file`].
fn write_member_secrets_file(
    stack: &str,
    member: &str,
    entries: &[(String, String)],
) -> Result<Option<String>> {
    write_env_file(&member_secrets_path(stack, member), entries)
}

/// Drop a member's now-unneeded secret env file. Best-effort, and called
/// ONLY after `apply` has rewritten the unit that used to name it: until
/// that write lands, the installed unit still carries `--env-file <path>`,
/// and systemd restarting the member in that window would run
/// `ply run --env-file <gone>` and fail. A failed apply leaves the file
/// exactly where the still-installed unit expects it.
fn remove_member_secrets_file(stack: &str, member: &str) {
    let _ = std::fs::remove_file(member_secrets_path(stack, member));
}

/// The write itself: `entries` as a 0600 env file at `path` (temp file +
/// atomic rename, like `SecretStore::set`). Nothing to write is `None` and
/// touches the filesystem not at all.
fn write_env_file(path: &Path, entries: &[(String, String)]) -> Result<Option<String>> {
    if entries.is_empty() {
        return Ok(None);
    }
    let mut body = String::new();
    for (key, value) in entries {
        body.push_str(&env_file_line(key, value)?);
    }
    let dir = path.parent().expect("a secrets file has a parent dir");
    let file = path
        .file_name()
        .expect("a secrets file has a name")
        .to_string_lossy()
        .into_owned();
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .recursive(true)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{file}.tmp"));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(body.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(path.display().to_string()))
}

/// One `KEY="value"` line that `parse_env_file` reads back byte-for-byte.
/// The value is ALWAYS double-quoted: that parser trims the value and then
/// strips one matched pair of surrounding quotes, so quoting is what makes
/// a leading space, a trailing space, or a value that is itself quoted
/// survive the round trip — and an inner `"` needs no escaping, since only
/// the outermost pair is ever stripped. A newline cannot survive a
/// line-oriented file at all, so it is refused (by key — never printing the
/// value) instead of being silently truncated.
fn env_file_line(key: &str, value: &str) -> Result<String> {
    if value.contains('\n') || value.contains('\r') {
        bail!(
            "the resolved value of `{key}` contains a newline — it cannot be delivered \
             through an env file"
        );
    }
    Ok(format!("{key}=\"{value}\"\n"))
}

/// What resolving a deployment's source yields: the image file on this
/// host, the name to show for it, whether a repo rebuild produced new bytes
/// under the same version (which calls for a roll, not a no-op), and the
/// `{image}` built-in fact for a stack member.
struct Fetched {
    image: PathBuf,
    shown: String,
    force_restart: bool,
    /// The `{image}` fact, when the lane has one to give: the store digest
    /// for a registry app, the URL for a URL image — the same two answers
    /// `ply up` gives for the same two member kinds. `None` for the lanes a
    /// stack member cannot use.
    image_fact: Option<String>,
}

/// Resolve a deployment's source lane (`repo`/`url`/`github`/`app`/`image`)
/// down to an image on this host. Split out of [`apply`] because a stack
/// has to fetch EVERY member before it writes any unit: a member's
/// `{db.url}` resolves from `db`'s manifest, and that manifest lives inside
/// `db`'s image.
fn fetch_image(name: &str, spec: &Spec) -> Result<Fetched> {
    let mut force_restart = false;
    let mut image_fact: Option<String> = None;
    let (image, shown): (PathBuf, String) = match (&spec.app, &spec.image, &spec.github) {
        _ if spec.repo.is_some() => {
            let (image, shown, rebuilt) = build_from_repo(name, spec)?;
            force_restart = rebuilt;
            (image, shown)
        }
        _ if spec.url.is_some() => {
            let url = spec.url.as_deref().expect("guard");
            let (path, resolved) = ply_core::source::fetch_url_image(url)
                .with_context(|| format!("fetching {url}"))?;
            println!("{name}: {resolved} (url)");
            image_fact = Some(url.to_string());
            let shown = resolved.to_string();
            (named_image(name, &path, &shown)?, shown)
        }
        (None, None, Some(repo)) => {
            let token = read_token(spec)?;
            let asset_app = spec.asset.clone().unwrap_or_else(|| name.to_string());
            // exact x.y.z pins; anything else follows the latest release —
            // the repo's `latest` marker, or the newest `tag_prefix` match
            // for monorepos with several release streams
            let version = match spec.version.as_deref() {
                Some(v) if v.split('.').count() == 3 => v.to_string(),
                want => {
                    let latest = match spec.tag_prefix.as_deref() {
                        Some(prefix) => {
                            let dir = PathBuf::from("/var/lib/ply/deploys").join(name);
                            std::fs::create_dir_all(&dir)?;
                            ply_core::github::latest_version_matching(
                                repo,
                                prefix,
                                token.as_deref(),
                                &dir.join(".releases-etag"),
                            )?
                        }
                        None => ply_core::github::latest_version(repo, token.as_deref())?,
                    };
                    if let Some(prefix) = want {
                        let matches = latest == prefix || latest.starts_with(&format!("{prefix}."));
                        if !matches {
                            bail!("latest release {latest} does not match version = \"{prefix}\"");
                        }
                    }
                    latest
                }
            };
            let tag = match spec.tag_prefix.as_deref() {
                Some(prefix) => format!("{prefix}{version}"),
                None => format!("v{version}"),
            };
            let store = ply_core::store::Store::open_default()?;
            let (path, resolved) = ply_core::github::fetch_asset(
                repo,
                &asset_app,
                &version,
                &tag,
                token.as_deref(),
                &store,
            )
            .with_context(|| format!("fetching {asset_app} {tag} from {repo} releases"))?;
            println!("{name}: {resolved} (github:{repo})");
            let shown = resolved.to_string();
            (named_image(name, &path, &shown)?, shown)
        }
        (Some(app), None, None) => {
            let source = spec
                .source
                .clone()
                .unwrap_or_else(|| ply_core::catalog::OFFICIAL_RUN_SOURCE.to_string());
            // Ask the deploys dir before downloading: the last beat left the
            // deployed image hardlinked under its own filename. Same version
            // → same file → no fetch. This is what stops an `auto = true`
            // deployment pulling its whole image every minute.
            let deploys = PathBuf::from("/var/lib/ply/deploys").join(name);
            let (path, resolved, digest) = ply_core::catalog::fetch_app_image_unless(
                app,
                spec.version.as_deref(),
                &source,
                |image| {
                    let p = deploys.join(image.to_string());
                    p.is_file().then_some(p)
                },
            )
            .with_context(|| format!("fetching `{app}` from the registry"))?;
            println!("{name}: {resolved}");
            image_fact = Some(digest);
            let shown = resolved.to_string();
            (named_image(name, &path, &shown)?, shown)
        }
        (None, Some(path), None) => {
            let path = PathBuf::from(path);
            if !path.exists() {
                bail!("image {} does not exist on this host", path.display());
            }
            let shown = basename(&path);
            (path, shown)
        }
        _ => unreachable!("Spec::parse enforces exactly one"),
    };
    Ok(Fetched {
        image,
        shown,
        force_restart,
        image_fact,
    })
}

/// Converge one deployment onto its unit. `fetched` is the image a caller
/// already resolved (the stack path, which must fetch every member up
/// front); a single-app deployment passes `None` and resolves its own.
fn apply(
    name: &str,
    spec: &Spec,
    app_names: &mut BTreeSet<String>,
    fetched: Option<Fetched>,
) -> Result<Applied> {
    let Fetched {
        image,
        shown,
        force_restart,
        image_fact: _,
    } = match fetched {
        Some(fetched) => fetched,
        None => fetch_image(name, spec)?,
    };

    // The deployment name is the running app's identity (passed as --name):
    // its state pool, its `.ply` DNS name, its `--after` target. Keying on
    // the file name lets two deployments of one image (two postgres, say)
    // coexist — each addressable as <name>.ply.
    if !app_names.insert(name.to_string()) {
        bail!("two deployments named `{name}` — names must be unique");
    }

    let mut flags = spec.flags();
    flags.push("--name".into());
    flags.push(name.to_string());
    if spec.grant_links {
        flags.push("--grant-links".into());
    }

    let unit_text = format!(
        "{UNIT_MARKER}\n{}",
        ply_core::lifecycle::systemd_unit(&image, &flags, &spec.after, false)?
    );
    let unit_path = PathBuf::from(UNIT_DIR).join(format!("ply-{name}.service"));
    let existing = std::fs::read_to_string(&unit_path).unwrap_or_default();
    if existing == unit_text {
        if force_restart {
            // same unit, new bytes (a repo rebuild): roll, don't restart
            run("systemctl", &["enable", &format!("ply-{name}")])?;
            let verb = roll_or_restart(name, &image);
            return Ok(Applied {
                changed: false,
                detail: format!("{verb} {shown}"),
            });
        }
        // converged already; make sure it's on
        run("systemctl", &["enable", "--now", &format!("ply-{name}")])?;
        return Ok(Applied {
            changed: false,
            detail: format!("unchanged ({shown})"),
        });
    }
    let image_only = image_only_change(&existing, &unit_text);
    std::fs::write(&unit_path, &unit_text)
        .with_context(|| format!("writing {}", unit_path.display()))?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", &format!("ply-{name}")])?;
    // A version bump changes only the image path in the unit: the running
    // parent can roll onto the new image health-gated, zero-downtime at
    // scale >= 2, while the rewritten unit covers the next boot. Any other
    // unit change (env, publish, scale…) needs the parent restarted to
    // take effect.
    let verb = if image_only {
        roll_or_restart(name, &image)
    } else {
        match run("systemctl", &["restart", &format!("ply-{name}")]) {
            Ok(()) => "deployed",
            Err(e) => return Err(e),
        }
    };
    Ok(Applied {
        changed: true,
        detail: format!("{verb} {shown}"),
    })
}

/// Prefer the health-gated roll `ply deploy` does; fall back to a unit
/// restart when there is nothing running to roll (first boot, crashed
/// app) or the roll cannot start.
fn roll_or_restart(name: &str, image: &std::path::Path) -> &'static str {
    match ply_core::lifecycle::deploy(image, 120) {
        Ok(report) if report.complete => "rolled",
        Ok(_) => "rolling (health watch timed out; the roll continues)",
        Err(_) => {
            let _ = run("systemctl", &["restart", &format!("ply-{name}")]);
            "redeployed"
        }
    }
}

/// True when the two unit texts differ ONLY in the image path — the last
/// token of the ExecStart line. That is the version-bump shape; anything
/// else means flags changed and a roll would not apply them.
fn image_only_change(existing: &str, fresh: &str) -> bool {
    if existing.is_empty() {
        return false;
    }
    let old_lines: Vec<&str> = existing.lines().collect();
    let new_lines: Vec<&str> = fresh.lines().collect();
    if old_lines.len() != new_lines.len() {
        return false;
    }
    for (old, new) in old_lines.iter().zip(&new_lines) {
        if old == new {
            continue;
        }
        let (Some(o), Some(n)) = (
            old.strip_prefix("ExecStart="),
            new.strip_prefix("ExecStart="),
        ) else {
            return false;
        };
        let (o_head, _) = o.rsplit_once(' ').unwrap_or(("", o));
        let (n_head, _) = n.rsplit_once(' ').unwrap_or(("", n));
        if o_head != n_head {
            return false;
        }
    }
    true
}

/// A relative secret path (deploy_key, token_file) resolves against the
/// deployments dir — the one path a spec author (the dashboard included)
/// always knows.
/// A secret nothing reads. Now that `.env/<name>.env` is picked up BY NAME,
/// renaming a deployment silently orphans its env file — the app would come
/// up with none of its secrets and no diagnostic. So say it: any `.env/*.env`
/// whose stem matches no deployment and that no spec references by path.
/// Catches the rename, the typo, and the stale secret left after a teardown.
fn warn_orphaned_env_files(_app_names: &BTreeSet<String>) {
    let dir = deployments::dir().join(".env");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return; // no .env/ at all is the normal case, not a problem
    };
    // What reads an env file, across every deployment file SHAPE — not just
    // the specs that reached apply() this beat. The first version built this
    // from `deployments::list()`, which Spec::parse-fails every stack file,
    // so a `[stack] env_file` was reported as an orphan on every beat.
    //   - a single-app spec:  its `env_file`, or `.env/<name>.env` by convention
    //   - a stack file:       its `[stack] env_file`
    //   - a stack reference:  its `env_file`
    // Compared as canonical paths, so `./.env/x.env` and `.env/x.env` agree.
    let canon = |p: &str| {
        let p = std::path::PathBuf::from(resolve_secret(p));
        std::fs::canonicalize(&p).unwrap_or(p)
    };
    let mut referenced: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    for (name, path) in deployments::list_files().unwrap_or_default() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // every deployment name claims its conventional file, whether or not
        // it converged this beat (held, auto = false, backing off…)
        referenced.insert(canon(&format!(".env/{name}.env")));
        if let Ok(Some(stack)) = ply_core::stack::parse(&text, &path) {
            if let Some(ef) = stack.env_file {
                referenced.insert(canon(&ef));
            }
        } else if let Some(r) = ply_core::stack::parse_ref(&text) {
            if let Some(ef) = r.env_file {
                referenced.insert(canon(&ef));
            }
        } else if let Ok(spec) = Spec::parse(&text) {
            if let Some(ef) = spec.env_file {
                referenced.insert(canon(&ef));
            }
        }
    }

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("env") {
            continue;
        }
        let here = std::fs::canonicalize(&path).unwrap_or(path.clone());
        if referenced.contains(&here) {
            continue;
        }
        let stem = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        eprintln!(
            "ply: warning: {} is read by nothing — no deployment named `{stem}`, and no spec \
             names it in `env_file`. A renamed deployment leaves its secrets behind like this.",
            path.display()
        );
    }
}

/// The `$VAR` lookup for a single-app deployment: its own `env_file` first,
/// then the process environment — the same order `converge_stack` uses.
/// A missing or unreadable env_file is an error in its own right, returned
/// to the caller to record — never a silent empty table that later fails as
/// "`$X` is not set" far from the cause.
fn spec_env_lookup(spec: &Spec) -> std::result::Result<impl Fn(&str) -> Option<String>, String> {
    let mut file_env: std::collections::BTreeMap<String, String> = Default::default();
    if let Some(ef) = &spec.env_file {
        let path = resolve_secret(ef);
        match ply_core::runtime::run::parse_env_file(std::path::Path::new(&path)) {
            Ok(pairs) => file_env = pairs.into_iter().collect(),
            Err(e) => return Err(format!("env_file {path}: {e:#}")),
        }
    }
    Ok(move |k: &str| file_env.get(k).cloned().or_else(|| std::env::var(k).ok()))
}

/// Expand `$VAR` in a single-app deployment's `env`, `publish` and `domain`,
/// matching what a stack member already gets. `$$` is a literal `$`, and an
/// undefined variable is an error naming the key.
fn expand_spec_holes(lookup: &impl Fn(&str) -> Option<String>, spec: &mut Spec) -> Result<()> {
    use ply_core::stack::{expand_member_list, expand_vars};
    let who = |k: &str| format!("deployment env {k}");
    let mut env = std::collections::BTreeMap::new();
    for (k, v) in &spec.env {
        env.insert(k.clone(), expand_vars(v, &who(k), lookup)?);
    }
    spec.env = env;
    spec.publish = expand_member_list(&spec.publish, "deployment", "publish", lookup)?;
    spec.domain = expand_member_list(&spec.domain, "deployment", "domain", lookup)?;
    Ok(())
}

fn resolve_secret(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        ply_core::deployments::dir()
            .join(path)
            .display()
            .to_string()
    }
}

/// The fine-grained PAT from token_file, if the spec carries one. One
/// credential covers both git lanes: https clones and release assets.
fn read_token(spec: &Spec) -> Result<Option<String>> {
    match &spec.token_file {
        Some(file) => {
            let path = resolve_secret(file);
            Ok(Some(
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading token_file {path}"))?
                    .trim()
                    .to_string(),
            ))
        }
        None => Ok(None),
    }
}

/// Lane 2: clone/fetch, build in a fenced ply container, `ply build` the
/// checkout. The checkout persists — node_modules and framework caches ARE
/// the cache, so first build pays full price and the rest are incremental.
fn build_from_repo(name: &str, spec: &Spec) -> Result<(PathBuf, String, bool)> {
    let repo = spec.repo.as_deref().expect("caller checked");
    let checkout = PathBuf::from("/var/lib/ply/builds").join(name);
    std::fs::create_dir_all(checkout.parent().unwrap())?;

    let git_ssh = spec.deploy_key.as_deref().map(|key| {
        format!(
            "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new",
            resolve_secret(key)
        )
    });
    // PAT auth for https clones: a per-invocation header (the
    // actions/checkout trick) — the token never lands in .git/config.
    let token_header = read_token(spec)?.map(|token| {
        format!(
            "AUTHORIZATION: basic {}",
            base64(format!("x-access-token:{token}").as_bytes())
        )
    });
    let git = |args: &[&str], cwd: Option<&PathBuf>| -> Result<String> {
        let mut cmd = std::process::Command::new("git");
        if let Some(header) = &token_header {
            cmd.arg("-c").arg(format!("http.extraheader={header}"));
        }
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if let Some(ssh) = &git_ssh {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        let out = cmd.output().context("running git — is it installed?")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    // clone once, fetch forever; reset --hard keeps the tree honest while
    // node_modules (untracked) survives as the cache
    let reference = spec.r#ref.as_deref().unwrap_or("HEAD");
    if !checkout.join(".git").exists() {
        println!("{name}: cloning {repo}");
        git(
            &["clone", "--depth", "1", repo, checkout.to_str().unwrap()],
            None,
        )?;
    }
    git(
        &["fetch", "--depth", "1", "origin", reference],
        Some(&checkout),
    )?;
    git(&["reset", "--hard", "FETCH_HEAD"], Some(&checkout))?;
    git(
        &[
            "clean",
            "-fd",
            "-e",
            "node_modules",
            "-e",
            ".next",
            "-e",
            "target",
            "-e",
            ".ply-build",
            "-e",
            ".npm-cache",
            "-e",
            ".tmp",
            "-e",
            "*.img",
        ],
        Some(&checkout),
    )?;
    let commit = git(&["rev-parse", "--short=12", "HEAD"], Some(&checkout))?;
    let version = repo_version(&checkout, spec);

    // Nothing new to build? (same commit, same spec, image exists) — skip:
    // reconcile fires for every dir change including OTHER deployments'.
    let spec_fingerprint = format!("{commit} {}", spec_hash(spec));
    let marker = checkout.join(".ply-build/built");
    let canonical = checkout.join(format!(
        "{name}-{version}-linux-{}.img",
        ply_core::image::name::Arch::host().as_str()
    ));
    if std::fs::read_to_string(&marker).unwrap_or_default() == spec_fingerprint
        && canonical.exists()
    {
        return Ok((
            canonical.clone(),
            format!("{} @ {commit}", basename(&canonical)),
            false,
        ));
    }

    // build step, memory-fenced, toolchain from the registry
    if let Some(build) = &spec.build {
        let runtime = spec.runtime.as_deref().unwrap_or("node@24");
        let (rt_name, rt_version) = match runtime.split_once('@') {
            Some((n, v)) => (n, v),
            None => (runtime, "*"),
        };
        guard_memory(rt_name)?;

        let builder_dir = checkout.join(".ply-build");
        std::fs::create_dir_all(&builder_dir)?;
        let mem_max = builder_mem_bytes();
        let manifest = format!(
            "[package]\nname = \"{name}-builder\"\nversion = \"0.1.0\"\nentrypoint = [\"/bin/sh\", \"-c\", {build:?}]\nworkdir = \"/work\"\nbase = \"debian@13\"\n\n[dependencies]\n{rt_name} = \"{rt_version}\"\n\n[resources]\nmem = \"{mem_max}\"\ncpu_weight = 25\n\n[sources]\ndefault = \"https://registry.plybox.sh/ply/{{package}}\"\n",
        );
        std::fs::write(builder_dir.join("ply.toml"), manifest)?;
        let outcome = ply_core::build::build(&ply_core::build::BuildOptions {
            dir: builder_dir.clone(),
            output: None,
            allow_insecure: false,
            arch: None,
            // CD lanes are non-interactive: a repo that carries a .env
            // must fail loudly, never ship it.
            allow_secrets: false,
        })
        .context("building the builder image")?;

        // Builds take minutes; the status file is the dashboard's only
        // window in — say what is happening before going quiet.
        deployments::write_status(name, true, &format!("building @ {commit}…"));
        println!("{name}: build `{build}` (fenced at {mem_max}, {runtime})");
        let mut cli_env: Vec<(String, String)> = spec
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if rt_name == "node" {
            // Generous heap, tight residency: the cgroup fence + swap decide
            // where pages LIVE; node's own limit must not strangle the build
            // (node sizes its default heap from host RAM — on a 512MB droplet
            // that's a guaranteed JS heap OOM regardless of swap).
            let heap_mb = if meminfo("SwapTotal") > 0 {
                1536
            } else {
                (mem_bytes_to_mb(&mem_max) * 9 / 10).max(512)
            };
            cli_env.push((
                "NODE_OPTIONS".into(),
                format!("--max-old-space-size={heap_mb}"),
            ));
            // The overlay is RAM-backed tmpfs — caches must land on disk, in
            // the checkout, where they also persist between builds.
            cli_env.push(("npm_config_cache".into(), "/work/.npm-cache".into()));
            cli_env.push(("npm_config_update_notifier".into(), "false".into()));
        }
        // scratch space on disk for every runtime, not in the tmpfs overlay
        cli_env.push(("TMPDIR".into(), "/work/.tmp".into()));
        std::fs::create_dir_all(checkout.join(".tmp"))?;
        let code = ply_core::runtime::run::run(&ply_core::runtime::run::RunOptions {
            image: outcome.image_path,
            name: None,
            cli_env,
            allow_insecure: true,
            scale: 1,
            links: vec![(checkout.clone(), "/work".into())],
            publish: vec![],
            netns: None,
            netns_peers: vec![],
            netns_dns: None,
            after: vec![],
            after_timeout: std::time::Duration::from_secs(60),
            privileged: false,
            entrypoint: None,
            domains: vec![],
            volumes: vec![],
        })
        .context("running the build container")?;
        if code != 0 {
            bail!("build failed (exit {code}) — `ply logs {name}-builder` has the output");
        }
    }

    // the app manifest: the repo's own ply.toml wins; else generate from spec
    if !checkout.join("ply.toml").exists() || !spec.entrypoint.is_empty() {
        if spec.entrypoint.is_empty() {
            bail!(
                "{repo} has no ply.toml — give the deployment an `entrypoint = [\"…\"]` (and usually `include`)"
            );
        }
        let mut manifest = format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\nentrypoint = {}\nbase = \"debian@13\"\n",
            toml_array(&spec.entrypoint)
        );
        if !spec.include.is_empty() {
            manifest.push_str(&format!("include = {}\n", toml_array(&spec.include)));
        }
        let runtime = spec.runtime.as_deref().unwrap_or("node@24");
        let (rt_name, rt_version) = match runtime.split_once('@') {
            Some((n, v)) => (n, v),
            None => (runtime, "*"),
        };
        manifest.push_str(&format!("\n[dependencies]\n{rt_name} = \"{rt_version}\"\n"));
        manifest.push_str("\n[env]\nNODE_ENV = \"production\"\nHOSTNAME = \"0.0.0.0\"\n");
        if let Some(port) = spec.port {
            manifest.push_str(&format!(
                "\n[ports]\nweb = {port}\n\n[health]\nport = {port}\ngrace = \"15s\"\n"
            ));
        }
        manifest.push_str("\n[restart]\npolicy = \"on-failure\"\n\n[sources]\ndefault = \"https://registry.plybox.sh/ply/{package}\"\n");
        std::fs::write(checkout.join("ply.toml"), manifest)?;
    }

    let outcome = ply_core::build::build(&ply_core::build::BuildOptions {
        dir: checkout.clone(),
        output: None,
        allow_insecure: false,
        arch: None,
        // CD lanes are non-interactive: a repo that carries a .env
        // must fail loudly, never ship it.
        allow_secrets: false,
    })
    .context("packing the app image")?;
    // same path every build — remember the digest so a new commit forces a
    // restart even though the unit text cannot change
    let digest_file = checkout.join(".ply-build/last-digest");
    let previous = std::fs::read_to_string(&digest_file).unwrap_or_default();
    let rebuilt = previous.trim() != outcome.digest;
    let _ = std::fs::create_dir_all(checkout.join(".ply-build"));
    let _ = std::fs::write(&digest_file, &outcome.digest);
    let _ = std::fs::write(checkout.join(".ply-build/built"), &spec_fingerprint);
    println!("{name}: {} @ {commit}", outcome.image_name);
    Ok((
        outcome.image_path,
        format!("{} @ {commit}", outcome.image_name),
        rebuilt,
    ))
}

/// JS builds on tiny RAM with no swap die at 90% — refuse with the fix.
fn guard_memory(runtime: &str) -> Result<()> {
    if runtime != "node" {
        return Ok(());
    }
    let mem_kb = meminfo("MemTotal");
    let swap_kb = meminfo("SwapTotal");
    if mem_kb > 0 && mem_kb < 1_500_000 && swap_kb == 0 {
        bail!(
            "this host has {} MB RAM and no swap — a JS build needs ~2 GB somewhere.\nfix once: sudo ply setup --swap 2G",
            mem_kb / 1024
        );
    }
    Ok(())
}

fn meminfo(key: &str) -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

/// The fence: ~60% of RAM, floor 384M so tiny droplets still make progress.
fn builder_mem_bytes() -> String {
    let mem_kb = meminfo("MemTotal");
    let max_mb = ((mem_kb / 1024) * 60 / 100).max(384);
    format!("{max_mb}M")
}

fn mem_bytes_to_mb(size: &str) -> u64 {
    size.trim_end_matches('M').parse().unwrap_or(512)
}

fn toml_array(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("{i:?}")).collect();
    format!("[{}]", quoted.join(", "))
}

/// Cheap content hash of the spec fields that shape the build/manifest.
fn spec_hash(spec: &Spec) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        spec.build, spec.runtime, spec.entrypoint, spec.include, spec.port, spec.env, spec.r#ref
    )
    .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn basename(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd).args(args).status()?;
    if !status.success() {
        bail!("{cmd} {} exited with {status}", args.join(" "));
    }
    Ok(())
}

/// Standard base64 (padded) — one header's worth; a crate would be overkill.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The app's version, read from what the repo itself declares: its own
/// ply.toml when that manifest is in charge, else package.json, else
/// Cargo.toml — so images carry the project's version, not a made-up one.
/// Strict x.y.z or the 0.1.0 fallback (image names must parse as semver).
fn repo_version(checkout: &std::path::Path, spec: &Spec) -> String {
    let fallback = "0.1.0".to_string();
    let strict = |v: &str| -> Option<String> {
        let v = v.trim();
        semver::Version::parse(v).ok().map(|_| v.to_string())
    };
    if spec.entrypoint.is_empty() && checkout.join("ply.toml").exists() {
        if let Ok(raw) = std::fs::read_to_string(checkout.join("ply.toml")) {
            if let Ok(manifest) = ply_core::manifest::Manifest::parse(&raw) {
                return strict(&manifest.package.version.to_string()).unwrap_or(fallback);
            }
        }
    }
    if let Ok(raw) = std::fs::read_to_string(checkout.join("package.json")) {
        if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(v) = pkg.get("version").and_then(|v| v.as_str()).and_then(strict) {
                return v;
            }
        }
    }
    if let Ok(raw) = std::fs::read_to_string(checkout.join("Cargo.toml")) {
        for line in raw.lines().take(20) {
            if let Some(rest) = line.trim().strip_prefix("version") {
                if let Some(v) = rest.split('"').nth(1).and_then(strict) {
                    return v;
                }
            }
        }
    }
    fallback
}

#[cfg(test)]
mod secret_env_tests {
    use super::*;
    use ply_core::stack::{EnvSource, ResolvedEnv};
    use std::os::unix::fs::PermissionsExt;

    fn entry(key: &str, value: &str, secret: bool) -> ResolvedEnv {
        ResolvedEnv {
            key: key.to_string(),
            value: value.to_string(),
            secret,
            source: if secret {
                EnvSource::Minted("secrets/db.password".to_string())
            } else {
                EnvSource::StackE
            },
        }
    }

    /// Secret-tainted entries go to the env FILE (mode 0600, unreadable to
    /// anyone but root); plain ones stay `-e` flags in the world-readable
    /// unit. Order within each bucket is the resolver's own.
    #[test]
    fn split_env_sends_only_tainted_entries_to_the_file() {
        let entries = vec![
            entry("POSTGRES_DB", "todos", false),
            entry("POSTGRES_PASSWORD", "s3cr3t", true),
            entry("NODE_ENV", "production", false),
        ];
        let (flags, file) = split_env(&entries);
        assert_eq!(
            flags,
            vec![
                ("POSTGRES_DB".to_string(), "todos".to_string()),
                ("NODE_ENV".to_string(), "production".to_string()),
            ]
        );
        assert_eq!(
            file,
            vec![("POSTGRES_PASSWORD".to_string(), "s3cr3t".to_string())]
        );
    }

    /// A member with no secrets at all writes no file — that is what keeps
    /// a params-free stack's unit byte-identical to yesterday's.
    #[test]
    fn split_env_of_plain_entries_leaves_the_file_empty() {
        let (flags, file) = split_env(&[entry("A", "1", false)]);
        assert_eq!(flags, vec![("A".to_string(), "1".to_string())]);
        assert!(file.is_empty());
    }

    /// Whatever the resolver produced has to come back out of
    /// `parse_env_file` byte-for-byte — a password that loses a trailing
    /// space, or gets truncated at a `#`, is an auth failure a long way
    /// from its cause.
    #[test]
    fn secret_values_round_trip_through_the_env_file_parser() {
        let dir = tempfile::tempdir().unwrap();
        // the real shape: `.secrets/<stack>/env/<member>.env`, both levels
        // created by the writer
        let path = dir.path().join("todos").join("env").join("db.env");
        let values = [
            ("PLAIN", "s3cr3t"),
            ("WITH_EQUALS", "postgres://u:p@host:5432/db?x=1"),
            ("WITH_SPACES", "  padded value  "),
            ("WITH_HASH", "pa#ss #not-a-comment"),
            ("WITH_QUOTE", "a\"b"),
            ("ALREADY_QUOTED", "\"quoted\""),
            ("SINGLE_QUOTED", "'sq'"),
            ("EMPTY", ""),
            ("TRAILING_BACKSLASH", "abc\\"),
        ];
        let entries: Vec<(String, String)> = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let written = write_env_file(&path, &entries).unwrap().unwrap();
        assert_eq!(written, path.display().to_string());

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "a unit is world-readable; this is not");
        for dir in [
            path.parent().unwrap(),
            path.parent().unwrap().parent().unwrap(),
        ] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{}", dir.display());
        }

        let back: std::collections::BTreeMap<String, String> =
            ply_core::runtime::run::parse_env_file(&path)
                .unwrap()
                .into_iter()
                .collect();
        for (k, v) in values {
            assert_eq!(back[k], v, "{k} did not survive the round trip");
        }
    }

    /// A member that stops having secrets (its `[params]` dropped, its ref
    /// rewritten) asks for no `--env-file` — but its stale file SURVIVES the
    /// write step. The installed unit still names that path until `apply`
    /// rewrites it, and systemd restarting the member in that window would
    /// run `ply run --env-file <gone>`. Only the post-apply removal (which
    /// a failed apply never reaches) may delete it.
    #[test]
    fn no_secrets_asks_for_no_env_file_and_leaves_the_stale_file_for_after_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.env");
        write_env_file(&path, &[("PW".to_string(), "x".to_string())]).unwrap();
        assert!(path.exists());

        assert_eq!(write_env_file(&path, &[]).unwrap(), None);
        assert!(
            path.exists(),
            "the unit still names this file until apply rewrites it"
        );

        // what the successful-apply arm calls, once the new unit is on disk
        let _ = std::fs::remove_file(&path);
        assert!(!path.exists());
    }

    /// The env file must not land in the `<member>.<param>` namespace
    /// `SecretStore` owns: flat, `db.env` IS the file for a param named
    /// `env` — this writer would clobber an operator's secret, the stale
    /// sweep would delete it, and `ply secret ls` would list one phantom
    /// secret per member.
    #[test]
    fn the_env_file_sits_in_a_subdirectory_not_among_the_secret_files() {
        let store = ply_core::secrets::SecretStore::for_deployments("todos");
        let env_file = member_secrets_path("todos", "db");
        assert_ne!(env_file, store.path("db", "env"));
        assert!(
            env_file.ends_with("todos/env/db.env"),
            "{}",
            env_file.display()
        );
        assert_eq!(
            env_file.parent().unwrap(),
            store.path("db", "password").parent().unwrap().join("env"),
            "a subdirectory OF the store dir — same .secrets/, out of its namespace"
        );
    }

    /// The refusal names the KEY. Printing the value would put the secret
    /// in a status detail, an event and the journal at once.
    #[test]
    fn a_newline_in_a_value_is_refused_without_printing_it() {
        let e = env_file_line("PW", "line1\nline2").unwrap_err().to_string();
        assert!(e.contains("PW"), "{e}");
        assert!(!e.contains("line1"), "{e}");
    }

    /// One member's fetch failure must not freeze the members that do not
    /// depend on it, nor stamp them with its error.
    #[test]
    fn a_failure_blocks_its_dependants_transitively_and_nobody_else() {
        let edges: BTreeMap<String, BTreeSet<String>> = [
            ("db", vec![]),
            ("server", vec!["db"]),
            ("web", vec!["server"]),
            ("cache", vec![]),
        ]
        .into_iter()
        .map(|(m, deps)| {
            (
                m.to_string(),
                deps.into_iter()
                    .map(str::to_string)
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect();
        let failed = BTreeSet::from(["db".to_string()]);

        let blocked = blocked_by_failures(&failed, &edges);

        assert_eq!(
            blocked.keys().cloned().collect::<Vec<_>>(),
            vec!["db".to_string(), "server".to_string(), "web".to_string()],
            "the failure plus its transitive dependants — and nothing else"
        );
        assert_eq!(
            blocked["db"], None,
            "its own fetch error is already written"
        );
        assert_eq!(
            blocked["server"],
            Some("db".to_string()),
            "the status names the peer it waits on"
        );
        assert_eq!(
            blocked["web"],
            Some("server".to_string()),
            "transitively blocked, named by its DIRECT missing dependency"
        );
        assert!(
            !blocked.contains_key("cache"),
            "a member that depends on nothing broken keeps converging"
        );
    }

    /// Nothing failed, nothing is blocked — the ordinary beat.
    #[test]
    fn no_failures_blocks_nobody() {
        let edges: BTreeMap<String, BTreeSet<String>> =
            [("server".to_string(), BTreeSet::from(["db".to_string()]))]
                .into_iter()
                .collect();
        assert!(blocked_by_failures(&BTreeSet::new(), &edges).is_empty());
    }

    /// The edge set a member is excluded on is the same union the resolver
    /// waits on: explicit `after` (parsed down to the app) ∪ the members its
    /// `{app.param}` refs name, across env, publish and domain.
    #[test]
    fn member_edges_unions_explicit_after_with_every_derived_ref() {
        // Built directly rather than parsed: stack parse now REJECTS a `{}`
        // hole in `publish`/`domain` (v1 never interpolated them), but
        // `member_edges` — like `stack::derived_after` — still scans those
        // fields, and this keeps that scan covered.
        let web = ply_core::stack::Member {
            name: "web".to_string(),
            source: ply_core::stack::MemberSource::Path(std::path::PathBuf::from("./web")),
            env: vec![("DATABASE_URL".to_string(), "{db.url}".to_string())],
            params: Vec::new(),
            after: vec!["cache.finish_boot == 'ok'".to_string()],
            publish: Vec::new(),
            volume: Vec::new(),
            domain: vec!["{cdn.hostname}".to_string()],
            scale: None,
        };
        assert_eq!(
            member_edges(&web),
            BTreeSet::from(["cache".to_string(), "cdn".to_string(), "db".to_string()]),
            "a condition orders on the app it names, and every ref is an edge too"
        );
    }

    /// The property the whole split exists for: a tainted value is nowhere
    /// in the flags the unit carries, and the unit reads it from the 0600
    /// file instead.
    #[test]
    fn a_tainted_value_is_absent_from_the_flags_and_present_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.env");
        let entries = vec![
            entry("POSTGRES_DB", "todos", false),
            entry("POSTGRES_PASSWORD", "S3CR3T", true),
        ];
        let (flags, file) = split_env(&entries);

        let mut spec = Spec::parse("from = \"postgres@17\"\n").unwrap();
        spec.env = flags.into_iter().collect();
        spec.env_file = write_env_file(&path, &file).unwrap();

        let rendered = spec.flags().join(" ");
        assert!(
            !rendered.contains("S3CR3T"),
            "the unit is world-readable: {rendered}"
        );
        assert!(rendered.contains("-e POSTGRES_DB=todos"), "{rendered}");
        assert!(
            rendered.contains(&format!("--env-file {}", path.display())),
            "{rendered}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "POSTGRES_PASSWORD=\"S3CR3T\"\n"
        );
    }

    /// Additive, at the flag level: a member with no `[params]` and no `{}`
    /// holes converges to exactly the flags it did before params existed —
    /// same `-e`s, same `--after`, and no `--env-file` — so its unit text
    /// is byte-identical and reconcile reports "unchanged".
    #[test]
    fn a_params_free_member_keeps_the_flags_it_always_had() {
        let stack = ply_core::stack::parse(
            "[[app]]\nrun=\"postgres@17\"\nname=\"db\"\ne=[\"POSTGRES_DB=todos\"]\n\n\
             [[app]]\nrun=\"umami@3\"\nname=\"web\"\nafter=[\"db\"]\ne=[\"NODE_ENV=production\"]\npublish=[\"internal:3000\"]\n",
            Path::new("todos.toml"),
        )
        .unwrap()
        .unwrap();
        let lookup = |_: &str| None;

        let before: Vec<Vec<String>> = stack
            .members
            .iter()
            .map(|m| {
                Spec::from_stack_member(m, Some("todos"), &lookup)
                    .unwrap()
                    .flags()
            })
            .collect();

        let inputs: Vec<ply_core::stack::MemberInput> = stack
            .members
            .iter()
            .map(|m| ply_core::stack::MemberInput {
                name: m.name.clone(),
                manifest: None,
                env: ply_core::stack::expand_member_env(m, &lookup).unwrap(),
                params: Default::default(),
                after: m.after.clone(),
                publish: m.publish.clone(),
                domain: m.domain.clone(),
                version: None,
                port: ply_core::stack::container_port(&m.publish),
                scale: m.scale,
                image: None,
            })
            .collect();
        let resolution = ply_core::stack::resolve_stack(&inputs, None, false).unwrap();

        for (i, m) in stack.members.iter().enumerate() {
            let mut spec = Spec::from_stack_member(m, Some("todos"), &lookup).unwrap();
            let (flags, file) = split_env(&resolution.env[&m.name]);
            assert!(file.is_empty(), "nothing tainted, nothing to hide");
            spec.env = flags.into_iter().collect();
            spec.after = resolution.waits[&m.name].clone();
            assert_eq!(spec.flags(), before[i], "member `{}`", m.name);
            assert!(spec.env_file.is_none());
        }
    }
}

#[cfg(test)]
mod token_tests {
    use super::{expand_spec_holes, Spec};

    #[test]
    fn standalone_deployment_expands_var_holes() {
        let mut spec = Spec::parse(
            r#"
            app = "myapp"
            publish = ["$WEB_PORT:3000"]
            domain = ["$SITE"]

            [env]
            SUPER_SECRET = "$SUPER_SECRET"
            LITERAL = "cost is $$5"
            "#,
        )
        .unwrap();

        let table: std::collections::BTreeMap<String, String> = [
            ("SUPER_SECRET", "s3cret"),
            ("WEB_PORT", "8080"),
            ("SITE", "example.com"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let lookup = |k: &str| table.get(k).cloned();

        expand_spec_holes(&lookup, &mut spec).unwrap();
        assert_eq!(spec.env["SUPER_SECRET"], "s3cret");
        assert_eq!(spec.env["LITERAL"], "cost is $5", "$$ is a literal $");
        assert_eq!(spec.publish, vec!["8080:3000"]);
        assert_eq!(spec.domain, vec!["example.com"]);

        // a missing hole is an error naming the key — never a silent literal
        let mut spec = Spec::parse("app = \"myapp\"\n\n[env]\nX = \"$NOPE\"\n").unwrap();
        let err = expand_spec_holes(&|_: &str| None, &mut spec)
            .unwrap_err()
            .to_string();
        assert!(err.contains("NOPE"), "{err}");
    }
    #[test]
    fn base64_matches_reference() {
        assert_eq!(
            super::base64(b"x-access-token:ghp_abc"),
            "eC1hY2Nlc3MtdG9rZW46Z2hwX2FiYw=="
        );
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"a"), "YQ==");
        assert_eq!(super::base64(b"ab"), "YWI=");
        assert_eq!(super::base64(b"abc"), "YWJj");
    }
}

// --- GitOps fleet: the deployments dir, synced from a repo ------------------
//
// /etc/ply/fleet.toml names a repo and this host's identity. Every
// reconcile beat pulls it and applies shared/*.toml + hosts/<host>/*.toml
// into the deployments dir. Content-compared: an unchanged file is never
// rewritten, so mtime-as-intent (auto = false, deploy-now) keeps working.
// A managed-list makes git own exactly the files it introduced — local
// and dashboard-created deployments coexist untouched. Fail-open: a git
// error leaves the host converging on what it already has.

#[derive(serde::Deserialize)]
struct FleetConfig {
    repo: String,
    #[serde(default)]
    host: Option<String>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    deploy_key: Option<String>,
    /// Fine-grained PAT for private https repos — the same one-credential
    /// story the deployment lanes use. Relative paths resolve against the
    /// deployments dir.
    #[serde(default)]
    token_file: Option<String>,
}

const FLEET_CONFIG: &str = "/etc/ply/fleet.toml";
const FLEET_CHECKOUT: &str = "/var/lib/ply/fleet";

/// Enrollment is a file. The dashboard (or anyone with the deployments
/// grant) writes `.fleet.toml` beside the specs; `ply setup --fleet`
/// writes the /etc/ply copy. The in-dir file wins.
fn fleet_config_text() -> Option<String> {
    std::fs::read_to_string(deployments::dir().join(".fleet.toml"))
        .or_else(|_| std::fs::read_to_string(FLEET_CONFIG))
        .ok()
}

pub fn fleet_sync() {
    let Some(raw) = fleet_config_text() else {
        return; // not a fleet host
    };
    let config: FleetConfig = match toml::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            fleet_status(false, &format!("{FLEET_CONFIG}: {e}"));
            return;
        }
    };
    let host = config.host.clone().unwrap_or_else(|| {
        nix::unistd::gethostname()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    match fleet_pull_apply(&config, &host) {
        Ok(summary) => fleet_status(true, &summary),
        Err(e) => {
            eprintln!("ply: fleet sync: {e:#}");
            fleet_status(false, &format!("{e:#}"));
        }
    }
}

fn fleet_pull_apply(config: &FleetConfig, host: &str) -> Result<String> {
    let checkout = PathBuf::from(FLEET_CHECKOUT);
    let git_ssh = config.deploy_key.as_deref().map(|key| {
        format!(
            "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new",
            resolve_secret(key)
        )
    });
    let token_header = match &config.token_file {
        Some(file) => {
            let path = resolve_secret(file);
            let token = std::fs::read_to_string(&path)
                .with_context(|| format!("reading fleet token_file {path}"))?;
            Some(format!(
                "AUTHORIZATION: basic {}",
                base64(format!("x-access-token:{}", token.trim()).as_bytes())
            ))
        }
        None => None,
    };
    let git = |args: &[&str], cwd: Option<&PathBuf>| -> Result<String> {
        let mut cmd = std::process::Command::new("git");
        if let Some(header) = &token_header {
            cmd.arg("-c").arg(format!("http.extraheader={header}"));
        }
        cmd.args(args);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        if let Some(ssh) = &git_ssh {
            cmd.env("GIT_SSH_COMMAND", ssh);
        }
        let out = cmd.output().context("running git — is it installed?")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    let reference = config.reference.as_deref().unwrap_or("HEAD");
    if !checkout.join(".git").exists() {
        std::fs::create_dir_all(checkout.parent().unwrap())?;
        git(
            &[
                "clone",
                "--depth",
                "1",
                &config.repo,
                checkout.to_str().unwrap(),
            ],
            None,
        )?;
    }
    git(
        &["fetch", "--depth", "1", "origin", reference],
        Some(&checkout),
    )?;
    git(&["reset", "--hard", "FETCH_HEAD"], Some(&checkout))?;
    let commit = git(&["rev-parse", "--short=12", "HEAD"], Some(&checkout))?;

    // desired set: shared/ first, hosts/<host>/ wins on name conflicts
    let mut desired: std::collections::BTreeMap<String, String> = Default::default();
    for dir in [checkout.join("shared"), checkout.join("hosts").join(host)] {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some(name) = file.strip_suffix(".toml") else {
                continue;
            };
            if !valid_name(name) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(entry.path()) {
                desired.insert(name.to_string(), content);
            }
        }
    }

    let managed_path = deployments::status_dir().join(".fleet-managed");
    let previously: std::collections::BTreeSet<String> = std::fs::read_to_string(&managed_path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let mut changed = 0usize;
    for (name, content) in &desired {
        let target = deployments::spec_path(name);
        if std::fs::read_to_string(&target).ok().as_deref() == Some(content) {
            continue; // identical: no write, no mtime bump, no fake touch
        }
        let tmp = deployments::dir().join(format!(".{name}.toml.fleet"));
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &target)?;
        changed += 1;
    }
    let mut removed = 0usize;
    for name in &previously {
        if !desired.contains_key(name) {
            // git introduced it, git dropped it: the app goes with it
            if std::fs::remove_file(deployments::spec_path(name)).is_ok() {
                removed += 1;
            }
        }
    }
    std::fs::create_dir_all(deployments::status_dir())?;
    let list: Vec<&str> = desired.keys().map(|s| s.as_str()).collect();
    std::fs::write(&managed_path, format!("{}\n", list.join("\n")))?;

    Ok(format!(
        "@ {commit} — {} managed, {changed} changed, {removed} removed",
        desired.len()
    ))
}

/// `.status/fleet.json`: the sync outcome, where the dashboard's existing
/// deployments grant can read it.
fn fleet_status(ok: bool, detail: &str) {
    let dir = deployments::status_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = now_unix();
    let line = format!("{{\"ok\":{ok},\"detail\":{detail:?},\"ts\":{ts}}}\n");
    let tmp = dir.join(".fleet.json.tmp");
    if std::fs::write(&tmp, line).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("fleet.json"));
    }
}
