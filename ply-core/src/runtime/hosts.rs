//! Managed /etc/hosts entries: `<ip>\t<app>.ply  # ply:<app>.<n>`.
//!
//! Names map to IPs only (never ports). `.ply` avoids mDNS-reserved `.local`.
//! Every managed line carries its instance tag, so removal is exact and
//! `rm -rf /var/lib/ply` still leaves /etc/hosts recoverable by tag.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

const HOSTS: &str = "/etc/hosts";

#[cfg(target_os = "linux")]
fn tag(app: &str, n: u32) -> String {
    format!("# ply:{app}.{n}")
}

#[cfg(target_os = "linux")]
fn rewrite<F>(edit: F) -> Result<()>
where
    F: FnOnce(Vec<String>) -> Vec<String>,
{
    let path = Path::new(HOSTS);
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.into(),
        source,
    })?;
    let ends_nl = text.ends_with('\n');
    let lines: Vec<String> = text.lines().map(String::from).collect();
    let mut new_lines = edit(lines);
    let mut out = new_lines.join("\n");
    if ends_nl || new_lines.last().map(|l| !l.is_empty()).unwrap_or(false) {
        out.push('\n');
    }
    new_lines.clear();
    std::fs::write(path, out).map_err(|source| Error::Io {
        path: path.into(),
        source,
    })
}

/// The host's `/etc/hosts` is the namespace backend's mechanism and only
/// its mechanism: a container resolves `<peer>.ply` through a bind mount of
/// this file. A microVM has its own `/etc/hosts` INSIDE the guest, written
/// by the guest init from the spec disk, so there is nothing here to manage
/// — and trying anyway is not merely useless, it is destructive:
/// `/etc/hosts` on a Mac is root-owned, the rewrite fails with EACCES, and
/// `reap_stale`'s `?` propagates that failure before it removes the state
/// file. The visible symptom is `ply ps` listing an instance as `dead`
/// forever after its VM is gone.
#[cfg(not(target_os = "linux"))]
pub fn add_entry(_app: &str, _n: u32, _ip: Ipv4Addr) -> Result<()> {
    Ok(())
}

/// See `add_entry`: nothing to remove, and failing to not-remove it used to
/// strand every dead instance in `ply ps`.
#[cfg(not(target_os = "linux"))]
pub fn remove_entry(_app: &str, _n: u32) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn add_entry(app: &str, n: u32, ip: Ipv4Addr) -> Result<()> {
    let line = format!("{ip}\t{app}.ply\t{}", tag(app, n));
    let t = tag(app, n);
    rewrite(move |mut lines| {
        lines.retain(|l| !l.ends_with(&t));
        lines.push(line);
        lines
    })?;
    refresh_instances();
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn remove_entry(app: &str, n: u32) -> Result<()> {
    let t = tag(app, n);
    rewrite(move |mut lines| {
        lines.retain(|l| !l.ends_with(&t));
        lines
    })?;
    refresh_instances();
    Ok(())
}

/// The name file a container reads. It is bind-mounted onto the container's
/// /etc/hosts rather than copied into it: an instance that dies and comes
/// back returns on a NEW bridge address, and a sibling holding a start-time
/// copy would keep dialling the dead one until someone restarted it. One
/// file, rewritten in place, and every running container sees the change.
pub fn instance_file(instance_dir: &Path) -> PathBuf {
    instance_dir.join("hosts")
}

/// The instance's own lines — its hostname, and loopback aliases for the
/// siblings sharing its namespace. They never change, so they are kept
/// beside the composed file and re-appended on every refresh.
fn local_file(instance_dir: &Path) -> PathBuf {
    instance_dir.join("hosts.local")
}

/// Record this instance's own lines and compose its name file.
pub fn write_instance_file(instance_dir: &Path, local: &str) -> Result<()> {
    write_local(instance_dir, local)?;
    compose(instance_dir)
}

fn write_local(instance_dir: &Path, local: &str) -> Result<()> {
    let path = local_file(instance_dir);
    std::fs::write(&path, local).map_err(|source| Error::Io { path, source })
}

fn compose(instance_dir: &Path) -> Result<()> {
    let host = std::fs::read_to_string(HOSTS).map_err(|source| Error::Io {
        path: HOSTS.into(),
        source,
    })?;
    compose_from(&host, instance_dir)
}

fn compose_from(host: &str, instance_dir: &Path) -> Result<()> {
    let mut text = host.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&std::fs::read_to_string(local_file(instance_dir)).unwrap_or_default());

    // Overwrite in place, then trim: the container holds this very inode
    // through a bind mount, and a create-truncate-write would leave a window
    // where a resolver reads an empty name database.
    use std::io::{Seek, Write};
    let path = instance_file(instance_dir);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
    f.write_all(text.as_bytes())
        .and_then(|()| f.stream_position())
        .and_then(|len| f.set_len(len))
        .map_err(|source| Error::Io { path, source })
}

/// Push the current /etc/hosts into every instance's name file. Called after
/// any managed edit — this is what makes a peer's new address visible to
/// containers that are already running.
#[cfg(target_os = "linux")]
fn refresh_instances() {
    let dir = crate::paths::run_dir().join("instances");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let instance_dir = entry.path();
        // hosts.local marks an instance that took the bind-mounted file;
        // anything else in here is not ours to write.
        if local_file(&instance_dir).exists() {
            let _ = compose(&instance_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// The instance file is the host's table plus the instance's own lines,
    /// and a refresh must REPLACE the host half — the whole point is that a
    /// peer's new address reaches a container that is already running.
    #[test]
    fn a_refresh_replaces_the_host_half_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_local(dir.path(), "127.0.0.1\tweb-1\n").expect("local lines");

        compose_from(
            "127.0.0.1 localhost\n10.77.0.3\tdb.ply\t# ply:db.1\n10.77.0.9\tcache.ply\t# ply:cache.1\n",
            dir.path(),
        )
        .expect("compose");
        let first = std::fs::read_to_string(instance_file(dir.path())).expect("read");
        assert!(first.contains("10.77.0.3\tdb.ply"));
        assert!(
            first.ends_with("127.0.0.1\tweb-1\n"),
            "own lines kept: {first:?}"
        );
        let inode = std::fs::metadata(instance_file(dir.path()))
            .expect("stat")
            .ino();

        // db restarts on a new address and cache is gone — a SHORTER table,
        // so anything left past its end would be read as a live entry.
        compose_from(
            "127.0.0.1 localhost\n10.77.0.5\tdb.ply\t# ply:db.2\n",
            dir.path(),
        )
        .expect("recompose");
        let second = std::fs::read_to_string(instance_file(dir.path())).expect("read");
        assert_eq!(
            second, "127.0.0.1 localhost\n10.77.0.5\tdb.ply\t# ply:db.2\n127.0.0.1\tweb-1\n",
            "the file must be exactly the new table plus this instance's own lines — \
             a shorter table that leaves the old tail behind resolves dead peers",
        );

        // Same inode: the container holds this file through a bind mount, so
        // rewriting it is the only way the change is ever seen inside.
        assert_eq!(
            inode,
            std::fs::metadata(instance_file(dir.path())).expect("stat").ino(),
            "the instance file was replaced, not rewritten — a bound container would still read the old inode",
        );
    }
}
