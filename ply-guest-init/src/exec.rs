//! Running commands inside the instance, beside the app.
//!
//! This is what `ply exec` reaches. The host sends `{"exec":…}` on the
//! control channel, this module forks the command, streams its two output
//! streams back tagged with the request's id, and reports the exit status.
//! Several commands may run at once; each is a thread and an id.
//!
//! # Why the exits come through here rather than from `waitpid`
//!
//! PID 1 reaps every child, and it does so in one place: the boot path's
//! `wait_for`, which loops on `waitpid(-1)` until the app's own pid comes
//! back. An exec thread calling `waitpid(pid)` for itself would be racing
//! that loop, and losing the race means `ECHILD` and a command whose exit
//! code is gone forever — intermittently, under load, which is the worst
//! way to find a bug.
//!
//! So there is exactly one reaper, the one that already existed, and it
//! hands anything that is not the app to [`note_exit`]. The exec thread
//! waits for its own pid to appear rather than waiting on the process.
//!
//! # What a command inherits
//!
//! The instance's environment, workdir and user — the same three things
//! the app itself got, because "inside the instance" has to mean the same
//! thing for a command as it does for the entrypoint. The request may add
//! environment entries and name a different directory; it cannot ask for a
//! different user, because there is no path by which a caller who could not
//! already run as root should gain one.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::fd::FromRawFd;
use std::sync::{Condvar, Mutex};

use ply_vm_proto::{
    b64_decode, b64_encode, ExecDone, ExecOutput, ExecRequest, GuestLine, EXEC_CHUNK,
    STREAM_STDERR, STREAM_STDOUT,
};

use crate::control::Control;

/// What every exec'd command inherits from the instance.
pub struct Context {
    pub env: Vec<(String, String)>,
    pub workdir: String,
    /// `(uid, gid)`, or none for an instance that runs as root.
    pub user: Option<(u32, u32)>,
}

/// A command that is running now: enough to feed it and to kill it.
struct Running {
    pid: i32,
    /// The write end of its standard input, until `eof` closes it.
    stdin: Option<i32>,
}

static RUNNING: Mutex<BTreeMap<u64, Running>> = Mutex::new(BTreeMap::new());

/// Exit statuses the reaper collected, by pid, for the thread that wants one.
static REAPED: Mutex<BTreeMap<i32, i32>> = Mutex::new(BTreeMap::new());
static REAPED_READY: Condvar = Condvar::new();

/// The single reaper saw a child that was not the app. Hand it over.
///
/// Called from the boot path's `waitpid(-1)` loop, which is the only place
/// in this program that reaps.
pub fn note_exit(pid: i32, raw_status: i32) {
    if let Ok(mut reaped) = REAPED.lock() {
        reaped.insert(pid, raw_status);
    }
    REAPED_READY.notify_all();
}

/// Block until the reaper reports `pid`, and give back its raw status.
fn wait_status(pid: i32) -> i32 {
    let Ok(mut reaped) = REAPED.lock() else {
        return 0;
    };
    loop {
        if let Some(status) = reaped.remove(&pid) {
            return status;
        }
        let Ok(next) = REAPED_READY.wait(reaped) else {
            return 0;
        };
        reaped = next;
    }
}

/// A command's exit code as a caller expects it: its own status, or
/// `128 + signal` when a signal ended it. The same rule the boot path uses
/// for the app.
fn exit_code(raw_status: i32) -> i32 {
    if libc::WIFEXITED(raw_status) {
        libc::WEXITSTATUS(raw_status)
    } else if libc::WIFSIGNALED(raw_status) {
        128 + libc::WTERMSIG(raw_status)
    } else {
        255
    }
}

fn is_executable_file(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The environment a command runs with: the instance's, then the request's
/// additions, which win on a repeated name.
fn merged_env(ctx: &Context, extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ctx.env.clone();
    for (key, value) in extra {
        match env.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value.clone(),
            None => env.push((key.clone(), value.clone())),
        }
    }
    env
}

/// Start `req`, and report it to the host until it ends.
///
/// The fork happens HERE, on the caller's thread, and the command is in
/// `RUNNING` before this returns. That ordering is the whole point: the
/// control channel delivers lines in order, so a `{"stdin":…}` sent
/// immediately after the request would otherwise race the fork and be
/// dropped — and losing the `eof` with it means anything reading to
/// end-of-input hangs forever, which is exactly what `echo x | ply exec
/// app cat` did. Only the waiting and the output pumping go on threads.
pub fn start(control: Control, ctx: &Context, req: ExecRequest) {
    let env = merged_env(ctx, &req.env);
    let cwd = req.cwd.clone().unwrap_or_else(|| ctx.workdir.clone());
    if let Err((code, why)) = run(&control, req.id, &req.argv, &env, &cwd, ctx.user) {
        finish(&control, req.id, code, Some(why));
    }
}

