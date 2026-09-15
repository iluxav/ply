use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ply_core::build::{build, BuildOptions, BuildOutcome};
use ply_core::image::name::Arch;
use ply_core::manifest::Manifest;

use crate::cli::BuildArgs;

pub fn run(args: BuildArgs) -> Result<()> {
    let arch = parse_arch(args.arch.as_deref())?;
    // A `[build] command` runs INSIDE a Linux builder image before packing, so
    // `ply build` on any host produces correct Linux artifacts (native addons
    // compiled for the target) rather than packing host-native node_modules.
    let manifest_path = args.dir.join("ply.toml");
    if manifest_path.exists() {
        if let Some(command) = Manifest::load(&manifest_path)?.build_command.clone() {
            return build_with_stage(&args, &command, arch);
        }
    }
    build_and_report(&BuildOptions {
        dir: args.dir,
        output: args.output,
        allow_insecure: args.insecure_source,
        arch,
        allow_secrets: args.allow_secrets,
        manifest: None,
    })?;
    Ok(())
}

/// Build a directory that declares a `[build] command`: run the command in a
/// Linux builder image over a COPY of the source (so the working dir is never
/// mutated and no builder-owned files leak back), then pack the built copy —
/// writing the image to the user's dir (or `--output`), like a plain build.
fn build_with_stage(args: &BuildArgs, command: &str, arch: Option<Arch>) -> Result<()> {
    let work = std::env::temp_dir().join(format!(
        "ply-build-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let result = build_with_stage_inner(args, command, arch, &work);
    let _ = std::fs::remove_dir_all(&work); // always clean up the copy
    result
}

fn build_with_stage_inner(
    args: &BuildArgs,
    command: &str,
    arch: Option<Arch>,
    work: &Path,
) -> Result<()> {
    copy_source(&args.dir, work)
        .with_context(|| format!("copying {} for the build stage", args.dir.display()))?;
    // Pin the builder to the app's declared runtime so native addons build
    // against the interpreter the image bundles.
    let runtime =
        crate::commands::reconcile::repo_runtime(&args.dir).unwrap_or_else(|| "node@24".into());
    let manifest = Manifest::load(&args.dir.join("ply.toml"))?;
    let env: Vec<(String, String)> = manifest
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    println!(
        "build stage: `{command}` in a {runtime} builder image ({})",
        if cfg!(target_os = "macos") {
            "microVM"
        } else {
            "namespace sandbox"
        }
    );
    crate::commands::reconcile::run_build_stage(
        &manifest.package.name,
        work,
        &runtime,
        command,
        &env,
    )?;
    // Pack the built copy; write the image to the user's dir (or --output).
    let built = build(&BuildOptions {
        dir: work.to_path_buf(),
        output: None,
        allow_insecure: args.insecure_source,
        arch,
        allow_secrets: args.allow_secrets,
        manifest: None,
    })?;
    let dest = match &args.output {
        Some(o) => o.clone(),
        None => args
            .dir
            .join(built.image_path.file_name().expect("image has a filename")),
    };
    std::fs::copy(&built.image_path, &dest)
        .with_context(|| format!("writing {}", dest.display()))?;
    for (name, version) in &built.locked {
        println!("locked {name} {version}");
    }
    println!(
        "built {} ({})",
        dest.display(),
        human_size(built.size_bytes)
    );
    println!("{}", built.digest);
    Ok(())
}

/// Recursively copy a source tree for the build stage, skipping VCS, prior
/// builds/images, and `node_modules` (the build stage regenerates them for the
/// target — that's the whole point).
fn copy_source(src: &Path, dst: &Path) -> std::io::Result<()> {
    fn skip(name: &str) -> bool {
        matches!(name, ".git" | "node_modules" | ".ply-build" | ".tmp") || name.ends_with(".img")
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if skip(&name.to_string_lossy()) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_source(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
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
        manifest: None,
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
