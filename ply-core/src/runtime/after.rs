//! `ply run --after COND`: block until another app on this host is ready.
//!
//! Three forms, and nothing else:
//!  - `APP` — an instance of APP is alive and, when its manifest declares
//!    `[health] port`, that port accepts a TCP connection (the same bar the
//!    deploy health gate uses).
//!  - `APP.PARAM` — APP has published `PARAM` under its live params tree
//!    (`/run/ply/APP/PARAM`, see `runtime::params_tree`).
//!  - `APP.PARAM == 'value'` (or `"value"`) — APP has published `PARAM` and
//!    its current value is exactly `value`.
//!
//! The file is the truth for the last two forms: unlike the bare form, they
//! never require APP to be alive or port-healthy, since an app may publish a
//! fact and then legitimately restart.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::runtime::{params_tree, state};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    NotRunning,
    Unhealthy(String),
}

/// A parsed `--after` condition: `app`, `app.param`, or
/// `app.param == 'literal'`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    pub app: String,
    pub param: Option<String>,
    pub equals: Option<String>,
}

impl Wait {
    /// Canonical rendering: `app`, `app.param`, or `app.param == 'literal'`.
    fn label(&self) -> String {
        match (&self.param, &self.equals) {
            (Some(p), Some(e)) => format!("{}.{p} == '{e}'", self.app),
            (Some(p), None) => format!("{}.{p}", self.app),
            (None, _) => self.app.clone(),
        }
    }
}

/// Same identifier shape as the template engine's `{app.param}` refs
/// (`params::parse_ref`'s `ident` closure — not exposed as a helper there,
/// so mirrored here): non-empty, ASCII alphanumeric, `_`, or `-`.
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn wait_grammar_error(s: &str) -> Error {
    Error::Runtime(format!(
        "--after `{s}`: expected APP, APP.PARAM, or APP.PARAM == 'value'"
    ))
}

/// Parse a `--after` condition. Exactly three forms are accepted — anything
/// else (`!=`, `>`, `&&`, an unquoted or unterminated literal, whitespace in
/// an identifier…) is the same grammar error, since the grammar is closed.
pub fn parse_wait(s: &str) -> Result<Wait> {
    let (head, equals) = match s.split_once("==") {
        Some((head, rest)) => {
            let rest = rest.trim();
            let quote = rest.chars().next().ok_or_else(|| wait_grammar_error(s))?;
            if quote != '\'' && quote != '"' {
                return Err(wait_grammar_error(s));
            }
            let body = &rest[quote.len_utf8()..];
            let end = body.find(quote).ok_or_else(|| wait_grammar_error(s))?;
            if !body[end + quote.len_utf8()..].is_empty() {
                return Err(wait_grammar_error(s));
            }
            (head.trim(), Some(body[..end].to_string()))
        }
        None => (s, None),
    };

    let (app, param) = match head.split_once('.') {
        Some((a, p)) if is_ident(a) && is_ident(p) => (a.to_string(), Some(p.to_string())),
        None if is_ident(head) => (head.to_string(), None),
        _ => return Err(wait_grammar_error(s)),
    };

    // `app == 'x'` (no param) isn't one of the three forms.
    if equals.is_some() && param.is_none() {
        return Err(wait_grammar_error(s));
    }

    Ok(Wait { app, param, equals })
}

/// Readiness of one `--after` condition. `param: None` is the bare form,
/// unchanged from before conditions existed (delegates to `check`). A
/// param condition never requires the app to be alive or port-healthy —
/// the published file is the truth.
pub fn check_wait(w: &Wait) -> Readiness {
    let Some(p) = w.param.as_deref() else {
        return check(&w.app);
    };
    match params_tree::read(&w.app, p) {
        None => Readiness::Unhealthy(format!("{}.{p} (currently unset)", w.app)),
        Some(v) => match &w.equals {
            Some(e) if &v != e => {
                Readiness::Unhealthy(format!("{}.{p} == '{e}' (currently '{v}')", w.app))
            }
            _ => Readiness::Ready,
        },
    }
}

