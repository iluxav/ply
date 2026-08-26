use anyhow::{bail, Result};
use ply_core::runtime::{control, state};

use crate::cli::{RestartArgs, ScaleArgs};

pub fn scale(args: ScaleArgs) -> Result<()> {
    ensure_running(&args.app)?;
    control::submit(&args.app, "scale", &args.n.to_string())?;
    println!(
        "scale request filed — the parent acts within ~2s (watch: ply ps; result: cat {})",
        control::dir(&args.app).join("last-result").display()
    );
    Ok(())
}

pub fn restart(args: RestartArgs) -> Result<()> {
    ensure_running(&args.app)?;
    control::submit(&args.app, "restart", "")?;
    println!("rolling restart filed — health-gated, one slot at a time (watch: ply ps)");
    Ok(())
}

/// A command for a stopped app would sit in the dir until some future run
/// consumed it, surprising everyone — refuse instead.
fn ensure_running(app: &str) -> Result<()> {
    let running = state::list()?.iter().any(|s| s.app == app && s.alive());
    if !running {
        bail!("no running instances of `{app}` — commands act on a live run parent (ply ps)");
    }
    Ok(())
}
