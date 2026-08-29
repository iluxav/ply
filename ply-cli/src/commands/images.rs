//! bundle / import / rebase — image transformation commands.

use anyhow::{Context, Result};

use crate::cli::{BundleArgs, ImportArgs, InspectArgs, RebaseArgs};
use crate::commands::build::human_size;

/// Show what an image declares, as the catalog records it — the same
/// derivation `ply push` sends and the registry stores. `--json` prints the
/// raw metadata (used by the registry pipeline); the default is a summary.
pub fn inspect(args: InspectArgs) -> Result<()> {
    let meta = ply_core::catalog::derive_push_meta(&args.image)
        .with_context(|| format!("reading {}", args.image.display()))?;
    if args.json {
        println!("{}", serde_json::to_string(&meta)?);
        return Ok(());
    }
    println!(
        "type:         {}",
        format!("{:?}", meta.kind).to_lowercase()
    );
    println!(
        "volumes:      {}",
        if meta.volumes.is_empty() {
            "—".into()
        } else {
            meta.volumes.join(", ")
        }
    );
    println!(
        "links:        {}",
        if meta.links.is_empty() {
            "—".into()
        } else {
            meta.links.join(", ")
        }
    );
    println!("dependencies: {}", meta.dependencies.len());
    for d in &meta.dependencies {
        println!("  {} {}", d.name, d.version);
    }
    Ok(())
}

pub fn bundle(args: BundleArgs) -> Result<()> {
    let outcome = ply_core::bundle::bundle(&args.image, &args.output, true)?;
    println!(
        "bundled {} ({}) — self-sufficient, zero fetches at run",
        outcome.image_path.display(),
        human_size(outcome.size_bytes)
    );
    println!("{}", outcome.digest);
    Ok(())
}

pub fn import(args: ImportArgs) -> Result<()> {
    let outcome = ply_core::oci::import(&args.source, &args.output)?;
    println!(
        "imported {} -> {} ({}, fat mode)",
        args.source,
        outcome.image_path.display(),
        human_size(outcome.size_bytes)
    );
    println!("{}", outcome.digest);
    Ok(())
}

pub fn rebase(args: RebaseArgs) -> Result<()> {
    let output = args.output.clone().unwrap_or_else(|| args.image.clone());
    let outcome =
        ply_core::rebase::rebase(&args.image, &args.runtime, &output, args.insecure_source)?;
    let (name, old, new) = &outcome.replaced;
    println!("rebased {name}: {old} -> {new}");
    println!("{} ({})", outcome.image_path.display(), outcome.digest);
    Ok(())
}
