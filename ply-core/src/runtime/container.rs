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
    /// Declared volume paths, to hand to `run_user` from inside the user
    /// namespace. Not --link: that is the caller's own working tree.
    pub volume_targets: Vec<String>,
    /// Read end of the parent's sync pipe: proceed on 1 byte (parent placed
    /// us in the cgroup), abort on EOF (parent failed).
    pub sync_rx: std::os::fd::OwnedFd,
    /// Capabilities the app keeps after stripping. Empty for ply-native
    /// packages; Docker's fourteen for `capabilities = "oci"` imports.
    pub keep_caps: Vec<caps::Capability>,
    /// Skip rights stripping entirely — ONLY for `ply craft` authoring
    /// sessions, where installing packages needs real root inside.
    pub privileged: bool,
    /// Running in a user namespace as an unprivileged user: /dev nodes are
    /// bind-mounted from the host (mknod is denied), devpts is best-effort.
    pub rootless: bool,
    /// Names that must resolve to loopback inside this container: the other
    /// members of a shared namespace. Rootful writes `<app>.ply` into the
    /// host's /etc/hosts against real bridge IPs; sharing a namespace, the
    /// members ARE loopback, so the same names are written here instead.
    pub local_aliases: Vec<String>,
    /// Switch to this user before exec ([package] user = "name:uid:gid").
    pub run_user: Option<crate::manifest::RunUser>,
    /// Write end of the parent's log tee: stdout+stderr are redirected here
    /// so the parent can copy the stream to its own stdout AND the bounded
    /// log ring. None = inherit (craft shells stay interactive).
    pub log_fd: Option<std::os::fd::OwnedFd>,
}

