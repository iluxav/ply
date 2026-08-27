//! The control dir: commands as files, the daemontools way.
//!
//! `<apps>/<app>/control/` is watched (2s poll) by the app's run parent —
//! the process that already supervises the instances, so no new resident
//! anything. A command is a file, atomic-renamed in by whoever holds write
//! permission on the dir: the CLI, the dashboard (via an rw link the
//! operator granted), or `echo` over ssh in a pinch. The parent consumes
//! the file, acts, and writes `last-result` — auditable with `cat`.
//!
//! Commands:
//!   scale       content = target instance count
//!   restart     rolling restart on the current image (content ignored)
//!   next-image  the deploy pointer (lives one level up, unchanged — the
//!               poll makes file-only deploys work; SIGHUP stays instant)
//!
//! Permissions ARE the ACL: who may write the dir may command the app.

use std::path::PathBuf;

use crate::error::{Error, Result};

pub fn dir(app: &str) -> PathBuf {
    crate::paths::apps_dir().join(app).join("control")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Scale(u32),
    Restart,
    /// Open a terminal into a slot: the parent answers with a PTY served
    /// on `control/term-<nonce>.sock`.
    Exec {
        slot: u32,
        nonce: String,
    },
}

/// Read and CONSUME pending commands (files are removed; half-written
/// content is the writer's bug — atomic rename is the documented protocol).
/// Unknown files are left alone: forward-compat and scratch-safety.
pub fn poll(app: &str) -> Vec<Command> {
    let dir = dir(app);
    let mut out = Vec::new();

    let scale = dir.join("scale");
    if let Ok(text) = std::fs::read_to_string(&scale) {
        let _ = std::fs::remove_file(&scale);
        match text.trim().parse::<u32>() {
            Ok(n) if (1..=100).contains(&n) => out.push(Command::Scale(n)),
            _ => write_result(
                app,
                "scale",
                false,
                &format!("invalid target `{}` (want 1..=100)", text.trim()),
            ),
        }
    }

    let restart = dir.join("restart");
    if restart.exists() {
        let _ = std::fs::remove_file(&restart);
        out.push(Command::Restart);
    }

    let exec = dir.join("exec");
    if let Ok(text) = std::fs::read_to_string(&exec) {
        let _ = std::fs::remove_file(&exec);
        let mut parts = text.split_whitespace();
        let slot = parts.next().and_then(|s| s.parse::<u32>().ok());
        let nonce = parts
            .next()
            .filter(|n| (8..=64).contains(&n.len()) && n.chars().all(|c| c.is_ascii_hexdigit()));
        match (slot, nonce) {
            (Some(slot), Some(nonce)) => out.push(Command::Exec {
                slot,
                nonce: nonce.to_string(),
            }),
            _ => write_result(app, "exec", false, "exec wants `<slot> <hex-nonce>`"),
        }
    }

    out
}

/// Submit a command from the outside (CLI, tools): atomic rename into the
/// control dir. The parent picks it up within its poll interval; pass the
/// parent pids to `nudge` for instant handling.
pub fn submit(app: &str, name: &str, content: &str) -> Result<()> {
    let dir = dir(app);
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, content).map_err(|source| Error::Io {
        path: tmp.clone(),
        source,
    })?;
    let target = dir.join(name);
    std::fs::rename(&tmp, &target).map_err(|source| Error::Io {
        path: target,
        source,
    })
}

/// `last-result` — one JSON line the parent writes after acting.
pub fn write_result(app: &str, command: &str, ok: bool, detail: &str) {
    let dir = dir(app);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!(
        "{{\"command\":{:?},\"ok\":{ok},\"detail\":{:?},\"ts\":{ts}}}\n",
        command, detail
    );
    let tmp = dir.join(".last-result.tmp");
    if std::fs::write(&tmp, line).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("last-result"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_apps_dir<T>(f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("PLY_DATA_DIR");
        std::env::set_var("PLY_DATA_DIR", tmp.path());
        let out = f();
        match prev {
            Some(v) => std::env::set_var("PLY_DATA_DIR", v),
            None => std::env::remove_var("PLY_DATA_DIR"),
        }
        drop(tmp);
        out
    }

    #[test]
    fn submit_poll_consume_roundtrip() {
        with_apps_dir(|| {
            submit("web", "scale", "4").unwrap();
            submit("web", "restart", "").unwrap();
            assert_eq!(poll("web"), vec![Command::Scale(4), Command::Restart]);
            // consumed: second poll is empty
            assert_eq!(poll("web"), vec![]);
        });
    }

    #[test]
    fn bad_scale_writes_a_result_instead_of_acting() {
        with_apps_dir(|| {
            submit("web", "scale", "zero").unwrap();
            assert_eq!(poll("web"), vec![]);
            let result = std::fs::read_to_string(dir("web").join("last-result")).unwrap();
            assert!(result.contains("\"ok\":false"), "{result}");
            assert!(result.contains("invalid target"), "{result}");
        });
    }

    #[test]
    fn unknown_files_are_left_alone() {
        with_apps_dir(|| {
            submit("web", "future-command", "x").unwrap();
            assert_eq!(poll("web"), vec![]);
            assert!(dir("web").join("future-command").exists());
        });
    }
}
