//! App records: `/var/lib/ply/apps/<name>.json` — the GC root set.
//!
//! Written on every `ply run`; removed by `ply rm`. Reachability for the
//! store is defined by these records plus currently-running instances.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const APPS_DIR: &str = "/var/lib/ply/apps";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub name: String,
    /// Absolute path of the app image last run.
    pub image: PathBuf,
    /// Locked store digests this app needs (its dependency closure).
    pub digests: Vec<String>,
    /// Unix seconds of the last run.
    pub updated: u64,
}

fn record_path(name: &str) -> PathBuf {
    Path::new(APPS_DIR).join(format!("{name}.json"))
}

impl AppRecord {
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(APPS_DIR).map_err(|source| Error::Io {
            path: APPS_DIR.into(),
            source,
        })?;
        let path = record_path(&self.name);
        std::fs::write(&path, serde_json::to_vec_pretty(self).expect("serializes"))
            .map_err(|source| Error::Io { path, source })
    }

    pub fn remove(name: &str) -> bool {
        std::fs::remove_file(record_path(name)).is_ok()
    }

    pub fn load(name: &str) -> Option<AppRecord> {
        let text = std::fs::read_to_string(record_path(name)).ok()?;
        serde_json::from_str(&text).ok()
    }
}

pub fn list() -> Result<Vec<AppRecord>> {
    let mut records = Vec::new();
    let entries = match std::fs::read_dir(APPS_DIR) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(records),
        Err(source) => {
            return Err(Error::Io {
                path: APPS_DIR.into(),
                source,
            })
        }
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            if let Ok(record) = serde_json::from_str::<AppRecord>(&text) {
                records.push(record);
            }
        }
    }
    records.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(records)
}
