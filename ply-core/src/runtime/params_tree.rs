//! Live params tree: `run_dir()/params/<app>/...`, bind-mounted into every
//! container at `/run/ply` (read-only) with the app's own directory
//! re-mounted read-write at `/run/ply/self` inside it (see
//! `runtime/container.rs`). An app reads any neighbor under `/run/ply/<app>`
//! and self-publishes facts by writing files under `/run/ply/self` — e.g.
//! `/run/ply/self/finish_boot`.
//!
//! Four files are PARENT-owned: the container's mount sequence re-binds
//! each of them, individually, read-only over itself inside `/run/ply/self`
//! (their targets must already exist on disk before that mount runs — the
//! parent writes them before spawning the instance, see `runtime/run.rs`).
//! An app can write anything else under its own tree but cannot forge
//! these.
//!
//! Only facts the parent already holds go in here — never a secret, never
//! a declared `[params]` value (that's the static side, `params.rs`/
//! `secrets.rs`; nothing here touches those).

use std::path::PathBuf;

use crate::error::{Error, Result};

/// Files only the parent ever writes. The container mount sequence
/// re-binds each of these read-only over itself inside `/run/ply/self`.
pub const PARENT_OWNED: &[&str] = &["state", "instances", "started_at", "restarts"];

/// Root of the whole tree — bind-mounted whole (read-only) into every
/// container at `/run/ply`.
pub fn root() -> PathBuf {
    crate::paths::run_dir().join("params")
}

/// One app's directory: `run_dir()/params/<app>`. Every instance of `app`
/// shares this node — last-writer-wins is accepted (spec).
pub fn dir(app: &str) -> PathBuf {
    root().join(app)
}

/// Write `key` = `value` for `app`, IN PLACE — deliberately NOT the
/// tmp-file-then-rename idiom used elsewhere in this codebase
/// (`deployments::write_status`). The container's mount sequence
/// (`runtime/container.rs`) bind-mounts each `PARENT_OWNED` file onto
/// itself individually, and that bind pins a specific inode: a `rename`
/// over the destination runs `detach_mounts()` on the replaced dentry in
/// every mount namespace, which would silently unmount every running
/// container's read-only re-bind on that file the moment it is next
/// republished. `state` is republished within seconds of every health
/// probe, so a rename-based `publish` would leave `state` forgeable for
/// effectively the whole life of every instance. Writing in place (open,
/// truncate, write, close) keeps the inode — and therefore every bind on
/// it — stable across every republish.
///
/// Values here are tiny, single writes on tmpfs; a reader racing a write
/// can see a shorter/older value rather than a torn one, and is expected
/// to re-poll (Task 8's `--after` condition wait already does).
pub fn publish(app: &str, key: &str, value: &str) -> Result<()> {
    use std::io::Write;

    let dir = dir(app);
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join(key);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
    file.write_all(value.as_bytes())
        .map_err(|source| Error::Io { path, source })
}

/// Read `key` for `app`, trimmed. `None` if it was never published (or
/// `app` has no tree yet).
pub fn read(app: &str, key: &str) -> Option<String> {
    std::fs::read_to_string(dir(app).join(key))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Best-effort cleanup on the app's final stop (its last instance gone
/// anywhere on the host, canaries included) — never fails the stop path.
pub fn remove_app(app: &str) {
    let _ = std::fs::remove_dir_all(dir(app));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::ENV_LOCK;
    use std::os::unix::fs::MetadataExt;

    // run_dir() honors $XDG_RUNTIME_DIR rootless (paths.rs:33-41) — point
    // it at a tempdir. XDG_RUNTIME_DIR is process-global, so this test
    // serializes through the crate-shared lock and restores the previous
    // value on the way out.
    #[test]
    fn publish_read_roundtrip_keeps_the_same_inode_on_republish() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        let td = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", td.path());

        publish("db", "state", "healthy").unwrap();
        assert_eq!(read("db", "state").as_deref(), Some("healthy"));
        let ino_before = std::fs::metadata(dir("db").join("state")).unwrap().ino();

        // The property the container's per-file read-only bind depends on:
        // republishing must write IN PLACE, never rename over the
        // destination, or the bind would silently detach the moment this
        // runs (see the doc comment on `publish`).
        publish("db", "state", "stopped").unwrap();
        assert_eq!(read("db", "state").as_deref(), Some("stopped"));
        let ino_after = std::fs::metadata(dir("db").join("state")).unwrap().ino();
        assert_eq!(
            ino_before, ino_after,
            "publish must write in place — a new inode would detach a bind mount on this file"
        );

        assert!(read("db", "ghost").is_none());

        match previous {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn remove_app_deletes_its_whole_directory() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        let td = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_RUNTIME_DIR", td.path());

        publish("web", "state", "starting").unwrap();
        assert!(dir("web").exists());
        remove_app("web");
        assert!(!dir("web").exists());
        // best-effort: removing an app with no tree at all never panics
        remove_app("ghost");

        match previous {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
    }

    #[test]
    fn parent_owned_is_exactly_the_four_lifecycle_files() {
        assert_eq!(
            PARENT_OWNED,
            &["state", "instances", "started_at", "restarts"]
        );
    }
}
