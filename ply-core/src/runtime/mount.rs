//! Thin mount helpers over nix::mount.

use std::path::Path;

use nix::mount::{mount, MsFlags};

use crate::error::{Error, Result};

fn merr(what: &str, target: &Path, e: nix::errno::Errno) -> Error {
    Error::Runtime(format!("mount {what} at {}: {e}", target.display()))
}

pub fn mount_squashfs_ro(device: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target).map_err(|source| Error::Io {
        path: target.to_path_buf(),
        source,
    })?;
    mount(
        Some(device),
        target,
        Some("squashfs"),
        MsFlags::MS_RDONLY | MsFlags::MS_NODEV,
        None::<&str>,
    )
    .map_err(|e| merr("squashfs", target, e))
}

pub fn mount_overlay(lower: &[&Path], upper: &Path, work: &Path, target: &Path) -> Result<()> {
    let lowerdir: Vec<String> = lower.iter().map(|p| p.display().to_string()).collect();
    let options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lowerdir.join(":"),
        upper.display(),
        work.display()
    );
    mount(
        Some("overlay"),
        target,
        Some("overlay"),
        MsFlags::empty(),
        Some(options.as_str()),
    )
    .map_err(|e| {
        Error::Runtime(format!(
            "mount overlay at {}: {e} (unprivileged overlayfs needs kernel >= 5.11)",
            target.display()
        ))
    })
}

pub fn mount_tmpfs(target: &Path, options: &str) -> Result<()> {
    mount_tmpfs_flags(target, options, MsFlags::empty())
}

/// Scratch tmpfs: always nosuid; callers add noexec/nodev where appropriate.
pub fn mount_tmpfs_flags(target: &Path, options: &str, extra: MsFlags) -> Result<()> {
    mount(
        Some("tmpfs"),
        target,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | extra,
        Some(options),
    )
    .map_err(|e| merr("tmpfs", target, e))
}

pub fn mount_proc(target: &Path) -> Result<()> {
    mount(
        Some("proc"),
        target,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(|e| merr("proc", target, e))
}

pub fn mount_devpts(target: &Path) -> Result<()> {
    mount(
        Some("devpts"),
        target,
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some("newinstance,ptmxmode=0666,mode=0620"),
    )
    .map_err(|e| merr("devpts", target, e))
}

/// Stop mount events propagating to the host (must run before any mount in a
/// fresh mount namespace).
pub fn make_all_private() -> Result<()> {
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| merr("rprivate", Path::new("/"), e))
}

pub fn unmount_detach(target: &Path) {
    let _ = nix::mount::umount2(target, nix::mount::MntFlags::MNT_DETACH);
}
