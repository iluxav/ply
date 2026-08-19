//! `ply run` — the whole runtime: mount the closure and exec.
//! `--scale N` = N identical processes/cgroups/netns, one shared store.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use nix::sched::CloneFlags;
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;

use crate::env::compose_env;
use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{read_embedded, read_lockfile, read_manifest};
use crate::manifest::{Layer, Manifest};
use crate::runtime::cgroup::Cgroup;
use crate::runtime::container::{child_main, ContainerSpec};
use crate::runtime::state::InstanceState;
use crate::runtime::{hosts, loopdev, mount, network, state};
use crate::source::Source;
use crate::store::Store;

pub const RUN_DIR: &str = "/run/ply";

pub struct RunOptions {
    pub image: PathBuf,
    /// CLI -e KEY=VALUE overrides (highest precedence).
    pub cli_env: Vec<(String, String)>,
    pub allow_insecure: bool,
    pub scale: u32,
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

    let layer_refs: Vec<&Layer> = dep_layers.iter().collect();
    let mut env = compose_env(&layer_refs, &manifest.env, &opts.cli_env);
    env.entry("HOME".into()).or_insert("/root".into());
    if let Ok(term) = std::env::var("TERM") {
        env.entry("TERM".into()).or_insert(term);
    }
    let env: Vec<(String, String)> = env.into_iter().collect();

    network::ensure_bridge()?;
    let _ = state::reap_stale(); // free IPs/dirs leaked by killed runs

    // Forward SIGTERM/SIGINT to every instance.
    unsafe {
        signal::signal(Signal::SIGTERM, signal::SigHandler::Handler(forward_signal)).ok();
        signal::signal(Signal::SIGINT, signal::SigHandler::Handler(forward_signal)).ok();
    }

    let mut instances: Vec<Instance> = Vec::new();
    for _ in 0..opts.scale.max(1) {
        let instance = launch_instance(&manifest, &entrypoint, &env, &opts.image, &dep_images)?;
        register_child(instance.child.as_raw());
        instances.push(instance);
    }

    // Reap all children; exit with the first non-zero code.
    let mut exit_code = 0;
    let mut remaining = instances.len();
    while remaining > 0 {
        match waitpid(Pid::from_raw(-1), None) {
            Ok(WaitStatus::Exited(pid, code)) => {
                if instances.iter().any(|i| i.child == pid) {
                    remaining -= 1;
                    if exit_code == 0 && code != 0 {
                        exit_code = code;
                    }
                }
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) => {
                if instances.iter().any(|i| i.child == pid) {
                    remaining -= 1;
                    if exit_code == 0 {
                        exit_code = 128 + sig as i32;
                    }
                }
            }
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(e) => return Err(Error::Runtime(format!("waitpid: {e}"))),
        }
    }
    drop(instances); // unmount, remove state + hosts entries
    Ok(exit_code)
}

struct Instance {
    app: String,
    n: u32,
    child: Pid,
    _cgroup: Cgroup,
    guard: InstanceGuard,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = hosts::remove_entry(&self.app, self.n);
        InstanceState::remove(&self.app, self.n);
        let _ = &self.guard; // unmounts layers, removes instance dir
    }
}