/// Pull the `(currently ...)` tail off an `Unhealthy` reason built by
/// `check_wait`, to reuse in the terser timeout message (the rightmost
/// match, in case a published value itself happens to contain the marker).
fn currently_tail(why: &str) -> &str {
    why.rsplit_once("(currently ")
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .unwrap_or(why)
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

/// Poll `poll(wait)` for every condition until all are `Ready`. `report`
/// gets one human line per state change. Errors after `timeout` naming the
/// laggards.
///
/// A bare wait (`param: None`) reports and errors byte-for-byte as before
/// conditions existed — `check_wait`'s `Unhealthy` reason for a condition
/// already names the app/param/expected value, so it is reported as-is
/// rather than wrapped a second time.
pub fn wait_until(
    waits: &[Wait],
    timeout: Duration,
    interval: Duration,
    mut poll: impl FnMut(&Wait) -> Readiness,
    mut report: impl FnMut(String),
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last: Vec<Option<Readiness>> = vec![None; waits.len()];
    let mut done = vec![false; waits.len()];
    loop {
        for (i, w) in waits.iter().enumerate() {
            if done[i] {
                continue;
            }
            let now = poll(w);
            if last[i].as_ref() != Some(&now) {
                report(match &now {
                    Readiness::Ready => format!("{} is healthy", w.label()),
                    Readiness::NotRunning => {
                        format!("waiting for {} (not running yet)", w.label())
                    }
                    Readiness::Unhealthy(why) => {
                        if w.param.is_some() {
                            format!("waiting for {why}")
                        } else {
                            format!("waiting for {} ({why})", w.app)
                        }
                    }
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
            let laggards: Vec<usize> = (0..waits.len()).filter(|&i| !done[i]).collect();
            // Bare laggards keep today's single comma-joined sentence,
            // byte-for-byte, whether there's one or several — the shape a
            // bare `--after app` has always produced. Conditional laggards
            // are new: each gets its own `waiting for … (currently …, Ns
            // elapsed)` clause. Both kinds can be timing out together (a
            // bare `a` and a conditional `a.x == 'y'` are two gates), so
            // the bare sentence — if any — comes first, then one clause per
            // condition, joined with "; ".
            let mut parts: Vec<String> = Vec::new();
            let bare_names: Vec<&str> = laggards
                .iter()
                .filter(|&&i| waits[i].param.is_none())
                .map(|&i| waits[i].app.as_str())
                .collect();
            if !bare_names.is_empty() {
                parts.push(format!(
                    "{} is not healthy after {}s — is it running? (ply ps)",
                    bare_names.join(", "),
                    timeout.as_secs()
                ));
            }
            for &i in &laggards {
                let w = &waits[i];
                if w.param.is_none() {
                    continue;
                }
                let currently = match &last[i] {
                    Some(Readiness::Unhealthy(why)) => currently_tail(why),
                    // check_wait's param path only ever reports Unhealthy
                    // or Ready, and a laggard here is by definition never
                    // Ready — unreachable in practice, kept as a safe
                    // fallback rather than a panic.
                    _ => "unset",
                };
                parts.push(format!(
                    "waiting for {} (currently {currently}, {}s elapsed)",
                    w.label(),
                    timeout.as_secs()
                ));
            }
            return Err(Error::Runtime(parts.join("; ")));
        }
        std::thread::sleep(interval);
    }
}

/// `--after` as the run parent uses it: real state, 500 ms polls, stderr.
pub fn wait_for(waits: &[Wait], timeout: Duration) -> Result<()> {
    wait_until(
        waits,
        timeout,
        Duration::from_millis(500),
        check_wait,
        |line| eprintln!("ply: {line}"),
    )
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

    /// A bare `--after APP` wait, for tests that don't care about params.
    fn bare(app: &str) -> Wait {
        Wait {
            app: app.into(),
            param: None,
            equals: None,
        }
    }

    #[test]
    fn probe_distinguishes_open_and_closed_ports() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        assert!(probe(Ipv4Addr::LOCALHOST, open).is_ok());
        assert!(probe(Ipv4Addr::LOCALHOST, CLOSED_PORT).is_err());
    }

    #[test]
    fn wait_grammar_three_forms_only() {
        assert_eq!(parse_wait("db").unwrap().app, "db");
        let w = parse_wait("server.finish_boot == 'ok'").unwrap();
        assert_eq!(
            (w.param.as_deref(), w.equals.as_deref()),
            (Some("finish_boot"), Some("ok"))
        );
        assert!(parse_wait("server.finish_boot != 'ok'").is_err());
        assert!(parse_wait("a.b == 'x' && c").is_err());
    }

    #[test]
    fn wait_grammar_rejects_unquoted_and_unterminated_literals() {
        assert!(parse_wait("server.finish_boot == ok").is_err());
        assert!(parse_wait("server.finish_boot == 'ok").is_err());
        assert!(parse_wait("server == 'ok'").is_err(), "no param to compare");
        let w = parse_wait("server.finish_boot == \"ok\"").unwrap();
        assert_eq!(w.equals.as_deref(), Some("ok"));
    }

    #[test]
    fn condition_waits_until_value_matches() {
        // None → unset, then a value that doesn't match, then the match.
        let seq = std::cell::RefCell::new(vec![
            None,
            Some("booting".to_string()),
            Some("ok".to_string()),
        ]);
        let wait = parse_wait("server.finish_boot == 'ok'").unwrap();
        let mut lines = Vec::new();
        let r = wait_until(
            &[wait],
            Duration::from_secs(5),
            Duration::from_millis(1),
            |w| {
                // Mirrors check_wait's contract (params_tree::read → Unhealthy/Ready)
                // without touching the filesystem.
                let p = w.param.as_deref().unwrap();
                let e = w.equals.as_deref().unwrap();
                match seq.borrow_mut().remove(0) {
                    None => Readiness::Unhealthy(format!("{}.{p} (currently unset)", w.app)),
                    Some(v) if v != e => {
                        Readiness::Unhealthy(format!("{}.{p} == '{e}' (currently '{v}')", w.app))
                    }
                    Some(_) => Readiness::Ready,
                }
            },
            |l| lines.push(l),
        );
        assert!(r.is_ok());
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[0].contains("currently unset"),
            "first poll finds nothing published: {lines:?}"
        );
        assert!(
            lines[1].contains("currently 'booting'"),
            "second poll finds the wrong value: {lines:?}"
        );
        assert_eq!(lines[2], "server.finish_boot == 'ok' is healthy");
    }

    #[test]
    fn wait_until_returns_when_everything_is_ready() {
        let mut lines = Vec::new();
        let r = wait_until(
            &[bare("pgdb"), bare("redis")],
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
            &[bare("pgdb")],
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
            &[bare("pgdb"), bare("redis")],
            Duration::from_millis(20),
            Duration::from_millis(1),
            |w| {
                if w.app == "redis" {
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
    fn wait_until_times_out_joining_multiple_bare_laggards_in_one_sentence() {
        // Two bare laggards, neither ever ready: today's message is one
        // comma-joined sentence, not one clause per app.
        let err = wait_until(
            &[bare("pgdb"), bare("redis")],
            Duration::from_millis(20),
            Duration::from_millis(1),
            |_| Readiness::NotRunning,
            |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("pgdb, redis is not healthy after 0s — is it running? (ply ps)"),
            "{err}"
        );
        assert_eq!(
            err.matches("is not healthy after").count(),
            1,
            "one sentence for the whole bare set, not one per app: {err}"
        );
    }

    #[test]
    fn wait_until_times_out_with_mixed_bare_and_conditional_laggards() {
        let cond = parse_wait("a.finish_boot == 'ok'").unwrap();
        let err = wait_until(
            &[bare("pgdb"), cond],
            Duration::from_millis(20),
            Duration::from_millis(1),
            |w| {
                if let Some(p) = w.param.as_deref() {
                    Readiness::Unhealthy(format!("{}.{p} (currently unset)", w.app))
                } else {
                    Readiness::NotRunning
                }
            },
            |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("pgdb is not healthy after 0s — is it running? (ply ps)"),
            "bare clause kept its own shape: {err}"
        );
        assert!(
            err.contains("waiting for a.finish_boot == 'ok' (currently unset, 0s elapsed)"),
            "conditional clause kept its own shape: {err}"
        );
        assert!(
            err.contains("; "),
            "the two clauses are joined, not merged into one sentence: {err}"
        );
    }

    #[test]
    fn condition_wait_times_out_with_the_currently_and_elapsed_shape() {
        let wait = parse_wait("server.finish_boot == 'ok'").unwrap();
        let err = wait_until(
            &[wait],
            Duration::from_millis(20),
            Duration::from_millis(1),
            |w| {
                Readiness::Unhealthy(format!(
                    "{}.{} (currently unset)",
                    w.app,
                    w.param.as_deref().unwrap()
                ))
            },
            |_| {},
        )
        .unwrap_err()
        .to_string();
        // `Error::Runtime`'s Display adds a "runtime error: " wrapper (see
        // error.rs) — the message itself is the exact shape.
        assert!(
            err.contains("waiting for server.finish_boot == 'ok' (currently unset, 0s elapsed)"),
            "{err}"
        );
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
