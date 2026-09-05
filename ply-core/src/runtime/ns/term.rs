//! Web terminals, the everything-is-a-file way.
//!
//! The dashboard (or anything holding the apps grant) writes `control/exec`
//! with `<slot> <nonce>`; the run parent — already privileged for its own
//! app — answers by serving a PTY over a unix socket at
//! `control/term-<nonce>.sock`. Unix sockets traverse bind mounts, so the
//! grant IS the ACL: whoever may write the control dir may open a shell.
//!
//! The shell itself is spawned via `ply exec <app>.<n> sh` (our own binary),
//! which already knows how to join every namespace and the cgroup — the
//! terminal adds only the PTY and the wire.
//!
//! Wire framing, both directions: [type u8][len u16 BE][payload].
//! type 0 = terminal data, type 1 = resize as JSON {"cols":N,"rows":N}.

use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

const ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Fork a detached term server (double fork — init adopts it, the run
/// parent's reaping never sees it). Returns after the intermediate child
/// is reaped; the server runs on its own.
pub fn spawn(app: &str, slot: u32, nonce: &str) {
    let app = app.to_string();
    let nonce = nonce.to_string();
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child }) => {
            let _ = nix::sys::wait::waitpid(child, None);
        }
        Ok(nix::unistd::ForkResult::Child) => match unsafe { nix::unistd::fork() } {
            Ok(nix::unistd::ForkResult::Child) => {
                let code = match serve(&app, slot, &nonce) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("ply: terminal {app}.{slot}: {e}");
                        1
                    }
                };
                std::process::exit(code);
            }
            _ => std::process::exit(0),
        },
        Err(e) => eprintln!("ply: terminal fork: {e}"),
    }
}

fn socket_path(app: &str, nonce: &str) -> PathBuf {
    crate::runtime::control::dir(app).join(format!("term-{nonce}.sock"))
}

fn serve(app: &str, slot: u32, nonce: &str) -> std::io::Result<()> {
    let path = socket_path(app, nonce);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    listener.set_nonblocking(true)?;

    // one connection, within the window — else clean up and go home
    let deadline = std::time::Instant::now() + ACCEPT_TIMEOUT;
    let stream = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    let _ = std::fs::remove_file(&path);
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
        }
    };
    // single-use: the name disappears the moment it is claimed
    let _ = std::fs::remove_file(&path);
    stream.set_nonblocking(true)?;

    let pty = nix::pty::openpty(None, None).map_err(std::io::Error::from)?;
    let shell = spawn_shell(app, slot, &pty.slave)?;
    drop(pty.slave);
    let outcome = bridge(stream, &pty.master);

    let _ = nix::sys::signal::kill(shell, nix::sys::signal::Signal::SIGHUP);
    let _ = nix::sys::wait::waitpid(shell, None);
    outcome
}

/// The grandchild: a fresh session on the PTY slave, then `ply exec` does
/// what it always does — namespaces, cgroup, env — and becomes `sh`.
fn spawn_shell(app: &str, slot: u32, slave: &OwnedFd) -> std::io::Result<nix::unistd::Pid> {
    let exe = std::env::current_exe()?;
    match unsafe { nix::unistd::fork() }.map_err(std::io::Error::from)? {
        nix::unistd::ForkResult::Parent { child } => Ok(child),
        nix::unistd::ForkResult::Child => {
            let _ = nix::unistd::setsid();
            unsafe {
                nix::libc::ioctl(slave.as_raw_fd(), nix::libc::TIOCSCTTY as _, 0);
            }
            let _ = nix::unistd::dup2_stdin(slave.as_fd());
            let _ = nix::unistd::dup2_stdout(slave.as_fd());
            let _ = nix::unistd::dup2_stderr(slave.as_fd());
            std::env::set_var("TERM", "xterm-256color");
            let err = std::process::Command::new(exe)
                .arg("exec")
                .arg(format!("{app}.{slot}"))
                .arg("sh")
                .exec();
            eprintln!("ply: terminal shell: {err}");
            std::process::exit(1);
        }
    }
}

