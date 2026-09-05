//! The `--vswitch` wire: a 4-byte big-endian length followed by one raw
//! Ethernet frame, over a `SOCK_STREAM`.
//!
//! Big-endian u32, not the u16 the design sketched, because this is exactly
//! what passt and gvproxy speak (ruling R0-2 from the milestone 0 spike):
//! the same tooling that debugs a podman machine's network debugs this one.
//! macOS has no `SOCK_SEQPACKET` for unix sockets, which is why the length
//! prefix exists at all.
//!
//! # Who speaks it
//!
//! A single-member `ply run` puts the device and the switch in ONE process,
//! so frames cross a channel and never a socket — the framing is not on that
//! path at all. It carries a `ply up` stack: the switch runs in the `ply up`
//! parent and each member's `ply run` is a client of it over a unix socket
//! ([`super::unix`]), and every frame between them goes through this
//! function pair.
//!
//! It was written and tested before anything spoke it, which is why the four
//! tests below are about the framing itself rather than about a stack: a
//! length prefix invented against a running system is invented against
//! whatever that system happened to do.

use std::io::{Read, Write};

/// The largest frame either side will send or accept.
///
/// An Ethernet frame on this network is at most 1514 bytes (MTU 1500 plus
/// the header). 65535 is the ceiling of the length field's usefulness, and
/// the point of the check is not to be tight: it is that four bytes from a
/// broken or hostile peer must never make the parent allocate a gigabyte.
pub const MAX_FRAME: usize = 65_535;

/// Write one frame, length first. One `write_all`, so a frame is never
/// interleaved with another writer's on the same socket.
pub fn write_frame(out: &mut impl Write, frame: &[u8]) -> std::io::Result<()> {
    if frame.len() > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("frame of {} bytes exceeds {MAX_FRAME}", frame.len()),
        ));
    }
    let mut buf = Vec::with_capacity(4 + frame.len());
    buf.extend_from_slice(&(frame.len() as u32).to_be_bytes());
    buf.extend_from_slice(frame);
    out.write_all(&buf)
}

/// Read exactly one frame into `buf`, replacing whatever was there, and
/// return its length.
///
/// `SOCK_STREAM` has no message boundaries: one `read` may carry half a
/// frame or three of them, so both halves are read with `read_exact` and the
/// caller loops on this function rather than on `read`.
pub fn read_frame(input: &mut impl Read, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    // Checked BEFORE the buffer is grown: the whole point of a bound here is
    // that the allocation never happens.
    if len > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("peer announced a {len}-byte frame; the limit is {MAX_FRAME}"),
        ));
    }
    buf.clear();
    buf.resize(len, 0);
    input.read_exact(buf)?;
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_through_the_length_prefix() {
        let frame = b"\xff\xff\xff\xff\xff\xffRT\x00\x77\x00\x02\x08\x06payload";
        let mut wire = Vec::new();
        write_frame(&mut wire, frame).expect("write");
        // The length is BIG-endian and leads: that is the whole contract, so
        // assert the bytes rather than only the round trip.
        assert_eq!(&wire[..4], &(frame.len() as u32).to_be_bytes());
        assert_eq!(&wire[4..], frame);

        let mut buf = Vec::new();
        let n = read_frame(&mut wire.as_slice(), &mut buf).expect("read");
        assert_eq!(n, frame.len());
        assert_eq!(buf, frame);
    }

    #[test]
    fn two_frames_in_one_read_are_both_delivered() {
        // SOCK_STREAM has no message boundaries: the reader must never
        // assume one read is one frame.
        let mut wire = Vec::new();
        write_frame(&mut wire, b"first").expect("write");
        write_frame(&mut wire, b"second").expect("write");
        let mut input = wire.as_slice();
        let mut buf = Vec::new();
        read_frame(&mut input, &mut buf).expect("first");
        assert_eq!(buf, b"first");
        read_frame(&mut input, &mut buf).expect("second");
        assert_eq!(buf, b"second");
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() {
        /// A reader that hands back one byte at a time — the worst case a
        /// real socket can produce, and the one a `read`-per-frame reader
        /// gets wrong.
        struct Dribble<'a>(&'a [u8]);
        impl std::io::Read for Dribble<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() || out.is_empty() {
                    return Ok(0);
                }
                out[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }

        let mut wire = Vec::new();
        write_frame(&mut wire, b"a frame that arrives in pieces").expect("write");
        let mut buf = Vec::new();
        read_frame(&mut Dribble(&wire), &mut buf).expect("read");
        assert_eq!(buf, b"a frame that arrives in pieces");
    }

    #[test]
    fn an_oversized_length_is_refused_before_it_allocates() {
        // A malicious or broken peer must not be able to make the parent
        // allocate 4 GiB by sending four bytes.
        let wire = u32::MAX.to_be_bytes();
        let mut buf = Vec::new();
        let err = read_frame(&mut wire.as_slice(), &mut buf).expect_err("refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            buf.is_empty(),
            "the buffer must not have been grown to the announced length"
        );
        // And the same limit on the way out, so this process cannot be the
        // one that puts an unreadable frame on the wire.
        assert!(write_frame(&mut Vec::new(), &vec![0u8; MAX_FRAME + 1]).is_err());
    }
}
