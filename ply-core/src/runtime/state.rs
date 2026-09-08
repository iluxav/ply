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
    /// The port the app itself listens on. Inside a stack's own network that
    /// is what a sibling dials — the published pair is the HOST's side of
    /// the proxy and means nothing in there.
    #[serde(default)]
    pub instance_port: Option<u16>,
    /// `--domain` hostnames the edge should route to this app's published
    /// address. Parent-owned like the published pair; every instance of an
    /// app records the same list. `ply proxy` turns these into vhosts.
    #[serde(default)]
    pub domains: Vec<String>,
    /// How to reach `ip` from a process that is not this instance's parent.
    ///
    /// `None` — every Linux path, and every state file written before this
    /// field existed — means `ip` is an address the host can dial. A microVM
    /// instance's is not: it lives on a userspace switch inside its run
    /// parent, and this names the unix socket that switch listens on. It is
    /// the only thing that lets a `--after` gate in a DIFFERENT `ply run`
    /// probe this instance's health port at all.
    ///
    /// `skip_serializing_if` — unlike its neighbours — so that `ply ps
    /// --json` on Linux, where this is always `None`, prints exactly what it
    /// printed before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<PathBuf>,
    /// Is this instance actually taking traffic on the app's published
    /// ports? Written `false` at launch for a published app and flipped
    /// when the run parent seats it in the pool, which happens after its
    /// health gate passes.
    ///
    /// The two moments are not the same, and only the parent can see the
    /// second: a watcher in another process cannot tell a pool with a
    /// backend from one without, because the parent binds the host port at
    /// startup and accepts on it either way. `ply deploy` used to report
    /// "complete" in that gap, and the next request after a deploy could be
    /// answered by nothing at all.
    ///
    /// Defaults to `true`, which is what an app with nothing published
    /// means and what every state file written before this field existed
    /// has to keep meaning.
    #[serde(default = "yes")]
    pub serving: bool,
}

fn yes() -> bool {
    true
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
        // Written whole beside the file, then renamed over it: a reader sees
        // the old file or the new one, never half of either. `mark_serving`
        // rewrites this while the instance is alive, and a `ply exec`
        // listing the directory in that instant could read a torn file and
        // conclude the instance was gone.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("serializes")).map_err(
            |source| Error::Io {
                path: tmp.clone(),
                source,
            },
        )?;
        std::fs::rename(&tmp, &path).map_err(|source| Error::Io { path, source })
    }

    /// The run parent has seated this instance in its published pools.
    /// Cheap and best-effort: a state file that cannot be read or written
    /// here means `ply deploy` waits a little longer, never that it lies.
    pub fn mark_serving(app: &str, n: u32) {
        let path = Self::path(app, n);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(mut state) = serde_json::from_str::<InstanceState>(&text) else {
            return;
        };
        if state.serving {
            return;
        }
        state.serving = true;
        let _ = state.save();
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
        // Only the real files: `save` writes a `.json.tmp` beside each one
        // and renames it into place, and a complete-but-not-yet-renamed one
        // must not count as a second instance.
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            if let Ok(state) = serde_json::from_str::<InstanceState>(&text) {
                states.push(state);
            }
        }
    }
    states.sort_by(|a, b| (&a.app, a.n).cmp(&(&b.app, b.n)));
    Ok(states)
}

/// The running instance a user meant by `myapp` or `myapp.2`.
///
/// One resolver for both backends: `ply exec` means the same thing whether
/// it enters a namespace or talks to a microVM, so the way it picks an
/// instance — and the error when nothing matches — should not depend on
/// which one is underneath.
pub fn find(target: &str) -> Result<InstanceState> {
    let states = list()?;
    let exact: Option<&InstanceState> = target.rsplit_once('.').and_then(|(app, n)| {
        let n: u32 = n.parse().ok()?;
        states.iter().find(|s| s.app == app && s.n == n)
    });
    let found = exact
        .or_else(|| states.iter().find(|s| s.app == target && s.alive()))
        .cloned();
    found.filter(|s| s.alive()).ok_or_else(|| {
        let running: Vec<String> = states
            .iter()
            .filter(|s| s.alive())
            .map(|s| format!("{}.{}", s.app, s.n))
            .collect();
        Error::Runtime(format!(
            "no running instance matches `{target}` — running: [{}]",
            running.join(", ")
        ))
    })
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
            crate::runtime::backend::scrub_instance_dir(&instance_dir);
            let _ = crate::paths::force_remove_dir_all(&instance_dir);
        }
        crate::runtime::hosts::remove_entry(&state.app, state.n)?;
        InstanceState::remove(&state.app, state.n);
        reaped.push(state);
    }
    Ok(reaped)
}
