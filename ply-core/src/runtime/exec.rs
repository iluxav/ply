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

pub fn exec(target: &str, cmd: &[String]) -> Result<i32> {
    if !nix::unistd::geteuid().is_root() {
        return Err(Error::Runtime(
            "ply exec needs root for now — try `sudo ply exec …`".into(),
        ));
    }
    let instance = find_instance(target)?;
    let pid = instance.pid;

    // Join the instance's cgroup before entering namespaces (limits apply
    // to exec'd commands too).
    let cgroup_procs = format!(
        "/sys/fs/cgroup/ply-{}.{}/cgroup.procs",
        instance.app, instance.n
    );
    let _ = std::fs::write(cgroup_procs, std::process::id().to_string());

    // Read the container's env before switching mount namespaces.
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
    let env: Vec<CString> = environ
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| CString::new(s.to_vec()).ok())
        .collect();

    // Open all namespace fds first (paths vanish once we switch mnt ns).
    let ns = |name: &str| -> Result<std::fs::File> {
        let path = format!("/proc/{pid}/ns/{name}");
        std::fs::File::open(&path).map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    };
    let (ipc, uts, net, pidns, mnt) = (ns("ipc")?, ns("uts")?, ns("net")?, ns("pid")?, ns("mnt")?);

    let join = |file: &std::fs::File, flag: CloneFlags, what: &str| -> Result<()> {
        nix::sched::setns(file.as_fd(), flag)
            .map_err(|e| Error::Runtime(format!("setns {what}: {e}")))
    };
    join(&ipc, CloneFlags::CLONE_NEWIPC, "ipc")?;
    join(&uts, CloneFlags::CLONE_NEWUTS, "uts")?;
    join(&net, CloneFlags::CLONE_NEWNET, "net")?;
    join(&pidns, CloneFlags::CLONE_NEWPID, "pid")?; // children enter the pidns
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
        crate::runtime::security::drop_capabilities(false)?;
        crate::runtime::security::no_new_privs()?;
        crate::runtime::security::apply_seccomp()
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
    let resolved = crate::runtime::container::resolve_program(&cmd[0], &path);
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
