//! What the TUI renders — a per-refresh snapshot gathered from the same state
//! files `ply ps`, `ply why` and the events journal already read, plus a few
//! `systemctl` probes for the host tab. No new backend: this is a read.

use std::collections::BTreeMap;
use std::process::Command;

use ply_core::runtime::events::{self, Event};
use ply_core::runtime::state::{self, InstanceState};

pub struct Snapshot {
    pub apps: Vec<AppRow>,
    pub events: Vec<Event>, // most-recent first
    pub host: Host,
    pub deploys: Vec<DeployRow>,
    pub root: bool,
}

pub struct DeployRow {
    pub name: String,
    pub source: String,          // "repo …" / "app …" / "stack …" / "image …"
    pub version: Option<String>, // pinned version, if any (blank = follow latest)
    pub ok: bool,
    pub detail: String, // last reconcile outcome
    /// The services this deployment expands to, when it is a composition
    /// (a repo whose ply.toml has [[service]], a stack file, or a published
    /// stack ref). Empty for a single-app deployment.
    pub members: Vec<DeployMember>,
}

pub struct DeployMember {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub struct AppRow {
    pub name: String,
    pub up: usize,
    pub scale: usize,
    pub restarts: u32,
    pub uptime: u64,
    pub version: String,
    pub published: String,
    pub domains: Vec<String>,
    pub instances: Vec<InstanceRow>,
    pub service: ServiceStatus,
}

pub struct InstanceRow {
    pub name: String,
    pub pid: i32,
    pub ip: String,
    pub uptime: u64,
    pub restarts: u32,
    pub alive: bool,
    // From the cgroup-v2 sample behind `ply stats`. `None` = no cgroup (a
    // rootless instance) or unreadable. The sparkline history lives in `App`.
    pub cpu: Option<f64>,
    pub mem: Option<u64>,
    pub mem_max: Option<u64>, // the `[resources] mem` cap; None = unlimited
    pub cpu_max_cores: Option<f64>, // the `[resources] cpu` cap in cores; None = unlimited
}

pub struct Host {
    pub edge_installed: bool,
    pub caddy: ServiceStatus,                   // ply-edge.service
    pub proxy: ServiceStatus,                   // ply-proxy.service
    pub reconcile: ServiceStatus,               // deployments watcher
    pub services: Vec<(String, ServiceStatus)>, // ply-<app>.service, per app
    pub domains: Vec<(String, String)>,         // (domain, app)
    pub disk_pct: Option<u32>,
    pub version: String,
    // Machine caps — so an instance's cpu% / mem read against something.
    pub cpus: usize,
    pub mem_total: u64,
    pub mem_avail: u64,
    pub load1: f64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ServiceStatus {
    Active,
    Failed,
    Inactive,
    NoUnit,
}

impl ServiceStatus {
    pub fn label(self) -> &'static str {
        match self {
            ServiceStatus::Active => "active",
            ServiceStatus::Failed => "failed",
            ServiceStatus::Inactive => "inactive",
            ServiceStatus::NoUnit => "no unit",
        }
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn human_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Fill each instance's cpu/mem from the cgroup-v2 sample behind `ply stats`
/// (one shared ~120 ms window). Best-effort: empty for a rootless instance or
/// one with no readable cgroup.
fn merge_stats(apps: &mut [AppRow]) {
    let stats = ply_core::stats::collect(None, 120).unwrap_or_default();
    let by_name: BTreeMap<String, &ply_core::stats::InstanceStats> = stats
        .iter()
        .map(|s| (format!("{}.{}", s.app, s.n), s))
        .collect();
    for app in apps.iter_mut() {
        for inst in app.instances.iter_mut() {
            if let Some(s) = by_name.get(&inst.name) {
                inst.cpu = s.cpu_percent;
                inst.mem = s.mem_current;
                inst.mem_max = s.mem_max;
                inst.cpu_max_cores = read_cpu_max(&s.app, s.n);
            }
        }
    }
}

/// The `[resources] cpu` cap in cores, from the instance's cgroup `cpu.max`
/// (`quota period`; `max` = unlimited). None off a cgroup or when unlimited.
fn read_cpu_max(app: &str, n: u32) -> Option<f64> {
    // The instance's cgroup dir (rootful Linux); off Linux / rootless the file
    // is simply absent and this returns None.
    let text = std::fs::read_to_string(format!("/sys/fs/cgroup/ply-{app}.{n}/cpu.max")).ok()?;
    let mut it = text.split_whitespace();
    let quota = it.next()?;
    if quota == "max" {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    let period: f64 = it.next()?.parse().ok()?;
    (period > 0.0).then_some(quota / period)
}

impl Snapshot {
    pub fn gather() -> Self {
        let root = ply_core::paths::is_root();
        let states = state::list().unwrap_or_default();
        let mut apps = aggregate(&states, root);
        merge_stats(&mut apps);
        let mut events = events::read();
        events.reverse(); // most-recent first
        events.truncate(200);
        let host = Host::gather(&apps, root);
        let deploys = gather_deploys();
        Snapshot {
            apps,
            events,
            host,
            deploys,
            root,
        }
    }
}

fn gather_deploys() -> Vec<DeployRow> {
    let list = ply_core::deployments::list().unwrap_or_default();
    list.into_iter()
        .map(|(name, spec)| {
            let members = deploy_members(&name, &spec);
            let (source, version) = match &spec {
                Ok(s) => (spec_source(s), s.version.clone()),
                // A file that fails Spec::parse but is a composition (a stack
                // file) is not "unreadable" — say what it is.
                Err(_) if !members.is_empty() => {
                    (format!("composition · {} services", members.len()), None)
                }
                Err(_) => ("(unreadable spec)".into(), None),
            };
            let (ok, detail) = read_status_detail(&name);
            DeployRow {
                name,
                source,
                version,
                ok,
                detail,
                members,
            }
        })
        .collect()
}

/// The services a deployment expands to, each with its own reconcile status.
/// Three shapes: a `repo=` whose built checkout's ply.toml is a composition,
/// a deployment file that IS a composition (a stack file), or a published
/// stack ref (whose member names reconcile remembered in `.status`).
fn deploy_members(
    name: &str,
    spec: &Result<ply_core::deployments::Spec, ply_core::Error>,
) -> Vec<DeployMember> {
    let with_status = |stack: &ply_core::stack::Stack| -> Vec<DeployMember> {
        stack
            .members
            .iter()
            .map(|m| {
                let (ok, detail) = read_status_detail(&m.name);
                DeployMember {
                    name: m.name.clone(),
                    ok,
                    detail,
                }
            })
            .collect()
    };

    // 1. repo= composition — read the built checkout's ply.toml
    if let Ok(s) = spec {
        if s.repo.is_some() {
            let dir = std::path::PathBuf::from("/var/lib/ply/builds").join(name);
            if let Ok(Some(stack)) = ply_core::stack::load(&dir) {
                return with_status(&stack);
            }
            return Vec::new(); // single-app repo, or not cloned yet
        }
    }
    // 2. the deployment file itself is a composition (a stack file)
    let path = ply_core::deployments::spec_path(name);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(Some(stack)) = ply_core::stack::parse(&text, &path) {
            return with_status(&stack);
        }
    }
    // 3. a published stack ref — reconcile remembered its member names
    let remembered = ply_core::deployments::status_dir().join(format!("{name}.members"));
    if let Ok(text) = std::fs::read_to_string(&remembered) {
        return text
            .lines()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(|m| {
                let (ok, detail) = read_status_detail(m);
                DeployMember {
                    name: m.to_string(),
                    ok,
                    detail,
                }
            })
            .collect();
    }
    Vec::new()
}

fn spec_source(s: &ply_core::deployments::Spec) -> String {
    if let Some(r) = &s.repo {
        format!("repo {r}")
    } else if let Some(g) = &s.github {
        format!("github {g}")
    } else if let Some(u) = &s.url {
        format!("url {u}")
    } else if let Some(a) = &s.app {
        format!("app {a}")
    } else if let Some(i) = &s.image {
        format!("image {}", i.rsplit('/').next().unwrap_or(i))
    } else if let Some(st) = &s.stack {
        format!("stack {st}")
    } else {
        "—".into()
    }
}

/// `.status/<name>.status` is a one-line JSON `{ok, detail, ts}`.
fn read_status_detail(name: &str) -> (bool, String) {
    let path = ply_core::deployments::status_path(name);
    let Ok(text) = std::fs::read_to_string(path) else {
        return (false, "no status yet".into());
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => (
            v["ok"].as_bool().unwrap_or(false),
            v["detail"].as_str().unwrap_or("").to_string(),
        ),
        Err(_) => (false, "unreadable status".into()),
    }
}

fn aggregate(states: &[InstanceState], root: bool) -> Vec<AppRow> {
    let now = now();
    let mut by_app: BTreeMap<String, Vec<&InstanceState>> = BTreeMap::new();
    for s in states {
        by_app.entry(s.app.clone()).or_default().push(s);
    }
    by_app
        .into_iter()
        .map(|(name, insts)| {
            let up = insts.iter().filter(|s| s.alive()).count();
            let restarts = insts.iter().map(|s| s.restarts).max().unwrap_or(0);
            let uptime = insts
                .iter()
                .map(|s| now.saturating_sub(s.started))
                .max()
                .unwrap_or(0);
            let version = insts
                .first()
                .map(|s| image_version(&s.image))
                .unwrap_or_default();
            let published = insts
                .iter()
                .find_map(|s| s.published_addr.clone())
                .unwrap_or_else(|| "—".into());
            let mut domains: Vec<String> = insts.iter().flat_map(|s| s.domains.clone()).collect();
            domains.sort();
            domains.dedup();
            let instances = insts
                .iter()
                .map(|s| InstanceRow {
                    name: format!("{}.{}", s.app, s.n),
                    pid: s.pid,
                    ip: s.ip.to_string(),
                    uptime: now.saturating_sub(s.started),
                    restarts: s.restarts,
                    alive: s.alive(),
                    cpu: None,
                    mem: None,
                    mem_max: None,
                    cpu_max_cores: None,
                })
                .collect();
            let service = service_status(&format!("ply-{name}"), !root);
            AppRow {
                name,
                up,
                scale: insts.len(),
                restarts,
                uptime,
                version,
                published,
                domains,
                instances,
                service,
            }
        })
        .collect()
}

/// `path/myapp-1.2.0-linux-x64.img` -> `1.2.0`; best-effort (first digit-led
/// segment of the filename).
fn image_version(image: &str) -> String {
    let base = image.rsplit('/').next().unwrap_or(image);
    let base = base.strip_suffix(".img").unwrap_or(base);
    base.split('-')
        .find(|seg| seg.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or(base)
        .to_string()
}

impl Host {
    fn gather(apps: &[AppRow], root: bool) -> Self {
        let user = !root;
        let caddy = service_status("ply-edge", user);
        let proxy = service_status("ply-proxy", user);
        let reconcile = service_status("ply-reconcile.timer", user);
        let edge_installed = which("caddy") || caddy != ServiceStatus::NoUnit;
        let services = apps
            .iter()
            .map(|a| (format!("ply-{}", a.name), a.service))
            .collect();
        let mut domains: Vec<(String, String)> = apps
            .iter()
            .flat_map(|a| a.domains.iter().map(|d| (d.clone(), a.name.clone())))
            .collect();
        domains.sort();
        let (mem_total, mem_avail) = meminfo();
        Host {
            edge_installed,
            caddy,
            proxy,
            reconcile,
            services,
            domains,
            disk_pct: disk_pct(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            cpus: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            mem_total,
            mem_avail,
            load1: loadavg1(),
        }
    }
}

/// `systemctl is-active` (`--user` when rootless). Distinguishes a missing
/// unit from a stopped one so the host tab can offer "install unit".
pub fn service_status(unit: &str, user: bool) -> ServiceStatus {
    let out = match systemctl(user).arg("is-active").arg(unit).output() {
        Ok(o) => o,
        Err(_) => return ServiceStatus::NoUnit, // no systemctl (e.g. macOS)
    };
    match String::from_utf8_lossy(&out.stdout).trim() {
        "active" | "activating" | "deactivating" | "reloading" => ServiceStatus::Active,
        "failed" => ServiceStatus::Failed,
        _ if unit_exists(unit, user) => ServiceStatus::Inactive,
        _ => ServiceStatus::NoUnit,
    }
}

fn unit_exists(unit: &str, user: bool) -> bool {
    systemctl(user)
        .arg("list-unit-files")
        .arg(format!("{unit}.service"))
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(unit))
        .unwrap_or(false)
}

fn systemctl(user: bool) -> Command {
    let mut cmd = Command::new("systemctl");
    if user {
        cmd.arg("--user");
    }
    cmd
}

fn which(bin: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn disk_pct() -> Option<u32> {
    let out = Command::new("df")
        .arg("--output=pcent")
        .arg(ply_core::paths::data_dir())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)
        .and_then(|l| l.trim().trim_end_matches('%').parse().ok())
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`, in bytes (0 off Linux).
fn meminfo() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |key: &str| -> u64 {
        text.lines()
            .find_map(|l| {
                l.strip_prefix(key)?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
            .map(|kb| kb * 1024)
            .unwrap_or(0)
    };
    (kb("MemTotal:"), kb("MemAvailable:"))
}

/// The 1-minute load average from `/proc/loadavg` (0.0 off Linux).
fn loadavg1() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}
