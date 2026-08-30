//! `ply reconcile` — make systemd agree with /var/lib/ply/deployments/.
//!
//! Runs as a oneshot from ply-deployments.path (kernel inotify on the dir),
//! or by hand. Idempotent by construction: it converges, never accumulates.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use ply_core::deployments::{self, Spec, UNIT_MARKER};

const UNIT_DIR: &str = "/etc/systemd/system";

pub fn exec() -> Result<()> {
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
                if held(&name, true) {
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
                let spec = match Spec::parse(&text) {
                    Ok(spec) => spec,
                    Err(e) => {
                        deployments::write_status(&name, false, &format!("spec: {e}"));
                        continue;
                    }
                };
                // Cadence discipline. A spec file touched at/after its last
                // status is an explicit order — converge it. Untouched specs
                // on a background beat (the timer, another app's change):
                // auto = false holds at the current artifact, and a recent
                // failure backs off instead of rebuilding every minute.
                if held(&name, spec.auto) {
                    desired.insert(name.clone());
                    continue;
                }
                match apply(&name, &spec, &mut app_names) {
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
                // has [[app]] but malformed, or the file is not valid TOML
                deployments::write_status(&name, false, &format!("{e:#}"));
                continue;
            }
        }
    }

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
fn held(name: &str, auto: bool) -> bool {
    if touched_since_status(name) {
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
    for member in &stack.members {
        // reserve the unit first: a failed beat must not orphan it
        desired.insert(member.name.clone());
        let spec = match Spec::from_stack_member(member, stack_label, &lookup) {
            Ok(s) => s,
            Err(e) => {
                deployments::write_status(&member.name, false, &format!("{e:#}"));
                errs.push(format!("{}: {e}", member.name));
                continue;
            }
        };
        match apply(&member.name, &spec, app_names) {
            Ok(applied) => {
                *changed_units |= applied.changed;
                deployments::write_status(&member.name, true, &applied.detail);
                if !applied.detail.starts_with("unchanged") {
                    ply_core::runtime::events::emit(&member.name, "deploy", &applied.detail);
                }
                oks += 1;
            }
            Err(e) => {
                deployments::write_status(&member.name, false, &format!("{e:#}"));
                ply_core::runtime::events::emit(&member.name, "deploy-failed", &format!("{e:#}"));
                eprintln!("ply: reconcile {}: {e:#}", member.name);
                errs.push(format!("{}: {e}", member.name));
            }
        }
    }
    // aggregate status on the stack file itself — the deploy screen's row
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

fn apply(name: &str, spec: &Spec, app_names: &mut BTreeSet<String>) -> Result<Applied> {
    // Resolve the image: registry runnable, or a file already on this host.
    let mut force_restart = false;
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
            let (path, resolved, _digest) =
                ply_core::catalog::fetch_app_image(app, spec.version.as_deref(), &source)
                    .with_context(|| format!("fetching `{app}` from the registry"))?;
            println!("{name}: {resolved}");
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
mod token_tests {
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
