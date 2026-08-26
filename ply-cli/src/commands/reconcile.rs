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
        match apply(&name, &spec, &mut app_names) {
            Ok(applied) => {
                changed_units |= applied.changed;
                desired.insert(name.clone());
                deployments::write_status(&name, true, &applied.detail);
            }
            Err(e) => {
                deployments::write_status(&name, false, &format!("{e:#}"));
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
        let _ = std::fs::remove_file(deployments::dir().join(format!("{stem}.status")));
        changed_units = true;
    }

    if changed_units {
        run("systemctl", &["daemon-reload"])?;
    }
    Ok(())
}

struct Applied {
    changed: bool,
    detail: String,
}

fn apply(name: &str, spec: &Spec, app_names: &mut BTreeSet<String>) -> Result<Applied> {
    // Resolve the image: registry runnable, or a file already on this host.
    let (image, shown): (PathBuf, String) = match (&spec.app, &spec.image) {
        (Some(app), None) => {
            let source = spec
                .source
                .clone()
                .unwrap_or_else(|| ply_core::catalog::OFFICIAL_RUN_SOURCE.to_string());
            let (path, resolved, _digest) =
                ply_core::catalog::fetch_app_image(app, spec.version.as_deref(), &source)
                    .with_context(|| format!("fetching `{app}` from the registry"))?;
            println!("{name}: {resolved}");
            let shown = resolved.to_string();
            (path, shown)
        }
        (None, Some(path)) => {
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