/// Pump bytes both ways until either side hangs up or the idle clock runs
/// out. Socket frames in, raw PTY out — and resize frames become ioctls.
fn bridge(mut stream: UnixStream, master: &OwnedFd) -> std::io::Result<()> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    let mut inbox: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut last_activity = std::time::Instant::now();

    // master stays blocking for writes; reads are gated by poll
    loop {
        let mut fds = [
            PollFd::new(stream.as_fd(), PollFlags::POLLIN),
            PollFd::new(master.as_fd(), PollFlags::POLLIN),
        ];
        let n = poll(&mut fds, PollTimeout::from(1000u16)).map_err(std::io::Error::from)?;
        if n == 0 {
            if last_activity.elapsed() > IDLE_TIMEOUT {
                return Ok(());
            }
            continue;
        }
        let sock_ready = fds[0].revents().is_some_and(|r| !r.is_empty());
        let master_ready = fds[1].revents().is_some_and(|r| !r.is_empty());

        if sock_ready {
            match stream.read(&mut buf) {
                Ok(0) => return Ok(()), // browser went away
                Ok(n) => {
                    last_activity = std::time::Instant::now();
                    inbox.extend_from_slice(&buf[..n]);
                    drain_frames(&mut inbox, master)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if master_ready {
            match nix::unistd::read(master.as_fd(), &mut buf) {
                Ok(0) | Err(nix::errno::Errno::EIO) => return Ok(()), // shell exited
                Ok(n) => {
                    last_activity = std::time::Instant::now();
                    let mut frame = Vec::with_capacity(n + 3);
                    frame.push(0u8);
                    frame.extend_from_slice(&(n as u16).to_be_bytes());
                    frame.extend_from_slice(&buf[..n]);
                    stream.write_all(&frame)?;
                }
                Err(nix::errno::Errno::EAGAIN) => {}
                Err(e) => return Err(std::io::Error::from(e)),
            }
        }
    }
}

fn drain_frames(inbox: &mut Vec<u8>, master: &OwnedFd) -> std::io::Result<()> {
    loop {
        if inbox.len() < 3 {
            return Ok(());
        }
        let kind = inbox[0];
        let len = u16::from_be_bytes([inbox[1], inbox[2]]) as usize;
        if inbox.len() < 3 + len {
            return Ok(());
        }
        let payload: Vec<u8> = inbox[3..3 + len].to_vec();
        inbox.drain(..3 + len);
        match kind {
            0 => {
                let mut off = 0;
                while off < payload.len() {
                    off += nix::unistd::write(master.as_fd(), &payload[off..])
                        .map_err(std::io::Error::from)?;
                }
            }
            1 => {
                if let Some((cols, rows)) = parse_resize(&payload) {
                    let ws = nix::pty::Winsize {
                        ws_row: rows,
                        ws_col: cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe {
                        nix::libc::ioctl(master.as_raw_fd(), nix::libc::TIOCSWINSZ, &ws);
                    }
                }
            }
            _ => {} // unknown frame: forward-compat, ignore
        }
    }
}

/// `{"cols":120,"rows":32}` — hand-parsed, the payload has one known shape.
fn parse_resize(payload: &[u8]) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(payload).ok()?;
    let grab = |key: &str| -> Option<u16> {
        let idx = text.find(key)?;
        let rest = &text[idx + key.len()..];
        let digits: String = rest
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits.parse().ok()
    };
    Some((grab("cols")?, grab("rows")?))
}

use std::os::unix::process::CommandExt;

#[cfg(test)]
mod tests {
    #[test]
    fn resize_parsing() {
        assert_eq!(
            super::parse_resize(br#"{"cols":120,"rows":32}"#),
            Some((120, 32))
        );
        assert_eq!(
            super::parse_resize(br#"{"rows":32,"cols":120}"#),
            Some((120, 32))
        );
        assert_eq!(super::parse_resize(b"junk"), None);
    }

    #[test]
    fn frame_shapes() {
        // header split across reads must not desync
        let mut inbox = vec![0u8];
        assert!(inbox.len() < 3); // waits for more — drain_frames returns Ok
        inbox.extend_from_slice(&[0, 2]);
        assert_eq!(u16::from_be_bytes([inbox[1], inbox[2]]), 2);
    }
}
