//! `ply stats` — live per-instance usage. The kernel is the telemetry
//! agent: cgroup v2 files for cpu/mem/pids, veth interface counters for
//! network. Rootless instances (no cgroup) fall back to /proc/<pid>.

use std::path::PathBuf;

use serde::Serialize;

use crate::error::Result;
use crate::runtime::state::{self, InstanceState};

#[derive(Debug, Serialize)]
pub struct InstanceStats {
    pub app: String,
    pub n: u32,
    pub pid: i32,
    /// bytes
    pub mem_current: Option<u64>,
    /// bytes
    pub mem_peak: Option<u64>,
    /// bytes; None = unlimited
    pub mem_max: Option<u64>,
    /// percent of one core over the sample window
    pub cpu_percent: Option<f64>,
    /// times the cpu.max limit throttled this instance (since start)
    pub throttled: Option<u64>,
    pub pids_current: Option<u64>,
    /// container-perspective received bytes (since start)
    pub net_rx: Option<u64>,
    /// container-perspective sent bytes (since start)
    pub net_tx: Option<u64>,
    /// "cgroup" or "proc" (rootless fallback: pid 1 only, no throttle info)
    pub source: &'static str,
}

/// Collect stats for all (or one app's / one instance's) live instances.
/// CPU% is a derivative — sampled over `sample_ms`.
pub fn collect(filter: Option<&str>, sample_ms: u64) -> Result<Vec<InstanceStats>> {
    let instances: Vec<InstanceState> = state::list()?
        .into_iter()
        .filter(|s| s.alive())
        .filter(|s| match filter {
            None => true,
            Some(f) => s.app == f || format!("{}.{}", s.app, s.n) == f,
        })
        .collect();

    // First CPU sample, then one shared sleep, then the rest.
    let first: Vec<Option<u64>> = instances.iter().map(cpu_usage_usec).collect();
    std::thread::sleep(std::time::Duration::from_millis(sample_ms));

    let mut out = Vec::new();
    for (instance, first_usec) in instances.iter().zip(first) {
        // `None` off Linux (there is no cgroup fact to have there) and,
        // even on Linux, whenever this instance has none (rootless) — both
        // must fall through to the /proc branch below, never to a path
        // that merely happens not to exist.
        let cg = cgroup_dir(instance).filter(|p| p.exists());
        let use_cgroup = cg.is_some();
        let second_usec = cpu_usage_usec(instance);
        let cpu_percent = match (first_usec, second_usec) {
            (Some(a), Some(b)) if b >= a => {
                Some((b - a) as f64 / (sample_ms as f64 * 1000.0) * 100.0)
            }
            _ => None,
        };

        let (mem_current, mem_peak, mem_max, throttled, pids_current) = if let Some(cg) = &cg {
            (
                read_u64(cg.join("memory.current")),
                read_u64(cg.join("memory.peak")),
                read_u64(cg.join("memory.max")), // "max" parses as None
                cpu_stat_field(instance, "nr_throttled"),
                read_u64(cg.join("pids.current")),
            )
        } else {
            // rootless: pid-1 RSS from /proc (children not aggregated)
            let rss = std::fs::read_to_string(format!("/proc/{}/status", instance.pid))
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmRSS:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|kb| kb.parse::<u64>().ok())
                        .map(|kb| kb * 1024)
                });
            (rss, None, None, None, None)
        };

        // Host-side veth counters, swapped to the container's perspective.
        let veth = veth_name(instance);
        let net_rx = read_u64(PathBuf::from(format!(
            "/sys/class/net/{veth}/statistics/tx_bytes"
        )));
        let net_tx = read_u64(PathBuf::from(format!(
            "/sys/class/net/{veth}/statistics/rx_bytes"
        )));

        out.push(InstanceStats {
            app: instance.app.clone(),
            n: instance.n,
            pid: instance.pid,
            mem_current,
            mem_peak,
            mem_max,
            cpu_percent,
            throttled,
            pids_current,
            net_rx,
            net_tx,
            source: if use_cgroup { "cgroup" } else { "proc" },
        });
    }
    Ok(out)
}

// The cgroup path is a Linux fact (`runtime::ns::cgroup` owns the literal);
// `None` says plainly "there is no such path on this platform" so a caller
// can never mistake absence for a path that merely doesn't exist yet
// (elsewhere, or rootless on Linux — either way the /proc fallback below is
// what must run).
#[cfg(target_os = "linux")]
fn cgroup_dir(instance: &InstanceState) -> Option<PathBuf> {
    Some(crate::runtime::ns::cgroup::instance_dir(
        &instance.app,
        instance.n,
    ))
}

#[cfg(not(target_os = "linux"))]
fn cgroup_dir(_instance: &InstanceState) -> Option<PathBuf> {
    None
}

fn veth_name(instance: &InstanceState) -> String {
    let octets = instance.ip.octets();
    format!("ply{:02x}{:02x}", octets[2], octets[3])
}

/// Cumulative CPU time in microseconds — cgroup if present, else /proc
/// (pid 1 only; utime+stime in clock ticks, 100/s on every mainstream build).
fn cpu_usage_usec(instance: &InstanceState) -> Option<u64> {
    if let Some(usec) = cpu_stat_field(instance, "usage_usec") {
        return Some(usec);
    }
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", instance.pid)).ok()?;
    // fields 14+15 (utime, stime) come after the parenthesised comm field
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) * 10_000) // 100 ticks/s → 10_000 usec per tick
}

fn cpu_stat_field(instance: &InstanceState, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(cgroup_dir(instance)?.join("cpu.stat")).ok()?;
    text.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn read_u64(path: PathBuf) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}
