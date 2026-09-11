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
    /// The names of this app's declared volumes — what `ply snapshot` backs
    /// up. Recorded so a reader (the dashboard) can tell a stateful app
    /// from a stateless one without reading the image, and offer snapshots
    /// only where there is data to snapshot. Empty for a volume-less app,
    /// and absent in state files written before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    /// The path the run parent was STARTED with, before symlinks were
    /// resolved — `image` above is what actually runs. They differ exactly
    /// when the app was started from a `current.img` link, and that is the
    /// case `ply deploy` needs to see: it re-points the link after a roll so
    /// a restart brings back the deployed version. Absent in state files
    /// from before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_path: Option<String>,
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
        // Best-effort: rootless cannot rewrite root-owned /etc/hosts (and has
        // no `.ply` entry there anyway), and that must not block reaping the
        // rest — the state file and dir removal below are what matter.
        let _ = crate::runtime::hosts::remove_entry(&state.app, state.n);
        InstanceState::remove(&state.app, state.n);
        reaped.push(state);
    }
    Ok(reaped)
}

/// What [`clean`] stops and reaps.
pub enum CleanTarget {
    /// Only orphans: an instance whose supervisor died is reparented away from
    /// its `ply run` (to init, or to a process that is not `ply`). Safe — it
    /// never touches an instance a live `ply run` is still supervising.
    Orphans,
    /// Every instance recorded on this host, supervised or not.
    All,
    /// Every instance of one app (`myapp`) or a single instance (`myapp.2`).
    App(String),
}

/// Does a pid exist? `kill(pid, 0)`: `0` = yes; `ESRCH` = no; `EPERM` = yes
/// (it exists, it is just not ours to signal). [`InstanceState::alive`] reads
/// only `== 0`, so it calls an `EPERM` process dead; this does not.
fn pid_exists(pid: i32) -> bool {
    if unsafe { nix::libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(nix::libc::ESRCH)
}

/// The parent pid from `/proc/<pid>/stat`. The 2nd field (`comm`) is wrapped
/// in parentheses and may itself contain spaces or `)`, so the numeric fields
/// begin after the LAST `)`: then `<state> <ppid> ...`.
fn ppid_of(pid: i32) -> Option<i32> {
    parse_ppid(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// The ppid is the 4th field of `/proc/<pid>/stat`: `pid (comm) state ppid …`.
fn parse_ppid(stat: &str) -> Option<i32> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

fn comm_of(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// A supervised instance's parent is its `ply run` (comm `ply`). Reparented to
/// init, or to anything that is not `ply`, means the supervisor is gone.
fn is_orphan(pid: i32) -> bool {
    match ppid_of(pid) {
        Some(1) => true,
        Some(ppid) => comm_of(ppid).as_deref() != Some("ply"),
        None => false,
    }
}

/// The process group (5th field of `/proc/<pid>/stat`) — every process of one
/// `ply run` shares it: the instance's pid-namespace init, its parked netns
/// holder, slirp, and the app. Reaping the whole group is the only way to take
/// the holder too; leaving it behind wedges the next run of the same app.
fn pgid_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_pgid(&stat)
}

/// pgid is the 5th field: `pid (comm) state ppid pgrp …`.
fn parse_pgid(stat: &str) -> Option<i32> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(2)?.parse().ok()
}

/// Signal a run's whole process group — its pid-namespace init, parked netns
/// holder, slirp and the app all share one pgid — plus the pid itself as a
/// fallback. Killing the pid-namespace init also tears down anything it left
/// under a subuid, which a bare `kill` by the launching user could not reach.
/// Hard-guarded so it never signals our OWN group.
fn signal_group(pid: i32, sig: i32) {
    let own = unsafe { nix::libc::getpgrp() };
    unsafe {
        if let Some(g) = pgid_of(pid).filter(|&g| g > 1 && g != own) {
            nix::libc::kill(-g, sig);
        }
        nix::libc::kill(pid, sig);
    }
}

/// SIGTERM the run's process group, wait up to 5s for the instance to exit,
/// then SIGKILL the group to sweep the parked netns holder and slirp (they do
/// not exit on their own).
fn stop_pid(pid: i32) {
    signal_group(pid, nix::libc::SIGTERM);
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !pid_exists(pid) {
            break;
        }
    }
    signal_group(pid, nix::libc::SIGKILL);
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if !pid_exists(pid) {
            return;
        }
    }
}

