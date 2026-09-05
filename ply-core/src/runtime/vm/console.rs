//! virtio-mmio consoles. `hvc0` is the app's stdout+stderr (teed to the log
//! ring); `hvc1` is the newline-delimited JSON control channel.
//!
//! # Two single-port devices, not one two-port device
//!
//! The plan called for one `VIRTIO_CONSOLE_F_MULTIPORT` device with two
//! ports. That works, but it costs the whole control-queue handshake
//! (`DEVICE_READY`, `DEVICE_ADD`, `PORT_READY`, `CONSOLE_PORT`, `PORT_OPEN`)
//! before either port carries a byte — and Linux only creates `/dev/hvcN`
//! for a port the device announced with `VIRTIO_CONSOLE_CONSOLE_PORT`;
//! everything else lands at `/dev/vport0p<n>`.
//!
//! A device with the multiport feature **off** is a console by definition:
//! `virtio_console.c`'s `add_port` calls `init_port_console` unconditionally
//! in that case, and `hvc_alloc` hands out indices in registration order. So
//! two of them, emitted into the device tree in this order, become exactly
//! `/dev/hvc0` and `/dev/hvc1` — the two devices `ply-guest-init` opens —
//! with two queues each and no handshake to get wrong. The guest accepts the
//! `vport0p1` spelling too, so this choice cannot break it either way.

use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::{mpsc, Arc, Mutex};

use applevisor::memory::Memory;
use ply_vm_proto::{host_line, parse_guest_line, GuestLine, HostLine};

use super::machine::{Device, Mmio};

/// virtio device id 3.
const VIRTIO_ID_CONSOLE: u32 = 3;
/// Queue 0 is guest-receive (host → guest); queue 1 is guest-transmit.
const RX: usize = 0;
const TX: usize = 1;

/// The longest control line the host will accumulate from the guest.
///
/// `{"publish":…}` is the biggest thing that legitimately crosses, and it is
/// one key and one value. A guest that writes bytes and never a newline —
/// a bug, or an app that found the port and is writing binary at it — must
/// not be able to grow this buffer until the *host* runs out of memory.
const MAX_LINE: usize = 64 * 1024;

/// Where a console's guest→host bytes end up.
enum Sink {
    /// Port 0: straight through to a pipe the supervisor reads.
    Pipe(std::fs::File),
    /// Port 1: split into lines and parsed.
    Lines {
        buf: Vec<u8>,
        tx: mpsc::Sender<GuestLine>,
    },
}

/// The host's writing half of the control channel.
#[derive(Clone)]
pub struct ControlHandle {
    outbound: Arc<Mutex<VecDeque<u8>>>,
}

impl ControlHandle {
    /// Queue one host→guest line. Best effort by design: a guest that has
    /// stopped reading must never make `ply stop` fail — the machine can
    /// always be torn down instead.
    pub fn send(&self, line: &HostLine) {
        if let Ok(mut q) = self.outbound.lock() {
            // Bounded for the same reason `MAX_LINE` is: a guest that never
            // drains its receive queue would otherwise grow this forever.
            if q.len() < MAX_LINE {
                q.extend(host_line(line).as_bytes());
            }
        }
    }
}

pub struct VirtioConsole {
    mmio: Mmio,
    sink: Sink,
    outbound: Arc<Mutex<VecDeque<u8>>>,
}

impl VirtioConsole {
    /// `hvc0`: everything the app writes goes to the returned pipe, byte for
    /// byte, exactly as the namespace backend's log pipe carries it.
    pub fn to_pipe() -> std::result::Result<(VirtioConsole, std::fs::File), String> {
        let (reader, writer) =
            nix::unistd::pipe().map_err(|e| format!("console pipe for the app's stdout: {e}"))?;
        Ok((
            VirtioConsole {
                mmio: Mmio::new(VIRTIO_ID_CONSOLE, 0, 2),
                sink: Sink::Pipe(std::fs::File::from(writer)),
                outbound: Arc::new(Mutex::new(VecDeque::new())),
            },
            std::fs::File::from(reader),
        ))
    }

    /// `hvc1`: newline-delimited JSON, `ply_vm_proto::{GuestLine, HostLine}`.
    pub fn control() -> (VirtioConsole, ControlHandle, mpsc::Receiver<GuestLine>) {
        let (tx, rx) = mpsc::channel();
        let outbound = Arc::new(Mutex::new(VecDeque::new()));
        (
            VirtioConsole {
                mmio: Mmio::new(VIRTIO_ID_CONSOLE, 0, 2),
                sink: Sink::Lines {
                    buf: Vec::new(),
                    tx,
                },
                outbound: outbound.clone(),
            },
            ControlHandle { outbound },
            rx,
        )
    }

