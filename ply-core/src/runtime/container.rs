//! The container process: everything between clone() and execve().
//!
//! Runs inside fresh mount/PID/UTS/IPC namespaces. Keep it small and
//! infallible-or-die: any error prints and exits 127.

use std::ffi::CString;
use std::path::{Path, PathBuf};

use nix::sys::stat::{makedev, mknod, Mode, SFlag};

use crate::error::Result;
use crate::runtime::mount;

pub struct ContainerSpec {
    /// Mounted layer dirs, top (app) first, base last.
    pub layers: Vec<PathBuf>,
    /// Instance dir containing rw/, work/, root/.
    pub instance_dir: PathBuf,
    pub hostname: String,
    /// Working directory inside the container (the app's prefix).
    pub cwd: PathBuf,
    /// Composed environment, KEY=VALUE.
    pub env: Vec<(String, String)>,
    pub argv: Vec<String>,
    /// Bind mounts: (host dir, absolute path inside the container).
    /// Volumes and --link both come through here.
    pub binds: Vec<(PathBuf, String)>,
    /// Read end of the parent's sync pipe: proceed on 1 byte (parent placed
    /// us in the cgroup), abort on EOF (parent failed).
    pub sync_rx: std::os::fd::OwnedFd,
    /// Keep CAP_NET_BIND_SERVICE (a declared port is < 1024).
    pub keep_net_bind: bool,
    /// Skip rights stripping entirely — ONLY for `ply craft` authoring
    /// sessions, where installing packages needs real root inside.
    pub privileged: bool,
    /// Running in a user namespace as an unprivileged user: /dev nodes are
    /// bind-mounted from the host (mknod is denied), devpts is best-effort.
    pub rootless: bool,
}

/// Child entry point. Never returns on success (execve).
pub fn child_main(spec: &ContainerSpec) -> isize {
    let mut byte = [0u8; 1];
    match nix::unistd::read(&spec.sync_rx, &mut byte) {
        Ok(1) => {}
        _ => {
            // parent died or failed before releasing us
            return 125;
        }
    }
    match setup_and_exec(spec) {
        Ok(never) => never,
        Err(e) => {
            eprintln!("ply: container setup failed: {e}");
            127
        }
    }
}