/// Tell the host a command ended, and forget it.
fn finish(control: &Control, id: u64, code: i32, error: Option<String>) {
    if let Ok(mut running) = RUNNING.lock() {
        if let Some(entry) = running.remove(&id) {
            if let Some(fd) = entry.stdin {
                // SAFETY: an fd this module opened and nobody else closes.
                unsafe { libc::close(fd) };
            }
        }
    }
    control.send(&GuestLine::Done {
        done: ExecDone { id, code, error },
    });
}

/// Everything from the fork to the exit report. `Err` means the command
/// never started, with the code and the reason to report.
fn run(
    control: &Control,
    id: u64,
    argv: &[String],
    env: &[(String, String)],
    cwd: &str,
    user: Option<(u32, u32)>,
) -> Result<(), (i32, String)> {
    let Some(program) = argv.first() else {
        return Err((127, "no command was given".into()));
    };
    let path_env = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let resolved = crate::spec::resolve_program(program, path_env, is_executable_file);

    // Everything that allocates happens before the fork: between fork and
    // execve only async-signal-safe calls are legal, and an allocator lock
    // held by another thread at the moment of the fork would deadlock the
    // child forever.
    let cstring = |s: &str| {
        std::ffi::CString::new(s).map_err(|_| (127, format!("{s:?} contains a NUL byte")))
    };
    let prog_c = cstring(&resolved)?;
    let cwd_c = cstring(cwd)?;
    let argv_c = argv
        .iter()
        .map(|a| cstring(a))
        .collect::<Result<Vec<_>, _>>()?;
    let env_c = env
        .iter()
        .map(|(k, v)| cstring(&format!("{k}={v}")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut argv_p: Vec<*const libc::c_char> = argv_c.iter().map(|a| a.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut env_p: Vec<*const libc::c_char> = env_c.iter().map(|e| e.as_ptr()).collect();
    env_p.push(std::ptr::null());

    let (stdin_r, stdin_w) = pipe().map_err(|e| (126, format!("stdin pipe: {e}")))?;
    let (stdout_r, stdout_w) = pipe().map_err(|e| (126, format!("stdout pipe: {e}")))?;
    let (stderr_r, stderr_w) = pipe().map_err(|e| (126, format!("stderr pipe: {e}")))?;

    // SAFETY: the child calls only async-signal-safe functions, on pointers
    // and descriptors prepared above.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let e = std::io::Error::last_os_error();
        for fd in [stdin_r, stdin_w, stdout_r, stdout_w, stderr_r, stderr_w] {
            unsafe { libc::close(fd) };
        }
        return Err((126, format!("fork: {e}")));
    }
    if pid == 0 {
        // SAFETY: every call below is async-signal-safe.
        unsafe {
            libc::dup2(stdin_r, 0);
            libc::dup2(stdout_w, 1);
            libc::dup2(stderr_w, 2);
            for fd in [stdin_r, stdin_w, stdout_r, stdout_w, stderr_r, stderr_w] {
                if fd > 2 {
                    libc::close(fd);
                }
            }
            if let Some((uid, gid)) = user {
                let gids = [gid];
                if libc::setgroups(1, gids.as_ptr()) != 0
                    || libc::setgid(gid) != 0
                    || libc::setuid(uid) != 0
                {
                    libc::_exit(126);
                }
            }
            // The same clamp the app gets: a setuid binary in the image
            // cannot be a way up from a command a caller asked for.
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
            libc::chdir(cwd_c.as_ptr());
            libc::execve(prog_c.as_ptr(), argv_p.as_ptr(), env_p.as_ptr());
            libc::_exit(127);
        }
    }

    // Parent: the child's ends are the child's.
    // SAFETY: descriptors this function created and has not handed out.
    unsafe {
        libc::close(stdin_r);
        libc::close(stdout_w);
        libc::close(stderr_w);
    }
    if let Ok(mut running) = RUNNING.lock() {
        running.insert(
            id,
            Running {
                pid,
                stdin: Some(stdin_w),
            },
        );
    }

    let pumps = [(stdout_r, STREAM_STDOUT), (stderr_r, STREAM_STDERR)].map(|(fd, stream)| {
        let control = control.clone();
        std::thread::spawn(move || pump(control, id, stream, fd))
    });

    // The wait goes on a thread of its own so that `start` returns: the
    // control pump has to get back to reading, or this command's own stdin
    // never arrives.
    let control = control.clone();
    std::thread::spawn(move || {
        let status = wait_status(pid);
        // Joined before the exit is reported, so `done` really is the last
        // word about this id and a caller can stop reading when it sees one.
        for pump in pumps {
            let _ = pump.join();
        }
        finish(&control, id, exit_code(status), None);
    });
    Ok(())
}

/// One of the command's output streams, to the host, in chunks.
fn pump(control: Control, id: u64, stream: u8, fd: i32) {
    // SAFETY: the read end of a pipe this module owns; `File` closes it.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut buf = vec![0u8; EXEC_CHUNK];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => control.send(&GuestLine::Output {
                output: ExecOutput {
                    id,
                    stream,
                    data: b64_encode(&buf[..n]),
                },
            }),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
    }
}

