//! Instance state = files: `/run/ply/state/<app>.<n>.json`. tmpfs, gone on
//! reboot. `ply ps --json` is just these files, pid-checked.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceState {
    pub app: String,
    pub n: u32,
    pub pid: i32,
    pub ip: Ipv4Addr,
    pub ports: BTreeMap<String, u16>,
    pub image: String,
    /// Unix seconds.
    pub started: u64,
    /// Times the run parent respawned this slot ([restart] policy).
    #[serde(default)]
    pub restarts: u32,
    /// `[health] port` of the manifest this instance runs — what `--after`
    /// probes. Absent in state written by older parents (alive is the bar).
    #[serde(default)]
    pub health_port: Option<u16>,
    /// `--publish`: the host port this app's run parent listens on, and the
    /// address a depending app should dial. Every instance of an app records
    /// the same pair — it belongs to the parent, not the instance, and this
    /// is the only place a *reader* can find it. None = not published.
    #[serde(default)]
    pub published_port: Option<u16>,
    #[serde(default)]
    pub published_addr: Option<String>,
}

fn state_dir() -> PathBuf {
    crate::paths::run_dir().join("state")
}

impl InstanceState {
    pub fn path(app: &str, n: u32) -> PathBuf {
        state_dir().join(format!("{app}.{n}.json"))
    }

    pub fn save(&self) -> Result<()> {
        let dir = state_dir();
        std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
            path: dir.clone(),
            source,
        })?;
        let path = Self::path(&self.app, self.n);
        std::fs::write(&path, serde_json::to_vec_pretty(self).expect("serializes"))
            .map_err(|source| Error::Io { path, source })
    }

    pub fn remove(app: &str, n: u32) {
        let _ = std::fs::remove_file(Self::path(app, n));
    }

    pub fn alive(&self) -> bool {
        // signal 0 = existence probe
        unsafe { nix::libc::kill(self.pid, 0) == 0 }
    }
}

/// All state files, sorted by (app, n).
pub fn list() -> Result<Vec<InstanceState>> {
    let dir = state_dir();
    let mut states = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(states),
        Err(source) => return Err(Error::Io { path: dir, source }),
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            if let Ok(state) = serde_json::from_str::<InstanceState>(&text) {
                states.push(state);
            }
        }
    }
    states.sort_by(|a, b| (&a.app, a.n).cmp(&(&b.app, b.n)));
    Ok(states)
}

/// Remove state (+ leftover instance dirs, mounts, hosts lines) of dead
/// instances — the recovery path after a kill -9 of ply itself.
pub fn reap_stale() -> Result<Vec<InstanceState>> {
    let mut reaped = Vec::new();
    for state in list()? {
        if state.alive() {
            continue;
        }
        let instance_dir = crate::paths::run_dir()
            .join("instances")
            .join(format!("{}.{}", state.app, state.n));
        if instance_dir.exists() {
            let layers = instance_dir.join("layers");
            if let Ok(entries) = std::fs::read_dir(&layers) {
                for entry in entries.filter_map(|e| e.ok()) {
                    crate::runtime::mount::unmount_detach(&entry.path());
                }
            }
            let _ = crate::paths::force_remove_dir_all(&instance_dir);
        }
        crate::runtime::hosts::remove_entry(&state.app, state.n)?;
        InstanceState::remove(&state.app, state.n);
        reaped.push(state);
    }
    Ok(reaped)
}
