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
    /// Read end of the parent's sync pipe: proceed on 1 byte (parent placed
    /// us in the cgroup), abort on EOF (parent failed).
    pub sync_rx: std::os::fd::OwnedFd,
    /// Keep CAP_NET_BIND_SERVICE (a declared port is < 1024).
    pub keep_net_bind: bool,
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

    nix::unistd::pivot_root(&root, &root.join(".pivot"))
        .map_err(|e| crate::Error::Runtime(format!("pivot_root: {e}")))?;
    nix::unistd::chdir("/").map_err(|e| crate::Error::Runtime(format!("chdir /: {e}")))?;
    mount::unmount_detach(Path::new("/.pivot"));
    let _ = std::fs::remove_dir("/.pivot");

    mount::mount_proc(Path::new("/proc"))?;
    setup_dev()?;
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
    crate::runtime::security::drop_capabilities(spec.keep_net_bind)?;
    crate::runtime::security::no_new_privs()?;
    crate::runtime::security::apply_seccomp()?;

    let e = nix::unistd::execve(&program, &argv, &env).unwrap_err();
    Err(crate::Error::Runtime(format!(
        "exec {:?} (resolved to {resolved}): {e} — not on the image's PATH, or its interpreter/libc is missing from the layers",
        spec.argv
    )))
}

/// PATH lookup inside the container filesystem (post-pivot). Paths with `/`
/// are used as-is (relative ones resolve against the cwd).
fn resolve_program(name: &str, path: &str) -> String {
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

/// Minimal /dev: tmpfs + ~10 nodes, per spec.
fn setup_dev() -> Result<()> {
    let dev = Path::new("/dev");
    mount::mount_tmpfs(dev, "mode=755")?;

    let chr = |name: &str, major: u64, minor: u64| -> Result<()> {
        mknod(
            &dev.join(name),
            SFlag::S_IFCHR,
            Mode::from_bits_truncate(0o666),
            makedev(major, minor),
        )
        .map_err(|e| crate::Error::Runtime(format!("mknod /dev/{name}: {e}")))
    };
    chr("null", 1, 3)?;
    chr("zero", 1, 5)?;
    chr("full", 1, 7)?;
    chr("random", 1, 8)?;
    chr("urandom", 1, 9)?;
    chr("tty", 5, 0)?;

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
    mount::mount_devpts(&dev.join("pts"))?;
    mount::mount_tmpfs_flags(
        &dev.join("shm"),
        "mode=1777",
        nix::mount::MsFlags::MS_NOEXEC | nix::mount::MsFlags::MS_NODEV,
    )?;
    Ok(())
}
