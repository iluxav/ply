//! `ply exec` into a microVM.
//!
//! The Linux backend enters an instance with `setns`: the instance is a
//! process on the same machine, so there is a namespace to step into. A
//! microVM is a different machine with its own kernel, and there is no
//! process on the host to enter at all. The only ways in are the wires the
//! VMM built at boot, and one of them — the control channel — already
//! carries messages in both directions.
//!
//! So this is a client, not a syscall. It connects to the socket the
//! instance's worker listens on, sends one exec request, and relays what
//! comes back: the command's two output streams, kept apart, and its exit
//! code. The worker forwards the request over the control channel; the
//! guest's init forks the command beside the app and streams it back.
//!
//! # What it does not do
//!
//! No pseudo-terminal. The microVM kernel is built without one — there is
//! no `/dev/pts` in the guest — so an interactive shell has nothing to sit
//! on. A command runs with pipes, which is what a script, a CI step or an
//! agent wants; `sh -c '…'` covers the rest.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use ply_vm_proto::{
    b64_decode, b64_encode, host_line, parse_guest_line, ExecRequest, ExecStdin, GuestLine,
    HostLine, EXEC_CHUNK, STREAM_STDERR,
};

use crate::error::{Error, Result};
use crate::runtime::state;

/// Where an instance's worker listens.
fn socket_path(app: &str, n: u32) -> PathBuf {
    crate::paths::run_dir()
        .join("instances")
        .join(format!("{app}.{n}"))
        .join(super::worker::EXEC_SOCKET)
}

/// Run `cmd` inside the instance `target` names, and give back its exit
/// code. Output goes to this process's own stdout and stderr, kept apart.
pub fn exec(target: &str, cmd: &[String]) -> Result<i32> {
    let instance = state::find(target)?;
    let path = socket_path(&instance.app, instance.n);
    let stream = UnixStream::connect(&path).map_err(|e| {
        Error::Runtime(format!(
            "{}.{} is not accepting commands ({e}) — `ply exec` needs an instance started by \
             this version of ply on the microVM runtime",
            instance.app, instance.n
        ))
    })?;

    let request = HostLine::Exec {
        exec: ExecRequest {
            // The worker allocates the real one: two `ply exec` processes
            // know nothing of each other, and a collision would cross two
            // commands' output.
            id: 0,
            argv: cmd.to_vec(),
            env: Vec::new(),
            cwd: None,
        },
    };
    let mut writer = stream
        .try_clone()
        .map_err(|e| Error::Runtime(format!("the exec connection: {e}")))?;
    writer
        .write_all(host_line(&request).as_bytes())
        .and_then(|_| writer.flush())
        .map_err(|e| Error::Runtime(format!("sending the command: {e}")))?;

    // Forward this process's stdin, but only when it is not a terminal.
    // Piped input is the case that matters (`echo x | ply exec app cat`);
    // a terminal would just hold a read open that nothing ever ends, and
    // without a pty in the guest there is no interactive session to have.
    if !std::io::stdin().is_terminal() {
        let mut writer = writer;
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = vec![0u8; EXEC_CHUNK];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let line = host_line(&HostLine::Stdin {
                            stdin: ExecStdin {
                                id: 0,
                                data: b64_encode(&buf[..n]),
                                eof: false,
                            },
                        });
                        if writer.write_all(line.as_bytes()).is_err() {
                            return;
                        }
                    }
                }
            }
            let eof = host_line(&HostLine::Stdin {
                stdin: ExecStdin {
                    id: 0,
                    data: String::new(),
                    eof: true,
                },
            });
            let _ = writer.write_all(eof.as_bytes());
        });
    }

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    for line in std::io::BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        match parse_guest_line(&line) {
            Some(GuestLine::Output { output }) => {
                let Some(bytes) = b64_decode(&output.data) else {
                    continue;
                };
                // Written through as bytes, never as text: a command's
                // output is whatever it wrote, and re-encoding it would
                // corrupt anything that is not UTF-8.
                if output.stream == STREAM_STDERR {
                    let _ = stderr.write_all(&bytes);
                    let _ = stderr.flush();
                } else {
                    let _ = stdout.write_all(&bytes);
                    let _ = stdout.flush();
                }
            }
            Some(GuestLine::Done { done }) => {
                if let Some(error) = done.error {
                    return Err(Error::Runtime(format!(
                        "{}: {error}",
                        cmd.first().map(String::as_str).unwrap_or("the command")
                    )));
                }
                return Ok(done.code);
            }
            _ => {}
        }
    }
    Err(Error::Runtime(format!(
        "{}.{} stopped before the command finished",
        instance.app, instance.n
    )))
}