fn launch_instance(
    manifest: &Manifest,
    entrypoint: &[String],
    env: &[(String, String)],
    app_image: &Path,
    dep_images: &[PathBuf],
) -> Result<Instance> {
    let app = &manifest.package.name;
    let (instance_dir, n) = allocate_instance(app)?;
    let guard = InstanceGuard {
        dir: instance_dir.clone(),
        mounted_layers: std::cell::RefCell::new(Vec::new()),
    };

    // Loop-mount every squashfs layer.
    let mut layers: Vec<PathBuf> = Vec::new();
    let all_images: Vec<PathBuf> = std::iter::once(app_image.to_path_buf())
        .chain(dep_images.iter().cloned())
        .collect();
    for (i, img) in all_images.iter().enumerate() {
        let target = instance_dir.join("layers").join(i.to_string());
        let (device, dev_fd) = loopdev::attach_ro(img)?;
        mount::mount_squashfs_ro(&device, &target)?;
        drop(dev_fd); // mount holds the device now; autoclear arms for unmount
        guard.mounted_layers.borrow_mut().push(target.clone());
        layers.push(target);
    }

    // Sync pipe: the child waits for cgroup + network before setup.
    let (sync_rx, sync_tx) =
        nix::unistd::pipe().map_err(|e| Error::Runtime(format!("pipe: {e}")))?;

    let keep_net_bind = manifest.ports.values().any(|p| *p < 1024);
    let spec = ContainerSpec {
        layers,
        instance_dir: instance_dir.clone(),
        hostname: app.clone(),
        cwd: PathBuf::from(format!("/opt/{app}")),
        env: env.to_vec(),
        argv: entrypoint.to_vec(),
        sync_rx,
        keep_net_bind,
    };

    let mut stack = vec![0u8; 1024 * 1024];
    let flags = CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC
        | CloneFlags::CLONE_NEWNET;
    let child = unsafe {
        nix::sched::clone(
            Box::new(|| child_main(&spec)),
            &mut stack,
            flags,
            Some(nix::libc::SIGCHLD),
        )
    }
    .map_err(|e| Error::Runtime(format!("clone: {e}")))?;

    // cgroup + veth while the child is parked on the pipe.
    let prepared = (|| -> Result<(Cgroup, Ipv4Addr)> {
        let cgroup = Cgroup::create(&format!("{app}.{n}"), manifest.resources.as_ref())?;
        cgroup.add_pid(child.as_raw())?;
        let used: Vec<Ipv4Addr> = state::list()?.iter().map(|s| s.ip).collect();
        let ip = network::allocate_ip(&used)?;
        network::setup_instance(child.as_raw(), ip)?;
        Ok((cgroup, ip))
    })();
    let (cgroup, ip) = match prepared {
        Ok(ok) => ok,
        Err(e) => {
            drop(sync_tx); // EOF → child aborts
            let _ = signal::kill(child, Signal::SIGKILL);
            let _ = waitpid(child, None);
            return Err(e);
        }
    };

    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let state = InstanceState {
        app: app.clone(),
        n,
        pid: child.as_raw(),
        ip,
        ports: manifest.ports.clone(),
        image: app_image.display().to_string(),
        started,
    };
    state.save()?;
    hosts::add_entry(app, n, ip)?;

    // Release the child.
    let _ = nix::unistd::write(&sync_tx, &[1u8]);
    drop(sync_tx);

    Ok(Instance {
        app: app.clone(),
        n,
        child,
        _cgroup: cgroup,
        guard,
    })
}

const MAX_CHILDREN: usize = 256;
static CHILD_PIDS: [AtomicI32; MAX_CHILDREN] = [const { AtomicI32::new(0) }; MAX_CHILDREN];
static CHILD_COUNT: AtomicUsize = AtomicUsize::new(0);

fn register_child(pid: i32) {
    let idx = CHILD_COUNT.fetch_add(1, Ordering::SeqCst);
    if idx < MAX_CHILDREN {
        CHILD_PIDS[idx].store(pid, Ordering::SeqCst);
    }
}

extern "C" fn forward_signal(sig: i32) {
    let count = CHILD_COUNT.load(Ordering::SeqCst).min(MAX_CHILDREN);
    for slot in CHILD_PIDS.iter().take(count) {
        let pid = slot.load(Ordering::SeqCst);
        if pid > 0 {
            unsafe { nix::libc::kill(pid, sig) };
        }
    }
}

/// `/run/ply/instances/<app>.<n>` with rw/, work/, root/, layers/.
fn allocate_instance(app: &str) -> Result<(PathBuf, u32)> {
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
                return Ok((dir, n));
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
