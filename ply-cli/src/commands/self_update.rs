//! `ply self-update` — the platform is the last thing that should need a
//! human with ssh. Resolve the newest release (the redirect trick — no
//! API, no rate limit), download the binary for this arch, verify it
//! answers `--version` with the expected number, and atomically replace
//! the running executable.
//!
//! What it deliberately does NOT do: restart apps. Long-running run
//! parents keep executing the old binary until their unit restarts —
//! `ply ps` marks those instances stale, and each app's next deploy or
//! restart absorbs the update naturally.

use anyhow::{bail, Context, Result};

use crate::cli::SelfUpdateArgs;

const REPO: &str = "iluxav/ply";

pub fn exec(args: SelfUpdateArgs) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let latest =
        ply_core::github::latest_version(REPO, None).context("resolving the latest release")?;

    if args.check {
        if latest == current {
            println!("ply {current} — current");
        } else {
            println!("ply {current} — v{latest} available (run `ply self-update`)");
        }
        return Ok(());
    }
    if latest == current {
        println!("ply {current} — already current");
        return Ok(());
    }

    let exe = std::env::current_exe().context("locating own binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent().context("own binary has no parent directory")?;

    let arch = ply_core::image::name::Arch::host();
    let url = format!(
        "https://github.com/{REPO}/releases/download/v{latest}/ply-linux-{}",
        arch.as_str()
    );
    println!("ply {current} -> v{latest} ({url})");

    // same directory as the target: rename stays atomic (same filesystem)
    let tmp = dir.join(format!(".ply-update.{}", std::process::id()));
    let outcome = (|| -> Result<()> {
        ply_core::github::download(&url, &tmp).context("downloading")?;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&tmp, perms)?;

        // the downloaded binary must introduce itself correctly before it
        // may replace the one that works
        let said = std::process::Command::new(&tmp)
            .arg("--version")
            .output()
            .context("running the downloaded binary")?;
        let said = String::from_utf8_lossy(&said.stdout);
        if !said.contains(&latest) {
            bail!(
                "downloaded binary answers `{}` — expected {latest}",
                said.trim()
            );
        }
        std::fs::rename(&tmp, &exe).with_context(|| {
            format!(
                "installing over {} (root required for system installs)",
                exe.display()
            )
        })?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    outcome?;

    println!("ply v{latest} installed at {}", exe.display());
    println!("running apps keep their old supervisor until restarted — `ply ps` marks them stale");
    Ok(())
}
