//! Day-2 verbs: gc, rm, sync, systemd emit, audit, outdated.

use std::collections::BTreeSet;
use std::path::Path;

use crate::apps::{self, AppRecord};
use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{read_lockfile, read_manifest};
use crate::policy::Policy;
use crate::runtime::state;
use crate::source::Source;
use crate::store::Store;

/// `ply gc` — delete store entries unreachable from app records + running
/// instances. Reachability is defined by lockfiles; this is `npm prune`.
pub struct GcReport {
    pub kept: usize,
    pub deleted: Vec<String>,
    pub freed_bytes: u64,
}

pub fn gc(dry_run: bool) -> Result<GcReport> {
    let store = Store::open_default()?;
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for record in apps::list()? {
        roots.extend(record.digests.iter().cloned());
    }
    // Running instances count even without a record.
    for instance in state::list()? {
        if let Ok(Some(lock)) = read_lockfile(Path::new(&instance.image)) {
            roots.extend(lock.packages.iter().map(|p| p.sha256.clone()));
        }
    }

    let mut report = GcReport {
        kept: 0,
        deleted: Vec::new(),
        freed_bytes: 0,
    };
    let entries = std::fs::read_dir(store.root()).map_err(|source| Error::Io {
        path: store.root().to_path_buf(),
        source,
    })?;
    for entry in entries.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("sha256:") {
            continue;
        }
        if roots.contains(&name) {
            report.kept += 1;
            continue;
        }
        let size = entry
            .path()
            .join("pkg.img")
            .metadata()
            .map(|m| m.len())
            .unwrap_or(0);
        if !dry_run {
            std::fs::remove_dir_all(entry.path()).map_err(|source| Error::Io {
                path: entry.path(),
                source,
            })?;
        }
        report.freed_bytes += size;
        report.deleted.push(name);
    }
    Ok(report)
}

/// `ply rm <app>` — stop instances, drop the app record; volumes stay
/// unless `volumes` (data deletion is always separate + explicit).
pub struct RmReport {
    pub stopped: usize,
    pub record_removed: bool,
    pub volumes_removed: bool,
}

pub fn rm(app: &str, volumes: bool) -> Result<RmReport> {
    let mut stopped = 0;
    for instance in state::list()? {
        if instance.app == app && instance.alive() {
            unsafe { nix::libc::kill(instance.pid, nix::libc::SIGTERM) };
            stopped += 1;
        }
    }
    // Give the owning `ply run` processes a moment to tear down cleanly,
    // then escalate: a handler-less entrypoint is PID 1 in its pid ns and
    // silently drops SIGTERM — only SIGKILL gets through.
    if stopped > 0 {
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if state::list()?.iter().all(|s| s.app != app || !s.alive()) {
                break;
            }
        }
        for instance in state::list()? {
            if instance.app == app && instance.alive() {
                eprintln!(
                    "ply: {}.{} ignored SIGTERM (no handler as pid 1) — killing",
                    instance.app, instance.n
                );
                unsafe { nix::libc::kill(instance.pid, nix::libc::SIGKILL) };
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = state::reap_stale();

    let record_removed = AppRecord::remove(app);
    let mut volumes_removed = false;
    if volumes {
        let dir = crate::paths::volumes_dir().join(app);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|source| Error::Io { path: dir, source })?;
            volumes_removed = true;
        }
    }
    Ok(RmReport {
        stopped,
        record_removed,
        volumes_removed,
    })
}

/// `ply systemd <image>` — supervision is systemd's job; emit a unit.
pub fn systemd_unit(image: &Path) -> Result<String> {
    let manifest = read_manifest(image)?;
    let app = &manifest.package.name;
    let image_abs = std::path::absolute(image).map_err(|source| Error::Io {
        path: image.to_path_buf(),
        source,
    })?;
    let ply = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/local/bin/ply".into());
    Ok(format!(
        "# ply-{app}.service — install with:\n\
         #   ply systemd {image} | sudo tee /etc/systemd/system/ply-{app}.service\n\
         #   sudo systemctl enable --now ply-{app}\n\
         [Unit]\n\
         Description=ply app {app}\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=exec\n\
         ExecStart={ply} run {image_path}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         KillMode=mixed\n\
         TimeoutStopSec=15\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        image = image.display(),
        image_path = image_abs.display(),
    ))
}

