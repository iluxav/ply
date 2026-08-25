//! gc / rm / systemd / sync / audit / outdated / check — thin printers over
//! ply-core's lifecycle and policy modules.

use anyhow::{bail, Result};
use ply_core::image::read::{read_lockfile, read_manifest};
use ply_core::policy::{Policy, Severity};
use ply_core::{lifecycle, Error};

use crate::cli::{AuditArgs, CheckArgs, GcArgs, OutdatedArgs, RmArgs, SyncArgs, SystemdArgs};

pub fn gc(args: GcArgs) -> Result<()> {
    let report = lifecycle::gc(args.dry_run)?;
    let verb = if args.dry_run {
        "would delete"
    } else {
        "deleted"
    };
    for digest in &report.deleted {
        println!("{verb} {digest}");
    }
    println!(
        "{} kept, {} {verb} ({:.1} MiB)",
        report.kept,
        report.deleted.len(),
        report.freed_bytes as f64 / (1024.0 * 1024.0)
    );
    Ok(())
}

pub fn rm(args: RmArgs) -> Result<()> {
    let report = lifecycle::rm(&args.app, args.volumes)?;
    if report.stopped > 0 {
        println!("stopped {} instance(s)", report.stopped);
    }
    if report.record_removed {
        println!("removed app record for {}", args.app);
    }
    if report.volumes_removed {
        println!("removed volumes of {}", args.app);
    } else if !args.volumes {
        println!("volumes kept (use --volumes to destroy data)");
    }
    if report.stopped == 0 && !report.record_removed {
        println!("nothing to remove for `{}`", args.app);
    }
    println!("run `ply gc` to free unreferenced store entries");
    Ok(())
}

pub fn deploy(args: crate::cli::DeployArgs) -> Result<()> {
    println!("deploying {} …", args.image.display());
    let report = lifecycle::deploy(&args.image, args.timeout)?;
    for name in &report.rolled {
        println!("rolled {name}");
    }
    if report.complete {
        println!(
            "deploy complete: {} instance(s) of {} on the new image",
            report.rolled.len(),
            report.app
        );
        Ok(())
    } else {
        bail!(
            "deploy incomplete after timeout — {} instance(s) rolled; check the `ply run` output / `ply ps --json` for what happened",
            report.rolled.len()
        );
    }
}

pub fn systemd(args: SystemdArgs) -> Result<()> {
    let mut flags: Vec<String> = Vec::new();
    if let Some(scale) = args.scale {
        flags.extend(["--scale".into(), scale.to_string()]);
    }
    if let Some(publish) = &args.publish {
        // Validate now — a typo should fail here, not at boot via systemd.
        ply_core::runtime::publish::parse_publish(publish)?;
        flags.extend(["--publish".into(), publish.clone()]);
    }
    for pair in &args.env {
        flags.extend(["-e".into(), pair.clone()]);
    }
    if let Some(file) = &args.env_file {
        let file = std::path::absolute(file)?;
        flags.extend(["--env-file".into(), file.display().to_string()]);
    }
    for app in &args.after {
        flags.extend(["--after".into(), app.clone()]);
    }
    print!(
        "{}",
        lifecycle::systemd_unit(&args.image, &flags, &args.after, args.user)?
    );
    Ok(())
}

pub fn sync(args: SyncArgs) -> Result<()> {
    let policy = match &args.policy {
        Some(path) => Policy::load(path)?,
        None => Policy::load_default()?.ok_or_else(|| {
            Error::Runtime(format!(
                "no policy file at {} — create one or pass --policy FILE",
                ply_core::policy::DEFAULT_POLICY_PATH
            ))
        })?,
    };
    let report = lifecycle::sync(&policy, args.insecure_source)?;
    for image in &report.fetched {
        println!("fetched {image}");
    }
    for skip in &report.skipped {
        println!("skipped {skip}");
    }
    println!(
        "{} fetched, {} already present, {} skipped",
        report.fetched.len(),
        report.present,
        report.skipped.len()
    );
    Ok(())
}

pub fn audit(_args: AuditArgs) -> Result<()> {
    let report = lifecycle::audit()?;
    if report.shared_volumes.is_empty() && report.findings.is_empty() {
        println!("nothing to report");
        return Ok(());
    }
    for vol in &report.shared_volumes {
        println!("shared volume: {vol} (multiple writers possible — by design?)");
    }
    for (app, finding) in &report.findings {
        println!("{app}: {finding}");
    }
    Ok(())
}

pub fn outdated(_args: OutdatedArgs) -> Result<()> {
    let lines = lifecycle::outdated(true)?;
    if lines.is_empty() {
        println!(
            "everything is at the lowest satisfying version (MVS) with nothing newer published"
        );
    } else {
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

/// Structural validation + optional policy check. Pure — CI-friendly.
pub fn check(args: CheckArgs) -> Result<()> {
    let manifest = read_manifest(&args.image)?;
    println!(
        "ok: manifest — {} {} ({})",
        manifest.package.name,
        manifest.package.version,
        if manifest.is_app() { "app" } else { "package" }
    );
    let lockfile = read_lockfile(&args.image)?;
    match &lockfile {
        Some(lock) => println!("ok: lockfile — {} locked package(s)", lock.packages.len()),
        None => {
            if !manifest.dep_specs().is_empty() {
                bail!("image declares dependencies but embeds no lockfile — rebuild it");
            }
            println!("ok: no dependencies (thin image)");
        }
    }

    let policy = match &args.against {
        Some(path) => Some(Policy::load(path)?),
        None => Policy::load_default()?,
    };
    let (Some(policy), Some(lock)) = (policy, lockfile) else {
        return Ok(());
    };
    let findings = policy.check_lockfile(&lock);
    let mut errors = 0;
    for finding in &findings {
        match finding.severity {
            Severity::Error => {
                errors += 1;
                println!("error: {}", finding.message);
            }
            Severity::Warning => println!("warning: {}", finding.message),
        }
    }
    if errors > 0 {
        bail!("{errors} policy error(s)");
    }
    println!("ok: policy — {} finding(s), none fatal", findings.len());
    Ok(())
}
