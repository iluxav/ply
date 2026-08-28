//! `ply volume` — see and delete the one kind of state ply never touches
//! on its own. A volume is a plain host directory,
//! `<volumes>/<app>/<name>.<slot>`; `ls` cross-references live instances
//! and installed apps, `rm` deletes with guards. Deletion stays explicit —
//! this exists so it stops requiring `rm -rf` archaeology (and so wiping a
//! database volume before a `BACKUP_RESTORE` redeploy is an audited act).

use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::cli::{VolumeLsArgs, VolumeRmArgs};
use crate::commands::build::human_size;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Status {
    /// A live instance is (or, for shared scope, may be) writing here.
    InUse,
    /// The app is installed but nothing is running on this slot.
    Idle,
    /// No installed app claims this volume.
    Orphaned,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::InUse => "in use",
            Status::Idle => "idle",
            Status::Orphaned => "orphaned",
        }
    }
}

struct Row {
    app: String,
    volume: String, // "<name>.<slot>"
    status: Status,
    size: Option<u64>,
    path: PathBuf,
}

/// Who is alive and who is installed — the two facts a volume's status
/// hangs on. Separated from the fs walk so classification is testable.
struct World {
    live: HashSet<(String, String)>, // (app, slot as string)
    live_apps: HashSet<String>,      // apps with any live instance
    installed: HashSet<String>,
}

impl World {
    fn observe() -> Self {
        let mut live = HashSet::new();
        let mut live_apps = HashSet::new();
        for s in ply_core::runtime::state::list().unwrap_or_default() {
            if s.alive() {
                live.insert((s.app.clone(), s.n.to_string()));
                live_apps.insert(s.app);
            }
        }
        let installed = ply_core::apps::list()
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.name)
            .collect();
        World {
            live,
            live_apps,
            installed,
        }
    }

    fn classify(&self, app: &str, slot: &str) -> Status {
        let in_use = if slot == "shared" {
            // any instance may write a shared volume — assume the worst
            self.live_apps.contains(app)
        } else {
            self.live.contains(&(app.to_string(), slot.to_string()))
        };
        if in_use {
            Status::InUse
        } else if self.installed.contains(app) {
            Status::Idle
        } else {
            Status::Orphaned
        }
    }
}

/// None = unreadable (rootless volumes are subuid-owned): shown as `?`,
/// never as a misleading 0.
fn dir_size(path: &Path) -> Option<u64> {
    let entries = std::fs::read_dir(path).ok()?;
    let mut total = 0;
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if meta.is_dir() {
                total += dir_size(&entry.path()).unwrap_or(0);
            } else {
                total += meta.len();
            }
        }
    }
    Some(total)
}

fn shown_size(size: Option<u64>) -> String {
    size.map(human_size).unwrap_or_else(|| "?".into())
}

fn scan(world: &World) -> Vec<Row> {
    let mut rows = Vec::new();
    let root = ply_core::paths::volumes_dir();
    let Ok(apps) = std::fs::read_dir(&root) else {
        return rows;
    };
    for app in apps.flatten() {
        if !app.path().is_dir() {
            continue;
        }
        let app_name = app.file_name().to_string_lossy().into_owned();
        let Ok(vols) = std::fs::read_dir(app.path()) else {
            continue;
        };
        for vol in vols.flatten() {
            if !vol.path().is_dir() {
                continue;
            }
            let volume = vol.file_name().to_string_lossy().into_owned();
            let slot = volume.rsplit('.').next().unwrap_or("").to_string();
            rows.push(Row {
                status: world.classify(&app_name, &slot),
                size: dir_size(&vol.path()),
                path: vol.path(),
                app: app_name.clone(),
                volume,
            });
        }
    }
    rows.sort_by(|a, b| (&a.app, &a.volume).cmp(&(&b.app, &b.volume)));
    rows
}

pub fn ls(args: &VolumeLsArgs) -> Result<()> {
    let rows = scan(&World::observe());
    if args.json {
        let v: Vec<_> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "app": r.app, "volume": r.volume, "status": r.status.as_str(),
                    "bytes": r.size, "path": r.path,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no volumes — apps declare them in ply.toml [volumes]");
        return Ok(());
    }
    let wa = rows.iter().map(|r| r.app.len()).max().unwrap_or(3).max(3);
    let wv = rows
        .iter()
        .map(|r| r.volume.len())
        .max()
        .unwrap_or(6)
        .max(6);
    println!(
        "{:wa$}  {:wv$}  {:>8}  {:8}  PATH",
        "APP", "VOLUME", "SIZE", "STATUS"
    );
    for r in &rows {
        println!(
            "{:wa$}  {:wv$}  {:>8}  {:8}  {}",
            r.app,
            r.volume,
            shown_size(r.size),
            r.status.as_str(),
            r.path.display()
        );
    }
    Ok(())
}