/// `ply sync` — pre-fetch everything the host policy lists into the store.
pub struct SyncReport {
    pub fetched: Vec<String>,
    pub present: usize,
    pub skipped: Vec<String>,
}

pub fn sync(policy: &Policy, allow_insecure: bool) -> Result<SyncReport> {
    let store = Store::open_default()?;
    let mut report = SyncReport {
        fetched: Vec::new(),
        present: 0,
        skipped: Vec::new(),
    };
    for entry in &policy.runtimes {
        if entry.status == "refused" {
            continue;
        }
        let Some(source_spec) = &entry.source else {
            report.skipped.push(format!(
                "{} {} (no source in policy)",
                entry.name, entry.version
            ));
            continue;
        };
        if let Some(digest) = &entry.sha256 {
            if store.image_path(digest).is_some() {
                report.present += 1;
                continue;
            }
        }
        let source = Source::parse(source_spec, allow_insecure)?;
        let image = ImageName::new(&entry.name, entry.version.clone(), Os::Linux, Arch::host())?;
        source.fetch(&image, entry.sha256.as_deref(), &store)?;
        report.fetched.push(image.to_string());
    }
    Ok(report)
}

/// `ply audit` — risk surface: shared volumes, policy findings on running
/// instances.
pub struct AuditReport {
    pub shared_volumes: Vec<String>,
    pub findings: Vec<(String, String)>,
}

pub fn audit() -> Result<AuditReport> {
    let mut report = AuditReport {
        shared_volumes: Vec::new(),
        findings: Vec::new(),
    };

    if let Ok(apps) = std::fs::read_dir(crate::paths::volumes_dir()) {
        for app in apps.filter_map(|e| e.ok()) {
            if let Ok(vols) = std::fs::read_dir(app.path()) {
                for vol in vols.filter_map(|e| e.ok()) {
                    let name = vol.file_name().to_string_lossy().into_owned();
                    if name.ends_with(".shared") {
                        report.shared_volumes.push(format!(
                            "{}/{}",
                            app.file_name().to_string_lossy(),
                            name
                        ));
                    }
                }
            }
        }
    }

    if let Some(policy) = Policy::load_default()? {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for instance in state::list()? {
            if !instance.alive() || !seen.insert(instance.app.clone()) {
                continue;
            }
            if let Ok(Some(lock)) = read_lockfile(Path::new(&instance.image)) {
                for finding in policy.check_lockfile(&lock) {
                    report
                        .findings
                        .push((instance.app.clone(), finding.message));
                }
            }
        }
    }
    Ok(report)
}

/// `ply outdated` — newer versions available at each app's sources.
pub fn outdated(allow_insecure: bool) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for record in apps::list()? {
        let Ok(Some(lock)) = read_lockfile(&record.image) else {
            lines.push(format!(
                "{}: image {} unreadable — skipped",
                record.name,
                record.image.display()
            ));
            continue;
        };
        for pkg in &lock.packages {
            let Ok(source) = Source::parse(&pkg.source, allow_insecure) else {
                continue;
            };
            match source.list_versions(&pkg.name, Os::Linux, Arch::host()) {
                Ok(versions) => {
                    if let Some(newest) = versions.iter().max() {
                        if *newest > pkg.version {
                            lines.push(format!(
                                "{}: {} {} -> {newest}",
                                record.name, pkg.name, pkg.version
                            ));
                        }
                    }
                }
                Err(_) => continue, // forge sources can't list; skip quietly
            }
        }
    }
    Ok(lines)
}