fn pipe() -> std::io::Result<(i32, i32)> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` is a live array of two ints, which is what pipe(2) fills.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

/// Feed a running command's standard input. `eof` closes it, which is the
/// only way anything reading to end-of-input ever finishes.
pub fn stdin(id: u64, data: &str, eof: bool) {
    let Ok(mut running) = RUNNING.lock() else {
        return;
    };
    let Some(entry) = running.get_mut(&id) else {
        return;
    };
    let Some(fd) = entry.stdin else {
        return;
    };
    if !data.is_empty() {
        if let Some(bytes) = b64_decode(data) {
            let mut at = 0;
            while at < bytes.len() {
                // SAFETY: a pipe descriptor this module owns, and a slice
                // that outlives the call.
                let wrote = unsafe {
                    libc::write(
                        fd,
                        bytes[at..].as_ptr() as *const libc::c_void,
                        bytes.len() - at,
                    )
                };
                if wrote <= 0 {
                    break;
                }
                at += wrote as usize;
            }
        }
    }
    if eof {
        // SAFETY: closing an fd this module owns, once — `stdin` is cleared
        // so a second `eof` cannot close it again.
        unsafe { libc::close(fd) };
        entry.stdin = None;
    }
}

/// Signal a running command, by the same names `HostLine::Signal` uses.
/// This is how a caller enforces a timeout on something it started.
pub fn kill(id: u64, name: &str) {
    let Some(signal) = crate::boot::signal_number(name) else {
        return;
    };
    if let Ok(running) = RUNNING.lock() {
        if let Some(entry) = running.get(&id) {
            // SAFETY: a pid this module forked and has not yet reaped.
            unsafe { libc::kill(entry.pid, signal) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("HOME".into(), "/root".into()),
            ],
            workdir: "/opt/app".into(),
            user: None,
        }
    }

    #[test]
    fn a_request_adds_to_the_instances_environment_and_overrides_by_name() {
        let env = merged_env(
            &ctx(),
            &[("HOME".into(), "/tmp".into()), ("EXTRA".into(), "1".into())],
        );
        // The instance's PATH survives: a command with no PATH could not
        // resolve its own program.
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.as_str()),
            Some("/usr/bin:/bin")
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "HOME")
                .map(|(_, v)| v.as_str()),
            Some("/tmp"),
            "the request wins on a name the instance also sets"
        );
        assert!(env.iter().any(|(k, v)| k == "EXTRA" && v == "1"));
        assert_eq!(env.len(), 3, "an override replaces, it does not append");
    }

    #[test]
    fn an_exit_code_follows_the_shells_conventions() {
        // Same encoding the boot path uses for the app, so a caller reads
        // one rule for both.
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(7 << 8), 7);
        assert_eq!(exit_code(libc::SIGTERM), 128 + libc::SIGTERM);
    }

    #[test]
    fn the_reaper_hands_a_status_to_the_thread_that_wants_it() {
        // The race this module exists to avoid: the status arrives from the
        // one reaper, not from a `waitpid` of our own.
        let waiter = std::thread::spawn(|| wait_status(4242));
        // Delivered after the waiter is already blocked, which is the
        // ordering that matters.
        while REAPED.lock().map(|r| r.is_empty()).unwrap_or(false) {
            note_exit(4242, 3 << 8);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(exit_code(waiter.join().expect("the waiter returns")), 3);
    }
}
