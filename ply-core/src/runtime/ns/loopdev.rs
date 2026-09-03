//! Native loop-device attach (no shelling out to losetup/mount).
//!
//! LO_FLAGS_AUTOCLEAR means the kernel frees the device on last unmount —
//! no bookkeeping to leak.

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[allow(clippy::unnecessary_cast)]
const LOOP_CTL_GET_FREE: u64 = 0x4C82;
const LOOP_SET_FD: u64 = 0x4C00;
const LOOP_SET_STATUS64: u64 = 0x4C04;

const LO_FLAGS_READ_ONLY: u32 = 1;
const LO_FLAGS_AUTOCLEAR: u32 = 4;

#[repr(C)]
struct LoopInfo64 {
    lo_device: u64,
    lo_inode: u64,
    lo_rdevice: u64,
    lo_offset: u64,
    lo_sizelimit: u64,
    lo_number: u32,
    lo_encrypt_type: u32,
    lo_encrypt_key_size: u32,
    lo_flags: u32,
    lo_file_name: [u8; 64],
    lo_crypt_name: [u8; 64],
    lo_encrypt_key: [u8; 32],
    lo_init: [u64; 2],
}

impl Default for LoopInfo64 {
    fn default() -> Self {
        // large arrays lack Default; zeroed is the documented initial state
        unsafe { std::mem::zeroed() }
    }
}

/// Attach `backing` read-only to a free loop device; returns its path and
/// the open device fd. CALLER MUST keep the fd alive until the device is
/// mounted: with AUTOCLEAR set, closing the last opener detaches the device.
/// After mount, the mount holds the reference and auto-detach happens at
/// unmount — exactly what we want.
pub fn attach_ro(backing: &Path) -> Result<(PathBuf, std::fs::File)> {
    let ioerr = |what: &str, e: nix::errno::Errno| {
        Error::Runtime(format!("{what} for {}: {e}", backing.display()))
    };

    let control = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/loop-control")
        .map_err(|source| Error::Io {
            path: PathBuf::from("/dev/loop-control"),
            source,
        })?;
    let file = std::fs::File::open(backing).map_err(|source| Error::Io {
        path: backing.to_path_buf(),
        source,
    })?;

    // GET_FREE → SET_FD races with other processes; retry on EBUSY.
    for _ in 0..16 {
        let n = unsafe { nix::libc::ioctl(control.as_raw_fd(), LOOP_CTL_GET_FREE as _) };
        if n < 0 {
            return Err(ioerr("LOOP_CTL_GET_FREE", nix::errno::Errno::last()));
        }
        let dev_path = PathBuf::from(format!("/dev/loop{n}"));
        let dev = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev_path)
            .map_err(|source| Error::Io {
                path: dev_path.clone(),
                source,
            })?;
        let rc = unsafe {
            nix::libc::ioctl(
                dev.as_raw_fd(),
                LOOP_SET_FD as _,
                file.as_raw_fd() as nix::libc::c_int,
            )
        };
        if rc < 0 {
            let errno = nix::errno::Errno::last();
            if errno == nix::errno::Errno::EBUSY {
                continue; // someone grabbed it first
            }
            return Err(ioerr("LOOP_SET_FD", errno));
        }

        let mut info = LoopInfo64 {
            lo_flags: LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR,
            ..Default::default()
        };
        let name = backing.to_string_lossy();
        let bytes = name.as_bytes();
        let len = bytes.len().min(63);
        info.lo_file_name[..len].copy_from_slice(&bytes[..len]);
        let rc = unsafe { nix::libc::ioctl(dev.as_raw_fd(), LOOP_SET_STATUS64 as _, &info) };
        if rc < 0 {
            return Err(ioerr("LOOP_SET_STATUS64", nix::errno::Errno::last()));
        }
        return Ok((dev_path, dev));
    }
    Err(Error::Runtime(format!(
        "could not claim a free loop device for {} (kept losing the race)",
        backing.display()
    )))
}
