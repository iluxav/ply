//! A network namespace ply owns, for rootless runs.
//!
//! Rootful ply puts every instance on the `ply0` bridge and gives it an IP.
//! Rootless cannot: attaching a veth to the HOST's namespace needs real
//! `CAP_NET_ADMIN`, which is exactly what a normal user lacks. The old
//! answer was to share the host's network, and that one decision is where
//! ply's two modes stopped meaning the same thing — apps had to be told
//! which port to bind (`PORT` injection), ports collided with whatever the
//! machine already ran, and `<name>.ply` names did not exist.
//!
//! What a user CAN do is create a namespace and configure it. So ply makes
//! one per stack (per `ply run`, for a lone app) and puts the members
//! inside it: they bind their own natural ports, reach each other on
//! loopback, and never touch a host port. The stack file then means the
//! same thing on a laptop and on a droplet, which was the point.
//!
//! A namespace lives as long as a process is in it, so ply holds one: a
//! forked child that unshares the network, brings `lo` up, and parks. Its
//! `/proc/<pid>/ns/net` is the handle everything else joins through — no
//! bind mounts, nothing to clean up but a pid.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;

use nix::sched::CloneFlags;

use crate::error::{Error, Result};

/// A namespace and the process keeping it alive.
#[derive(Debug)]
pub struct NetNs {
    /// The holder. Killing it releases the namespace.
    pub holder: nix::unistd::Pid,
}

impl NetNs {
    /// Fork a holder that unshares the network namespace, brings `lo` up,
    /// and waits to be killed.
    ///
    /// `lo` matters: a fresh namespace has loopback DOWN, so `127.0.0.1`
    /// does not answer and every member would fail to reach its neighbours
    /// for reasons that look nothing like "the interface is down".
    pub fn create() -> Result<NetNs> {
        // Two pipes: the holder says "my user namespace exists", the parent
        // maps it (only the parent can — see `write_id_maps`), then the
        // holder makes the network namespace and reports.
        let (ready_r, ready_w) =
            nix::unistd::pipe().map_err(|e| Error::Runtime(format!("netns: pipe: {e}")))?;
        let (go_r, go_w) =
            nix::unistd::pipe().map_err(|e| Error::Runtime(format!("netns: pipe: {e}")))?;

        match unsafe { nix::unistd::fork() }
            .map_err(|e| Error::Runtime(format!("netns: fork: {e}")))?
        {
            nix::unistd::ForkResult::Child => {
                drop(ready_r);
                drop(go_w);
                let code = match child_hold(ready_w, go_r) {
                    Ok(()) => 0,
                    Err(_) => 1,
                };
                // the reason already went down the pipe; never unwind
                // never unwind past a fork: the parent's atexit handlers and
                // buffered state are not ours to run
                unsafe { nix::libc::_exit(code) }
            }
            nix::unistd::ForkResult::Parent { child } => {
                drop(ready_w);
                drop(go_r);
                let fail = |child: nix::unistd::Pid, why: String| -> Error {
                    let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                    let _ = nix::sys::wait::waitpid(child, None);
                    Error::Runtime(format!("netns: {why}"))
                };
                // wait for the user namespace, map it, release the holder
                {
                    use std::io::{Read, Write};
                    let mut ready = std::fs::File::from(
                        ready_r
                            .try_clone()
                            .map_err(|e| Error::Runtime(format!("netns: dup: {e}")))?,
                    );
                    let mut byte = [0u8; 1];
                    if !matches!(ready.read(&mut byte), Ok(1)) || byte[0] != b'u' {
                        return Err(fail(
                            child,
                            "the holder never created a user namespace".into(),
                        ));
                    }
                    std::mem::forget(ready); // the real reader is used below
                    if let Err(e) = crate::runtime::run::write_id_maps(child.as_raw()) {
                        return Err(fail(child, format!("mapping the holder: {e}")));
                    }
                    let mut go = std::fs::File::from(go_w);
                    if go.write_all(b"1").is_err() {
                        return Err(fail(
                            child,
                            "the holder died before it could be mapped".into(),
                        ));
                    }
                }
                // The child reports "1" on success, else why it failed —
                // "could not create a namespace" without a reason is the
                // kind of message that costs an afternoon.
                let said = {
                    use std::io::Read;
                    let mut ready = std::fs::File::from(ready_r);
                    let mut said = String::new();
                    let _ = ready.read_to_string(&mut said);
                    said
                };
                if said != "1" {
                    let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                    let _ = nix::sys::wait::waitpid(child, None);
                    let why = if said.is_empty() {
                        "the holder died before reporting".to_string()
                    } else {
                        said
                    };
                    return Err(Error::Runtime(format!(
                        "netns: could not create a network namespace: {why}"
                    )));
                }
                Ok(NetNs { holder: child })
            }
        }
    }

