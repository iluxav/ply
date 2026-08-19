//! bundle / import / rebase — image transformation commands.

use anyhow::Result;

use crate::cli::{BundleArgs, ImportArgs, RebaseArgs};
use crate::commands::build::human_size;

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
