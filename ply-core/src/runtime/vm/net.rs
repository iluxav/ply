//! virtio-mmio net, bridged to the switch.
//!
//! Ported from the plyvm spike's `net.rs` — the DEVICE half of it. The
//! spike's other half, a hand-written TCP/IP stack with a NAT in it, is not
//! here: it lives in `switch/`, in smoltcp, portable and tested on Linux.
//! What this file keeps is the part that is genuinely about the hardware:
//! two virtqueues, a 12-byte header, and a MAC in config space.
//!
//! It sits on `machine.rs`'s generic virtio-mmio transport ([`Mmio`],
//! [`Queue`], [`Device`]) exactly as virtio-blk, virtio-console and
//! virtio-rng do, so none of the transport is duplicated here.
//!
//! # The two facts about virtio-net this file depends on
//!
//! * **The header is 12 bytes.** Linux's `virtio_net.c` sets
//!   `vi->hdr_len = sizeof(struct virtio_net_hdr_mrg_rxbuf)` — 12 — when
//!   *either* `VIRTIO_NET_F_MRG_RXBUF` *or* `VIRTIO_F_VERSION_1` is
//!   negotiated, and this device negotiates VERSION_1 (every modern
//!   virtio-mmio device does; `Mmio::new` sets it). The extra `num_buffers`
//!   field is present and ignored, since merging is not offered. Getting
//!   this wrong by two bytes shifts every frame and shows up as a NIC that
//!   is up and drops everything.
//! * **Queue 0 is receive and queue 1 is transmit**, from the driver's point
//!   of view: the guest posts empty buffers on 0 for the device to fill, and
//!   full frames on 1 for the device to send.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use applevisor::prelude::*;

use super::machine::{Device, Mmio};
use super::switch::{FrameSink, MTU};

/// virtio device id 1: a network card.
const DEVICE_ID: u32 = 1;

/// `VIRTIO_NET_F_MAC` — "config space carries the MAC". Without it Linux
/// makes one up at every boot, and a guest whose address changes under it
/// is a guest whose ARP entries on the switch go stale for no reason.
const VIRTIO_NET_F_MAC: u64 = 1 << 5;

/// `struct virtio_net_hdr_v1`. See the module comment for why it is 12 and
/// not 10.
const HDR_LEN: usize = 12;

/// Frames held for a guest that has posted no receive buffers.
///
/// A NIC drops when its ring is full and so does this. The switch already
/// caps what it hands over, so this is the second of two bounds; between
/// them, a guest that stops reading costs the parent a fixed amount of
/// memory rather than an unbounded one.
const RX_BACKLOG: usize = 256;

/// The largest Ethernet frame this device will carry: the switch's IP MTU
/// plus the 14-byte header.
const MAX_FRAME: usize = MTU + 14;

pub struct VirtioNet {
    mmio: Mmio,
    mac: [u8; 6],
    /// Frames this guest transmits.
    uplink: FrameSink,
    /// Frames the switch has for this guest.
    downlink: mpsc::Receiver<Vec<u8>>,
    pending: VecDeque<Vec<u8>>,
    /// Counted, and reported once at teardown under `PLY_VM_DEBUG`: a
    /// silent drop is exactly the kind of thing that turns into "the
    /// network is slow sometimes".
    dropped: AtomicUsize,
}

impl VirtioNet {
    pub fn new(mac: [u8; 6], uplink: FrameSink, downlink: mpsc::Receiver<Vec<u8>>) -> VirtioNet {
        VirtioNet {
            // Two queues: 0 receive, 1 transmit. No control queue, so no
            // VIRTIO_NET_F_CTRL_VQ and no multiqueue — one pair is what a
            // single-vCPU guest can use anyway.
            mmio: Mmio::new(DEVICE_ID, VIRTIO_NET_F_MAC, 2),
            mac,
            uplink,
            downlink,
            pending: VecDeque::new(),
            dropped: AtomicUsize::new(0),
        }
    }

