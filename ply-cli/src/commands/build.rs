use anyhow::Result;
use ply_core::build::{build, BuildOptions};

use crate::cli::BuildArgs;

pub fn run(args: BuildArgs) -> Result<()> {
    let outcome = build(&BuildOptions {
        dir: args.dir,
        output: args.output,
        allow_insecure: args.insecure_source,
    })?;
    for (name, version) in &outcome.locked {
        println!("locked {name} {version}");
    }
    println!(
        "built {} ({})",
        outcome.image_path.display(),
        human_size(outcome.size_bytes)
    );
    println!("{}", outcome.digest);
    Ok(())
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
