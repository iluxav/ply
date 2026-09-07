//! Root vs rootless path resolution. Root keeps the spec locations;
//! rootless lives entirely in the user's own directories.

use std::path::PathBuf;

/// `XDG_RUNTIME_DIR` (read by `run_dir()` below) is process-global —
/// every test across the crate that points it at a tempdir must serialize
/// through this one lock, or two tests in the same binary race each
/// other's mutation. Shared here (not duplicated per module) because this
/// is the module that actually reads the variable.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Real root — not root *inside a user namespace*.
///
/// Both look identical to `geteuid()`, and the difference decides
/// everything: whether ply builds a bridge, writes /etc/systemd, sets
/// cgroup limits. Inside `ply up`'s own namespace a process is uid 0 (it
/// must be, or `execve` would strip the capabilities its children need to
/// be mapped) while having none of root's actual reach. The initial user
/// namespace is the one mapping the whole id range as itself.
pub fn is_root() -> bool {
    nix::unistd::geteuid().is_root() && in_initial_user_ns()
}

/// Every "am I root?" in ply routes through `is_root` for this reason: a
/// process inside a user namespace sees euid 0 and would otherwise pick
/// root's paths (`/var/lib/ply/store`) that it cannot write.
fn in_initial_user_ns() -> bool {
    match std::fs::read_to_string("/proc/self/uid_map") {
        Ok(map) => {
            let f: Vec<&str> = map.split_whitespace().collect();
            f == ["0", "0", "4294967295"]
        }
        // unreadable (no /proc): trust the uid, as ply always did
        Err(_) => true,
    }
}

/// The uid this process has OUTSIDE any user namespace it sits in: the one
/// its files are owned by, and the one a sibling process on the host
/// (`ply ps`) computes paths from. Inside a namespace `geteuid()` says 0,
/// and a path built from it — `/tmp/ply-0` — is one nothing else can find.
pub fn host_uid() -> u32 {
    let euid = nix::unistd::geteuid().as_raw();
    match std::fs::read_to_string("/proc/self/uid_map") {
        Ok(map) => outer_uid(&map, euid),
        Err(_) => euid,
    }
}

/// `uid_map` lines are `inner outer count`; translate `euid` through the
/// entry that contains it. An id outside every entry maps to itself.
fn outer_uid(map: &str, euid: u32) -> u32 {
    for line in map.lines() {
        let f: Vec<u64> = line
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        if let [inner, outer, count] = f[..] {
            let e = euid as u64;
            if e >= inner && e < inner + count {
                return (outer + (e - inner)) as u32;
            }
        }
    }
    euid
}

/// Ephemeral state (instances, state files). tmpfs either way.
pub fn run_dir() -> PathBuf {
    if is_root() {
        PathBuf::from("/run/ply")
    } else if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(xdg).join("ply")
    } else {
        PathBuf::from(format!("/tmp/ply-{}", host_uid()))
    }
}

/// Durable data (volumes, app records, craft sessions).
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PLY_DATA_DIR") {
        PathBuf::from(dir)
    } else if is_root() {
        PathBuf::from("/var/lib/ply")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/ply")
    } else {
        PathBuf::from(format!("/tmp/ply-{}-data", host_uid()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The rootless parent lives in a user namespace where it is uid 0;
    /// the state directory it writes must be the one `ply ps` — outside,
    /// uid 1000 — reads. Seen on a fresh droplet as an app that `ply ps`
    /// could not see while it served.
    #[test]
    fn paths_are_built_from_the_uid_the_host_sees() {
        assert_eq!(outer_uid("         0       1000          1\n", 0), 1000);
        assert_eq!(outer_uid("0 1000 1\n1 100000 65536\n", 70), 100069);
        assert_eq!(outer_uid("0 0 4294967295\n", 1000), 1000);
        assert_eq!(outer_uid("garbage\n", 1000), 1000);
    }
}
