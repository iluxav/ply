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
    .map_err(|e| match userns_hint(e, kernel_restricts_userns()) {
        Some(hint) => Error::Runtime(format!("mount rprivate at /: {e}\nhint: {hint}")),
        None => merr("rprivate", Path::new("/"), e),
    })
}

pub fn unmount_detach(target: &Path) {
    let _ = nix::mount::umount2(target, nix::mount::MntFlags::MNT_DETACH);
}

/// The actionable hint for the first mount failing with EACCES in a fresh
/// user namespace: on kernels restricting unprivileged userns (Ubuntu
/// 24.04+ AppArmor policy), the fix is `sudo ply setup` — and running the
/// exact binary the installed profile names.
fn userns_hint(e: nix::errno::Errno, restricted: bool) -> Option<&'static str> {
    (e == nix::errno::Errno::EACCES && restricted).then_some(
        "this kernel restricts unprivileged user namespaces (Ubuntu 24.04+) — run `sudo ply setup` once; \
         if you already did, the AppArmor profile names a specific binary path: `which ply` must match the path in /etc/apparmor.d/ply",
    )
}

fn kernel_restricts_userns() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::errno::Errno;

    #[test]
    fn eacces_on_restricted_kernel_gets_setup_hint() {
        let hint = userns_hint(Errno::EACCES, true).expect("hint expected");
        assert!(hint.contains("sudo ply setup"));
        assert!(hint.contains("which ply"));
    }

    #[test]
    fn no_hint_when_unrestricted_or_other_errno() {
        assert!(userns_hint(Errno::EACCES, false).is_none());
        assert!(userns_hint(Errno::EPERM, true).is_none());
    }
}
