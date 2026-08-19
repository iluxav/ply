//! Root vs rootless path resolution. Root keeps the spec locations;
//! rootless lives entirely in the user's own directories.

use std::path::PathBuf;

pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Ephemeral state (instances, state files). tmpfs either way.
pub fn run_dir() -> PathBuf {
    if is_root() {
        PathBuf::from("/run/ply")
    } else if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("ply")
    } else {
        PathBuf::from(format!("/tmp/ply-{}", nix::unistd::geteuid()))
    }
}

/// Durable data (volumes, app records, craft sessions).
pub fn data_dir() -> PathBuf {
    if is_root() {
        PathBuf::from("/var/lib/ply")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/ply")
    } else {
        PathBuf::from(format!("/tmp/ply-{}-data", nix::unistd::geteuid()))
    }
}

pub fn volumes_dir() -> PathBuf {
    data_dir().join("volumes")
}

pub fn apps_dir() -> PathBuf {
    data_dir().join("apps")
}

pub fn craft_dir() -> PathBuf {
    data_dir().join("craft")
}

/// remove_dir_all that survives mode-000 directories (overlayfs creates its
/// `work/work` dir unreadable; without CAP_DAC_OVERRIDE a plain removal
/// fails). Heals permissions on the way down, then removes.
pub fn force_remove_dir_all(path: &std::path::Path) -> std::io::Result<()> {
    if std::fs::remove_dir_all(path).is_ok() {
        return Ok(());
    }
    fn heal(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.is_dir() && !p.is_symlink() {
                    heal(&p);
                }
            }
        }
    }
    heal(path);
    std::fs::remove_dir_all(path)
}