fn setup_and_exec(spec: &ContainerSpec) -> Result<isize> {
    mount::make_all_private()?;

    let root = spec.instance_dir.join("root");
    let lower: Vec<&Path> = spec.layers.iter().map(PathBuf::as_path).collect();
    mount::mount_overlay(
        &lower,
        &spec.instance_dir.join("rw"),
        &spec.instance_dir.join("work"),
        &root,
    )?;

    // Skeleton dirs land in the upperdir when a thin image lacks them.
    for dir in ["proc", "dev", "tmp", "etc", ".pivot"] {
        let _ = std::fs::create_dir(root.join(dir));
    }

    // Containers see the host's name database: `<app>.ply` entries ply
    // manages plus the host's DNS config. Written to the upperdir, so the
    // image layers stay untouched.
    for file in ["hosts", "resolv.conf"] {
        let _ = std::fs::copy(format!("/etc/{file}"), root.join("etc").join(file));
    }
    // The container's own hostname MUST resolve locally: anything calling
    // getfqdn()/gethostbyname(hostname) otherwise stalls ~5s in DNS
    // (python's http.server does this at bind).
    if let Ok(mut hosts) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("etc/hosts"))
    {
        use std::io::Write;
        let _ = writeln!(hosts, "127.0.0.1\t{}", spec.hostname);
    }

    // Volumes + dev links: bind host dirs over declared paths, pre-pivot
    // while host paths are still reachable in our private mount table.
    for (host_dir, container_path) in &spec.binds {
        let target = root.join(container_path.trim_start_matches('/'));
        std::fs::create_dir_all(&target)
            .map_err(|e| crate::Error::Runtime(format!("volume target {container_path}: {e}")))?;
        nix::mount::mount(
            Some(host_dir),
            &target,
            None::<&str>,
            nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )
        .map_err(|e| {
            crate::Error::Runtime(format!(
                "bind {} -> {container_path}: {e}",
                host_dir.display()
            ))
        })?;
    }

    // /dev nodes before pivot: the bind-mount fallback needs host paths.
    setup_dev_nodes(&root, spec.rootless)?;

    // Rootless: a fresh proc mount in a user ns is refused (EPERM) while
    // the current /proc carries overmounts (binfmt_misc, systemd bits).
    // Our mount table is private — detaching them here touches nothing
    // on the host.
    if spec.rootless {
        if let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") {
            for line in mountinfo.lines() {
                if let Some(mountpoint) = line.split(' ').nth(4) {
                    if mountpoint.starts_with("/proc/") {
                        mount::unmount_detach(Path::new(mountpoint));
                    }
                }
            }
        }
    }

    // proc must mount BEFORE pivot_root: an unprivileged user ns may only
    // mount a new proc while a fully-visible one still exists in the mount
    // namespace (the host's). Reflects the child's fresh pid ns either way.
    mount::mount_proc(&root.join("proc"))?;

    nix::unistd::pivot_root(&root, &root.join(".pivot"))
        .map_err(|e| crate::Error::Runtime(format!("pivot_root: {e}")))?;
    nix::unistd::chdir("/").map_err(|e| crate::Error::Runtime(format!("chdir /: {e}")))?;
    mount::unmount_detach(Path::new("/.pivot"));
    let _ = std::fs::remove_dir("/.pivot");

    setup_dev_mounts(spec.rootless)?;
    mount::mount_tmpfs_flags(
        Path::new("/tmp"),
        "mode=1777",
        nix::mount::MsFlags::MS_NOEXEC | nix::mount::MsFlags::MS_NODEV,
    )?;
    crate::runtime::security::mask_proc()?;

    nix::unistd::sethostname(&spec.hostname)
        .map_err(|e| crate::Error::Runtime(format!("sethostname: {e}")))?;

    if nix::unistd::chdir(&spec.cwd).is_err() {
        // App prefix missing (unusual but legal) — stay at /.
    }

    let argv: Vec<CString> = spec
        .argv
        .iter()
        .map(|a| CString::new(a.as_str()).unwrap())
        .collect();
    let env: Vec<CString> = spec
        .env
        .iter()
        .map(|(k, v)| CString::new(format!("{k}={v}")).unwrap())
        .collect();
    // Resolve argv[0] against the COMPOSED Path ourselves: execvpe would
    // search the *caller's* PATH (both musl and glibc), which is the host's.
    let composed_path = spec
        .env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let resolved = resolve_program(&spec.argv[0], composed_path);
    let program = CString::new(resolved.as_str()).unwrap();

    // Rights stripping LAST — everything above needed privilege.
    if !spec.privileged {
        crate::runtime::security::drop_capabilities(spec.keep_net_bind)?;
        crate::runtime::security::no_new_privs()?;
        crate::runtime::security::apply_seccomp()?;
    }

    // Mini-init: PID 1 in a pid namespace silently drops default-action
    // signals, so a handler-less app (python, shell scripts) would ignore
    // SIGTERM. Instead ply stays as a tiny PID 1 that forwards signals,
    // reaps zombies, and exits with the app's code — the app runs as PID 2
    // with completely normal signal semantics.
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            let e = nix::unistd::execve(&program, &argv, &env).unwrap_err();
            eprintln!(
                "ply: exec {:?} (resolved to {resolved}): {e} — not on the image's PATH, or its interpreter/libc is missing from the layers",
                spec.argv
            );
            std::process::exit(127);
        }
        Ok(nix::unistd::ForkResult::Parent { child }) => Ok(init_loop(child)),
        Err(e) => Err(crate::Error::Runtime(format!("init fork: {e}"))),
    }
}

