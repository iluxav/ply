//! The egress audit log: what an instance resolved and connected to.
//!
//! One JSON-lines file per instance, `<app>.<n>.log` under
//! `data_dir()/egress`, written by the per-instance egress thread (Task 7)
//! and read here and by `ply egress`. Unlike the log ring
//! (`runtime::logring`) this persists across restarts — `Writer::open`
//! appends, it never truncates — but rotates the same way once a file
//! passes `CAP_BYTES`: the live file becomes `.1` and a fresh one starts.

use std::io::Write;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Rotate threshold per file; two files kept (live + `.1`), same cap as
/// `runtime::logring`.
pub const CAP_BYTES: u64 = 512 * 1024;

/// One audit-log line. `kind` tags the JSON so `ply egress --json` and the
/// table renderer can tell records apart without guessing from fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Record {
    Resolved {
        t: String,
        app: String,
        n: u32,
        name: String,
        declared: bool,
        addrs: Vec<Ipv4Addr>,
        ttl: u32,
        /// Queries for this name since the last record of this kind was
        /// written: both DNS records are damped to one a minute per
        /// `(name, kind)`, so a chatty app cannot rotate the connection
        /// evidence out of the log with its own lookups. `default` so a
        /// line written before the field existed still parses (as 0).
        #[serde(default)]
        count: u64,
    },
    Refused {
        t: String,
        app: String,
        n: u32,
        name: String,
        declared: bool,
        /// Queries refused for this name since the last `refused` record.
        #[serde(default)]
        count: u64,
    },
    Allowed {
        t: String,
        app: String,
        n: u32,
        proto: String,
        dst: Ipv4Addr,
        port: u16,
        name: Option<String>,
        count: u64,
    },
    Blocked {
        t: String,
        app: String,
        n: u32,
        proto: String,
        dst: Ipv4Addr,
        port: u16,
        name: Option<String>,
        count: u64,
    },
    /// The same counter as `blocked`, read under `audit`: the traffic went
    /// through, and the kind must not say otherwise.
    Undeclared {
        t: String,
        app: String,
        n: u32,
        proto: String,
        dst: Ipv4Addr,
        port: u16,
        name: Option<String>,
        count: u64,
    },
}

pub fn dir() -> PathBuf {
    crate::paths::data_dir().join("egress")
}

pub fn path(app: &str, n: u32) -> PathBuf {
    dir().join(format!("{app}.{n}.log"))
}

fn rotated(path: &std::path::Path) -> PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(".1");
    PathBuf::from(os)
}

/// Append-only writer with one rotation. Opening does NOT rotate or
/// truncate — the audit log is durable, so `open` picks up wherever the
/// last generation left off.
pub struct Writer {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
}

impl Writer {
    pub fn open(app: &str, n: u32) -> Result<Writer> {
        Writer::open_at(self::path(app, n))
    }

    pub fn open_at(path: PathBuf) -> Result<Writer> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| Error::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
        let written = file
            .metadata()
            .map(|m| m.len())
            .map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
        Ok(Writer {
            file,
            path,
            written,
        })
    }

    /// Serializes one record as a JSON line and appends it, rotating first
    /// if the file is already at or past the cap.
    pub fn write(&mut self, r: &Record) {
        if self.written >= CAP_BYTES {
            let _ = std::fs::rename(&self.path, rotated(&self.path));
            match std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)
            {
                Ok(f) => {
                    self.file = f;
                    self.written = 0;
                }
                Err(_) => return, // data dir gone; drop silently
            }
        }
        let Ok(mut line) = serde_json::to_string(r) else {
            return;
        };
        line.push('\n');
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.written += line.len() as u64;
        }
    }
}

/// Every record for `app`, oldest first: each instance's `.1` (previous
/// generation) then its live file, instances sorted by `n`. Lines that
/// fail to parse (partial write, foreign content) are skipped.
pub fn read_app(app: &str) -> Vec<Record> {
    let mut instances: Vec<u32> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir()) {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".log") else {
                continue;
            };
            let Some((a, n)) = stem.rsplit_once('.') else {
                continue;
            };
            if a != app {
                continue;
            }
            if let Ok(n) = n.parse() {
                if !instances.contains(&n) {
                    instances.push(n);
                }
            }
        }
    }
    instances.sort();

    let mut out = Vec::new();
    for n in instances {
        let live = path(app, n);
        for file in [rotated(&live), live] {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for line in text.lines() {
                if let Ok(r) = serde_json::from_str::<Record>(line) {
                    out.push(r);
                }
            }
        }
    }
    out
}

