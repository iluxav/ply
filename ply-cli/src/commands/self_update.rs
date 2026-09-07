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

    let asset = asset_name(
        std::env::consts::OS,
        ply_core::image::name::Arch::host().as_str(),
    );
    let url = format!("https://github.com/{REPO}/releases/download/v{latest}/{asset}");
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

/// The release asset for a host: `ply-<os>-<arch>`, the names `release.yml`
/// uploads and `install.sh` downloads. Rust calls the Mac `macos`; the asset
/// says `darwin`, because the installer picks its file by `uname -s` and
/// that is what `uname` says there.
///
/// The macOS binary in a release is signed with the hypervisor entitlement,
/// and the signature lives inside the Mach-O, so the download is the
/// installed binary: nothing to re-sign here.
fn asset_name(os: &str, arch: &str) -> String {
    let os = match os {
        "macos" => "darwin",
        other => other,
    };
    format!("ply-{os}-{arch}")
}

#[cfg(test)]
mod tests {
    use super::asset_name;

    /// One name, three places: this function, the `artifact:` names in
    /// `release.yml`, and the `url=` line in `install.sh`. The installer
    /// picks the file by `uname -s`, so the macOS asset says `darwin`.
    #[test]
    fn asset_names_match_the_release_workflow() {
        assert_eq!(asset_name("linux", "x64"), "ply-linux-x64");
        assert_eq!(asset_name("linux", "arm64"), "ply-linux-arm64");
        assert_eq!(asset_name("macos", "arm64"), "ply-darwin-arm64");
    }
}