    /// The path other processes join through.
    pub fn path(&self) -> PathBuf {
        ns_path(self.holder.as_raw())
    }

    /// An fd for this namespace, for `setns` in a thread.
    pub fn open(&self) -> Result<OwnedFd> {
        open_ns(self.holder.as_raw())
    }
}

impl Drop for NetNs {
    fn drop(&mut self) {
        // the namespace dies with its last member
        let _ = nix::sys::signal::kill(self.holder, nix::sys::signal::Signal::SIGKILL);
        let _ = nix::sys::wait::waitpid(self.holder, None);
    }
}

/// `/proc/<pid>/ns/net` — the handle to a live namespace.
pub fn ns_path(pid: i32) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/ns/net"))
}

pub fn open_ns(pid: i32) -> Result<OwnedFd> {
    let path = ns_path(pid);
    std::fs::File::open(&path)
        .map(OwnedFd::from)
        .map_err(|source| Error::Io { path, source })
}

/// Put THIS THREAD into `ns`. Per-thread and sticky: dedicate a thread to
/// it rather than expecting to come back.
pub fn enter(ns: &OwnedFd) -> Result<()> {
    nix::sched::setns(ns, CloneFlags::CLONE_NEWNET)
        .map_err(|e| Error::Runtime(format!("joining a network namespace: {e}")))
}

impl NetNs {
    /// Move this process into the holder's USER namespace only.
    ///
    /// This is the key that unlocks the network one. Joining a network
    /// namespace needs CAP_SYS_ADMIN both in the namespace that owns it and
    /// in the caller's own user namespace (`netns_install` checks both), and
    /// an unprivileged process has neither out here. Inside the user
    /// namespace it owns, it has both.
    ///
    /// Only the user namespace, deliberately: the network stays the host's,
    /// so this process and the children it spawns can still resolve names
    /// and fetch images. Each child joins the network namespace itself,
    /// after its downloads and before it launches anything. Children keep
    /// the capability across `execve` because inside here they are uid 0.
    ///
    /// Must be called while single-threaded: `setns(CLONE_NEWUSER)` refuses
    /// a threaded process.
    pub fn enter_user(&self) -> Result<()> {
        let user = std::fs::File::open(format!("/proc/{}/ns/user", self.holder))
            .map(OwnedFd::from)
            .map_err(|e| Error::Runtime(format!("opening the holder's user namespace: {e}")))?;
        nix::sched::setns(&user, CloneFlags::CLONE_NEWUSER)
            .map_err(|e| Error::Runtime(format!("joining the user namespace: {e}")))
    }
}

