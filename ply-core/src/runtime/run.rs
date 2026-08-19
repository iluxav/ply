//! `ply run` — the whole runtime: mount the closure and exec.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};

use nix::sched::CloneFlags;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitStatus};

use crate::env::compose_env;
use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{read_embedded, read_lockfile, read_manifest};
use crate::manifest::Layer;
use crate::runtime::container::{child_main, ContainerSpec};
use crate::runtime::{loopdev, mount};
use crate::source::Source;
use crate::store::Store;

pub const RUN_DIR: &str = "/run/ply";

pub struct RunOptions {
    pub image: PathBuf,
    /// CLI -e KEY=VALUE overrides (highest precedence).
    pub cli_env: Vec<(String, String)>,
    pub allow_insecure: bool,
}

pub fn run(opts: &RunOptions) -> Result<i32> {
    if !nix::unistd::geteuid().is_root() {
        return Err(Error::Runtime(
            "ply run needs root for now (mounts, namespaces) — try `sudo ply run …`; rootless mode is planned"
                .into(),
        ));
    }

    let manifest = read_manifest(&opts.image)?;
    let entrypoint = manifest.package.entrypoint.clone().ok_or_else(|| {
        Error::Runtime(format!(
            "{} is a library/runtime package (no entrypoint) — only app images run",
            opts.image.display()
        ))
    })?;
    if manifest.package.isolation == "vm" {
        return Err(Error::Runtime(
            "isolation = \"vm\" is not implemented yet — only \"ns\" runs today".into(),
        ));
    }
    let lockfile = read_lockfile(&opts.image)?;

    // Ensure the store has every locked digest; fetch by hash if missing.
    let store = Store::open_default()?;
    let mut dep_images: Vec<PathBuf> = Vec::new();
    let mut dep_layers: Vec<Layer> = Vec::new();
    if let Some(lockfile) = &lockfile {
        for pkg in &lockfile.packages {
            let path = match store.image_path(&pkg.sha256) {
                Some(path) => path,
                None => {
                    let source = Source::parse(&pkg.source, opts.allow_insecure)?;
                    let image =
                        ImageName::new(&pkg.name, pkg.version.clone(), Os::Linux, Arch::host())?;
                    eprintln!("ply: fetching {image} ({})", pkg.sha256);
                    source.fetch(&image, Some(&pkg.sha256), &store)?.1
                }
            };
            if let Some(bytes) = read_embedded(&path, "/.layer.toml")? {
                dep_layers.push(
                    toml::from_str(&String::from_utf8_lossy(&bytes)).map_err(|e| {
                        Error::Runtime(format!("{}: bad /.layer.toml: {e}", pkg.name))
                    })?,
                );
            }
            dep_images.push(path);
        }
    }

    let instance = allocate_instance(&manifest.package.name)?;
    let guard = InstanceGuard {
        dir: instance.clone(),
        mounted_layers: std::cell::RefCell::new(Vec::new()),
    };

    // Loop-mount every squashfs layer (host-visible under the instance dir;
    // the container sees them via its inherited, then-private mount table).
    let mut layers: Vec<PathBuf> = Vec::new();
    for (i, img) in std::iter::once(&opts.image)
        .chain(dep_images.iter())
        .enumerate()
    {
        let target = instance.join("layers").join(i.to_string());
        let (device, dev_fd) = loopdev::attach_ro(img)?;
        mount::mount_squashfs_ro(&device, &target)?;
        drop(dev_fd); // mount holds the device now; autoclear arms for unmount
        guard.mounted_layers.borrow_mut().push(target.clone());
        layers.push(target);
    }

    let layer_refs: Vec<&Layer> = dep_layers.iter().collect();
    let mut env = compose_env(&layer_refs, &manifest.env, &opts.cli_env);
    env.entry("HOME".into()).or_insert("/root".into());
    if let Ok(term) = std::env::var("TERM") {
        env.entry("TERM".into()).or_insert(term);
    }

    // Sync pipe: the child waits to be placed into its cgroup before setup.
    let (sync_rx, sync_tx) =
        nix::unistd::pipe().map_err(|e| Error::Runtime(format!("pipe: {e}")))?;

    let keep_net_bind = manifest.ports.values().any(|p| *p < 1024);
    let spec = ContainerSpec {
        layers,
        instance_dir: instance.clone(),
        hostname: manifest.package.name.clone(),
        cwd: PathBuf::from(format!("/opt/{}", manifest.package.name)),
        env: env.into_iter().collect(),
        argv: entrypoint,
        sync_rx,
        keep_net_bind,
    };

    // clone(2) into fresh namespaces; the child becomes PID 1 in its pidns.
    let mut stack = vec![0u8; 1024 * 1024];
    let flags = CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC;
    let child = unsafe {
        nix::sched::clone(
            Box::new(|| child_main(&spec)),
            &mut stack,
            flags,
            Some(nix::libc::SIGCHLD),
        )
    }
    .map_err(|e| Error::Runtime(format!("clone: {e}")))?;

    // cgroup v2 limits; the child stays parked on the pipe until this holds.
    let instance_name = instance
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cgroup =
        crate::runtime::cgroup::Cgroup::create(&instance_name, manifest.resources.as_ref())
            .and_then(|cg| {
                cg.add_pid(child.as_raw())?;
                Ok(cg)
            });
    let _cgroup = match cgroup {
        Ok(cg) => {
            // release the child: one byte, then close our write end
            let _ = nix::unistd::write(&sync_tx, &[1u8]);
            drop(sync_tx);
            Some(cg)
        }
        Err(e) => {
            drop(sync_tx); // EOF → child aborts
            let _ = nix::sys::signal::kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            return Err(e);
        }
    };

    // Forward SIGTERM/SIGINT to the container.
    CHILD_PID.store(child.as_raw(), Ordering::SeqCst);
    unsafe {
        signal::signal(Signal::SIGTERM, signal::SigHandler::Handler(forward_signal)).ok();
        signal::signal(Signal::SIGINT, signal::SigHandler::Handler(forward_signal)).ok();
    }

    let status = loop {
        match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, code)) => break code,
            Ok(WaitStatus::Signaled(_, sig, _)) => break 128 + sig as i32,
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(Error::Runtime(format!("waitpid: {e}"))),
        }
    };
    drop(guard);
    Ok(status)
}

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_signal(sig: i32) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe { nix::libc::kill(pid, sig) };
    }
}

