use anyhow::Result;
use ply_core::runtime::state::{self, CleanTarget};

use crate::cli::CleanArgs;

pub fn exec(args: CleanArgs) -> Result<()> {
    let (target, what) = if let Some(app) = args.app {
        let label = format!("instances of `{app}`");
        (CleanTarget::App(app), label)
    } else if args.all {
        (CleanTarget::All, "instances".to_string())
    } else {
        (CleanTarget::Orphans, "orphaned instances".to_string())
    };

    let stopped = state::clean(&target)?;
    if stopped.is_empty() {
        println!("ply clean: no {what} to reap");
        return Ok(());
    }
    for s in &stopped {
        println!("reaped {}.{} (pid {})", s.app, s.n, s.pid);
    }
    let n = stopped.len();
    println!(
        "ply clean: reaped {n} {}",
        if n == 1 { "instance" } else { "instances" }
    );
    Ok(())
}