    /// Drain the guest's transmit queue into the sink.
    fn transmit(&mut self, ram: &mut Memory) -> bool {
        if self.mmio.queues.len() < TX + 1 {
            return false;
        }
        let mut queue = std::mem::take(&mut self.mmio.queues[TX]);
        let mut did = false;
        while let Some(head) = queue.pop(ram) {
            for d in queue.chain(ram, head) {
                if d.write || d.len == 0 {
                    continue;
                }
                let mut buf = vec![0u8; d.len as usize];
                if ram.read(d.addr, &mut buf).is_err() {
                    continue;
                }
                self.sink.accept(&buf);
            }
            queue.push_used(ram, head, 0);
            did = true;
        }
        self.mmio.queues[TX] = queue;
        did
    }

    /// Hand the guest whatever the host has queued for it, as far as its
    /// receive buffers go.
    fn receive(&mut self, ram: &mut Memory) -> bool {
        if self.mmio.queues.len() < RX + 1 || !self.mmio.driver_ok() {
            return false;
        }
        // The handle is cloned before it is locked so the queue below can be
        // borrowed mutably out of the same `self`.
        let outbound = self.outbound.clone();
        let Ok(mut pending) = outbound.lock() else {
            return false;
        };
        if pending.is_empty() {
            return false;
        }
        let mut queue = std::mem::take(&mut self.mmio.queues[RX]);
        let mut did = false;
        while !pending.is_empty() {
            let Some(head) = queue.pop(ram) else { break };
            let mut written = 0u32;
            for d in queue.chain(ram, head) {
                if !d.write || d.len == 0 {
                    continue;
                }
                let take = (d.len as usize).min(pending.len());
                if take == 0 {
                    break;
                }
                let chunk: Vec<u8> = pending.drain(..take).collect();
                let _ = ram.write(d.addr, &chunk);
                written += take as u32;
                if pending.is_empty() {
                    break;
                }
            }
            queue.push_used(ram, head, written);
            did = true;
        }
        self.mmio.queues[RX] = queue;
        did
    }
}

impl Sink {
    fn accept(&mut self, bytes: &[u8]) {
        match self {
            Sink::Pipe(file) => {
                // Errors are ignored on purpose: a supervisor that has
                // stopped reading (its tee thread ended) must not wedge the
                // vCPU thread, and Rust's runtime already makes a write to a
                // closed pipe an `EPIPE` rather than a `SIGPIPE` death.
                let _ = file.write_all(bytes);
                let _ = file.flush();
            }
            Sink::Lines { buf, tx } => {
                for &b in bytes {
                    if b == b'\n' {
                        if let Ok(text) = std::str::from_utf8(buf) {
                            if let Some(line) = parse_guest_line(text) {
                                let _ = tx.send(line);
                            }
                        }
                        buf.clear();
                        continue;
                    }
                    if buf.len() < MAX_LINE {
                        buf.push(b);
                    }
                    // Past the cap the rest of the line is dropped and the
                    // line will fail to parse — which is the right outcome
                    // for a line nothing legitimate produces.
                }
            }
        }
    }
}

impl Device for VirtioConsole {
    fn kind(&self) -> &'static str {
        match self.sink {
            Sink::Pipe(_) => "console (hvc0, app stdout)",
            Sink::Lines { .. } => "console (hvc1, control)",
        }
    }

    fn read(&mut self, off: u64) -> u64 {
        // No feature is negotiated beyond VIRTIO_F_VERSION_1, so the driver
        // never reads `cols`, `rows` or `max_nr_ports`; config space is zero.
        self.mmio.read(off).unwrap_or(0)
    }

    fn write(&mut self, off: u64, val: u64, ram: &mut Memory) -> bool {
        if self.mmio.write(off, val) {
            return false;
        }
        if off != 0x050 {
            return false;
        }
        // 0x050 is QueueNotify. A notify on the receive queue means the
        // driver has posted fresh buffers, which may be what the host was
        // waiting for.
        let mut work = self.transmit(ram);
        work |= self.receive(ram);
        if work {
            self.mmio.irq_status |= 1;
            return true;
        }
        false
    }

    fn poll(&mut self, ram: &mut Memory) -> bool {
        // Host-originated bytes: the guest may be in WFI with nothing to
        // notify us about, so this is the only path that reaches it.
        if self.receive(ram) {
            self.mmio.irq_status |= 1;
            return true;
        }
        false
    }
}
