//! Notifications: ply telling you when something happened, so you do not
//! have to be watching.
//!
//! Everything ply does is already a line in the events journal
//! (`runtime::events`), and `ply reconcile` already runs every minute under
//! a systemd timer. So this is a reader over data that already exists, on a
//! loop that already turns — no resident process. Each beat, [`run`] reads
//! the events since a stored cursor, keeps the ones the operator subscribed
//! to, and delivers a short line to each destination.
//!
//! # Config
//!
//! `<data>/notify.toml`:
//!
//! ```toml
//! on = ["deploy-failed", "restart-loop", "snapshot-failed", "disk-high"]
//! to = ["telegram:<token>:<chat>", "discord:<webhook>", "https://…", "enc:v1:…"]
//! ```
//!
//! A `to` entry may be a sealed value (`enc:v1:…`, sealed under the name
//! `notify`), so a fleet repo carrying the file stays publishable — a bot
//! token is a secret. It is opened with the host key at delivery time and
//! never logged.
//!
//! # What is NOT here
//!
//! Severities, silences, escalation, on-call rotations. That is what
//! Prometheus + Alertmanager are for, and ply hands off to them rather than
//! reimplementing them. This is one honest message per event you asked
//! about, to a place you already read.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::runtime::events::{self, Event};

/// How far back `restart-loop` looks, and how many crashes in that window
/// make a loop. Chosen so a single crash-and-recover is not a loop but a
/// genuinely wedged app is caught within a couple of minutes.
const RESTART_WINDOW_SECS: u64 = 300;
const RESTART_THRESHOLD: usize = 3;
/// Disk is "high" at this fraction full, and while it stays high the warning
/// repeats no more than this often — enough to keep nagging, not enough to
/// be the thing that fills the inbox.
const DISK_HIGH_FRAC: f64 = 0.90;
const DISK_RENAG_SECS: u64 = 6 * 3600;
/// A synthetic event name the journal never carries — computed here.
const DERIVED: &[&str] = &["restart-loop", "disk-high"];

/// Where the notify config lives. Canonically `<data>/config/notify.toml`
/// — a directory that holds only notify config, so the dashboard can be
/// granted it read-write without exposing `host.key` and the rest of the
/// data dir. The old `<data>/notify.toml` is still read if present, so a
/// host configured before the move keeps working.
pub fn config_path() -> PathBuf {
    let dir = crate::paths::data_dir().join("config");
    let current = dir.join("notify.toml");
    if current.exists() {
        return current;
    }
    let legacy = crate::paths::data_dir().join("notify.toml");
    if legacy.exists() {
        return legacy;
    }
    current
}

/// The directory `ply setup` creates so the dashboard's grant has a source
/// that exists.
pub fn config_dir() -> PathBuf {
    crate::paths::data_dir().join("config")
}
fn state_path() -> PathBuf {
    crate::paths::data_dir().join("notify.state")
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    /// Event names to notify on: journal events (`deploy-failed`,
    /// `snapshot-failed`, `instance-restart`, `egress-blocked`, …) and the
    /// derived `restart-loop` and `disk-high`.
    #[serde(default)]
    pub on: Vec<String>,
    /// Where to send them.
    #[serde(default)]
    pub to: Vec<String>,
}

