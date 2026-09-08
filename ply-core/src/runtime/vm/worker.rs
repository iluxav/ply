//! One microVM in a process of its own: `ply __vm-worker <instance dir>`.
//!
//! Hypervisor.framework allows exactly one VM per process, so a `ply run`
//! parent that booted its instances in-process could have one and no more:
//! `--scale 2` died creating the second VM. The parent now spawns this
//! worker per instance and keeps for itself what is shared across them —
//! the switch, the published ports, the state files, the log ring — which
//! also gives every instance a real child pid, the thing `ply deploy` walks
//! up from to find the parent and `ply stop` signals.
//!
//! # The parent link
//!
//! The parent listens on `<instance dir>/control.sock` before it spawns the
//! worker; the worker connects and the two speak newline-delimited text:
//!
//! 1. worker → parent: `attached <ip>` (or `attached none`) once it is on
//!    the switch. The parent needs the address to write the spec disk.
//! 2. parent → worker: `boot`, once the spec disk is on disk.
//! 3. worker → parent: every guest control line, verbatim
//!    (`{"ready":true}`, `{"exit":N}`, `{"publish":…}`).
//! 4. parent → worker: every host control line, verbatim
//!    (`{"signal":"TERM"}`, `{"params":…}`), forwarded into the guest.
//!
//! The guest's console — the app's stdout and stderr — is the worker's own
//! stdout, which the parent reads as the instance's output exactly as it
//! read the in-process pipe before.
//!
//! A signal to the worker is forwarded into the guest by name, so `kill
//! -TERM <worker>` (what `ply stop` and the parent's own handler do) is a
//! polite stop, and only SIGKILL — which never reaches a handler — takes the
//! machine down with the process.
//!
//! The worker's exit code is the guest's, or 255 for a machine that stopped
//! without reporting one.

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use nix::sys::signal::Signal;
use ply_vm_proto::{guest_line, parse_host_line, GuestLine, HostLine};
use serde::{Deserialize, Serialize};

use super::{machine, switch};

/// Where the parent leaves the worker's instructions.
pub const SPEC_FILE: &str = "worker.json";
/// Where the parent listens for the worker.
pub const CONTROL_SOCKET: &str = "control.sock";
/// Where the WORKER listens, for `ply exec`. Beside the instance's other
/// state, so it disappears with the instance directory; reachable by anyone
/// who can already read that directory, which is the same trust boundary
/// the state files and the switch socket sit behind.
pub const EXEC_SOCKET: &str = "exec.sock";
/// The hidden subcommand.
pub const SUBCOMMAND: &str = "__vm-worker";

/// Everything the worker needs, written by the parent as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSpec {
    pub app: String,
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    /// `(path, read_only)`, in attach order.
    pub disks: Vec<(PathBuf, bool)>,
    pub mem_mib: u64,
    pub net: Option<WorkerNet>,
    pub shares: Vec<WorkerShare>,
}

/// The switch to join and the name to join it under.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerNet {
    pub socket: PathBuf,
    pub slot: String,
    pub alias: String,
}

/// A host directory to serve over 9p.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerShare {
    pub tag: String,
    pub root: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

/// The signal a handler last recorded, for the main loop to forward.
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn note_signal(sig: nix::libc::c_int) {
    PENDING_SIGNAL.store(sig, Ordering::SeqCst);
}

/// Run one instance to completion; the return value is the process's exit
/// code.
pub fn run(instance_dir: &Path) -> i32 {
    crate::ignore_sigpipe();
    match run_inner(instance_dir) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ply: microVM worker: {e}");
            255
        }
    }
}

