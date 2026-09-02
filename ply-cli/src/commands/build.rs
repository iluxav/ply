use std::path::{Path, PathBuf};

use anyhow::Result;
use ply_core::build::{build, BuildOptions, BuildOutcome};
use ply_core::image::name::Arch;

use crate::cli::BuildArgs;

pub fn run(args: BuildArgs) -> Result<()> {
    build_and_report(&BuildOptions {
        dir: args.dir,
        output: args.output,
        allow_insecure: args.insecure_source,
        arch: parse_arch(args.arch.as_deref())?,
        allow_secrets: args.allow_secrets,
    })?;
    Ok(())
}

/// `--arch x64|arm64`; `None` means the host's. Shared with `ply push`, which
/// accepts the same flag and must reject the same typos the same way.
pub fn parse_arch(arch: Option<&str>) -> Result<Option<Arch>> {
    match arch {
        None => Ok(None),
        Some("x64") => Ok(Some(Arch::X64)),
        Some("arm64") => Ok(Some(Arch::Arm64)),
        Some(other) => anyhow::bail!("--arch `{other}`: supported values are x64, arm64"),
    }
}

/// Build a directory for `ply push`: the same options and the same printed
/// lines `ply build DIR` produces, so the image a push publishes is exactly
/// the image a build would have written (canonical name, in DIR).
pub fn build_for_push(dir: &Path, arch: Option<Arch>) -> Result<PathBuf> {
    Ok(build_and_report(&BuildOptions {
        dir: dir.to_path_buf(),
        output: None,
        allow_insecure: false,
        arch,
        allow_secrets: false,
    })?
    .image_path)
}

fn build_and_report(opts: &BuildOptions) -> Result<BuildOutcome> {
    let outcome = build(opts)?;
    for (name, version) in &outcome.locked {
        println!("locked {name} {version}");
    }
    println!(
        "built {} ({})",
        outcome.image_path.display(),
        human_size(outcome.size_bytes)
    );
    println!("{}", outcome.digest);
    Ok(outcome)
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
