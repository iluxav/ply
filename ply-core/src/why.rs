//! `ply why APP`: the evidence ply already owns, joined into one report.
//! Pure — the CLI collects the files, this decides what they say.

use serde::Serialize;

use crate::egress::log::{rfc3339, Record};
use crate::manifest::Manifest;
use crate::runtime::events::Event;
use crate::runtime::state::InstanceState;

const MAX_RESTARTS: usize = 10;
const MAX_BLOCKED: usize = 10;
const MAX_CHANGES: usize = 20;
pub const LOG_TAIL_LINES: usize = 8;

/// What an `instance-exit` event says. Detail grammar:
/// `<app>.<slot> exited <code> after <secs>s; oom_kill=<n>; <outcome>`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExitInfo {
    pub slot: u32,
    pub code: i32,
    pub uptime_secs: u64,
    pub oom_kill: u64,
    pub outcome: String,
}

impl ExitInfo {
    /// The event detail the run parent writes; `parse` is its inverse.
    pub fn detail(
        app: &str,
        slot: u32,
        code: i32,
        uptime_secs: u64,
        oom_kill: u64,
        outcome: &str,
    ) -> String {
        format!("{app}.{slot} exited {code} after {uptime_secs}s; oom_kill={oom_kill}; {outcome}")
    }

    pub fn parse(detail: &str) -> Option<ExitInfo> {
        let mut parts = detail.splitn(3, "; ");
        let head = parts.next()?;
        let oom = parts.next()?.strip_prefix("oom_kill=")?.parse().ok()?;
        let outcome = parts.next()?.to_string();
        // "<app>.<slot> exited <code> after <secs>s"
        let mut w = head.split_whitespace();
        let who = w.next()?;
        let slot = who.rsplit('.').next()?.parse().ok()?;
        if w.next()? != "exited" {
            return None;
        }
        let code = w.next()?.parse().ok()?;
        if w.next()? != "after" {
            return None;
        }
        let uptime_secs = w.next()?.strip_suffix('s')?.parse().ok()?;
        Some(ExitInfo {
            slot,
            code,
            uptime_secs,
            oom_kill: oom,
            outcome,
        })
    }