/// `/run/ply/instances/<app>.<n>` with rw/, work/, root/, layers/.
fn allocate_instance(app: &str) -> Result<PathBuf> {
    let instances = Path::new(RUN_DIR).join("instances");
    std::fs::create_dir_all(&instances).map_err(|source| Error::Io {
        path: instances.clone(),
        source,
    })?;
    for n in 1..10_000u32 {
        let dir = instances.join(format!("{app}.{n}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                for sub in ["rw", "work", "root", "layers"] {
                    std::fs::create_dir_all(dir.join(sub)).map_err(|source| Error::Io {
                        path: dir.join(sub),
                        source,
                    })?;
                }
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(Error::Io { path: dir, source }),
        }
    }
    Err(Error::Runtime(format!("no free instance slot for {app}")))
}

/// Unmounts layer mounts and removes the instance dir on drop — including
/// error paths.
struct InstanceGuard {
    dir: PathBuf,
    mounted_layers: std::cell::RefCell<Vec<PathBuf>>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        for target in self.mounted_layers.borrow().iter() {
            mount::unmount_detach(target);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Parse an --env-file: KEY=VALUE lines, `#` comments, blanks ignored.
pub fn parse_env_file(path: &Path) -> Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut pairs = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| {
            Error::Runtime(format!(
                "{}:{}: expected KEY=VALUE",
                path.display(),
                lineno + 1
            ))
        })?;
        pairs.push((k.trim().to_string(), v.to_string()));
    }
    Ok(pairs)
}
