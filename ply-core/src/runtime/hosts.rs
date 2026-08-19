//! Managed /etc/hosts entries: `<ip>\t<app>.ply  # ply:<app>.<n>`.
//!
//! Names map to IPs only (never ports). `.ply` avoids mDNS-reserved `.local`.
//! Every managed line carries its instance tag, so removal is exact and
//! `rm -rf /var/lib/ply` still leaves /etc/hosts recoverable by tag.

use std::net::Ipv4Addr;
use std::path::Path;

use crate::error::{Error, Result};

const HOSTS: &str = "/etc/hosts";

fn tag(app: &str, n: u32) -> String {
    format!("# ply:{app}.{n}")
}

fn rewrite<F>(edit: F) -> Result<()>
where
    F: FnOnce(Vec<String>) -> Vec<String>,
{
    let path = Path::new(HOSTS);
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.into(),
        source,
    })?;
    let ends_nl = text.ends_with('\n');
    let lines: Vec<String> = text.lines().map(String::from).collect();
    let mut new_lines = edit(lines);
    let mut out = new_lines.join("\n");
    if ends_nl || new_lines.last().map(|l| !l.is_empty()).unwrap_or(false) {
        out.push('\n');
    }
    new_lines.clear();
    std::fs::write(path, out).map_err(|source| Error::Io {
        path: path.into(),
        source,
    })
}

pub fn add_entry(app: &str, n: u32, ip: Ipv4Addr) -> Result<()> {
    let line = format!("{ip}\t{app}.ply\t{}", tag(app, n));
    let t = tag(app, n);
    rewrite(move |mut lines| {
        lines.retain(|l| !l.ends_with(&t));
        lines.push(line);
        lines
    })
}

pub fn remove_entry(app: &str, n: u32) -> Result<()> {
    let t = tag(app, n);
    rewrite(move |mut lines| {
        lines.retain(|l| !l.ends_with(&t));
        lines
    })
}