/// `2026-09-04T21:03:11Z` for a unix timestamp (Howard Hinnant's
/// days-to-civil algorithm; valid for every date the epoch can hold).
pub fn rfc3339(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub fn now_rfc3339() -> String {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    rfc3339(unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_serialize_with_the_spec_shape_and_round_trip() {
        let r = Record::Allowed {
            t: "2026-09-04T21:03:11Z".into(),
            app: "web".into(),
            n: 1,
            proto: "tcp".into(),
            dst: "54.187.174.169".parse().unwrap(),
            port: 443,
            name: Some("api.stripe.com".into()),
            count: 12,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"allowed","t":"2026-09-04T21:03:11Z","app":"web","n":1,"proto":"tcp","dst":"54.187.174.169","port":443,"name":"api.stripe.com","count":12}"#
        );
        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), r);
    }

    /// Audit lets undeclared traffic through and the log says so: its kind
    /// is `undeclared`, never `blocked`, so `--json` readers are not lied to.
    #[test]
    fn an_undeclared_record_has_its_own_kind() {
        let r = Record::Undeclared {
            t: "2026-09-05T17:12:35Z".into(),
            app: "web".into(),
            n: 1,
            proto: "tcp".into(),
            dst: "100.63.40.118".parse().unwrap(),
            port: 443,
            name: Some("httpbin.org".into()),
            count: 1,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.starts_with(r#"{"kind":"undeclared","#), "{json}");
        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), r);
    }

    /// `refused` and `resolved` carry the damping's count, and a line
    /// written before the field existed still parses.
    #[test]
    fn the_dns_records_carry_their_damped_count_and_old_lines_still_parse() {
        let refused = Record::Refused {
            t: "2026-09-04T21:03:11Z".into(),
            app: "web".into(),
            n: 1,
            name: "evil.example".into(),
            declared: false,
            count: 417,
        };
        let json = serde_json::to_string(&refused).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"refused","t":"2026-09-04T21:03:11Z","app":"web","n":1,"name":"evil.example","declared":false,"count":417}"#
        );
        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), refused);

        let resolved = Record::Resolved {
            t: "2026-09-04T21:03:11Z".into(),
            app: "web".into(),
            n: 1,
            name: "api.stripe.com".into(),
            declared: true,
            addrs: vec!["54.187.174.169".parse().unwrap()],
            ttl: 300,
            count: 2,
        };
        let json = serde_json::to_string(&resolved).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"resolved","t":"2026-09-04T21:03:11Z","app":"web","n":1,"name":"api.stripe.com","declared":true,"addrs":["54.187.174.169"],"ttl":300,"count":2}"#
        );
        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), resolved);

        // A log written by the previous release has no `count` at all.
        let old = r#"{"kind":"refused","t":"t","app":"web","n":1,"name":"evil.example","declared":false}"#;
        assert!(matches!(
            serde_json::from_str::<Record>(old).unwrap(),
            Record::Refused { count: 0, .. }
        ));
        let old = r#"{"kind":"resolved","t":"t","app":"web","n":1,"name":"a.example","declared":true,"addrs":[],"ttl":300}"#;
        assert!(matches!(
            serde_json::from_str::<Record>(old).unwrap(),
            Record::Resolved { count: 0, .. }
        ));
    }

    #[test]
    fn the_writer_appends_lines_and_rotates_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web.1.log");
        let mut w = Writer::open_at(path.clone()).unwrap();
        let r = Record::Refused {
            t: "t".into(),
            app: "web".into(),
            n: 1,
            name: "x.example".into(),
            declared: false,
            count: 1,
        };
        for _ in 0..3 {
            w.write(&r);
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
        // force rotation
        let big = Record::Refused {
            t: "t".into(),
            app: "web".into(),
            n: 1,
            name: "y".repeat(CAP_BYTES as usize),
            declared: false,
            count: 1,
        };
        w.write(&big);
        w.write(&r);
        assert!(path.with_extension("log.1").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
    }

    #[test]
    fn rfc3339_matches_date_u() {
        assert_eq!(rfc3339(1_788_000_000), "2026-08-29T10:40:00Z");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn writer_open_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("web.1.log");
        let r = Record::Refused {
            t: "t".into(),
            app: "web".into(),
            n: 1,
            name: "x.example".into(),
            declared: false,
            count: 1,
        };
        {
            let mut w = Writer::open_at(p.clone()).unwrap();
            w.write(&r);
        }
        {
            let mut w = Writer::open_at(p.clone()).unwrap();
            w.write(&r);
        }
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 2);
    }
}