/// Child entry point. Never returns on success (execve).
pub fn child_main(spec: &ContainerSpec) -> isize {
    // First thing: route our own output through the parent's tee, so even
    // pre-exec setup errors land in the log ring.
    if let Some(fd) = &spec.log_fd {
        let _ = nix::unistd::dup2_stdout(fd);
        let _ = nix::unistd::dup2_stderr(fd);
    }
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
    let _ = std::fs::copy("/etc/hosts", root.join("etc/hosts"));
    let host_resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let upstream = std::fs::read_to_string("/run/systemd/resolve/resolv.conf").ok();
    let (resolv, warning) =
        resolv_conf_for_instance(&host_resolv, upstream.as_deref(), spec.rootless);
    if let Some(w) = warning {
        eprintln!("ply: warning: {w}");
    }
    let _ = std::fs::write(root.join("etc/resolv.conf"), resolv);
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
        // Siblings sharing this namespace: `db.ply` means the same thing
        // here as it does on a bridge, which is what lets one stack file
        // serve a laptop and a droplet.
        for alias in &spec.local_aliases {
            let _ = writeln!(hosts, "127.0.0.1\t{alias}.ply");
        }
    }

    // A declared run user gets a passwd/group entry (getpwuid must work —
    // postgres, sshd and friends insist) and a writable home.
    if let Some(user) = &spec.run_user {
        use std::io::Write;
        if let Ok(mut passwd) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("etc/passwd"))
        {
            let _ = writeln!(
                passwd,
                "{}:x:{}:{}::/home/{}:/bin/sh",
                user.name, user.uid, user.gid, user.name
            );
        }
        if let Ok(mut group) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("etc/group"))
        {
            let _ = writeln!(group, "{}:x:{}:", user.name, user.gid);
        }
        let home = root.join("home").join(&user.name);
        let _ = std::fs::create_dir_all(&home);
        let _ = std::os::unix::fs::chown(&home, Some(user.uid), Some(user.gid));
    }

    // Volumes + dev links: bind host dirs over declared paths, pre-pivot
    // while host paths are still reachable in our private mount table.
    for (host_dir, container_path) in &spec.binds {
        let target = root.join(container_path.trim_start_matches('/'));
        std::fs::create_dir_all(&target)
            .map_err(|e| crate::Error::Runtime(format!("volume target {container_path}: {e}")))?;
        // MS_REC: a source with locked child mounts (host /proc, /sys — they
        // carry overmounts) can only be bound recursively inside a user
        // namespace; a plain bind gets EINVAL. Recursive is also the honest
        // semantic for every other link.
        nix::mount::mount(
            Some(host_dir),
            &target,
            None::<&str>,
            nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| {
            crate::Error::Runtime(format!(
                "bind {} -> {container_path}: {e}",
                host_dir.display()
            ))
        })?;
    }

    // Volumes must belong to the app's user, and rootless that can only
    // happen here: the parent outside is unprivileged and may not chown to
    // another uid, while in this namespace we are root over the mapped range.
    // Without it a `[package] user` app cannot write its own data directory —
    // postgres fails at initdb with EPERM on a directory it appears not to own.
    if let Some(user) = &spec.run_user {
        for target in &spec.volume_targets {
            let path = root.join(target.trim_start_matches('/'));
            if let Err(e) = std::os::unix::fs::chown(&path, Some(user.uid), Some(user.gid)) {
                eprintln!(
                    "ply: warning: cannot give {target} to {} ({e}) — the app may not be able to write it",
                    user.name
                );
            }
        }
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

    // Staying at `/` is a legal fallback for a thin image with no prefix dir,
    // but it is silently wrong for anything whose entrypoint operates on `.`
    // (redis: `find . -exec chown redis {} +` walks the entire rootfs). Say so.
    if let Err(e) = nix::unistd::chdir(&spec.cwd) {
        eprintln!(
            "ply: warning: cannot enter {} ({e}) — running from / instead; \
             set [package] workdir if the entrypoint expects a directory",
            spec.cwd.display()
        );
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

    // Mini-init: PID 1 in a pid namespace silently drops default-action
    // signals, so a handler-less app (python, shell scripts) would ignore
    // SIGTERM. Instead ply stays as a tiny PID 1 that forwards signals,
    // reaps zombies, and exits with the app's code — the app runs as PID 2
    // with completely normal signal semantics.
    //
    // Rights stripping happens INSIDE each branch, after the fork: the app
    // child may first need CAP_SETUID/SETGID to become its [package] user.
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            // Order matters: the bounding-set drop needs CAP_SETPCAP, so it
            // happens while still root (permitted caps survive it — setuid
            // still works). setuid then clears effective/permitted; nnp +
            // seccomp close the rest.
            if !spec.privileged {
                if let Err(e) = crate::runtime::security::drop_capabilities(&spec.keep_caps) {
                    eprintln!("ply: rights stripping failed: {e}");
                    std::process::exit(126);
                }
            }
            if let Some(user) = &spec.run_user {
                let apply = || -> nix::Result<()> {
                    nix::unistd::setgroups(&[nix::unistd::Gid::from_raw(user.gid)])?;
                    nix::unistd::setgid(nix::unistd::Gid::from_raw(user.gid))?;
                    nix::unistd::setuid(nix::unistd::Uid::from_raw(user.uid))?;
                    Ok(())
                };
                if let Err(e) = apply() {
                    eprintln!("ply: cannot switch to user {}: {e}", user.name);
                    std::process::exit(126);
                }
            }
            if !spec.privileged {
                let clamps = || -> Result<()> {
                    crate::runtime::security::no_new_privs()?;
                    crate::runtime::security::apply_seccomp()
                };
                if let Err(e) = clamps() {
                    eprintln!("ply: rights stripping failed: {e}");
                    std::process::exit(126);
                }
            }
            let e = nix::unistd::execve(&program, &argv, &env).unwrap_err();
            eprintln!(
                "ply: exec {:?} (resolved to {resolved}): {e} — not on the image's PATH, or its interpreter/libc is missing from the layers",
                spec.argv
            );
            std::process::exit(127);
        }
        Ok(nix::unistd::ForkResult::Parent { child }) => {
            if !spec.privileged {
                crate::runtime::security::drop_capabilities(&[])?;
                crate::runtime::security::no_new_privs()?;
                crate::runtime::security::apply_seccomp()?;
            }
            Ok(init_loop(child))
        }
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
            // httpd's graceful-stop signal. Default action is ignore, so
            // without a handler here the forward would never happen.
            Signal::SIGWINCH,
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
            // umask clips the mknod mode — non-root apps must still be able
            // to write /dev/null & friends
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o666));
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

