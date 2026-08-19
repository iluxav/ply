use anyhow::Result;
use ply_core::runtime::state;

use crate::cli::PsArgs;

pub fn exec(args: PsArgs) -> Result<()> {
    // Reaping needs root (unmounts); without it just show liveness.
    if nix_is_root() {
        let _ = state::reap_stale();
    }
    let states = state::list()?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&states)?);
        return Ok(());
    }

    if states.is_empty() {
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
    for s in &states {
        let ports: Vec<String> = s.ports.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        let status = if s.alive() { "up" } else { "dead" };
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
    Ok(())
}

fn human_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

fn nix_is_root() -> bool {
    unsafe { nix_geteuid() == 0 }
}
extern "C" {
    #[link_name = "geteuid"]
    fn nix_geteuid() -> u32;
}
