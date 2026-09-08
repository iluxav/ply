//! virtio-9p: the transport that carries a [`p9::Server`] to the guest.
//!
//! Device id 9, one queue, and the one feature the guest driver wants,
//! `VIRTIO_9P_MOUNT_TAG`: the config space holds the tag the guest names in
//! `mount -t 9p <tag> …`, as `tag_len[2]` then the bytes. Linux reads the
//! length as one 16-bit access and the bytes one at a time, so the config
//! space is served a byte per offset, as the NIC serves its MAC.
//!
//! A request is one descriptor chain: the driver-written descriptors hold
//! the T-message, the device-written ones receive the R-message. Both are
//! gathered and scattered whole — a 9P message is one unit and the server
//! answers one at a time — so the queue walk stays the block device's.

use std::path::Path;

use applevisor::memory::Memory;

use super::machine::{Device, Mmio};
use super::p9;

/// virtio device id 9.
const VIRTIO_ID_9P: u32 = 9;
/// Feature bit 0: the config space carries a mount tag.
const VIRTIO_9P_MOUNT_TAG: u64 = 1 << 0;

pub struct VirtioP9 {
    mmio: Mmio,
    /// `tag_len[2] tag[…]`, as the guest reads it.
    config: Vec<u8>,
    server: p9::Server,
}

impl VirtioP9 {
    pub fn new(tag: &str, root: &Path, uid: u32, gid: u32) -> Result<VirtioP9, String> {
        let server = p9::Server::new(root, uid, gid)
            .map_err(|e| format!("sharing {} as {tag}: {e}", root.display()))?;
        let mut config = (tag.len() as u16).to_le_bytes().to_vec();
        config.extend_from_slice(tag.as_bytes());
        Ok(VirtioP9 {
            mmio: Mmio::new(VIRTIO_ID_9P, VIRTIO_9P_MOUNT_TAG, 1),
            config,
            server,
        })
    }

    /// Serve every request the driver has published. Returns whether any
    /// was, which is whether an interrupt is owed.
    fn process(&mut self, ram: &mut Memory) -> bool {
        if self.mmio.queues.is_empty() {
            return false;
        }
        let mut queue = std::mem::take(&mut self.mmio.queues[0]);
        let mut did = false;
        while let Some(head) = queue.pop(ram) {
            let chain = queue.chain(ram, head);
            did = true;
            // Gather the request from the driver's buffers.
            let mut request = Vec::new();
            for d in chain.iter().filter(|d| !d.write) {
                let mut buf = vec![0u8; d.len as usize];
                let _ = ram.read(d.addr, &mut buf);
                request.extend_from_slice(&buf);
            }
            let reply = self.server.handle(&request);
            // Scatter the reply into the device-writable buffers. A reply
            // longer than the space the driver gave it is truncated; the
            // driver sized those buffers from the msize it negotiated, so
            // that means the reply was already over msize, which the server
            // bounds it under.
            let mut written = 0usize;
            for d in chain.iter().filter(|d| d.write) {
                if written >= reply.len() {
                    break;
                }
                let n = (d.len as usize).min(reply.len() - written);
                let _ = ram.write(d.addr, &reply[written..written + n]);
                written += n;
            }
            queue.push_used(ram, head, written as u32);
        }
        self.mmio.queues[0] = queue;
        did
    }
}

impl Device for VirtioP9 {
    fn kind(&self) -> &'static str {
        "9p"
    }

    fn read(&mut self, off: u64) -> u64 {
        if let Some(v) = self.mmio.read(off) {
            return v;
        }
        // Config space, a byte per offset: `tag_len` is read as one 16-bit
        // access at 0x100, which a byte answers correctly for any tag under
        // 256 characters, and the tag bytes are read one at a time.
        match off.checked_sub(0x100) {
            Some(i) if (i as usize) < self.config.len() => self.config[i as usize] as u64,
            _ => 0,
        }
    }

    fn write(&mut self, off: u64, val: u64, ram: &mut Memory) -> bool {
        if self.mmio.write(off, val) {
            return false;
        }
        // 0x050 is QueueNotify: the doorbell.
        if off == 0x050 && self.process(ram) {
            self.mmio.irq_status |= 1;
            return true;
        }
        false
    }
}
