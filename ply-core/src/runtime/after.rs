//! `ply run --after APP`: block until another app on this host is healthy.
//!
//! Readiness is the same bar the deploy health gate uses: at least one
//! instance alive and, when its manifest declares `[health] port`, that port
//! accepting a TCP connection. No `[health]` → alive is enough.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::runtime::state;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    NotRunning,
    Unhealthy(String),
}

/// TCP connect with the health gate's 300 ms budget.
pub fn probe(ip: Ipv4Addr, port: u16) -> std::result::Result<(), String> {
    let addr = std::net::SocketAddr::from((ip, port));
    crate::runtime::publish::connect_either_family(addr, Duration::from_millis(300))
        .map(drop)
        .map_err(|e| e.to_string())
}

fn alive(pid: i32) -> bool {
    unsafe { nix::libc::kill(pid, 0) == 0 }
}

/// Readiness from (pid, ip) pairs and the app's `[health] port`, if any.
pub fn readiness_of(instances: &[(i32, Ipv4Addr)], health_port: Option<u16>) -> Readiness {
    let live: Vec<&(i32, Ipv4Addr)> = instances.iter().filter(|(pid, _)| alive(*pid)).collect();
    if live.is_empty() {
        return Readiness::NotRunning;
    }
    let Some(port) = health_port else {
        return Readiness::Ready;
    };
    let mut last = String::new();
    for (_, ip) in live {
        match probe(*ip, port) {
            Ok(()) => return Readiness::Ready,
            Err(e) => last = e,
        }
    }
    Readiness::Unhealthy(format!("port {port} not answering: {last}"))
}

/// Readiness of a named app on this host, from its instance state files
/// (which record the `[health] port` of the image they run).
pub fn check(app: &str) -> Readiness {
    let states: Vec<state::InstanceState> = match state::list() {
        Ok(all) => all.into_iter().filter(|s| s.app == app).collect(),
        Err(_) => return Readiness::NotRunning,
    };
    let health_port = states.iter().find_map(|s| s.health_port);
    let pairs: Vec<(i32, Ipv4Addr)> = states.iter().map(|s| (s.pid, s.ip)).collect();
    readiness_of(&pairs, health_port)
}

/// Poll `poll(app)` for every app until all are `Ready`. `report` gets one
/// human line per state change. Errors after `timeout` naming the laggards.
pub fn wait_until(
    apps: &[String],
    timeout: Duration,
    interval: Duration,
    mut poll: impl FnMut(&str) -> Readiness,
    mut report: impl FnMut(String),
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last: Vec<Option<Readiness>> = vec![None; apps.len()];
    let mut done = vec![false; apps.len()];
    loop {
        for (i, app) in apps.iter().enumerate() {
            if done[i] {
                continue;
            }
            let now = poll(app);
            if last[i].as_ref() != Some(&now) {
                report(match &now {
                    Readiness::Ready => format!("{app} is healthy"),
                    Readiness::NotRunning => format!("waiting for {app} (not running yet)"),
                    Readiness::Unhealthy(why) => format!("waiting for {app} ({why})"),
                });
                last[i] = Some(now.clone());
            }
            if now == Readiness::Ready {
                done[i] = true;
            }
        }
        if done.iter().all(|d| *d) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let laggards: Vec<&str> = apps
                .iter()
                .enumerate()
                .filter(|(i, _)| !done[*i])
                .map(|(_, a)| a.as_str())
                .collect();
            return Err(Error::Runtime(format!(
                "{} is not healthy after {}s — is it running? (ply ps)",
                laggards.join(", "),
                timeout.as_secs()
            )));
        }
        std::thread::sleep(interval);
    }
}

/// `--after` as the run parent uses it: real state, 500 ms polls, stderr.
pub fn wait_for(apps: &[String], timeout: Duration) -> Result<()> {
    wait_until(apps, timeout, Duration::from_millis(500), check, |line| {
        eprintln!("ply: {line}")
    })
}

/// A parent that is blocked on `--after` leaves this so `ply ps` can show it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitingMarker {
    pub app: String,
    pub after: Vec<String>,
    pub pid: i32,
    /// Unix seconds.
    pub since: u64,
}

/// Removes the marker file when dropped (wait finished, failed, or panicked).
pub struct WaitingGuard(PathBuf);

impl Drop for WaitingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn waiting_dir() -> PathBuf {
    crate::paths::run_dir().join("waiting")
}

impl WaitingMarker {
    pub fn write(app: &str, after: &[String]) -> Result<WaitingGuard> {
        Self::write_in(&waiting_dir(), app, after)
    }

