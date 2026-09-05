//! `ply egress <app>`: the egress audit log rendered as a table (or raw
//! JSON with `--json`), optionally narrowed to `--blocked` and kept live
//! with `--follow`.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ply_core::egress::log::{self, Record};

use crate::cli::EgressArgs;

pub fn exec(args: EgressArgs) -> Result<()> {
    let records = log::read_app(&args.app);
    print_output(&records, &args);

    if !args.follow {
        return Ok(());
    }

    // Plain polling, no inotify: re-read each live file every second and
    // print only the lines beyond what we've already consumed from it.
    let mut seen: HashMap<PathBuf, usize> = live_files(&args.app)
        .into_iter()
        .map(|path| {
            let n = std::fs::read_to_string(&path)
                .map(|t| t.lines().count())
                .unwrap_or(0);
            (path, n)
        })
        .collect();

    loop {
        std::thread::sleep(Duration::from_secs(1));
        let mut fresh = Vec::new();
        for path in live_files(&args.app) {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let prev = seen.entry(path.clone()).or_insert(0);
            if lines.len() < *prev {
                *prev = 0; // rotated under us: the fresh file starts over
            }
            for line in &lines[*prev..] {
                if let Ok(r) = serde_json::from_str::<Record>(line) {
                    fresh.push(r);
                }
            }
            *prev = lines.len();
        }
        if !fresh.is_empty() {
            print_output(&fresh, &args);
        }
    }
}

fn print_output(records: &[Record], args: &EgressArgs) {
    if args.json {
        for r in records {
            if args.blocked && !in_blocked_view(r) {
                continue;
            }
            if let Ok(line) = serde_json::to_string(r) {
                println!("{line}");
            }
        }
    } else {
        print!("{}", render_table(records, args.blocked));
    }
}

/// This app's live log files (not `.1`) — what `--follow` watches for
/// growth. `log::read_app` handles `.1` + live together for a one-shot
/// read; follow only ever needs to watch the file still being appended to.
fn live_files(app: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(log::dir()) else {
        return out;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".log") else {
            continue;
        };
        let Some((a, n)) = stem.rsplit_once('.') else {
            continue;
        };
        if a != app || n.parse::<u32>().is_err() {
            continue;
        }
        out.push(entry.path());
    }
    out.sort();
    out
}

/// One rendered table row.
pub struct Row {
    pub dst: String,
    pub name: String,
    pub port: String,
    pub proto: String,
    /// Packets in conntrack state `new`, NOT connections: only `ct state
    /// new` updates the sets, and a blocked TCP connect retransmits its SYN
    /// several times, so one refused connection shows up as several.
    pub packets: u64,
    pub first: String,
    pub last: String,
    pub verdict: String,
}

/// What `--blocked` keeps: what enforce dropped, what audit saw that
/// enforce WOULD drop, and the names refused at DNS.
pub fn in_blocked_view(r: &Record) -> bool {
    matches!(
        r,
        Record::Blocked { .. } | Record::Undeclared { .. } | Record::Refused { .. }
    )
}

/// One (dst, port, proto) × verdict bucket, accumulated while walking the
/// records in order; turned into a `Row` once every record has been seen.
struct Group {
    dst: Ipv4Addr,
    port: u16,
    proto: String,
    name: Option<String>,
    count: u64,
    first: String,
    last: String,
    verdict: &'static str,
}