/// The in-container init: forward TERM/INT/HUP/USR1/USR2/QUIT to the app,
/// reap orphans, exit with the app's status.
fn init_loop(app: nix::unistd::Pid) -> isize {
    use nix::sys::signal::{SigHandler, Signal};
    use nix::sys::wait::{waitpid, WaitStatus};

    APP_PID.store(app.as_raw(), std::sync::atomic::Ordering::SeqCst);
    unsafe {
        for sig in [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGHUP,
            Signal::SIGQUIT,
            Signal::SIGUSR1,
            Signal::SIGUSR2,
        ] {
            let _ = nix::sys::signal::signal(sig, SigHandler::Handler(init_forward));
        }
    }
    loop {
        match waitpid(nix::unistd::Pid::from_raw(-1), None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == app => return code as isize,
            Ok(WaitStatus::Signaled(pid, sig, _)) if pid == app => return 128 + sig as isize,
            Ok(_) => continue,                         // reaped an orphan
            Err(nix::errno::Errno::EINTR) => continue, // signal forwarded
            Err(_) => return 0,
        }
    }
}

static APP_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn init_forward(sig: i32) {
    let pid = APP_PID.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe { nix::libc::kill(pid, sig) };
    }
}

/// PATH lookup inside the container filesystem (post-pivot). Paths with `/`
/// are used as-is (relative ones resolve against the cwd).
pub fn resolve_program(name: &str, path: &str) -> String {
    if name.contains('/') {
        return name.to_string();
    }
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(name);
        if let Ok(meta) = candidate.metadata() {
            if meta.is_file()
                && std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0
            {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    name.to_string() // let execve produce the honest ENOENT
}

/// Minimal /dev, phase 1 (pre-pivot): tmpfs + ~10 nodes. Root uses mknod;
/// rootless bind-mounts the host's nodes (mknod is denied in a user ns).
fn setup_dev_nodes(root: &Path, rootless: bool) -> Result<()> {
    let dev = root.join("dev");
    std::fs::create_dir_all(&dev).ok();
    mount::mount_tmpfs(&dev, "mode=755")?;

    for (name, major, minor) in [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ] {
        let target = dev.join(name);
        if rootless {
            std::fs::write(&target, b"")
                .map_err(|e| crate::Error::Runtime(format!("create /dev/{name} stub: {e}")))?;
            nix::mount::mount(
                Some(&PathBuf::from("/dev").join(name)),
                &target,
                None::<&str>,
                nix::mount::MsFlags::MS_BIND,
                None::<&str>,
            )
            .map_err(|e| crate::Error::Runtime(format!("bind /dev/{name}: {e}")))?;
        } else {
            mknod(
                &target,
                SFlag::S_IFCHR,
                Mode::from_bits_truncate(0o666),
                makedev(major, minor),
            )
            .map_err(|e| crate::Error::Runtime(format!("mknod /dev/{name}: {e}")))?;
        }
    }

    for (link, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("ptmx", "pts/ptmx"),
    ] {
        let _ = std::os::unix::fs::symlink(target, dev.join(link));
    }
    std::fs::create_dir_all(dev.join("pts")).ok();
    std::fs::create_dir_all(dev.join("shm")).ok();
    Ok(())
}

/// Minimal /dev, phase 2 (post-pivot): devpts + shm. devpts is best-effort
/// rootless (some kernels refuse a fresh instance in a user ns).
fn setup_dev_mounts(rootless: bool) -> Result<()> {
    let dev = Path::new("/dev");
    match mount::mount_devpts(&dev.join("pts")) {
        Ok(()) => {}
        Err(e) if rootless => eprintln!("ply: warning: no /dev/pts in rootless mode ({e})"),
        Err(e) => return Err(e),
    }
    mount::mount_tmpfs_flags(
        &dev.join("shm"),
        "mode=1777",
        nix::mount::MsFlags::MS_NOEXEC | nix::mount::MsFlags::MS_NODEV,
    )?;
    Ok(())
}
