//! `ply exec <app> <cmd>` — run a command inside a running instance.
//!
//! setns() into the instance's namespaces, join its cgroup, apply the same
//! rights stripping the app got, then exec. No daemon: the state file has
//! the pid, /proc has the namespaces.

use std::ffi::CString;
use std::os::fd::AsFd;

use nix::sched::CloneFlags;
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::ForkResult;

use crate::error::{Error, Result};
use crate::runtime::state::{self, InstanceState};

/// The process the container's init started — the app itself.
fn first_child(pid: i32) -> Option<i32> {
    let kids = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    kids.split_whitespace().next()?.parse().ok()
}

/// The user namespace that owns `ns`, via `NS_GET_USERNS`. The kernel is
/// the only thing that knows this — walking /proc guesses.
fn owning_userns(ns: &std::fs::File) -> Option<std::fs::File> {
    // include/uapi/linux/nsfs.h: NSIO 0xb7, NS_GET_USERNS _IO(NSIO, 0x1)
    const NS_GET_USERNS: u64 = 0xb701;
    use std::os::fd::AsRawFd;
    let fd = unsafe { nix::libc::ioctl(ns.as_raw_fd(), NS_GET_USERNS as _) };
    if fd < 0 {
        return None;
    }
    use std::os::fd::FromRawFd;
    Some(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// Whether `ns` is the user namespace this thread already belongs to.
/// Namespace files are anonymous nsfs inodes, so identity is (dev, ino) —
/// the same pairing `readlink /proc/self/ns/user` prints.
fn is_own_userns(ns: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata("/proc/self/ns/user"), ns.metadata()) {
        (Ok(ours), Ok(theirs)) => ours.dev() == theirs.dev() && ours.ino() == theirs.ino(),
        _ => false,
    }
}

pub fn exec(target: &str, cmd: &[String]) -> Result<i32> {
    let instance = find_instance(target)?;
    let pid = instance.pid;

    // Rootless instances live in a user namespace we must join FIRST (it
    // grants the capabilities for the other setns calls). Root instances
    // have no user ns — skip.
    let target_userns = std::fs::read_link(format!("/proc/{pid}/ns/user")).ok();
    let own_userns = std::fs::read_link("/proc/self/ns/user").ok();
    if target_userns == own_userns && !nix::unistd::geteuid().is_root() {
        return Err(Error::Runtime(
            "ply exec needs root for root-started instances — try `sudo ply exec …`".into(),
        ));
    }

    // Join the instance's cgroup before entering namespaces (limits apply
    // to exec'd commands too).
    let cgroup_procs = format!(
        "/sys/fs/cgroup/ply-{}.{}/cgroup.procs",
        instance.app, instance.n
    );
    let _ = std::fs::write(cgroup_procs, std::process::id().to_string());

    // Open all namespace fds first (paths vanish once we switch mnt ns).
    let ns = |name: &str| -> Result<std::fs::File> {
        let path = format!("/proc/{pid}/ns/{name}");
        std::fs::File::open(&path).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    };
    let (ipc, uts, net, pidns, mnt) = (ns("ipc")?, ns("uts")?, ns("net")?, ns("pid")?, ns("mnt")?);
    let userns = if target_userns != own_userns {
        Some(ns("user")?)
    } else {
        None
    };

    // Skip namespaces the target shares with us (rootless instances use the
    // host netns; setns onto a namespace you're already in is EPERM unless
    // you own it).
    let same_ns = |name: &str| -> bool {
        let ours = std::fs::read_link(format!("/proc/self/ns/{name}")).ok();
        let theirs = std::fs::read_link(format!("/proc/{pid}/ns/{name}")).ok();
        ours.is_some() && ours == theirs
    };
    let join = |file: &std::fs::File, flag: CloneFlags, what: &str| -> Result<()> {
        nix::sched::setns(file.as_fd(), flag)
            .map_err(|e| Error::Runtime(format!("setns {what}: {e}")))
    };
    // The network namespace may be owned by an ANCESTOR of the instance's
    // own user namespace — a stack's members share one network that `ply up`
    // created, one level up. Joining a network namespace needs CAP_SYS_ADMIN
    // in its owner AND in our current user namespace, and a process in a
    // descendant has neither over its parent. So enter the network's OWNER
    // first (the kernel will name it for us), then the network, and only
    // then the instance's own user namespace, where we regain full rights.
    // ... unless that owner is the user namespace we are already in, which
    // is what a root-started instance gives us: its own netns, but the
    // initial user namespace. Joining your own is EINVAL, not a no-op.
    if !same_ns("net") {
        if let Some(owner) = owning_userns(&net).filter(|o| !is_own_userns(o)) {
            join(&owner, CloneFlags::CLONE_NEWUSER, "the network's user")?;
        }
        join(&net, CloneFlags::CLONE_NEWNET, "net")?;
    }
    if let Some(userns) = &userns {
        join(userns, CloneFlags::CLONE_NEWUSER, "user")?;
    }

    // The app's own environment — its composed PATH above all, which is how
    // `ply exec app node …` finds node in /opt/node-<version>/bin.
    //
    // Not from `pid`: that is the container's init, which is ply itself
    // (it forks the app rather than exec'ing it, so it can tee the logs),
    // and its environment is ply's own. The composed env belongs to the
    // process it started. Read it AFTER joining the user namespace — before
    // that the instance runs as a mapped uid we may not read — and BEFORE
    // the mount namespace, while /proc still describes the host's pids.
    let app_pid = first_child(pid).unwrap_or(pid);
    let environ = std::fs::read(format!("/proc/{app_pid}/environ")).unwrap_or_default();
    if environ.is_empty() {
        eprintln!(
            "ply exec: could not read {}.{}'s environment — commands run with a default PATH",
            instance.app, instance.n
        );
    }
    let env: Vec<CString> = environ
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| CString::new(s.to_vec()).ok())
        .collect();
    if !same_ns("ipc") {
        join(&ipc, CloneFlags::CLONE_NEWIPC, "ipc")?;
    }
    if !same_ns("uts") {
        join(&uts, CloneFlags::CLONE_NEWUTS, "uts")?;
    }
    if !same_ns("pid") {
        join(&pidns, CloneFlags::CLONE_NEWPID, "pid")?; // children enter the pidns
    }
    join(&mnt, CloneFlags::CLONE_NEWNS, "mnt")?;

    // Fork so the child is actually inside the pid namespace.
    match unsafe { nix::unistd::fork() }.map_err(|e| Error::Runtime(format!("fork: {e}")))? {
        ForkResult::Child => {
            let code = child_exec(&instance, cmd, env);
            std::process::exit(code);
        }
        ForkResult::Parent { child } => loop {
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => return Ok(code),
                Ok(WaitStatus::Signaled(_, sig, _)) => return Ok(128 + sig as i32),
                Ok(_) => continue,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(Error::Runtime(format!("waitpid: {e}"))),
            }
        },
    }
}

