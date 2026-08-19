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

pub struct RunOptions {
    pub image: PathBuf,
    /// CLI -e KEY=VALUE overrides (highest precedence).
    pub cli_env: Vec<(String, String)>,
    pub allow_insecure: bool,
    pub scale: u32,
    /// Dev-mode bind mounts: (host path, container path). Same mechanism
    /// as volumes.
    pub links: Vec<(PathBuf, String)>,
}

pub fn run(opts: &RunOptions) -> Result<i32> {
    let rootless = !crate::paths::is_root();
    if rootless {
        eprintln!(
            "ply: rootless mode — extracted layers, host network (no .ply names), no cgroup limits"
        );
        // Ubuntu >= 24.04 strips capabilities from unprivileged user
        // namespaces unless an AppArmor profile grants `userns`.
        let restricted =
            std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
                .map(|v| v.trim() == "1")
                .unwrap_or(false);
        if restricted && !Path::new("/etc/apparmor.d/ply").exists() {
            return Err(Error::Runtime(
                "this kernel restricts unprivileged user namespaces — one-time fix:\n  \
                 sudo ply setup\n  \
                 (or run with sudo instead)"
                    .into(),
            ));
        }
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

    // Host policy: refused runtimes don't run; deprecated ones warn.
    if let Some(lockfile) = &lockfile {
        if let Some(policy) = crate::policy::Policy::load_default()? {
            for finding in policy.check_lockfile(lockfile) {
                match finding.severity {
                    crate::policy::Severity::Error => {
                        return Err(Error::Runtime(format!("host policy: {}", finding.message)))
                    }
                    crate::policy::Severity::Warning => {
                        eprintln!("ply: warning: {}", finding.message)
                    }
                }
            }
        }
    }

    // Record the app as installed — the GC root set.
    let record = crate::apps::AppRecord {
        name: manifest.package.name.clone(),
        image: std::path::absolute(&opts.image).unwrap_or_else(|_| opts.image.clone()),
        digests: lockfile
            .as_ref()
            .map(|l| l.packages.iter().map(|p| p.sha256.clone()).collect())
            .unwrap_or_default(),
        updated: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    record.save()?;

    let layer_refs: Vec<&Layer> = dep_layers.iter().collect();
    let mut env = compose_env(&layer_refs, &manifest.env, &opts.cli_env);
    env.entry("HOME".into()).or_insert("/root".into());
    if let Ok(term) = std::env::var("TERM") {
        env.entry("TERM".into()).or_insert(term);
    }
    let env: Vec<(String, String)> = env.into_iter().collect();

    if !rootless {
        network::ensure_bridge()?;
    }
    let _ = state::reap_stale(); // free IPs/dirs leaked by killed runs

    // Forward SIGTERM/SIGINT to every instance.
    unsafe {
        signal::signal(Signal::SIGTERM, signal::SigHandler::Handler(forward_signal)).ok();
        signal::signal(Signal::SIGINT, signal::SigHandler::Handler(forward_signal)).ok();
    }

    let mut instances: Vec<Instance> = Vec::new();
    for _ in 0..opts.scale.max(1) {
        let instance = launch_instance(
            &manifest,
            &entrypoint,
            &env,
            &opts.image,
            &dep_images,
            &opts.links,
            &store,
            rootless,
        )?;
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
    _cgroup: Option<Cgroup>,
    guard: InstanceGuard,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = hosts::remove_entry(&self.app, self.n);
        InstanceState::remove(&self.app, self.n);
        let _ = &self.guard; // unmounts layers, removes instance dir
    }
}

#[allow(clippy::too_many_arguments)]
fn launch_instance(
    manifest: &Manifest,
    entrypoint: &[String],
    env: &[(String, String)],
    app_image: &Path,
    dep_images: &[PathBuf],
    links: &[(PathBuf, String)],
    store: &Store,
    rootless: bool,
) -> Result<Instance> {
    let app = &manifest.package.name;
    let (instance_dir, n) = allocate_instance(app)?;

    // Named volumes: per-instance by default (scaling can never silently
    // corrupt single-writer state); `scope = "shared"` is the explicit opt-in.
    let mut binds: Vec<(PathBuf, String)> = Vec::new();
    for (name, volume) in &manifest.volumes {
        let suffix = if volume.scope == "shared" {
            "shared".to_string()
        } else {
            n.to_string()
        };
        let host_dir = crate::paths::volumes_dir()
            .join(app)
            .join(format!("{name}.{suffix}"));
        std::fs::create_dir_all(&host_dir).map_err(|source| Error::Io {
            path: host_dir.clone(),
            source,
        })?;
        binds.push((host_dir, volume.path.clone()));
    }
    for (host, container) in links {
        let host = std::path::absolute(host).map_err(|source| Error::Io {
            path: host.clone(),
            source,
        })?;
        if !host.exists() {
            return Err(Error::Runtime(format!(
                "--link source {} does not exist",
                host.display()
            )));
        }
        binds.push((host, container.clone()));
    }
    let guard = InstanceGuard {
        dir: instance_dir.clone(),
        mounted_layers: std::cell::RefCell::new(Vec::new()),
    };

    // Layers: root loop-mounts squashfs; rootless uses store-cached
    // extractions (unprivileged kernels can't mount squashfs).
    let mut layers: Vec<PathBuf> = Vec::new();
    let all_images: Vec<PathBuf> = std::iter::once(app_image.to_path_buf())
        .chain(dep_images.iter().cloned())
        .collect();
    for (i, img) in all_images.iter().enumerate() {
        if rootless {
            let digest = crate::digest::sha256_file(img)?;
            let rootfs = store.extracted_rootfs(img, &digest)?;
            // overlayfs splits lowerdir at `:` — store paths contain
            // `sha256:`, so hand the kernel a colon-free symlink instead
            let link = instance_dir.join("layers").join(i.to_string());
            std::os::unix::fs::symlink(&rootfs, &link).map_err(|source| Error::Io {
                path: link.clone(),
                source,
            })?;
            layers.push(link);
        } else {
            let target = instance_dir.join("layers").join(i.to_string());
            let (device, dev_fd) = loopdev::attach_ro(img)?;
            mount::mount_squashfs_ro(&device, &target)?;
            drop(dev_fd); // mount holds the device now; autoclear arms for unmount
            guard.mounted_layers.borrow_mut().push(target.clone());
            layers.push(target);
        }
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
        binds,
        sync_rx,
        keep_net_bind,
        privileged: false,
        rootless,
    };

    let mut stack = vec![0u8; 1024 * 1024];
    let mut flags = CloneFlags::CLONE_NEWNS
        | CloneFlags::CLONE_NEWPID
        | CloneFlags::CLONE_NEWUTS
        | CloneFlags::CLONE_NEWIPC;
    if rootless {
        // user ns grants the mount rights; host netns (no veth privileges)
        flags |= CloneFlags::CLONE_NEWUSER;
    } else {
        flags |= CloneFlags::CLONE_NEWNET;
    }
    let child = unsafe {
        nix::sched::clone(
            Box::new(|| child_main(&spec)),
            &mut stack,
            flags,
            Some(nix::libc::SIGCHLD),
        )
    }
    .map_err(|e| Error::Runtime(format!("clone: {e}")))?;

    // cgroup + veth (root) / uid maps (rootless) while the child is parked
    // on the pipe.
    let prepared = (|| -> Result<(Option<Cgroup>, Ipv4Addr)> {
        if rootless {
            write_id_maps(child.as_raw())?;
            return Ok((None, Ipv4Addr::new(127, 0, 0, 1)));
        }
        let cgroup = Cgroup::create(&format!("{app}.{n}"), manifest.resources.as_ref())?;
        cgroup.add_pid(child.as_raw())?;
        let used: Vec<Ipv4Addr> = state::list()?.iter().map(|s| s.ip).collect();
        let ip = network::allocate_ip(&used)?;
        network::setup_instance(child.as_raw(), ip)?;
        Ok((Some(cgroup), ip))
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
    if !rootless {
        hosts::add_entry(app, n, ip)?; // /etc/hosts needs root
    }

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

/// Map the invoking user to root inside the child's user namespace.
fn write_id_maps(pid: i32) -> Result<()> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let write = |file: &str, contents: String| -> Result<()> {
        let path = format!("/proc/{pid}/{file}");
        std::fs::write(&path, contents).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    };
    write("setgroups", "deny".into())?;
    write("gid_map", format!("0 {gid} 1"))?;
    write("uid_map", format!("0 {uid} 1"))?;
    Ok(())
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
    let instances = crate::paths::run_dir().join("instances");
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