    pub fn write_in(dir: &Path, app: &str, after: &[String]) -> Result<WaitingGuard> {
        std::fs::create_dir_all(dir).map_err(|source| Error::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let marker = WaitingMarker {
            app: app.to_string(),
            after: after.to_vec(),
            pid: std::process::id() as i32,
            since: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        let path = dir.join(format!("{app}.json"));
        let text = serde_json::to_string(&marker).map_err(|e| Error::Runtime(e.to_string()))?;
        std::fs::write(&path, text).map_err(|source| Error::Io {
            path: path.clone(),
            source,
        })?;
        Ok(WaitingGuard(path))
    }

    /// Markers whose writer is still alive (a killed parent leaves a stale file).
    pub fn list() -> Vec<WaitingMarker> {
        Self::list_in(&waiting_dir())
    }

    pub fn list_in(dir: &Path) -> Vec<WaitingMarker> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out: Vec<WaitingMarker> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str::<WaitingMarker>(&t).ok())
            .filter(|m| alive(m.pid))
            .collect();
        out.sort_by(|a, b| a.app.cmp(&b.app));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    /// A port nothing can be listening on. Not a freed ephemeral one: the
    /// probe now also tries `::1` (apps bind `[::]`), and a sibling test
    /// binding across the ephemeral range can occupy a just-freed number
    /// between the drop and the probe.
    const CLOSED_PORT: u16 = 9; // discard — unused, and never auto-assigned

    #[test]
    fn probe_distinguishes_open_and_closed_ports() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        assert!(probe(Ipv4Addr::LOCALHOST, open).is_ok());
        assert!(probe(Ipv4Addr::LOCALHOST, CLOSED_PORT).is_err());
    }

    #[test]
    fn wait_until_returns_when_everything_is_ready() {
        let mut lines = Vec::new();
        let r = wait_until(
            &["pgdb".into(), "redis".into()],
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_| Readiness::Ready,
            |l| lines.push(l),
        );
        assert!(r.is_ok());
        assert_eq!(lines, vec!["pgdb is healthy", "redis is healthy"]);
    }

    #[test]
    fn wait_until_reports_each_reason_once_then_success() {
        let calls = Cell::new(0);
        let mut lines = Vec::new();
        let r = wait_until(
            &["pgdb".into()],
            Duration::from_secs(5),
            Duration::from_millis(1),
            |_| {
                calls.set(calls.get() + 1);
                match calls.get() {
                    1 | 2 => Readiness::NotRunning,
                    3 | 4 => Readiness::Unhealthy("port 5432 not answering".into()),
                    _ => Readiness::Ready,
                }
            },
            |l| lines.push(l),
        );
        assert!(r.is_ok());
        assert_eq!(
            lines,
            vec![
                "waiting for pgdb (not running yet)",
                "waiting for pgdb (port 5432 not answering)",
                "pgdb is healthy",
            ]
        );
    }

    #[test]
    fn wait_until_times_out_naming_the_laggards() {
        let mut lines = Vec::new();
        let err = wait_until(
            &["pgdb".into(), "redis".into()],
            Duration::from_millis(20),
            Duration::from_millis(1),
            |app| {
                if app == "redis" {
                    Readiness::Ready
                } else {
                    Readiness::NotRunning
                }
            },
            |l| lines.push(l),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("pgdb is not healthy after"), "{err}");
        assert!(err.contains("is it running? (ply ps)"), "{err}");
        assert!(!err.contains("redis"), "ready apps are not blamed: {err}");
    }

    #[test]
    fn marker_round_trips_and_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _guard = WaitingMarker::write_in(dir.path(), "pgapp", &["pgdb".into()]).unwrap();
            let found = WaitingMarker::list_in(dir.path());
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].app, "pgapp");
            assert_eq!(found[0].after, vec!["pgdb"]);
            assert_eq!(found[0].pid, std::process::id() as i32);
        }
        assert!(
            WaitingMarker::list_in(dir.path()).is_empty(),
            "guard drop removes the file"
        );
    }

    #[test]
    fn marker_list_skips_dead_writers() {
        let dir = tempfile::tempdir().unwrap();
        let stale = WaitingMarker {
            app: "x".into(),
            after: vec![],
            pid: 2_000_000_000,
            since: 0,
        };
        std::fs::write(
            dir.path().join("x.json"),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();
        assert!(WaitingMarker::list_in(dir.path()).is_empty());
    }

    #[test]
    fn readiness_from_instances() {
        // No instances → not running; alive + no health → ready.
        assert!(matches!(readiness_of(&[], None), Readiness::NotRunning));
        let me = std::process::id() as i32;
        assert!(matches!(
            readiness_of(&[(me, Ipv4Addr::LOCALHOST)], None),
            Readiness::Ready
        ));
        // dead pid only → not running
        assert!(matches!(
            readiness_of(&[(2_000_000_000, Ipv4Addr::LOCALHOST)], None),
            Readiness::NotRunning
        ));
        // alive + health port open → ready; closed → unhealthy
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = l.local_addr().unwrap().port();
        assert!(matches!(
            readiness_of(&[(me, Ipv4Addr::LOCALHOST)], Some(open)),
            Readiness::Ready
        ));
        let closed = CLOSED_PORT;
        match readiness_of(&[(me, Ipv4Addr::LOCALHOST)], Some(closed)) {
            Readiness::Unhealthy(why) => assert!(why.contains(&format!("port {closed}")), "{why}"),
            other => panic!("expected Unhealthy, got {other:?}"),
        }
    }
}