fn child_exec(instance: &InstanceState, cmd: &[String], env: Vec<CString>) -> i32 {
    // Same clamps as the app itself — an exec'd shell is not a side door.
    let clamps = || -> Result<()> {
        crate::runtime::ns::security::drop_capabilities(&[])?;
        crate::runtime::ns::security::no_new_privs()?;
        crate::runtime::ns::security::apply_seccomp()
    };
    if let Err(e) = clamps() {
        eprintln!("ply exec: {e}");
        return 126;
    }

    if nix::unistd::chdir(format!("/opt/{}", instance.app).as_str()).is_err() {
        let _ = nix::unistd::chdir("/");
    }

    let path = env
        .iter()
        .filter_map(|e| e.to_str().ok())
        .find_map(|e| e.strip_prefix("PATH="))
        .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .to_string();
    let resolved = crate::runtime::ns::container::resolve_program(&cmd[0], &path);
    let argv: Vec<CString> = cmd
        .iter()
        .map(|a| CString::new(a.as_str()).unwrap())
        .collect();
    let program = CString::new(resolved).unwrap();
    let e = nix::unistd::execve(&program, &argv, &env).unwrap_err();
    eprintln!("ply exec: {:?}: {e}", cmd);
    127
}

/// `<app>` (first live instance) or `<app>.<n>` (exact).
fn find_instance(target: &str) -> Result<InstanceState> {
    let states = state::list()?;
    let exact: Option<&InstanceState> = target.rsplit_once('.').and_then(|(app, n)| {
        let n: u32 = n.parse().ok()?;
        states.iter().find(|s| s.app == app && s.n == n)
    });
    let found = exact
        .or_else(|| states.iter().find(|s| s.app == target && s.alive()))
        .cloned();
    found.filter(|s| s.alive()).ok_or_else(|| {
        let running: Vec<String> = states
            .iter()
            .filter(|s| s.alive())
            .map(|s| format!("{}.{}", s.app, s.n))
            .collect();
        Error::Runtime(format!(
            "no running instance matches `{target}` — running: [{}]",
            running.join(", ")
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A root-started instance shares the initial user namespace with us,
    /// yet lives in its own netns — so `NS_GET_USERNS` on that netns names
    /// the namespace we are already in. `setns(CLONE_NEWUSER)` onto your own
    /// user namespace is EINVAL, so exec must recognise and skip it.
    #[test]
    fn own_user_namespace_is_recognised() {
        let ours = std::fs::File::open("/proc/self/ns/user").unwrap();
        assert!(is_own_userns(&ours));
    }

    /// A different namespace must not be mistaken for ours — otherwise the
    /// rootless path would skip a join it genuinely needs.
    #[test]
    fn a_foreign_namespace_is_not_ours() {
        let other = std::fs::File::open("/proc/self/ns/net").unwrap();
        assert!(!is_own_userns(&other));
    }
}