impl Config {
    /// `None` when there is no config file — notifications are simply off.
    pub fn load() -> Result<Option<Config>> {
        let path = config_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        };
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| Error::Runtime(format!("{}: {e}", config_path().display())))?;
        Ok(Some(cfg))
    }

    fn wants(&self, event: &str) -> bool {
        self.on.iter().any(|e| e == event)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct State {
    /// The newest event ts already delivered. Events with `ts > cursor`
    /// are new. On first run it is set to "now", so history is not replayed.
    cursor: u64,
    /// When `disk-high` last fired, to rate-limit re-warning; 0 = not high.
    #[serde(default)]
    disk_high_last: u64,
}

impl State {
    fn load() -> State {
        std::fs::read_to_string(state_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    fn save(&self) {
        if let Ok(text) = serde_json::to_string(self) {
            let tmp = state_path().with_extension("state.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, state_path());
            }
        }
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One message to send: the app it concerns and the line to deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub app: String,
    pub event: String,
    pub text: String,
}

fn line(app: &str, event: &str, detail: &str) -> Message {
    let text = if detail.is_empty() {
        format!("[ply] {app} {event}")
    } else {
        format!("[ply] {app} {event}: {detail}")
    };
    Message {
        app: app.to_string(),
        event: event.to_string(),
        text,
    }
}

/// The messages a batch of events produces for one config — pure, so the
/// selection and the derived-event logic are testable without a journal or
/// a network. `events` is the whole journal (oldest first); only those with
/// `ts > cursor` are considered.
pub fn messages_for(cfg: &Config, events: &[Event], cursor: u64) -> Vec<Message> {
    let mut out = Vec::new();
    for (i, e) in events.iter().enumerate() {
        if e.ts <= cursor {
            continue;
        }
        // A directly subscribed journal event.
        if cfg.wants(&e.event) && !DERIVED.contains(&e.event.as_str()) {
            out.push(line(&e.app, &e.event, &e.detail));
        }
        // restart-loop: fire once, on the crash that crosses the threshold,
        // so a wedged app sends one message, not one per respawn. "Crosses"
        // means exactly THRESHOLD crashes for this app in the window ending
        // at this event — the 4th, 5th … do not re-fire.
        if cfg.wants("restart-loop") && e.event == "instance-restart" {
            let window_start = e.ts.saturating_sub(RESTART_WINDOW_SECS);
            let count = events[..=i]
                .iter()
                .filter(|p| p.event == "instance-restart" && p.app == e.app && p.ts >= window_start)
                .count();
            if count == RESTART_THRESHOLD {
                out.push(line(
                    &e.app,
                    "restart-loop",
                    &format!(
                        "{RESTART_THRESHOLD} restarts in {}s — {}",
                        RESTART_WINDOW_SECS, e.detail
                    ),
                ));
            }
        }
    }
    out
}

/// The fraction of the data dir's filesystem that is in use, 0.0–1.0, or
/// `None` if it cannot be read. Availability, not free: `f_bavail` excludes
/// the root-reserved blocks, which is the space an app actually gets.
fn disk_used_frac() -> Option<f64> {
    let v = nix::sys::statvfs::statvfs(&crate::paths::data_dir()).ok()?;
    let total = v.blocks() as f64;
    if total <= 0.0 {
        return None;
    }
    Some((total - v.blocks_available() as f64) / total)
}

/// Read the journal since the cursor, deliver, advance the cursor. Called
/// each reconcile beat. Best-effort throughout: a config error is reported
/// and skipped, a delivery failure is logged, neither fails the caller.
pub fn run() {
    let Ok(Some(cfg)) = Config::load() else {
        return;
    };
    if cfg.to.is_empty() || cfg.on.is_empty() {
        return;
    }
    let mut state = State::load();
    let events = events::read();

    // First ever run: start from now, do not replay the whole journal into
    // someone's phone. A cursor of 0 (fresh state file) is that case.
    if state.cursor == 0 {
        state.cursor = events.iter().map(|e| e.ts).max().unwrap_or_else(now);
    }

    let mut messages = messages_for(&cfg, &events, state.cursor);

    // disk-high: not a journal event — checked here, rate-limited while high.
    if cfg.wants("disk-high") {
        match disk_used_frac() {
            Some(frac) if frac >= DISK_HIGH_FRAC => {
                if now().saturating_sub(state.disk_high_last) >= DISK_RENAG_SECS {
                    messages.push(line(
                        "host",
                        "disk-high",
                        &format!(
                            "{}% of the ply data filesystem is used",
                            (frac * 100.0) as u32
                        ),
                    ));
                    state.disk_high_last = now();
                }
            }
            // Back below the line: re-arm, so a recurrence warns promptly.
            _ => state.disk_high_last = 0,
        }
    }

    if !messages.is_empty() {
        let dests = resolve_destinations(&cfg.to);
        for m in &messages {
            for d in &dests {
                if let Err(e) = deliver(d, &m.text) {
                    eprintln!("ply: notify: delivery to {} failed: {e}", redacted(d));
                }
            }
        }
    }

    state.cursor = events.iter().map(|e| e.ts).max().unwrap_or(state.cursor);
    state.save();
}

/// Open any sealed destinations with the host key; a sealed one that will
/// not open is dropped with a warning rather than sent as ciphertext.
fn resolve_destinations(to: &[String]) -> Vec<String> {
    let key = crate::sealed::HostKey::load(&crate::sealed::key_path())
        .ok()
        .flatten();
    let mut out = Vec::new();
    for entry in to {
        if crate::sealed::is_sealed(entry) {
            match key
                .as_ref()
                .map(|k| crate::sealed::unseal("notify", entry, k))
            {
                Some(Ok(v)) => out.push(v),
                _ => eprintln!(
                    "ply: notify: a sealed destination could not be opened on this host — skipped"
                ),
            }
        } else {
            out.push(entry.clone());
        }
    }
    out
}

/// A destination with any secret masked, for logs.
fn redacted(dest: &str) -> String {
    match dest.split_once(':') {
        Some((scheme, _)) => format!("{scheme}:…"),
        None => "…".into(),
    }
}

/// What a destination resolves to — separated from doing it, so the parsing
/// is testable without a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// POST `body` as application/json to `url`.
    Http { url: String, body: String },
    /// Run `program` with `args`, the message on its stdin (email via
    /// `mail`/`sendmail`, or anything else).
    Command { program: String, args: Vec<String> },
}

/// Parse a destination and build what would be sent. Pure.
pub fn plan(dest: &str, text: &str) -> std::result::Result<Delivery, String> {
    let json_text = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into());
    if let Some(rest) = dest.strip_prefix("telegram:") {
        // The token itself contains a colon (`<digits>:<rest>`), so the
        // chat id is what follows the LAST colon.
        let (token, chat) = rest
            .rsplit_once(':')
            .ok_or("telegram: expected telegram:<token>:<chat-id>")?;
        if token.is_empty() || chat.is_empty() {
            return Err("telegram: expected telegram:<token>:<chat-id>".into());
        }
        let chat_json = serde_json::to_string(chat).unwrap_or_default();
        return Ok(Delivery::Http {
            url: format!("https://api.telegram.org/bot{token}/sendMessage"),
            body: format!("{{\"chat_id\":{chat_json},\"text\":{json_text}}}"),
        });
    }
    if let Some(url) = dest.strip_prefix("discord:") {
        return Ok(Delivery::Http {
            url: url.to_string(),
            body: format!("{{\"content\":{json_text}}}"),
        });
    }
    if let Some(cmd) = dest.strip_prefix("command:") {
        let mut parts = cmd.split_whitespace().map(str::to_string);
        let program = parts
            .next()
            .ok_or("command: expected command:<program> [args]")?;
        return Ok(Delivery::Command {
            program,
            args: parts.collect(),
        });
    }
    let url = dest.strip_prefix("webhook:").unwrap_or(dest);
    if url.starts_with("https://") || url.starts_with("http://") {
        return Ok(Delivery::Http {
            url: url.to_string(),
            body: json_text, // a bare string body; the receiver decides
        });
    }
    Err(format!(
        "unknown destination `{}` — expected telegram:, discord:, command:, or an https URL",
        redacted(dest)
    ))
}

fn deliver(dest: &str, text: &str) -> std::result::Result<(), String> {
    match plan(dest, text)? {
        Delivery::Http { url, body } => {
            let resp = ureq::post(&url)
                .header("Content-Type", "application/json")
                .send(body.as_bytes())
                .map_err(|e| format!("{e}"))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            Ok(())
        }
        Delivery::Command { program, args } => {
            use std::io::Write;
            let mut child = std::process::Command::new(&program)
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("spawn {program}: {e}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let status = child.wait().map_err(|e| format!("{e}"))?;
            if !status.success() {
                return Err(format!("{program} exited {}", status.code().unwrap_or(-1)));
            }
            Ok(())
        }
    }
}

/// `ply notify --test`: send a fixed line to every configured destination
/// (or the ones in `override_to`), so an operator can prove delivery. The
/// count of destinations tried and the failures are returned.
pub fn test(override_to: &[String]) -> Result<(usize, Vec<String>)> {
    let dests = if !override_to.is_empty() {
        override_to.to_vec()
    } else {
        let cfg = Config::load()?
            .ok_or_else(|| Error::Runtime(format!("no {}", config_path().display())))?;
        resolve_destinations(&cfg.to)
    };
    if dests.is_empty() {
        return Err(Error::Runtime(
            "no destinations — put some in notify.toml `to`, or pass --to".into(),
        ));
    }
    let text = "[ply] test notification — if you see this, delivery works";
    let mut failures = Vec::new();
    for d in &dests {
        if let Err(e) = deliver(d, text) {
            failures.push(format!("{}: {e}", redacted(d)));
        }
    }
    Ok((dests.len(), failures))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: u64, app: &str, event: &str, detail: &str) -> Event {
        Event {
            ts,
            app: app.into(),
            event: event.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn only_subscribed_events_after_the_cursor_are_sent() {
        let cfg = Config {
            on: vec!["deploy-failed".into(), "snapshot-failed".into()],
            to: vec!["x".into()],
        };
        let events = vec![
            ev(10, "web", "deploy", "1.0 -> 1.1"), // not subscribed
            ev(20, "web", "deploy-failed", "gate timeout"), // subscribed, but <= cursor
            ev(30, "db", "snapshot-failed", "no space"), // subscribed, new
            ev(40, "web", "scale", "2 -> 3"),      // not subscribed
        ];
        let msgs = messages_for(&cfg, &events, 25);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "[ply] db snapshot-failed: no space");
    }

    #[test]
    fn restart_loop_fires_once_on_the_threshold_crossing() {
        let cfg = Config {
            on: vec!["restart-loop".into()],
            to: vec!["x".into()],
        };
        // Five crashes in the window: the message fires on the 3rd only.
        let events: Vec<Event> = (0..5)
            .map(|i| ev(100 + i * 10, "db", "instance-restart", "OOM"))
            .collect();
        let msgs = messages_for(&cfg, &events, 0);
        assert_eq!(msgs.len(), 1, "one loop message, not one per crash");
        assert!(msgs[0].text.contains("restart-loop"), "{}", msgs[0].text);
        // A crash outside the window does not count toward the threshold.
        let spread = vec![
            ev(100, "db", "instance-restart", "x"),
            ev(500, "db", "instance-restart", "x"), // > 300s after the first
            ev(510, "db", "instance-restart", "x"),
        ];
        assert!(
            messages_for(&cfg, &spread, 0).is_empty(),
            "not within one window"
        );
        // Subscribing to instance-restart directly gets every crash instead.
        let each = Config {
            on: vec!["instance-restart".into()],
            to: vec!["x".into()],
        };
        assert_eq!(messages_for(&each, &events, 0).len(), 5);
    }

    #[test]
    fn a_derived_name_is_never_matched_as_a_raw_event() {
        // If a journal ever carried a literal "restart-loop" line, the
        // direct-match arm must not double-send it.
        let cfg = Config {
            on: vec!["restart-loop".into()],
            to: vec!["x".into()],
        };
        let events = vec![ev(10, "db", "restart-loop", "somehow")];
        assert!(messages_for(&cfg, &events, 0).is_empty());
    }

    #[test]
    fn destinations_parse_into_the_right_request() {
        // Telegram: token keeps its own colon; chat id is the last field.
        let d = plan("telegram:111:AAA-bbb:@chan", "hi").unwrap();
        assert_eq!(
            d,
            Delivery::Http {
                url: "https://api.telegram.org/bot111:AAA-bbb/sendMessage".into(),
                body: "{\"chat_id\":\"@chan\",\"text\":\"hi\"}".into(),
            }
        );
        // Discord webhook.
        assert_eq!(
            plan("discord:https://discord.com/api/webhooks/1/x", "hi").unwrap(),
            Delivery::Http {
                url: "https://discord.com/api/webhooks/1/x".into(),
                body: "{\"content\":\"hi\"}".into(),
            }
        );
        // Bare https and webhook: prefix are the generic sender.
        assert!(matches!(
            plan("https://h.example/x", "hi").unwrap(),
            Delivery::Http { .. }
        ));
        assert!(matches!(
            plan("webhook:https://h.example/x", "hi").unwrap(),
            Delivery::Http { .. }
        ));
        // Command.
        assert_eq!(
            plan("command:/usr/bin/mail -s ply ops@x", "hi").unwrap(),
            Delivery::Command {
                program: "/usr/bin/mail".into(),
                args: vec!["-s".into(), "ply".into(), "ops@x".into()],
            }
        );
        // Text is JSON-escaped, so a quote or newline cannot break the body.
        let d = plan("discord:https://x/y", "a\"b\nc").unwrap();
        if let Delivery::Http { body, .. } = d {
            assert_eq!(body, "{\"content\":\"a\\\"b\\nc\"}");
        } else {
            panic!();
        }
        // Junk is refused, and the error carries no secret.
        let err = plan("telegram:onlytoken", "hi").unwrap_err();
        assert!(err.contains("telegram:"));
        assert!(plan("ftp://nope", "hi").is_err());
    }

    #[test]
    fn the_message_line_reads_plainly() {
        assert_eq!(
            line("web", "deploy-failed", "gate timeout").text,
            "[ply] web deploy-failed: gate timeout"
        );
        assert_eq!(line("web", "restart", "").text, "[ply] web restart");
    }
}
