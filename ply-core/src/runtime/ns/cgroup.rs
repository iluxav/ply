//! cgroup v2 limits: ~100 lines, the kernel is the enforcement.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::manifest::Resources;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Fork bombs are contained even when [resources] says nothing.
const DEFAULT_PIDS_MAX: u32 = 4096;

pub struct Cgroup {
    pub dir: PathBuf,
}

/// The instance's cgroup directory, `/sys/fs/cgroup/ply-<app>.<n>` — for a
/// reader that only wants the path (`ply stats`), not a live `Cgroup`
/// handle it would own and remove on drop.
pub fn instance_dir(app: &str, n: u32) -> PathBuf {
    Path::new(CGROUP_ROOT).join(format!("ply-{app}.{n}"))
}

impl Cgroup {
    /// Create `/sys/fs/cgroup/ply-<instance>` and apply limits.
    pub fn create(instance: &str, resources: Option<&Resources>) -> Result<Cgroup> {
        // Enable the controllers we are about to use, don't trust the
        // inherited set: systemd recomputes root subtree_control around
        // daemon-reloads, and `cpu` in particular comes and goes — writing
        // cpu.weight then fails on a file that does not exist. Idempotent,
        // best-effort (rootless has no business writing here; the limit
        // write's error message stays the honest failure).
        let _ = std::fs::write(
            Path::new(CGROUP_ROOT).join("cgroup.subtree_control"),
            "+cpu +memory +pids",
        );
        let dir = Path::new(CGROUP_ROOT).join(format!("ply-{instance}"));
        std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
            path: dir.clone(),
            source,
        })?;
        let cg = Cgroup { dir };

        let pids = resources.and_then(|r| r.pids).unwrap_or(DEFAULT_PIDS_MAX);
        cg.write("pids.max", &pids.to_string())?;

        if let Some(res) = resources {
            if let Some(mem) = &res.mem {
                let bytes = parse_size(mem.initial())?;
                cg.write("memory.max", &bytes.to_string())?;
                // soft limit below the hard one: reclaim before the OOM kill
                cg.write("memory.high", &(bytes * 9 / 10).to_string())?;
            }
            if let Some(swap) = &res.swap {
                let bytes = parse_size(swap)?;
                cg.write("memory.swap.max", &bytes.to_string())?;
            }
            if let Some(weight) = res.cpu_weight {
                if !(1..=10000).contains(&weight) {
                    return Err(Error::Manifest(format!(
                        "resources.cpu_weight `{weight}`: cgroup v2 wants 1..=10000"
                    )));
                }
                cg.write("cpu.weight", &weight.to_string())?;
            }
            if let Some(cpu) = res.cpu.as_ref().map(|c| c.initial()) {
                let cores: f64 = cpu.parse().map_err(|_| {
                    Error::Manifest(format!(
                        "resources.cpu `{cpu}`: expected a number like \"1.5\""
                    ))
                })?;
                if cores <= 0.0 {
                    return Err(Error::Manifest(format!(
                        "resources.cpu `{cpu}` must be > 0"
                    )));
                }
                let period = 100_000u64;
                let quota = (cores * period as f64) as u64;
                cg.write("cpu.max", &format!("{quota} {period}"))?;
            }
        }
        Ok(cg)
    }

    pub fn add_pid(&self, pid: i32) -> Result<()> {
        self.write("cgroup.procs", &pid.to_string())
    }

    fn write(&self, file: &str, value: &str) -> Result<()> {
        let path = self.dir.join(file);
        std::fs::write(&path, value).map_err(|source| {
            Error::Runtime(format!(
                "cgroup limit {} = {value}: {source} (cgroup v2 with cpu/memory/pids controllers required)",
                path.display()
            ))
        })
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // Empty by now (container reaped); rmdir removes the group.
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Live resize of a running instance's memory limit (`memory.max`, with
/// `memory.high` at 90 % as at creation). No handle: the parent owns the
/// group and must not `Drop`-rmdir it.
pub fn set_memory(app: &str, n: u32, bytes: u64) -> Result<()> {
    let dir = instance_dir(app, n);
    write_at(&dir, "memory.high", &(bytes * 9 / 10).to_string())?;
    write_at(&dir, "memory.max", &bytes.to_string())
}

/// Live resize of the CPU quota, in millicores.
pub fn set_cpu(app: &str, n: u32, millicores: u64) -> Result<()> {
    let period = 100_000u64;
    let quota = millicores * period / 1000;
    write_at(
        &instance_dir(app, n),
        "cpu.max",
        &format!("{quota} {period}"),
    )
}

fn write_at(dir: &Path, file: &str, value: &str) -> Result<()> {
    let path = dir.join(file);
    std::fs::write(&path, value)
        .map_err(|source| Error::Runtime(format!("cgroup {} = {value}: {source}", path.display())))
}

pub use crate::autoscale::parse_size;

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("lots").is_err());
    }
}
