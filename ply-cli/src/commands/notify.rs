//! `ply notify [--test]` — flush notifications now, or prove delivery.

use anyhow::{bail, Result};

use crate::cli::NotifyArgs;

pub fn run(args: &NotifyArgs) -> Result<()> {
    if args.test {
        let (tried, failures) = ply_core::notify::test(&args.to)?;
        if failures.is_empty() {
            println!("sent a test message to {tried} destination(s)");
            return Ok(());
        }
        for f in &failures {
            eprintln!("failed: {f}");
        }
        bail!("{}/{} destination(s) failed", failures.len(), tried);
    }
    ply_core::notify::run();
    Ok(())
}