/// Groups `allowed`/`blocked` records by `(dst, port, proto)` — keeping the
/// max `count` and the first/last `t` seen for that trio — and lists each
/// `refused` name as its own row (`-` for port/proto, and the record's own
/// `count`: the queries that refusal stood for, since the DNS records are
/// damped to one a minute per name). A `resolved` record never makes a row
/// of its own; it only lends its name to a later-rendered row that doesn't
/// already have one, matched by address.
pub fn render_table(records: &[Record], blocked_only: bool) -> String {
    // Address -> most recently resolved name, for rows whose own record
    // carries no name (e.g. a blocked destination that was never resolved
    // by us, only connected to directly).
    let mut addr_names: HashMap<Ipv4Addr, String> = HashMap::new();
    for r in records {
        if let Record::Resolved { name, addrs, .. } = r {
            for addr in addrs {
                addr_names.insert(*addr, name.clone());
            }
        }
    }

    enum Entry {
        Group(Group),
        Refused { name: String, t: String, count: u64 },
    }
    let mut entries: Vec<Entry> = Vec::new();
    let mut index: HashMap<(Ipv4Addr, u16, String, &'static str), usize> = HashMap::new();

    for r in records {
        match r {
            Record::Allowed {
                t,
                proto,
                dst,
                port,
                name,
                count,
                ..
            }
            | Record::Blocked {
                t,
                proto,
                dst,
                port,
                name,
                count,
                ..
            }
            | Record::Undeclared {
                t,
                proto,
                dst,
                port,
                name,
                count,
                ..
            } => {
                let verdict = match r {
                    Record::Allowed { .. } => "allowed",
                    Record::Blocked { .. } => "blocked",
                    _ => "undeclared",
                };
                let key = (*dst, *port, proto.clone(), verdict);
                match index.get(&key) {
                    Some(&i) => {
                        if let Entry::Group(g) = &mut entries[i] {
                            g.count = g.count.max(*count);
                            g.last = t.clone();
                            if g.name.is_none() {
                                g.name = name.clone();
                            }
                        }
                    }
                    None => {
                        index.insert(key, entries.len());
                        entries.push(Entry::Group(Group {
                            dst: *dst,
                            port: *port,
                            proto: proto.clone(),
                            name: name.clone(),
                            count: *count,
                            first: t.clone(),
                            last: t.clone(),
                            verdict,
                        }));
                    }
                }
            }
            Record::Refused { t, name, count, .. } => {
                // Never deduped: each refusal is its own row.
                entries.push(Entry::Refused {
                    name: name.clone(),
                    t: t.clone(),
                    // A line written before `count` existed reads as 0, and
                    // a record always stands for at least the query that
                    // wrote it.
                    count: (*count).max(1),
                });
            }
            Record::Resolved { .. } => {}
        }
    }

    let mut rows: Vec<Row> = entries
        .into_iter()
        .map(|e| match e {
            Entry::Group(g) => {
                let name = g
                    .name
                    .or_else(|| addr_names.get(&g.dst).cloned())
                    .unwrap_or_default();
                Row {
                    dst: g.dst.to_string(),
                    name,
                    port: g.port.to_string(),
                    proto: g.proto,
                    packets: g.count,
                    first: g.first,
                    last: g.last,
                    verdict: g.verdict.to_string(),
                }
            }
            Entry::Refused { name, t, count } => Row {
                dst: name,
                name: "-".to_string(),
                port: "-".to_string(),
                proto: "-".to_string(),
                packets: count,
                first: t.clone(),
                last: t,
                verdict: "refused".to_string(),
            },
        })
        .collect();

    if blocked_only {
        rows.retain(|r| r.verdict != "allowed");
    }

    render_rows(&rows)
}

fn render_rows(rows: &[Row]) -> String {
    const HEADERS: [&str; 8] = [
        "DESTINATION",
        "NAME",
        "PORT",
        "PROTO",
        "NEW PKTS",
        "FIRST",
        "LAST",
        "VERDICT",
    ];
    let cells: Vec<[String; 8]> = rows
        .iter()
        .map(|r| {
            [
                r.dst.clone(),
                r.name.clone(),
                r.port.clone(),
                r.proto.clone(),
                r.packets.to_string(),
                r.first.clone(),
                r.last.clone(),
                r.verdict.clone(),
            ]
        })
        .collect();

    let mut widths: [usize; 8] = HEADERS.map(str::len);
    for row in &cells {
        for (w, c) in widths.iter_mut().zip(row.iter()) {
            *w = (*w).max(c.len());
        }
    }

    let render_line = |values: &[&str; 8]| -> String {
        let padded: Vec<String> = values
            .iter()
            .zip(widths.iter())
            .map(|(v, w)| format!("{v:<w$}"))
            .collect();
        padded.join("  ").trim_end().to_string()
    };

    let mut out = String::new();
    out.push_str(&render_line(&HEADERS));
    out.push('\n');
    for row in &cells {
        let refs: [&str; 8] = std::array::from_fn(|i| row[i].as_str());
        out.push_str(&render_line(&refs));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In audit the traffic went through; the row must not say otherwise,
    /// but `--blocked` still lists it — it is what enforce WOULD block.
    #[test]
    fn audit_rows_read_undeclared_and_still_show_under_blocked() {
        let recs = vec![Record::Undeclared {
            t: "2026-09-05T17:12:35Z".into(),
            app: "web".into(),
            n: 1,
            proto: "tcp".into(),
            dst: "100.63.40.118".parse().unwrap(),
            port: 443,
            name: Some("httpbin.org".into()),
            count: 1,
        }];
        let out = render_table(&recs, false);
        assert!(
            out.contains("httpbin.org") && out.contains("undeclared"),
            "{out}"
        );
        assert!(!out.contains("blocked"), "{out}");
        assert!(render_table(&recs, true).contains("httpbin.org"));
        assert!(in_blocked_view(&recs[0]));
    }

    #[test]
    fn the_table_groups_by_destination_and_marks_verdicts() {
        let recs = vec![
            Record::Resolved {
                t: "2026-09-04T21:00:00Z".into(),
                app: "web".into(),
                n: 1,
                name: "api.stripe.com".into(),
                declared: true,
                addrs: vec!["54.187.174.169".parse().unwrap()],
                ttl: 60,
                count: 1,
            },
            Record::Allowed {
                t: "2026-09-04T21:00:01Z".into(),
                app: "web".into(),
                n: 1,
                proto: "tcp".into(),
                dst: "54.187.174.169".parse().unwrap(),
                port: 443,
                name: Some("api.stripe.com".into()),
                count: 3,
            },
            Record::Allowed {
                t: "2026-09-04T21:05:00Z".into(),
                app: "web".into(),
                n: 1,
                proto: "tcp".into(),
                dst: "54.187.174.169".parse().unwrap(),
                port: 443,
                name: Some("api.stripe.com".into()),
                count: 12,
            },
            Record::Blocked {
                t: "2026-09-04T21:06:00Z".into(),
                app: "web".into(),
                n: 1,
                proto: "tcp".into(),
                dst: "203.0.113.9".parse().unwrap(),
                port: 8443,
                name: None,
                count: 1,
            },
            Record::Refused {
                t: "2026-09-04T21:07:00Z".into(),
                app: "web".into(),
                n: 1,
                name: "evil.example".into(),
                declared: false,
                count: 9,
            },
        ];
        let out = render_table(&recs, false);
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("DESTINATION"), "{out}");
        // (Corrected from the brief's literal: with `8443` in the data the
        // PORT column pads wider than the brief's example alone implied.
        // Rendering rule unchanged — pad every column, header included, to
        // its widest value, two spaces between columns.)
        assert!(
            out.contains("54.187.174.169  api.stripe.com  443   tcp    12        2026-09-04T21:00:01Z  2026-09-04T21:05:00Z  allowed"),
            "{out}"
        );
        // The count column is PACKETS in conntrack state `new`, not
        // connections: a blocked TCP connect retransmits its SYN.
        assert!(lines[0].contains("NEW PKTS"), "{out}");
        // …and a refused row shows the queries its damped record stood for.
        assert!(
            out.contains("evil.example    -               -     -      9  "),
            "{out}"
        );
        assert!(
            out.contains("203.0.113.9") && out.contains("blocked"),
            "{out}"
        );
        assert!(
            out.contains("evil.example") && out.contains("refused"),
            "{out}"
        );
        let only = render_table(&recs, true);
        assert!(!only.contains("api.stripe.com"), "{only}");
        assert!(only.contains("203.0.113.9") && only.contains("evil.example"));
    }
}