/// What the instance's /etc/resolv.conf should say. Rootless shares the
/// host netns, so the host file works as is. A rootful instance has its own
/// netns, where a loopback stub resolver (systemd-resolved's 127.0.0.53) is
/// unreachable — substitute the resolver's real upstreams, keeping the
/// host's search/options lines.
pub fn resolv_conf_for_instance(
    host: &str,
    resolved_upstream: Option<&str>,
    rootless: bool,
) -> (String, Option<String>) {
    fn nameservers(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|l| l.trim().strip_prefix("nameserver"))
            .map(|r| r.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
    fn is_loopback(ns: &str) -> bool {
        ns.parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
    }
    if rootless {
        return (host.to_string(), None);
    }
    let host_ns = nameservers(host);
    if !host_ns.is_empty() && !host_ns.iter().all(|n| is_loopback(n)) {
        return (host.to_string(), None);
    }
    let mut upstream: Vec<String> = Vec::new();
    for n in resolved_upstream.map(nameservers).unwrap_or_default() {
        if !is_loopback(&n) && !upstream.contains(&n) {
            upstream.push(n);
        }
    }
    if upstream.is_empty() {
        return (
            host.to_string(),
            Some(
                "the host's /etc/resolv.conf points at a loopback stub resolver and no upstream was found — instances cannot resolve names; set a real nameserver in /etc/resolv.conf"
                    .into(),
            ),
        );
    }
    let mut out = String::new();
    for n in &upstream {
        out.push_str(&format!("nameserver {n}\n"));
    }
    for line in host.lines() {
        let t = line.trim();
        if t.starts_with("search") || t.starts_with("options") || t.starts_with("domain") {
            out.push_str(line);
            out.push('\n');
        }
    }
    (out, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STUB: &str =
        "# systemd-resolved\nnameserver 127.0.0.53\noptions edns0 trust-ad\nsearch example.com\n";
    const UPSTREAM: &str = "nameserver 10.124.15.254\nnameserver 10.124.15.254\n";
    const REAL: &str = "nameserver 1.1.1.1\nsearch lan\n";

    #[test]
    fn rootless_keeps_the_host_file_verbatim() {
        let (text, warn) = resolv_conf_for_instance(STUB, Some(UPSTREAM), true);
        assert_eq!(text, STUB);
        assert!(warn.is_none());
    }

    #[test]
    fn rootful_with_real_nameservers_keeps_the_host_file() {
        let (text, warn) = resolv_conf_for_instance(REAL, Some(UPSTREAM), false);
        assert_eq!(text, REAL);
        assert!(warn.is_none());
    }

    #[test]
    fn rootful_loopback_stub_is_replaced_by_systemd_resolved_upstreams() {
        let (text, warn) = resolv_conf_for_instance(STUB, Some(UPSTREAM), false);
        assert!(warn.is_none());
        assert!(text.contains("nameserver 10.124.15.254"), "{text}");
        assert!(!text.contains("127.0.0.53"), "{text}");
        assert!(
            text.contains("search example.com"),
            "host search/options survive: {text}"
        );
        assert_eq!(
            text.matches("nameserver").count(),
            1,
            "duplicates collapsed: {text}"
        );
    }

    #[test]
    fn rootful_loopback_stub_without_upstreams_warns() {
        let (text, warn) = resolv_conf_for_instance(STUB, None, false);
        assert_eq!(text, STUB);
        assert!(
            warn.as_deref().unwrap_or("").contains("loopback"),
            "{warn:?}"
        );
        let (_, warn) = resolv_conf_for_instance(STUB, Some("nameserver 127.0.0.1\n"), false);
        assert!(
            warn.is_some(),
            "upstream that is itself loopback is no upstream"
        );
    }
}