fn run_inner(instance_dir: &Path) -> Result<i32, String> {
    let spec: WorkerSpec = {
        let path = instance_dir.join(SPEC_FILE);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("parsing {}: {e}", path.display()))?
    };

    let socket = instance_dir.join(CONTROL_SOCKET);
    let stream = UnixStream::connect(&socket)
        .map_err(|e| format!("connecting to the parent at {}: {e}", socket.display()))?;
    let mut reader = std::io::BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("cloning the parent link: {e}"))?,
    );
    let writer = Arc::new(Mutex::new(stream));

    // --- the network, first: the parent wants the address before it
    // writes the spec disk -----------------------------------------------
    // The client is kept for the life of the process: the link's frames
    // travel over its socket.
    let mut client: Option<switch::unix::Client> = None;
    let link = match &spec.net {
        Some(net) => match switch::unix::Client::connect(&net.socket) {
            Ok(c) => match c.attach(&net.slot, &net.alias) {
                Ok(link) => {
                    client = Some(c);
                    Some(link)
                }
                Err(e) => {
                    eprintln!(
                        "ply: warning: {}: joining the run's network as {}: {e} — this \
                         instance boots with no network card",
                        spec.app, net.slot
                    );
                    None
                }
            },
            Err(e) => {
                eprintln!(
                    "ply: warning: {}: the run's switch at {} is not answering ({e}) — this \
                     instance boots with no network card",
                    spec.app,
                    net.socket.display()
                );
                None
            }
        },
        None => None,
    };
    let _keep_client = client;
    send(
        &writer,
        &match &link {
            Some(l) => format!("attached {}\n", l.ip),
            None => "attached none\n".to_string(),
        },
    )?;

    // --- wait for the parent's go-ahead ---------------------------------
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("reading the parent's boot line: {e}"))?;
    if line.trim() != "boot" {
        return Err(format!(
            "the parent said {line:?} where `boot` was expected — refusing to start a VM the \
             parent did not ask for"
        ));
    }

    // --- signals: forwarded into the guest by name ----------------------
    for sig in [
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGQUIT,
    ] {
        // SAFETY: the handler only stores an integer into an atomic.
        unsafe {
            let _ =
                nix::sys::signal::signal(sig, nix::sys::signal::SigHandler::Handler(note_signal));
        }
    }

    // --- boot -----------------------------------------------------------
    let mut running = machine::boot(machine::MachineConfig {
        kernel: spec.kernel.clone(),
        initramfs: spec.initramfs.clone(),
        disks: spec
            .disks
            .iter()
            .map(|(path, read_only)| machine::DiskSpec {
                path: path.clone(),
                read_only: *read_only,
            })
            .collect(),
        mem_bytes: spec.mem_mib * 1024 * 1024,
        net: link.map(|l| machine::NetSpec {
            mac: l.mac.0,
            uplink: l.tx,
            downlink: l.rx,
        }),
        shares: spec
            .shares
            .iter()
            .map(|s| machine::ShareSpec {
                tag: s.tag.clone(),
                root: s.root.clone(),
                uid: s.uid,
                gid: s.gid,
            })
            .collect(),
    })?;

    // The console → our stdout, byte for byte.
    if let Some(mut console) = running.take_stdout() {
        std::thread::Builder::new()
            .name("ply-vm-console".into())
            .spawn(move || {
                let mut out = std::io::stdout().lock();
                let _ = std::io::copy(&mut console, &mut out);
                let _ = out.flush();
            })
            .map_err(|e| format!("spawning the console pump: {e}"))?;
    }

    // `ply exec` sessions, by request id: the guest tags a command's output
    // and its exit with the id, and this is how each finds the connection
    // that asked for it.
    let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));

    // Whether this guest can run commands at all. Set when it reports
    // ready, which always precedes any `ply exec`: the run parent writes
    // the state file only after that, and the state file is how a client
    // finds this instance.
    let can_exec = Arc::new(AtomicBool::new(false));

    // Guest control lines → the session that owns them, or the parent.
    let exit: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    if let Some(lines) = running.take_control() {
        let writer = writer.clone();
        let exit = exit.clone();
        let sessions = sessions.clone();
        let can_exec = can_exec.clone();
        std::thread::Builder::new()
            .name("ply-vm-guest-lines".into())
            .spawn(move || {
                for line in lines {
                    if let GuestLine::Ready { features } = &line {
                        can_exec.store(
                            features.iter().any(|f| f == ply_vm_proto::FEATURE_EXEC),
                            Ordering::SeqCst,
                        );
                    }
                    // Exec traffic belongs to one connection and must not
                    // reach the run parent, whose pump reads ready, exit and
                    // publish and nothing else.
                    if let Some(id) = exec_id(&line) {
                        let done = matches!(line, GuestLine::Done { .. });
                        let mut sessions = match sessions.lock() {
                            Ok(sessions) => sessions,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        if let Some(tx) = sessions.get(&id) {
                            let _ = tx.send(line);
                        }
                        if done {
                            sessions.remove(&id);
                        }
                        continue;
                    }
                    if let GuestLine::Exit { code } = &line {
                        if let Ok(mut slot) = exit.lock() {
                            slot.get_or_insert(*code);
                        }
                    }
                    if send(&writer, &guest_line(&line)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| format!("spawning the control pump: {e}"))?;
    }

    // `ply exec` — one connection per command.
    serve_exec(instance_dir, running.control_handle(), sessions, can_exec)?;

    // Host control lines from the parent → the guest. EOF means the parent
    // is gone, and a VM with no parent is a VM nobody can stop: it goes.
    let orphaned = Arc::new(AtomicBool::new(false));
    let control = running.control_handle();
    {
        let orphaned = orphaned.clone();
        std::thread::Builder::new()
            .name("ply-vm-host-lines".into())
            .spawn(move || {
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Some(host) = parse_host_line(&line) {
                                control.send(&host);
                            }
                        }
                    }
                }
                orphaned.store(true, Ordering::SeqCst);
            })
            .map_err(|e| format!("spawning the host-line pump: {e}"))?;
    }

    // --- the life of the machine ----------------------------------------
    while running.running() {
        if orphaned.load(Ordering::SeqCst) {
            running.shutdown();
            break;
        }
        let sig = PENDING_SIGNAL.swap(0, Ordering::SeqCst);
        if sig != 0 {
            if let Ok(signal) = Signal::try_from(sig) {
                running.send_control(&HostLine::Signal {
                    name: signal.as_str().trim_start_matches("SIG").to_string(),
                });
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Joins the vCPU thread, which destroys the VM.
    drop(running);
    let code = exit.lock().ok().and_then(|slot| *slot).unwrap_or(255);
    Ok(code)
}

/// Which exec session a guest line belongs to, if any.
fn exec_id(line: &GuestLine) -> Option<u64> {
    match line {
        GuestLine::Output { output } => Some(output.id),
        GuestLine::Done { done } => Some(done.id),
        _ => None,
    }
}

type Sessions = Arc<Mutex<BTreeMap<u64, mpsc::Sender<GuestLine>>>>;

/// Listen for `ply exec`. One connection is one command: the client sends a
/// `{"exec":…}` line, then reads back that command's output and its exit,
/// and may send `{"stdin":…}` or `{"kill":…}` meanwhile.
///
/// Ids are allocated HERE, not by the client: two `ply exec` processes know
/// nothing of each other, and a collision would cross two commands' output.
fn serve_exec(
    instance_dir: &Path,
    control: super::console::ControlHandle,
    sessions: Sessions,
    can_exec: Arc<AtomicBool>,
) -> Result<(), String> {
    let path = instance_dir.join(EXEC_SOCKET);
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .map_err(|e| format!("listening for `ply exec` at {}: {e}", path.display()))?;
    std::thread::Builder::new()
        .name("ply-vm-exec".into())
        .spawn(move || {
            let next_id = AtomicU64::new(1);
            for stream in listener.incoming().flatten() {
                let id = next_id.fetch_add(1, Ordering::Relaxed);
                let control = control.clone();
                let sessions = sessions.clone();
                let can_exec = can_exec.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("ply-vm-exec-{id}"))
                    .spawn(move || exec_session(stream, id, control, sessions, can_exec));
            }
        })
        .map_err(|e| format!("spawning the exec listener: {e}"))?;
    Ok(())
}

/// One `ply exec`, start to finish.
fn exec_session(
    stream: UnixStream,
    id: u64,
    control: super::console::ControlHandle,
    sessions: Sessions,
    can_exec: Arc<AtomicBool>,
) {
    // A guest that never advertised `exec` will ignore the request as an
    // unknown line — the protocol's own rule — and answer nothing at all.
    // Refuse here instead, or `ply exec` waits forever.
    if !can_exec.load(Ordering::SeqCst) {
        let mut stream = stream;
        let _ = stream.write_all(
            guest_line(&GuestLine::Done {
                done: ply_vm_proto::ExecDone {
                    id: 0,
                    code: 126,
                    error: Some(
                        "this instance's guest cannot run commands — it booted an older \
                         microVM kernel than this ply expects (check PLY_MICROVM_KERNEL)"
                            .into(),
                    ),
                },
            })
            .as_bytes(),
        );
        return;
    }
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = std::io::BufReader::new(read_half);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let Some(HostLine::Exec { mut exec }) = parse_host_line(&first) else {
        // Not a request. Say so in the client's own language rather than
        // closing on it, so `ply exec` can print something useful.
        let mut stream = stream;
        let _ = stream.write_all(
            guest_line(&GuestLine::Done {
                done: ply_vm_proto::ExecDone {
                    id: 0,
                    code: 126,
                    error: Some("expected an exec request".into()),
                },
            })
            .as_bytes(),
        );
        return;
    };
    exec.id = id;

    let (tx, rx) = mpsc::channel();
    match sessions.lock() {
        Ok(mut sessions) => sessions.insert(id, tx),
        Err(poisoned) => poisoned.into_inner().insert(id, tx),
    };
    control.send(&HostLine::Exec { exec });

    // Client → guest: more input, or a signal. EOF means the client is gone,
    // and a command nobody is listening to should stop rather than run on
    // inside the instance forever.
    let signal_control = control.clone();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => match parse_host_line(&line) {
                    Some(HostLine::Stdin { mut stdin }) => {
                        stdin.id = id;
                        signal_control.send(&HostLine::Stdin { stdin });
                    }
                    Some(HostLine::Kill { mut kill }) => {
                        kill.id = id;
                        signal_control.send(&HostLine::Kill { kill });
                    }
                    _ => {}
                },
            }
        }
        signal_control.send(&HostLine::Kill {
            kill: ply_vm_proto::ExecKill {
                id,
                name: "TERM".into(),
            },
        });
    });

    // Guest → client, until the command ends.
    let mut stream = stream;
    for line in rx {
        let done = matches!(line, GuestLine::Done { .. });
        if stream.write_all(guest_line(&line).as_bytes()).is_err() || stream.flush().is_err() {
            break;
        }
        if done {
            break;
        }
    }
    match sessions.lock() {
        Ok(mut sessions) => sessions.remove(&id),
        Err(poisoned) => poisoned.into_inner().remove(&id),
    };
}

fn send(writer: &Arc<Mutex<UnixStream>>, text: &str) -> Result<(), String> {
    let mut w = writer
        .lock()
        .map_err(|_| "the parent link was left poisoned by a panic".to_string())?;
    w.write_all(text.as_bytes())
        .and_then(|_| w.flush())
        .map_err(|e| format!("writing to the parent: {e}"))
}
