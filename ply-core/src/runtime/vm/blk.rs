//! virtio-mmio block devices: the app's images read-only in overlay order,
//! the volumes writable, the spec disk read-only.
//!
//! The virtqueue walk is the plyvm spike's, which is a correct modern
//! virtio-mmio block device. Four things differ:
//!
//!  1. **N devices.** Each sits at its own MMIO window and its own SPI; the
//!     transport half of that lives in `machine.rs`.
//!  2. **File-backed, not an in-memory `Vec<u8>`.** A volume must survive the
//!     instance, and a 27 MiB image must not be copied into host RAM on every
//!     boot. Every request is a `read_exact_at`/`write_all_at` at
//!     `sector * 512`.
//!  3. **Read-only really is read-only.** A layer or the spec disk advertises
//!     `VIRTIO_BLK_F_RO`, so the guest itself refuses to write it; a write
//!     that arrives anyway is answered `VIRTIO_BLK_S_IOERR` rather than
//!     quietly dropped.
//!  4. **`VIRTIO_BLK_T_FLUSH` is honoured.** plyvm has no flush handling at
//!     all. A database that fsyncs, is told "done", and then loses the bytes
//!     is a corruption bug, not a performance one.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use applevisor::memory::Memory;

use super::machine::{Device, Mmio};

/// virtio device id 2.
const VIRTIO_ID_BLOCK: u32 = 2;
/// Feature bit 5: the device is read-only.
const VIRTIO_BLK_F_RO: u64 = 1 << 5;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const SECTOR: u64 = 512;

pub struct VirtioBlk {
    mmio: Mmio,
    file: File,
    /// Capacity in bytes, taken once at open. A volume's file does not grow
    /// under the guest: it is created at its full (sparse) size.
    len: u64,
    read_only: bool,
}

impl VirtioBlk {
    pub fn open(path: &Path, read_only: bool) -> std::result::Result<VirtioBlk, String> {
        let file = File::options()
            .read(true)
            .write(!read_only)
            .open(path)
            .map_err(|e| format!("attaching {} as a disk: {e}", path.display()))?;
        let len = file
            .metadata()
            .map_err(|e| format!("sizing {}: {e}", path.display()))?
            .len();
        let features = if read_only { VIRTIO_BLK_F_RO } else { 0 };
        Ok(VirtioBlk {
            mmio: Mmio::new(VIRTIO_ID_BLOCK, features, 1),
            file,
            len,
            read_only,
        })
    }

    /// Read `len` bytes at `offset`, zero-filling past the end of the file.
    /// A short read is not an error the guest can act on: the kernel asked
    /// for a sector inside the capacity we advertised, so answering with
    /// zeroes is the same thing a real disk does for a sparse region.
    fn read_at(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        if offset >= self.len {
            return buf;
        }
        let take = ((self.len - offset) as usize).min(len);
        let _ = self.file.read_exact_at(&mut buf[..take], offset);
        buf
    }

    /// Serve everything the driver has published. Returns whether anything
    /// was done, which is whether an interrupt is owed.
    fn process(&mut self, ram: &mut Memory) -> bool {
        // Taken out and put back so the queue's state can be driven while
        // `self.file` is borrowed. There is exactly one queue on a block
        // device, so this cannot renumber anything.
        if self.mmio.queues.is_empty() {
            return false;
        }
        let mut queue = std::mem::take(&mut self.mmio.queues[0]);
        let mut did = false;
        while let Some(head) = queue.pop(ram) {
            let chain = queue.chain(ram, head);
            did = true;
            // [header][data…][status] — at minimum a header and a status.
            if chain.len() < 2 {
                queue.push_used(ram, head, 0);
                continue;
            }
            let header = &chain[0];
            let req_type = ram.read_u32(header.addr).unwrap_or(u32::MAX);
            let sector = ram.read_u64(header.addr + 8).unwrap_or(0);
            let status_gpa = chain[chain.len() - 1].addr;
            let mut written = 0u32;
            let mut status = S_OK;
            let mut offset = sector.saturating_mul(SECTOR);

            for d in &chain[1..chain.len() - 1] {
                match req_type {
                    T_IN => {
                        let buf = self.read_at(offset, d.len as usize);
                        let _ = ram.write(d.addr, &buf);
                        written += d.len;
                        offset += d.len as u64;
                    }
                    T_OUT => {
                        if self.read_only {
                            // The guest was told VIRTIO_BLK_F_RO, so this is
                            // either a bug or an attempt; either way it is an
                            // I/O error and never a silent success.
                            status = S_IOERR;
                            continue;
                        }
                        let mut buf = vec![0u8; d.len as usize];
                        let _ = ram.read(d.addr, &mut buf);
                        if self.file.write_all_at(&buf, offset).is_err() {
                            status = S_IOERR;
                        }
                        offset += d.len as u64;
                    }
                    T_GET_ID => {
                        // 20 bytes, NUL-padded. Nothing depends on the value;
                        // the guest identifies its disks by content.
                        let mut id = [0u8; 20];
                        let tag = b"ply";
                        id[..tag.len()].copy_from_slice(tag);
                        let n = (d.len as usize).min(id.len());
                        let _ = ram.write(d.addr, &id[..n]);
                        written += n as u32;
                    }
                    T_FLUSH => {}
                    _ => status = S_UNSUPP,
                }
            }
            if req_type == T_FLUSH && !self.read_only && self.file.sync_data().is_err() {
                status = S_IOERR;
            }
            let _ = ram.write_u8(status_gpa, status);
            written += 1;
            queue.push_used(ram, head, written);
        }
        self.mmio.queues[0] = queue;
        did
    }
}

impl Device for VirtioBlk {
    fn kind(&self) -> &'static str {
        if self.read_only {
            "blk (ro)"
        } else {
            "blk (rw)"
        }
    }

    fn read(&mut self, off: u64) -> u64 {
        if let Some(v) = self.mmio.read(off) {
            return v;
        }
        let capacity = self.len / SECTOR;
        match off {
            0x100 => capacity & 0xffff_ffff,
            0x104 => capacity >> 32,
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
