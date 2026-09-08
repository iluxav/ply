use anyhow::Result;
use ply_core::runtime::state;

use crate::cli::PsArgs;

pub fn exec(args: PsArgs) -> Result<()> {
    // Reaping needs root (unmounts); without it just show liveness.
    if ply_core::paths::is_root() {
        let _ = state::reap_stale();
    }
    let states = state::list()?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&states)?);
        return Ok(());
    }

    let waiting = ply_core::runtime::after::WaitingMarker::list();
    let asleep = ply_core::runtime::after::AsleepMarker::list();
    if states.is_empty() && waiting.is_empty() && asleep.is_empty() {
        // State is per user: a unit's instances belong to root, and the
        // person who just started one and sees nothing here is not helped
        // by "no instances running".
        if ply_core::paths::is_root() {
            println!("no instances running");
        } else {
            println!(
                "no instances running as {} (instances started by root — a systemd unit — need `sudo ply ps`)",
                std::env::var("USER").unwrap_or_else(|_| "this user".into())
            );
        }
        return Ok(());
    }
    let header = format!(
        "{:<24} {:>8} {:<18} {:<20} {:>8} {:>8} STATUS",
        "NAME", "PID", "ADDRESS", "PORTS", "UPTIME", "RESTARTS"
    );
    println!("{header}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut any_stale = false;
    for s in &states {
        let ports: Vec<String> = s.ports.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        let stale = s.alive() && supervisor_stale(s.pid);
        any_stale |= stale;
        let status = match (s.alive(), stale) {
            (true, true) => "up*",
            (true, false) => "up",
            _ => "dead",
        };
        println!(
            "{:<24} {:>8} {:<18} {:<20} {:>8} {:>8} {}",
            format!("{}.{}", s.app, s.n),
            s.pid,
            address_column(s),
            ports.join(","),
            human_duration(now.saturating_sub(s.started)),
            s.restarts,
            status
        );
    }
    // Parents blocked on --after have no instances yet; show them so the
    // wait is visible rather than a silent gap.
    for w in &waiting {
        println!(
            "{:<24} {:>8} {:<18} {:<20} {:>8} {:>8} waiting on {}",
            w.app,
            w.pid,
            "—",
            "—",
            human_duration(now.saturating_sub(w.since)),
            "—",
            w.after.join(", ")
        );
    }
    // Asleep parents hold their port with no instance behind it; the row
    // says where the next connection wakes them.
    for m in &asleep {
        println!(
            "{:<24} {:>8} {:<14} {:<20} {:>8} {:>8} asleep, wakes on :{} (idle {})",
            m.app,
            m.pid,
            "—",
            "—",
            human_duration(now.saturating_sub(m.since)),
            "—",
            m.port,
            human_duration(m.idle_secs)
        );
    }
    if any_stale {
        println!("* supervisor predates the installed ply — restart the unit to pick it up");
    }
    Ok(())
}

/// After a binary replace, a still-running parent's /proc/<pid>/exe points
/// at a deleted inode — the honest tell that its code is stale.
fn supervisor_stale(instance_pid: i32) -> bool {
    let Some(ppid) = parent_pid(instance_pid) else {
        return false;
    };
    std::fs::read_link(format!("/proc/{ppid}/exe"))
        .map(|p| p.to_string_lossy().ends_with(" (deleted)"))
        .unwrap_or(false)
}

fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn human_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// What to dial. The published address when there is one — that is what a
/// caller uses, whatever the instance's own address is. A rootless instance
/// in its own namespace with nothing published has NO address anyone can
/// dial, and the audit found `ply ps` printing `127.0.0.1` for one, next to
/// `db:5432`, which a newcomer connects to and cannot. So: a dash.
fn address_column(s: &ply_core::runtime::state::InstanceState) -> String {
    match &s.published_addr {
        Some(addr) => addr.clone(),
        None if s.ip.is_loopback() => "-".into(),
        None => s.ip.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ply_core::runtime::state::InstanceState;

    fn instance(ip: &str, published: Option<&str>) -> InstanceState {
        InstanceState {
            app: "db".into(),
            n: 1,
            pid: 1,
            ip: ip.parse().unwrap(),
            ports: Default::default(),
            image: String::new(),
            started: 0,
            restarts: 0,
            health_port: None,
            published_port: published.map(|_| 5432),
            published_addr: published.map(str::to_string),
            instance_port: None,
            domains: vec![],
            network: None,
            serving: true,
            volumes: vec![],
            launch_path: None,
        }
    }

    #[test]
    fn the_address_column_shows_what_can_actually_be_dialled() {
        // Rootless, own namespace, nothing published: nothing to dial.
        assert_eq!(address_column(&instance("127.0.0.1", None)), "-");
        // Published: the address callers use, port included.
        assert_eq!(
            address_column(&instance("127.0.0.1", Some("127.0.0.1:5442"))),
            "127.0.0.1:5442"
        );
        // Rootful: the instance's own address is real and reachable.
        assert_eq!(address_column(&instance("10.77.0.3", None)), "10.77.0.3");
    }
}