/// Reap abandoned instances of every app EXCEPT `except` — orphans whose
/// supervisor died, which nothing re-adopts (a fresh run ADDS instances). Run
/// at `ply run <except>` startup so a crashed run's leftovers do not pile up.
/// The app being started is skipped on purpose: a fresh run of it would race
/// its own orphan's netns/port teardown (that one is `ply clean`'s job).
///
/// Bounded regardless of how many orphans exist — one SIGTERM sweep, one 1s
/// grace, one SIGKILL — and free when there are none (no orphan, no wait), so
/// it adds nothing to a normal startup. Returns what it actually took down.
pub fn reap_other_orphans(except: &str) -> Vec<InstanceState> {
    let Ok(states) = list() else {
        return Vec::new();
    };
    let orphans: Vec<InstanceState> = states
        .into_iter()
        .filter(|st| st.app != except && pid_exists(st.pid) && is_orphan(st.pid))
        .collect();
    if orphans.is_empty() {
        return orphans;
    }
    for st in &orphans {
        signal_group(st.pid, nix::libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    for st in &orphans {
        if pid_exists(st.pid) {
            signal_group(st.pid, nix::libc::SIGKILL);
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let _ = reap_stale();
    orphans
        .into_iter()
        .filter(|st| !pid_exists(st.pid))
        .collect()
}

/// Stop and reap selected instances, then clear the state of everything now
/// dead. Returns the instances it stopped. A rootless `ply run` that dies
/// abnormally leaves its instance running (by design, so a replacement
/// supervisor can re-adopt it); when none ever does, this is what reaps it.
pub fn clean(target: &CleanTarget) -> Result<Vec<InstanceState>> {
    let mut stopped = Vec::new();
    for st in list()? {
        if !pid_exists(st.pid) {
            continue; // already dead — reap_stale below clears its state
        }
        let selected = match target {
            CleanTarget::All => true,
            CleanTarget::App(a) => st.app == *a || format!("{}.{}", st.app, st.n) == *a,
            CleanTarget::Orphans => is_orphan(st.pid),
        };
        if selected {
            stop_pid(st.pid);
            stopped.push(st);
        }
    }
    let _ = reap_stale();
    Ok(stopped)
}

#[cfg(test)]
mod tests {
    use super::parse_ppid;

    #[test]
    fn parse_ppid_reads_the_fourth_field() {
        assert_eq!(
            parse_ppid("568904 (ply) S 568884 568876 568876 0"),
            Some(568884)
        );
    }

    #[test]
    fn parse_ppid_survives_parens_and_spaces_in_comm() {
        // A comm may itself contain spaces and a `)`; fields start after the last `)`.
        assert_eq!(parse_ppid("42 (odd )name) R 7 1 1 0"), Some(7));
        assert_eq!(parse_ppid("9 (a) Z 1 9"), Some(1));
    }

    #[test]
    fn parse_ppid_rejects_garbage() {
        assert_eq!(parse_ppid("no parens here"), None);
        assert_eq!(parse_ppid("12 (x) S notanumber"), None);
    }

    #[test]
    fn parse_pgid_reads_the_fifth_field() {
        use super::parse_pgid;
        // pid (comm) state ppid pgrp ...
        assert_eq!(
            parse_pgid("570258 (ply) S 570238 570224 570224 0"),
            Some(570224)
        );
        assert_eq!(parse_pgid("42 (odd )name) R 7 99 99"), Some(99));
    }
}
