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
    let specs = deployments::list()?;
    let mut desired: BTreeSet<String> = BTreeSet::new();
    let mut app_names: BTreeSet<String> = BTreeSet::new();
    let mut changed_units = false;

    for (name, spec) in specs {
        if !valid_name(&name) {
            deployments::write_status(&name, false, "deployment names are [a-z0-9-]");
            continue;
        }
        let spec = match spec {
            Ok(spec) => spec,
            Err(e) => {
                deployments::write_status(&name, false, &format!("spec: {e}"));
                continue;
            }
        };
        // Cadence discipline. A spec file touched at/after its last status
        // is an explicit order — converge it. Untouched specs on a
        // background beat (the timer, another app's change): auto = false
        // holds at the current artifact, and a recent failure backs off
        // instead of re-running an expensive build every minute.
        if !touched_since_status(&name) {
            if let Some((ok, ts)) = deployments::read_status(&name) {
                let hold_manual = !spec.auto;
                let hold_backoff = !ok && now_unix().saturating_sub(ts) < FAIL_BACKOFF_SECS;
                if hold_manual || hold_backoff {
                    desired.insert(name.clone());
                    continue;
                }
            }
        }
        match apply(&name, &spec, &mut app_names) {
            Ok(applied) => {
                changed_units |= applied.changed;
                desired.insert(name.clone());
                deployments::write_status(&name, true, &applied.detail);
                // journal state changes only — "unchanged" runs are silence
                if !applied.detail.starts_with("unchanged") {
                    ply_core::runtime::events::emit(&name, "deploy", &applied.detail);
                }
            }
            Err(e) => {
                // A failed attempt is NOT a deleted spec: the deployment
                // stays desired, its current unit stays untouched. Only a
                // removed FILE may orphan a unit.
                desired.insert(name.clone());
                deployments::write_status(&name, false, &format!("{e:#}"));
                ply_core::runtime::events::emit(&name, "deploy-failed", &format!("{e:#}"));
                eprintln!("ply: reconcile {name}: {e:#}");
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

fn apply(name: &str, spec: &Spec, app_names: &mut BTreeSet<String>) -> Result<Applied> {
    // Resolve the image: registry runnable, or a file already on this host.
    let mut force_restart = false;
    let (image, shown): (PathBuf, String) = match (&spec.app, &spec.image, &spec.github) {
        _ if spec.repo.is_some() => {
            let (image, shown, rebuilt) = build_from_repo(name, spec)?;
            force_restart = rebuilt;
            (image, shown)
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

    // Instance state is keyed by the app name INSIDE the image — two
    // deployments of one app would fight over a single pool.
    let manifest = ply_core::image::read::read_manifest(&image)?;
    if !app_names.insert(manifest.package.name.clone()) {
        bail!(
            "another deployment already runs app `{}` — one deployment per app name",
            manifest.package.name
        );
    }

    let mut flags = spec.flags();
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
            // same unit, new bytes (a repo build): the restart IS the deploy
            run("systemctl", &["enable", &format!("ply-{name}")])?;
            run("systemctl", &["restart", &format!("ply-{name}")])?;
            return Ok(Applied {
                changed: false,
                detail: format!("redeployed {shown}"),
            });
        }
        // converged already; make sure it's on
        run("systemctl", &["enable", "--now", &format!("ply-{name}")])?;
        return Ok(Applied {
            changed: false,
            detail: format!("unchanged ({shown})"),
        });
    }
    std::fs::write(&unit_path, &unit_text)
        .with_context(|| format!("writing {}", unit_path.display()))?;
    run("systemctl", &["daemon-reload"])?;
    run("systemctl", &["enable", &format!("ply-{name}")])?;
    run("systemctl", &["restart", &format!("ply-{name}")])?;
    Ok(Applied {
        changed: true,
        detail: format!("deployed {shown}"),
    })
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
            cli_env,
            allow_insecure: true,
            scale: 1,
            links: vec![(checkout.clone(), "/work".into())],
            publish: vec![],
            after: vec![],
            after_timeout: std::time::Duration::from_secs(60),
            privileged: false,
            entrypoint: None,
            domains: vec![],
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