/// `app/name.slot` — exact, no globs; deletion never guesses.
fn parse_target(target: &str) -> Result<(String, String)> {
    match target.split_once('/') {
        Some((app, vol)) if !app.is_empty() && vol.contains('.') => {
            Ok((app.to_string(), vol.to_string()))
        }
        _ => bail!("name the volume exactly as `ply volume ls` shows it: <app>/<name>.<slot>"),
    }
}

pub fn rm(args: &VolumeRmArgs) -> Result<()> {
    let world = World::observe();
    if args.orphans {
        let doomed: Vec<Row> = scan(&world)
            .into_iter()
            .filter(|r| r.status == Status::Orphaned)
            .collect();
        if doomed.is_empty() {
            println!("no orphaned volumes — every volume belongs to an installed app");
            return Ok(());
        }
        let total: u64 = doomed.iter().filter_map(|r| r.size).sum();
        for r in &doomed {
            println!(
                "would delete {}/{} ({})",
                r.app,
                r.volume,
                shown_size(r.size)
            );
        }
        if !args.yes {
            if !std::io::stdin().is_terminal() {
                bail!("refusing to delete without confirmation — pass --yes in scripts");
            }
            print!(
                "delete {} volume(s), {} — the data is gone for good [y/N]: ",
                doomed.len(),
                human_size(total)
            );
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            if !matches!(answer.trim(), "y" | "Y" | "yes") {
                println!("kept everything");
                return Ok(());
            }
        }
        for r in &doomed {
            remove(&r.path)?;
            let _ = std::fs::remove_dir(r.path.parent().unwrap_or(Path::new("/")));
            // app dir, if now empty
        }
        println!(
            "deleted {} volume(s), freed {}",
            doomed.len(),
            human_size(total)
        );
        return Ok(());
    }

    let Some(target) = &args.target else {
        bail!("what to delete? `ply volume rm <app>/<name>.<slot>` or `--orphans`");
    };
    let (app, volume) = parse_target(target)?;
    let path = ply_core::paths::volumes_dir().join(&app).join(&volume);
    if !path.is_dir() {
        bail!("no such volume — `ply volume ls` shows what exists");
    }
    let slot = volume.rsplit('.').next().unwrap_or("");
    if world.classify(&app, slot) == Status::InUse {
        bail!("{app}/{volume} is in use — stop the app first (`systemctl stop ply-{app}` or Ctrl-C the run)");
    }
    let size = dir_size(&path);
    remove(&path)?;
    let _ = std::fs::remove_dir(path.parent().unwrap_or(Path::new("/")));
    println!("deleted {app}/{volume}, freed {}", shown_size(size));
    Ok(())
}

/// Rootless volumes are owned by the subuid range the instance mapped —
/// a plain unlink gets EACCES even though the data is morally yours.
fn remove(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::anyhow!(
                "{e} — rootless volumes are owned by your subuid range; \
                 `sudo rm -rf {}` is the escape hatch (the files are yours by mapping)",
                path.display()
            )
        } else {
            e.into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut live = HashSet::new();
        live.insert(("db".to_string(), "1".to_string()));
        let mut live_apps = HashSet::new();
        live_apps.insert("db".to_string());
        let mut installed = HashSet::new();
        installed.insert("db".to_string());
        installed.insert("web".to_string());
        World {
            live,
            live_apps,
            installed,
        }
    }

    #[test]
    fn classification_covers_the_three_states() {
        let w = world();
        assert_eq!(w.classify("db", "1"), Status::InUse);
        assert_eq!(w.classify("db", "3"), Status::Idle, "installed, slot gone");
        assert_eq!(
            w.classify("db", "shared"),
            Status::InUse,
            "any live instance claims shared"
        );
        assert_eq!(w.classify("web", "1"), Status::Idle);
        assert_eq!(w.classify("gone", "1"), Status::Orphaned);
    }

    #[test]
    fn targets_are_exact() {
        assert!(parse_target("db/data.1").is_ok());
        assert!(parse_target("db/data.shared").is_ok());
        assert!(parse_target("db").is_err(), "an app alone never names data");
        assert!(parse_target("db/data").is_err(), "slot required");
        assert!(parse_target("/data.1").is_err());
    }
}
