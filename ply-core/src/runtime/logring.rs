//! Bounded per-instance log rings: `<run_dir>/logs/<app>.<n>.log`.
//!
//! The run parent tees every instance's stdout+stderr here (while still
//! passing it through to its own stdout, so journald/terminal behavior is
//! unchanged). Files rotate once at CAP bytes — recent history is bounded at
//! ~1 MiB per instance in the run dir's tmpfs, and journald under systemd
//! remains the unbounded archive. Logs are files: `ply logs` and the
//! dashboard read the same bytes with no daemon and no protocol.

use std::io::Write;
use std::path::PathBuf;

use crate::error::{Error, Result};

/// Rotate threshold per file; two files kept (live + `.1`).
pub const CAP_BYTES: u64 = 512 * 1024;

pub fn dir() -> PathBuf {
    crate::paths::run_dir().join("logs")
}

pub fn path(app: &str, n: u32) -> PathBuf {
    dir().join(format!("{app}.{n}.log"))
}

/// Append-only writer with one rotation. Created per instance launch;
/// restart of a slot truncates (each run's ring starts fresh, `.1` keeps
/// the previous generation's tail).
pub struct RingWriter {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
}

impl RingWriter {
    pub fn create(app: &str, n: u32) -> Result<RingWriter> {
        let dir = dir();
        std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
            path: dir.clone(),
            source,
        })?;
        let path = self::path(app, n);
        // rotate the previous generation out, start clean
        if path.exists() {
            let _ = std::fs::rename(&path, rotated(&path));
        }
        let file = std::fs::File::create(&path).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        Ok(RingWriter {
            file,
            path,
            written: 0,
        })
    }

    pub fn append(&mut self, bytes: &[u8]) {
        if self.written >= CAP_BYTES {
            let _ = std::fs::rename(&self.path, rotated(&self.path));
            match std::fs::File::create(&self.path) {
                Ok(f) => {
                    self.file = f;
                    self.written = 0;
                }
                Err(_) => return, // run dir gone (shutdown); drop silently
            }
        }
        if self.file.write_all(bytes).is_ok() {
            self.written += bytes.len() as u64;
        }
    }
}

fn rotated(path: &std::path::Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".1");
    PathBuf::from(os)
}

/// The last `lines` lines for one instance, rotation-aware.
pub fn tail(app: &str, n: u32, lines: usize) -> Vec<String> {
    let live = path(app, n);
    let mut text = String::new();
    if let Ok(prev) = std::fs::read_to_string(rotated(&live)) {
        text.push_str(&prev);
    }
    if let Ok(cur) = std::fs::read_to_string(&live) {
        text.push_str(&cur);
    }
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].iter().map(|s| s.to_string()).collect()
}

/// Instances that have a ring, as (app, n) — what `ply logs` can show.
pub fn list() -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".log") else {
            continue;
        };
        if let Some((app, n)) = stem.rsplit_once('.') {
            if let Ok(n) = n.parse() {
                out.push((app.to_string(), n));
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // XDG_RUNTIME_DIR is process-global; tests touching it must not overlap.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_run_dir<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("XDG_RUNTIME_DIR");
        std::env::set_var("XDG_RUNTIME_DIR", tmp.path());
        let out = f();
        match previous {
            Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
            None => std::env::remove_var("XDG_RUNTIME_DIR"),
        }
        drop(tmp);
        out
    }

    #[test]
    fn append_tail_and_rotation() {
        with_run_dir(|| {
            let mut w = RingWriter::create("web", 1).unwrap();
            for i in 0..10 {
                w.append(format!("line {i}\n").as_bytes());
            }
            assert_eq!(tail("web", 1, 3), vec!["line 7", "line 8", "line 9"]);
            assert_eq!(tail("web", 1, 100).len(), 10);

            // force rotation: exceed the cap, then write more
            let big = vec![b'x'; CAP_BYTES as usize];
            w.append(&big);
            w.append(b"\nafter rotation\n");
            let t = tail("web", 1, 2);
            assert_eq!(t.last().unwrap(), "after rotation");
            assert!(
                path("web", 1).with_extension("log.1").exists()
                    || rotated(&path("web", 1)).exists()
            );

            // a fresh writer (slot restart) rotates the old ring out
            let mut w2 = RingWriter::create("web", 1).unwrap();
            w2.append(b"new generation\n");
            let t = tail("web", 1, 1000);
            assert_eq!(t.last().unwrap(), "new generation");
            assert!(
                t.contains(&"after rotation".to_string()),
                "previous generation tail kept"
            );

            assert_eq!(list(), vec![("web".to_string(), 1)]);
        });
    }

    #[test]
    fn tail_of_nothing_is_empty() {
        with_run_dir(|| {
            assert!(tail("ghost", 9, 10).is_empty());
        });
    }
}
