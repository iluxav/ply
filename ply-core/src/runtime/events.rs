//! The append-only journal of state changes: deploys, scales, restarts,
//! crash respawns. One JSON line per event in `<apps>/events.log`,
//! ring-rotated like the log rings. The actor that DOES the thing logs it
//! (reconcile, the run parent) — never the one that merely asked, so a
//! dashboard click and an ssh command leave identical trails.
//! `tail -f` on the file is the free CLI.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const CAP: u64 = 512 * 1024;

/// One journal line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub ts: u64,
    pub app: String,
    pub event: String,
    pub detail: String,
}

/// Every parseable line of the journal, oldest first. A half-written last
/// line, or a line from before the schema, is skipped, not an error.
pub fn read() -> Vec<Event> {
    std::fs::read_to_string(path())
        .map(|text| {
            text.lines()
                .filter_map(|l| serde_json::from_str::<Event>(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn path() -> PathBuf {
    crate::paths::apps_dir().join("events.log")
}

/// Append one event. Best-effort by design: an unwritable journal must
/// never fail the deploy/scale/restart it was going to describe.
pub fn emit(app: &str, event: &str, detail: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!(
        "{{\"ts\":{ts},\"app\":{},\"event\":{},\"detail\":{}}}\n",
        serde_json::to_string(app).unwrap_or_default(),
        serde_json::to_string(event).unwrap_or_default(),
        serde_json::to_string(detail).unwrap_or_default(),
    );
    let path = path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() + line.len() as u64 > CAP {
            let _ = std::fs::rename(&path, path.with_extension("log.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}