    /// `exit code N`, or the signal a code ≥ 128 stands for.
    pub fn describe_code(&self) -> String {
        if self.code >= 128 {
            let sig = self.code - 128;
            let name = match sig {
                1 => "SIGHUP",
                2 => "SIGINT",
                6 => "SIGABRT",
                9 => "SIGKILL",
                11 => "SIGSEGV",
                15 => "SIGTERM",
                _ => "",
            };
            if name.is_empty() {
                format!("killed by signal {sig}")
            } else {
                format!("killed by signal {sig} ({name})")
            }
        } else {
            format!("exit code {}", self.code)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Instance {
    pub slot: u32,
    pub pid: i32,
    pub ip: String,
    pub uptime_secs: u64,
    pub restarts: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub instances: Vec<Instance>,
    pub image: Option<String>,
    pub published: Option<String>,
    pub scale: Option<String>,
    pub last_deploy: Option<String>,
    /// Asleep: no instances on purpose, the port held. Since when, and where
    /// the next connection wakes it.
    pub asleep: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Restart {
    pub at: u64,
    pub exit: ExitInfo,
    pub log_tail: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Block {
    pub at: String,
    pub what: String,
    pub kind: String,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub at: u64,
    pub event: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub app: String,
    pub generated_at: u64,
    pub status: Status,
    pub restarts: Vec<Restart>,
    pub blocked: Vec<Block>,
    pub changes: Vec<Change>,
    /// The oldest journal entry seen — the window the sections cover.
    pub journal_since: Option<u64>,
}

/// Join the evidence. `states`: this app's live instances; `events`: the
/// whole journal (filtered here); `egress`: this app's egress records;
/// `log_tail(slot)`: the last lines of a slot's log ring.
#[allow(clippy::too_many_arguments)]
pub fn build(
    app: &str,
    now: u64,
    states: &[InstanceState],
    events: &[Event],
    egress: &[Record],
    manifest: Option<&Manifest>,
    asleep: Option<&crate::runtime::after::AsleepMarker>,
    log_tail: impl Fn(u32) -> Vec<String>,
) -> Report {
    let mut status = Status::default();
    if let Some(m) = asleep {
        status.asleep = Some(format!(
            "since {} ({} ago), wakes on {}:{}",
            rfc3339(m.since),
            humanize(now.saturating_sub(m.since)),
            m.addr,
            m.port
        ));
        status.image.get_or_insert_with(|| m.image.clone());
        status.published.get_or_insert_with(|| format!("{}:{}", m.addr, m.port));
    }
    for s in states.iter().filter(|s| s.app == app) {
        status.instances.push(Instance {
            slot: s.n,
            pid: s.pid,
            ip: s.ip.to_string(),
            uptime_secs: now.saturating_sub(s.started),
            restarts: s.restarts,
        });
        status.image.get_or_insert_with(|| s.image.clone());
        if status.published.is_none() {
            status.published = s.published_addr.clone();
        }
    }
    status.instances.sort_by_key(|i| i.slot);
    if let Some(scale) = manifest.and_then(|m| m.scale.as_ref()) {
        let mut line = match (&scale.signal, &scale.target) {
            (Some(signal), Some(target)) => format!(
                "{}..{} on {signal} (target {target})",
                scale.min, scale.max
            ),
            _ => format!("{}..{}", scale.min, scale.max),
        };
        if let Some(idle) = &scale.idle {
            line.push_str(&format!(", sleeps after {idle} idle"));
        }
        status.scale = Some(line);
    }

    let mine: Vec<&Event> = events.iter().filter(|e| e.app == app).collect();
    status.last_deploy = mine
        .iter()
        .rev()
        .find(|e| e.event == "deploy")
        .map(|e| e.detail.clone());

    let mut restarts: Vec<Restart> = mine
        .iter()
        .filter(|e| e.event == "instance-exit")
        .filter_map(|e| {
            ExitInfo::parse(&e.detail).map(|exit| Restart {
                at: e.ts,
                log_tail: log_tail(exit.slot),
                exit,
            })
        })
        .collect();
    restarts.reverse();
    restarts.truncate(MAX_RESTARTS);

    let mut changes: Vec<Change> = mine
        .iter()
        .filter(|e| e.event != "instance-exit")
        .map(|e| Change {
            at: e.ts,
            event: e.event.clone(),
            detail: e.detail.clone(),
        })
        .collect();
    changes.reverse();
    changes.truncate(MAX_CHANGES);

    let mut blocked: Vec<Block> = egress
        .iter()
        .filter_map(|r| match r {
            Record::Refused { t, name, count, .. } => Some(Block {
                at: t.clone(),
                what: name.clone(),
                kind: "refused".into(),
                count: (*count).max(1),
            }),
            Record::Blocked {
                t,
                dst,
                port,
                count,
                ..
            } => Some(Block {
                at: t.clone(),
                what: format!("{dst}:{port}"),
                kind: "blocked".into(),
                count: *count,
            }),
            _ => None,
        })
        .collect();
    blocked.reverse();
    blocked.truncate(MAX_BLOCKED);

    Report {
        app: app.to_string(),
        generated_at: now,
        status,
        restarts,
        blocked,
        changes,
        journal_since: events.iter().map(|e| e.ts).min(),
    }
}

/// `42s`, `10m`, `1h 30m`, `2d 3h`.
pub fn humanize(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => {
            let m = (s % 3600) / 60;
            if m == 0 {
                format!("{}h", s / 3600)
            } else {
                format!("{}h {m}m", s / 3600)
            }
        }
        s => {
            let h = (s % 86_400) / 3600;
            if h == 0 {
                format!("{}d", s / 86_400)
            } else {
                format!("{}d {h}h", s / 86_400)
            }
        }
    }
}

impl Report {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let st = &self.status;
        let n = st.instances.len();
        out.push_str(&format!(
            "{} — {} instance{}",
            self.app,
            n,
            if n == 1 { "" } else { "s" }
        ));
        if let Some(img) = &st.image {
            out.push_str(&format!(", image {img}"));
        }
        if let Some(p) = &st.published {
            out.push_str(&format!(", published {p}"));
        }
        out.push('\n');
        for i in &st.instances {
            out.push_str(&format!(
                "  {}.{}  {}  pid {}  up {}  restarts {}\n",
                self.app,
                i.slot,
                i.ip,
                i.pid,
                humanize(i.uptime_secs),
                i.restarts
            ));
        }
        if let Some(a) = &st.asleep {
            out.push_str(&format!("  asleep {a}\n"));
        }
        if let Some(d) = &st.last_deploy {
            out.push_str(&format!("  last deploy: {d}\n"));
        }
        if let Some(s) = &st.scale {
            out.push_str(&format!("  scale: {s}\n"));
        }

        if !self.restarts.is_empty() {
            out.push_str(&format!(
                "\nrestarts: {} (newest first)\n",
                self.restarts.len()
            ));
            for r in &self.restarts {
                let oom = if r.exit.oom_kill > 0 {
                    format!(", OOM-killed (oom_kill={})", r.exit.oom_kill)
                } else {
                    String::new()
                };
                out.push_str(&format!(
                    "  {}  {}.{}  {}{}, up {}; {}\n",
                    rfc3339(r.at),
                    self.app,
                    r.exit.slot,
                    r.exit.describe_code(),
                    oom,
                    humanize(r.exit.uptime_secs),
                    r.exit.outcome
                ));
                for line in &r.log_tail {
                    out.push_str(&format!("    log: {line}\n"));
                }
            }
        }

        if !self.blocked.is_empty() {
            out.push_str(&format!(
                "\nblocked: {} (newest first)\n",
                self.blocked.len()
            ));
            for b in &self.blocked {
                let unit = if b.kind == "refused" {
                    "queries"
                } else {
                    "packets"
                };
                out.push_str(&format!(
                    "  {}  {:<24}  {:<8} {} {unit}\n",
                    b.at, b.what, b.kind, b.count
                ));
            }
            out.push_str(&format!(
                "  fix: declare it in [network] egress, or run with --egress-allow <entry>; `ply egress {}` has every destination\n",
                self.app
            ));
        }

        if !self.changes.is_empty() {
            out.push_str(&format!(
                "\nchanges: {} (newest first)\n",
                self.changes.len()
            ));
            for c in &self.changes {
                out.push_str(&format!(
                    "  {}  {:<16} {}\n",
                    rfc3339(c.at),
                    c.event,
                    c.detail
                ));
            }
        }

        out.push('\n');
        match self.journal_since {
            Some(ts) => out.push_str(&format!(
                "journal since {} (the journal is a 512 KiB ring: earlier history is gone)\n",
                rfc3339(ts)
            )),
            None => out.push_str("no events in the journal\n"),
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: u64, event: &str, detail: &str) -> crate::runtime::events::Event {
        crate::runtime::events::Event {
            ts,
            app: "web".into(),
            event: event.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn an_exit_detail_parses_and_a_high_code_is_a_signal() {
        let e = ExitInfo::parse(
            "web.1 exited 137 after 42s; oom_kill=1; policy on-failure -> restart in 2s",
        )
        .unwrap();
        assert_eq!((e.slot, e.code, e.uptime_secs, e.oom_kill), (1, 137, 42, 1));
        assert_eq!(e.outcome, "policy on-failure -> restart in 2s");
        assert_eq!(e.describe_code(), "killed by signal 9 (SIGKILL)");
        let e =
            ExitInfo::parse("web.2 exited 1 after 3s; oom_kill=0; not restarted (policy never)")
                .unwrap();
        assert_eq!(e.describe_code(), "exit code 1");
        assert_eq!(e.slot, 2);
        assert!(ExitInfo::parse("web.1 respawned (restart #3)").is_none());
    }

    #[test]
    fn a_quiet_healthy_app_reports_status_and_nothing_else() {
        let states = vec![crate::runtime::state::InstanceState {
            app: "web".into(),
            n: 1,
            pid: 4242,
            ip: "10.77.0.3".parse().unwrap(),
            ports: Default::default(),
            image: "/srv/web-1.2.0-linux-x64.img".into(),
            started: 1_000_000,
            restarts: 0,
            health_port: Some(8080),
            published_port: Some(8080),
            published_addr: Some("10.77.0.1:8080".into()),
            instance_port: Some(8080),
            domains: vec![],
            network: None,
        }];
        let r = build("web", 1_000_600, &states, &[], &[], None, None, |_| vec![]);
        assert_eq!(r.status.instances.len(), 1);
        assert_eq!(r.status.instances[0].uptime_secs, 600);
        assert!(r.restarts.is_empty() && r.blocked.is_empty() && r.changes.is_empty());
        let text = r.render();
        assert!(
            text.contains("web.1") && text.contains("10.77.0.3") && text.contains("10m"),
            "{text}"
        );
        assert!(
            !text.contains("restarts:") && !text.contains("blocked:"),
            "{text}"
        );
        assert!(text.contains("no events in the journal"), "{text}");
    }

    #[test]
    fn restarts_carry_the_exit_evidence_and_the_log_tail_newest_first() {
        let events = vec![
            ev(
                1_000_100,
                "instance-exit",
                "web.1 exited 1 after 30s; oom_kill=0; policy on-failure -> restart in 1s",
            ),
            ev(
                1_000_101,
                "instance-restart",
                "web.1 respawned (restart #1)",
            ),
            ev(
                1_000_200,
                "instance-exit",
                "web.1 exited 137 after 90s; oom_kill=1; policy on-failure -> restart in 2s",
            ),
        ];
        let r = build("web", 1_000_300, &[], &events, &[], None, None, |n| {
            vec![format!("slot {n}: fatal: out of memory")]
        });
        assert_eq!(r.restarts.len(), 2);
        assert_eq!(r.restarts[0].exit.code, 137, "newest first");
        assert_eq!(r.restarts[0].log_tail, vec!["slot 1: fatal: out of memory"]);
        let text = r.render();
        assert!(text.contains("killed by signal 9 (SIGKILL)"), "{text}");
        assert!(text.contains("OOM-killed"), "{text}");
        assert!(text.contains("fatal: out of memory"), "{text}");
        // changes list the respawn, not the exits (those are the restarts section)
        assert_eq!(r.changes.len(), 1);
        assert_eq!(r.changes[0].event, "instance-restart");
    }

    #[test]
    fn blocked_names_the_destinations_and_the_fix() {
        use crate::egress::log::Record;
        let recs = vec![
            Record::Refused {
                t: "2026-09-05T17:15:56Z".into(),
                app: "web".into(),
                n: 1,
                name: "pypi.org".into(),
                declared: false,
                count: 3,
            },
            Record::Blocked {
                t: "2026-09-05T17:15:57Z".into(),
                app: "web".into(),
                n: 1,
                proto: "tcp".into(),
                dst: "8.8.8.8".parse().unwrap(),
                port: 443,
                name: None,
                count: 8,
            },
            Record::Allowed {
                t: "2026-09-05T17:15:58Z".into(),
                app: "web".into(),
                n: 1,
                proto: "tcp".into(),
                dst: "1.1.1.1".parse().unwrap(),
                port: 443,
                name: None,
                count: 2,
            },
        ];
        let r = build("web", 1_000_000, &[], &[], &recs, None, None, |_| vec![]);
        assert_eq!(r.blocked.len(), 2);
        let text = r.render();
        assert!(
            text.contains("pypi.org") && text.contains("refused"),
            "{text}"
        );
        assert!(
            text.contains("8.8.8.8:443") && text.contains("blocked"),
            "{text}"
        );
        assert!(
            text.contains("[network] egress") && text.contains("--egress-allow"),
            "{text}"
        );
        assert!(
            !text.contains("1.1.1.1"),
            "allowed traffic is not a why: {text}"
        );
    }

    #[test]
    fn changes_are_newest_first_and_the_window_is_stated() {
        let events = vec![
            ev(1_000_000, "deploy", "web 1.1.0 -> 1.2.0"),
            ev(1_000_050, "scale-up", "1 -> 3: cpu 90% > 70% over 30s"),
            ev(
                1_000_090,
                "deploy-failed",
                "health gate: no answer on 8080 within 30s",
            ),
        ];
        let r = build("web", 1_000_100, &[], &events, &[], None, None, |_| vec![]);
        assert_eq!(
            r.changes
                .iter()
                .map(|c| c.event.as_str())
                .collect::<Vec<_>>(),
            vec!["deploy-failed", "scale-up", "deploy"]
        );
        assert_eq!(r.status.last_deploy.as_deref(), Some("web 1.1.0 -> 1.2.0"));
        let text = r.render();
        assert!(text.contains("journal since"), "{text}");
        assert!(
            text.find("deploy-failed").unwrap() < text.find("scale-up").unwrap(),
            "{text}"
        );
    }
}