/// The holder: unshare, raise `lo`, report, park.
fn child_hold(ready: OwnedFd, go: OwnedFd) -> Result<()> {
    use std::io::Write;
    let mut pipe = std::fs::File::from(ready);
    match hold_inner(&mut pipe, go) {
        Ok(()) => {
            let _ = pipe.write_all(b"1");
            drop(pipe);
            // Park. The namespace exists exactly as long as this process does.
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        Err(e) => {
            let _ = pipe.write_all(e.to_string().as_bytes());
            Err(e)
        }
    }
}

fn hold_inner(pipe: &mut std::fs::File, go: OwnedFd) -> Result<()> {
    use std::io::{Read, Write};
    // CLONE_NEWUSER first: an unprivileged process gets CAP_NET_ADMIN over
    // namespaces it creates only by owning a user namespace. It cannot map
    // itself, so it asks the parent to and waits.
    if !crate::paths::is_root() {
        nix::sched::unshare(CloneFlags::CLONE_NEWUSER)
            .map_err(|e| Error::Runtime(format!("unshare user: {e}")))?;
        pipe.write_all(b"u")
            .map_err(|e| Error::Runtime(format!("announcing the user namespace: {e}")))?;
        let mut go = std::fs::File::from(go);
        let mut byte = [0u8; 1];
        match go.read(&mut byte) {
            Ok(1) => {}
            _ => return Err(Error::Runtime("the parent never mapped us".into())),
        }
    }
    nix::sched::unshare(CloneFlags::CLONE_NEWNET)
        .map_err(|e| Error::Runtime(format!("unshare net: {e}")))?;
    loopback_up()
}

/// Bring `lo` up with an ioctl — no iproute2 needed, and this runs in a
/// namespace where PATH may not have it.
fn loopback_up() -> Result<()> {
    use nix::libc::{c_short, ifreq, IFF_UP, SIOCGIFFLAGS, SIOCSIFFLAGS};

    let sock = nix::sys::socket::socket(
        nix::sys::socket::AddressFamily::Inet,
        nix::sys::socket::SockType::Datagram,
        nix::sys::socket::SockFlag::empty(),
        None,
    )
    .map_err(|e| Error::Runtime(format!("netns: socket: {e}")))?;

    let mut req: ifreq = unsafe { std::mem::zeroed() };
    for (i, b) in b"lo".iter().enumerate() {
        req.ifr_name[i] = *b as std::ffi::c_char;
    }
    let fd = sock.as_raw_fd();
    let rc = unsafe { nix::libc::ioctl(fd, SIOCGIFFLAGS as _, &mut req) };
    if rc < 0 {
        return Err(Error::Runtime("netns: reading lo flags".into()));
    }
    unsafe { req.ifr_ifru.ifru_flags |= IFF_UP as c_short };
    let rc = unsafe { nix::libc::ioctl(fd, SIOCSIFFLAGS as _, &req) };
    if rc < 0 {
        return Err(Error::Runtime("netns: bringing lo up".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A namespace to test with, or `None` when this machine forbids
    /// unprivileged user namespaces — which most Ubuntu 24.04+ kernels do
    /// unless the caller is ply's AppArmor-profiled `/usr/local/bin/ply`.
    /// A cargo test binary never is, so these tests skip by default rather
    /// than fail everywhere.
    ///
    /// **They therefore prove nothing unless they run.** Set
    /// `PLY_NETNS_TESTS=1` to make an unavailable namespace a failure — do
    /// that wherever the mechanism is supposed to work.
    fn namespace_for_test() -> Option<NetNs> {
        match NetNs::create() {
            Ok(ns) => Some(ns),
            Err(e) if std::env::var("PLY_NETNS_TESTS").is_ok() => {
                panic!("PLY_NETNS_TESTS is set but namespaces are unavailable: {e}")
            }
            Err(e) => {
                eprintln!("SKIPPED (no unprivileged netns here): {e}");
                None
            }
        }
    }

    /// Two processes handed the same namespace must land in the SAME one —
    /// that is what lets a stack's members reach each other on loopback
    /// while staying invisible to the host.
    #[test]
    fn joiners_share_one_namespace() {
        let Some(ns) = namespace_for_test() else {
            return;
        };
        let inode = |fd: &OwnedFd| -> u64 {
            use std::os::fd::AsFd;
            nix::sys::stat::fstat(fd.as_fd()).expect("fstat").st_ino
        };
        let want = inode(&ns.open().unwrap());

        // a thread that joins reports the same namespace inode as the holder
        let fd = ns.open().unwrap();
        let got = std::thread::spawn(move || {
            enter(&fd).expect("enter");
            let own = std::fs::File::open("/proc/self/ns/net").expect("own ns");
            inode(&OwnedFd::from(own))
        })
        .join()
        .expect("thread");
        assert_eq!(got, want, "joiner must be in the holder's namespace");

        // and the host thread is NOT in it
        let host = std::fs::File::open("/proc/self/ns/net").unwrap();
        assert_ne!(
            inode(&OwnedFd::from(host)),
            want,
            "the caller keeps its own network"
        );
    }

    /// The namespace must be real and isolated: a port bound inside is
    /// invisible outside, which is the whole reason for doing this.
    #[test]
    fn namespace_is_isolated_and_loopback_works() {
        let Ok(ns) = NetNs::create() else {
            // kernels that forbid unprivileged user namespaces cannot run
            // this; the runtime reports that path with a clear error
            eprintln!("skipping: unprivileged netns unavailable here");
            return;
        };
        let fd = ns.open().expect("open ns");

        // a thread joins the namespace and binds a port there
        let handle = std::thread::spawn(move || {
            enter(&fd).expect("enter");
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("lo is up inside");
            let port = l.local_addr().unwrap().port();
            // hold it while the parent looks
            (port, l)
        });
        let (port, _listener) = handle.join().expect("thread");

        // …and the host sees nothing on that port
        assert!(
            std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                std::time::Duration::from_millis(200),
            )
            .is_err(),
            "a port bound inside the namespace must not be reachable outside it"
        );
    }
}
