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
    // Stop the `ply run` PARENTS, not the instances: signalling the parent
    // sets its shutting-down flag (no [restart] respawns) and it forwards to
    // its children. Killing children directly would race a restart policy.
    let mut stopped = 0;
    let mut parents: BTreeSet<i32> = BTreeSet::new();
    for instance in state::list()? {
        if instance.app == app && instance.alive() {
            stopped += 1;
            match parent_pid(instance.pid) {
                Some(ppid) if ppid > 1 => {
                    parents.insert(ppid);
                }
                _ => unsafe {
                    nix::libc::kill(instance.pid, nix::libc::SIGTERM);
                },
            }
        }
    }
    for ppid in &parents {
        unsafe { nix::libc::kill(*ppid, nix::libc::SIGTERM) };
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

/// `ply deploy` — rolling deploy via the run parent: validate the new image,
/// write the pointer file, SIGHUP the parents, watch the roll through state
/// files. On any failure before the signal, the pointer is cleaned up.
pub struct DeployReport {
    pub app: String,
    pub parents: usize,
    pub rolled: Vec<String>,
    pub complete: bool,
}

pub fn deploy(image: &Path, timeout_secs: u64) -> Result<DeployReport> {
    // Validate before touching anything on disk.
    let manifest = read_manifest(image)?;
    if manifest.package.entrypoint.is_none() {
        return Err(Error::Build(format!(
            "{}: not an app image (no entrypoint)",
            image.display()
        )));
    }
    let app = manifest.package.name.clone();
    if let (Some(policy), Ok(Some(lock))) = (Policy::load_default()?, read_lockfile(image)) {
        for finding in policy.check_lockfile(&lock) {
            if matches!(finding.severity, crate::policy::Severity::Error) {
                return Err(Error::Runtime(format!("host policy: {}", finding.message)));
            }
        }
    }

    // Find the app's run parents.
    let mut parents: BTreeSet<i32> = BTreeSet::new();
    for instance in state::list()? {
        if instance.app == app && instance.alive() {
            if let Some(ppid) = parent_pid(instance.pid) {
                if ppid > 1 {
                    parents.insert(ppid);
                }
            }
        }
    }
    if parents.is_empty() {
        return Err(Error::Runtime(format!(
            "no running instances of `{app}` — nothing to roll (just `ply run {}`)",
            image.display()
        )));
    }

    // Pointer file + signal. Clean the pointer up if signalling fails.
    let image_abs = std::path::absolute(image).map_err(|source| Error::Io {
        path: image.to_path_buf(),
        source,
    })?;
    let dir = crate::paths::apps_dir().join(&app);
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    let pointer = dir.join("next-image");
    std::fs::write(&pointer, format!("{}\n", image_abs.display())).map_err(|source| Error::Io {
        path: pointer.clone(),
        source,
    })?;
    for ppid in &parents {
        let rc = unsafe { nix::libc::kill(*ppid, nix::libc::SIGHUP) };
        if rc != 0 {
            let _ = std::fs::remove_file(&pointer); // leave no stale pointer
            return Err(Error::Runtime(format!(
                "cannot signal run parent (pid {ppid}) — deploy must run as the same user that runs the app"
            )));
        }
    }

    // Watch the roll: done when every slot that was live at the start runs
    // the new image (a mid-roll slot is briefly absent from state — counting
    // only visible instances would declare victory early).
    let expected: usize = state::list()?
        .iter()
        .filter(|s| s.app == app && s.alive())
        .count();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let want = image_abs.display().to_string();
    let mut rolled: BTreeSet<String> = BTreeSet::new();
    loop {
        let mut all = true;
        for instance in state::list()? {
            if instance.app != app || !instance.alive() {
                continue;
            }
            let name = format!("{}.{}", instance.app, instance.n);
            if instance.image == want {
                rolled.insert(name);
            } else {
                all = false;
            }
        }
        if all && rolled.len() >= expected {
            return Ok(DeployReport {
                app,
                parents: parents.len(),
                rolled: rolled.into_iter().collect(),
                complete: true,
            });
        }
        if std::time::Instant::now() >= deadline {
            return Ok(DeployReport {
                app,
                parents: parents.len(),
                rolled: rolled.into_iter().collect(),
                complete: false,
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// Parent pid from /proc/<pid>/stat (field 4, after the comm parens).
fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// systemd's ExecStart splits on whitespace: values with spaces or quotes
/// need double-quoting (systemd's own quoting rules, backslash-escaped).
fn quote_unit_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        return arg.to_string();
    }
    let escaped: String = arg
        .chars()
        .flat_map(|c| match c {
            '"' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

/// `ply systemd <image> [run flags]` — supervision is systemd's job; emit a
/// unit whose ExecStart carries the run flags (scale, publish, env).
pub fn systemd_unit(image: &Path, run_flags: &[String], after: &[String]) -> Result<String> {
    let manifest = read_manifest(image)?;
    let image_abs = std::path::absolute(image).map_err(|source| Error::Io {
        path: image.to_path_buf(),
        source,
    })?;
    let ply = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "/usr/local/bin/ply".into());
    Ok(render_unit(
        &manifest.package.name,
        &ply,
        &image.display().to_string(),
        &image_abs.display().to_string(),
        run_flags,
        after,
    ))
}

/// The unit text. `after` apps become `After=`/`Wants=` on their ply units
/// (systemd orders the start; `--after` in ExecStart gates on readiness).
pub fn render_unit(
    app: &str,
    ply: &str,
    image: &str,
    image_path: &str,
    run_flags: &[String],
    after: &[String],
) -> String {
    let mut unit = format!(
        "# ply-{app}.service — install with:\n\
         #   ply systemd {image} | sudo tee /etc/systemd/system/ply-{app}.service\n\
         #   sudo systemctl enable --now ply-{app}\n\
         [Unit]\n\
         Description=ply app {app}\n\
         After=network-online.target\n\
         Wants=network-online.target\n"
    );
    for dep in after {
        unit.push_str(&format!(
            "After=ply-{dep}.service\nWants=ply-{dep}.service\n"
        ));
    }
    unit.push_str(&format!(
        "\n\
         [Service]\n\
         Type=exec\n\
         ExecStart={ply} run{flags} {image_path}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         KillMode=mixed\n\
         TimeoutStopSec=15\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        flags = run_flags
            .iter()
            .map(|f| format!(" {}", quote_unit_arg(f)))
            .collect::<String>(),
    ));
    unit
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_args_quote_only_when_needed() {
        assert_eq!(quote_unit_arg("--scale"), "--scale");
        assert_eq!(quote_unit_arg("80:3000"), "80:3000");
        assert_eq!(quote_unit_arg("NODE_ENV=production"), "NODE_ENV=production");
        assert_eq!(
            quote_unit_arg("GREETING=hello world"),
            "\"GREETING=hello world\""
        );
        assert_eq!(quote_unit_arg("A=say \"hi\""), "\"A=say \\\"hi\\\"\"");
        assert_eq!(quote_unit_arg(""), "\"\"");
    }

    #[test]
    fn render_unit_orders_after_ply_units_and_passes_the_flag() {
        let flags = vec![
            "--scale".to_string(),
            "10".into(),
            "--after".into(),
            "pgdb".into(),
        ];
        let unit = render_unit(
            "pgapp",
            "/usr/local/bin/ply",
            "pgapp.img",
            "/srv/pgapp.img",
            &flags,
            &["pgdb".into()],
        );
        assert!(unit.contains("After=network-online.target\nWants=network-online.target\nAfter=ply-pgdb.service\nWants=ply-pgdb.service\n"), "{unit}");
        assert!(
            unit.contains(
                "ExecStart=/usr/local/bin/ply run --scale 10 --after pgdb /srv/pgapp.img\n"
            ),
            "{unit}"
        );
        let plain = render_unit(
            "pgdb",
            "/usr/local/bin/ply",
            "pgdb.img",
            "/srv/pgdb.img",
            &[],
            &[],
        );
        assert!(
            !plain.contains("After=ply-"),
            "no dependency lines without --after:\n{plain}"
        );
    }
}
