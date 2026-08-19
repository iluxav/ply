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
