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
        println!("no instances running");
        return Ok(());
    }
    let header = format!(
        "{:<24} {:>8} {:<14} {:<20} {:>8} {:>8} STATUS",
        "NAME", "PID", "IP", "PORTS", "UPTIME", "RESTARTS"
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
            "{:<24} {:>8} {:<14} {:<20} {:>8} {:>8} {}",
            format!("{}.{}", s.app, s.n),
            s.pid,
            s.ip.to_string(),
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
            "{:<24} {:>8} {:<14} {:<20} {:>8} {:>8} waiting on {}",
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
