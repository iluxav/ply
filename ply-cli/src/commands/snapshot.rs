//! `ply snapshot take|ls|rm` and `ply restore` — volume snapshots, the
//! backup that needs nothing from the image. The mechanism is in
//! `ply_core::snapshot`; this is the printing.

use anyhow::{bail, Context, Result};

use crate::cli::{RestoreArgs, SnapshotRmArgs, SnapshotTarget};

fn size(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KiB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MiB", b / K / K)
    } else {
        format!("{:.2} GiB", b / K / K / K)
    }
}

pub fn take(args: &SnapshotTarget) -> Result<()> {
    let ply = std::env::current_exe().context("locating ply")?;
    for taken in ply_core::snapshot::take(&args.app, &ply)? {
        println!(
            "snapshot {} ({}) of {} — volumes: {}",
            taken
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
                .trim_end_matches(".img"),
            size(taken.bytes),
            taken.instance,
            taken.volumes.join(", ")
        );
    }
    Ok(())
}

pub fn ls(args: &SnapshotTarget) -> Result<()> {
    let all = ply_core::snapshot::list(&args.app)?;
    if all.is_empty() {
        println!(
            "no snapshots of {} — `ply snapshot take {}` makes one",
            args.app, args.app
        );
        return Ok(());
    }
    println!(
        "{:<48} {:>4} {:<20} {:>10} VOLUMES",
        "NAME", "SLOT", "TAKEN", "SIZE"
    );
    for e in &all {
        println!(
            "{:<48} {:>4} {:<20} {:>10} {}",
            e.name,
            e.meta.slot,
            e.meta.taken,
            size(e.bytes),
            e.meta
                .volumes
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

pub fn rm(args: &SnapshotRmArgs) -> Result<()> {
    let gone = ply_core::snapshot::remove(&args.app, &args.name)?;
    println!("removed {} ({})", gone.name, size(gone.bytes));
    Ok(())
}

pub fn restore(args: &RestoreArgs) -> Result<()> {
    let entry = ply_core::snapshot::resolve(&args.app, &args.name)?;
    println!(
        "restoring {}.{} from {} (taken {}; volumes: {})",
        args.app,
        entry.meta.slot,
        entry.name,
        entry.meta.taken,
        entry
            .meta
            .volumes
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    let report = ply_core::snapshot::restore(&args.app, &entry, args.timeout)?;
    for name in &report.rolled {
        println!("rolled {name}");
    }
    if !report.complete {
        bail!(
            "restore incomplete after {}s — check `ply why {}`; the previous volume is kept beside \
             the new one under the app's volumes directory (.pre-restore)",
            args.timeout,
            args.app
        );
    }
    println!(
        "restored {}.{} — the previous volume is kept under the app's volumes directory (.pre-restore) until you remove it",
        args.app, entry.meta.slot
    );
    Ok(())
}