    /// Pull everything the guest wants to send and hand it to the switch.
    fn drain_tx(&mut self, ram: &mut Memory) -> bool {
        let VirtioNet {
            mmio,
            uplink,
            dropped,
            ..
        } = self;
        let Some(queue) = mmio.queues.get_mut(1) else {
            return false;
        };
        let mut did = false;
        while let Some(head) = queue.pop(ram) {
            let mut frame = Vec::new();
            for desc in queue.chain(ram, head) {
                // Device-writable descriptors in a TX chain are not ours to
                // read; a well-behaved driver posts none.
                if desc.write || desc.len == 0 {
                    continue;
                }
                let mut buf = vec![0u8; desc.len as usize];
                if ram.read(desc.addr, &mut buf).is_ok() {
                    frame.extend_from_slice(&buf);
                }
            }
            // The buffer is returned whatever happens to the frame: a
            // descriptor the device keeps is a queue the driver can never
            // refill, which stops the NIC dead after 256 packets.
            queue.push_used(ram, head, 0);
            did = true;
            if frame.len() <= HDR_LEN {
                continue;
            }
            if !uplink.send(frame[HDR_LEN..].to_vec()) {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        did
    }

    /// Take whatever the switch has for this guest.
    fn collect_rx(&mut self) {
        while let Ok(frame) = self.downlink.try_recv() {
            // Nothing here fragments and the guest's MTU is 1500, so a
            // longer frame is one this card cannot deliver. Dropping it is
            // what the hardware would do; carrying a truncated one would be
            // corruption the guest cannot detect.
            if frame.len() > MAX_FRAME {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if self.pending.len() >= RX_BACKLOG {
                // Drop the OLDEST: the newest frame is the one whose
                // retransmit timer has not started yet, so keeping it loses
                // the least time.
                self.pending.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            self.pending.push_back(frame);
        }
    }

    /// Fill as many posted receive buffers as there are frames waiting.
    fn flush_rx(&mut self, ram: &mut Memory) -> bool {
        let VirtioNet {
            mmio,
            pending,
            dropped,
            ..
        } = self;
        let Some(queue) = mmio.queues.get_mut(0) else {
            return false;
        };
        let mut did = false;
        while !pending.is_empty() {
            let Some(head) = queue.pop(ram) else {
                break; // the driver has posted no more buffers
            };
            let frame = match pending.pop_front() {
                Some(frame) => frame,
                None => break,
            };
            let chain = queue.chain(ram, head);
            let room: usize = chain
                .iter()
                .filter(|d| d.write)
                .map(|d| d.len as usize)
                .sum();
            if room < HDR_LEN + frame.len() {
                // The driver posted a buffer too small for this frame.
                // Nothing can be done with it but give it back empty —
                // holding the descriptor would stall the queue.
                queue.push_used(ram, head, 0);
                dropped.fetch_add(1, Ordering::Relaxed);
                did = true;
                continue;
            }
            // Header first, then the frame, across as many of the chain's
            // writable descriptors as it takes.
            let mut payload = vec![0u8; HDR_LEN];
            // `num_buffers` = 1. Ignored unless MRG_RXBUF was negotiated,
            // which it was not, but a 1 is what the field means here and a
            // 0 would be a lie about a buffer count.
            payload[10] = 1;
            payload.extend_from_slice(&frame);
            let mut at = 0usize;
            for desc in chain.iter().filter(|d| d.write) {
                if at >= payload.len() {
                    break;
                }
                let take = (desc.len as usize).min(payload.len() - at);
                let _ = ram.write(desc.addr, &payload[at..at + take]);
                at += take;
            }
            queue.push_used(ram, head, payload.len() as u32);
            did = true;
        }
        did
    }
}

impl Device for VirtioNet {
    fn kind(&self) -> &'static str {
        "net"
    }

    fn read(&mut self, off: u64) -> u64 {
        if let Some(value) = self.mmio.read(off) {
            return value;
        }
        // Config space: `struct virtio_net_config` starts with the 6 MAC
        // bytes, and this device advertises nothing after them.
        match off {
            0x100..=0x105 => self.mac[(off - 0x100) as usize] as u64,
            _ => 0,
        }
    }

    fn write(&mut self, off: u64, val: u64, ram: &mut Memory) -> bool {
        if self.mmio.write(off, val) {
            return false;
        }
        // QueueNotify: the value is the queue the driver just added to.
        if off != 0x050 {
            return false;
        }
        let mut work = false;
        if val == 1 {
            work |= self.drain_tx(ram);
        }
        // A notify on the receive queue means fresh buffers; either way,
        // anything already waiting can go now.
        self.collect_rx();
        work |= self.flush_rx(ram);
        if work {
            self.mmio.irq_status |= 1;
        }
        work
    }

    /// Frames that arrived while the guest was idle in WFI. Called on every
    /// waker tick, which is the only thing that gets host-originated bytes
    /// into a guest that is not currently trapping.
    fn poll(&mut self, ram: &mut Memory) -> bool {
        if !self.mmio.driver_ok() {
            return false;
        }
        self.collect_rx();
        if self.pending.is_empty() {
            return false;
        }
        if self.flush_rx(ram) {
            self.mmio.irq_status |= 1;
            return true;
        }
        false
    }
}

impl Drop for VirtioNet {
    fn drop(&mut self) {
        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped > 0 && std::env::var_os("PLY_VM_DEBUG").is_some() {
            eprintln!("ply: microVM NIC dropped {dropped} frame(s)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_the_size_linux_expects_for_a_version_1_device() {
        // `struct virtio_net_hdr_v1`: flags, gso_type, hdr_len, gso_size,
        // csum_start, csum_offset, num_buffers = 1+1+2+2+2+2+2. Linux uses
        // this size whenever VERSION_1 is negotiated, merging or not
        // (drivers/net/virtio_net.c, virtnet_probe).
        assert_eq!(HDR_LEN, 12);
        assert_eq!(MAX_FRAME, 1514);
    }

    #[test]
    fn the_mac_feature_bit_is_the_one_linux_reads_config_space_for() {
        // VIRTIO_NET_F_MAC is bit 5 (virtio 1.1 §5.1.3).
        assert_eq!(VIRTIO_NET_F_MAC, 32);
        assert_eq!(DEVICE_ID, 1, "virtio device id 1 is a network card");
    }
}
