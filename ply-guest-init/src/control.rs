//! The control channel: `/dev/hvc1`, newline-delimited JSON both ways.
//!
//! HVF has no vsock device, so a second virtio-console is how the host and
//! the guest talk. `hvc0` carries the app's own stdout and stderr and
//! nothing else, so nothing ply says can ever be mistaken for app output,
//! and the PL011 (`/dev/console`, `ttyAMA0`) carries the kernel's log and
//! this init's own diagnostics.
//!
//! Nothing here is Linux-specific — it is `std::fs` over a device path — so
//! it compiles and is type-checked on every platform CI runs.

use std::fs::File;
use std::io::{BufRead, BufReader, Read as _, Write};
use std::sync::{Arc, Mutex};

use ply_vm_proto::{guest_line, parse_host_line, GuestLine, HostLine};

/// Where the control port shows up when the VMM's virtio-console device
/// announces port 1 as a *console* port.
pub const CONTROL_DEV: &str = "/dev/hvc1";

/// Where it shows up when the device announces port 1 as a plain multiport
/// serial port instead. Which of the two a guest gets is the VMM's device
/// model's decision (Task 8), not the kernel config's: the kernel only makes
/// `/dev/hvcN` for a port that received `VIRTIO_CONSOLE_CONSOLE_PORT`, and
/// otherwise names it `/dev/vport0p<n>`. Both carry identical bytes, so
/// accepting either costs one `open` and removes a whole class of "the guest
/// booted and then said nothing" from Task 8's bring-up.
pub const CONTROL_DEV_FALLBACK: &str = "/dev/vport0p1";

/// The writing half of the control channel, cloneable so the app's watcher
/// thread and the init loop can both report without either owning it.
#[derive(Clone)]
pub struct Control {
    out: Arc<Mutex<File>>,
}

/// Take a virtio-console port out of line discipline.
///
/// Measured, not assumed: a `virtio-console` port is a **tty**, and the
/// kernel's default N_TTY line discipline does two things that break both
/// channels.
///
/// * `ECHO` — every `{"params":…}` line the host writes comes straight back
///   as if the guest had said it. Harmless today (the host's parser drops a
///   line it does not recognise) and confusing forever.
/// * `ICANON` + `ONLCR` — input is capped at 4096 bytes per line, which
///   silently truncates a large params tree, and every `\n` the app writes to
///   `hvc0` goes out as `\r\n`, so the log ring the host tees into would hold
///   CRLF where the namespace backend's pipe holds LF.
///
/// Best-effort: a VMM that exposes the port as a plain character device
/// answers `ENOTTY` here, which is not an error — there is simply no line
/// discipline to disable.
pub fn set_raw(fd: std::os::fd::RawFd) {
    // SAFETY: `termios` is a plain-old-data struct the two calls below fill
    // in and read back; `fd` is owned by the caller for the duration.
    unsafe {
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tio) != 0 {
            return;
        }
        libc::cfmakeraw(&mut tio);
        libc::tcsetattr(fd, libc::TCSANOW, &tio);
    }
}

impl Control {
    /// Open one specific device.
    pub fn open_at(dev: &str) -> std::io::Result<(Control, BufReader<File>)> {
        use std::os::fd::AsRawFd as _;

        let out = std::fs::OpenOptions::new().write(true).open(dev)?;
        set_raw(out.as_raw_fd());
        // A second file description for reading: virtio-console ports are
        // bidirectional, but one `File` cannot be read by the pump thread
        // while the writer half holds its lock.
        let inbound = BufReader::new(File::open(dev)?);
        Ok((
            Control {
                out: Arc::new(Mutex::new(out)),
            },
            inbound,
        ))
    }

    /// Open the control channel wherever the VMM put it, preferring
    /// `CONTROL_DEV`. Returns the device that answered alongside the halves.
    pub fn open() -> std::io::Result<(Control, BufReader<File>, &'static str)> {
        match Control::open_at(CONTROL_DEV) {
            Ok((c, r)) => Ok((c, r, CONTROL_DEV)),
            Err(first) => match Control::open_at(CONTROL_DEV_FALLBACK) {
                Ok((c, r)) => Ok((c, r, CONTROL_DEV_FALLBACK)),
                // The first device is the contract, so its error is the one
                // worth reporting; the fallback is a courtesy.
                Err(_) => Err(first),
            },
        }
    }

    /// Best-effort: a control channel that has gone away must never be the
    /// reason an app stops running.
    pub fn send(&self, line: &GuestLine) {
        if let Ok(mut out) = self.out.lock() {
            let _ = out.write_all(guest_line(line).as_bytes());
            let _ = out.flush();
        }
    }
}

/// The longest single host→guest line this guest will accumulate.
///
/// `read_line` has no bound of its own: a host that writes bytes and never a
/// newline — a bug, a wedged VMM device, or a chardev wired to the wrong
/// thing — would grow this `String` until the guest runs out of RAM, and an
/// OOM inside a VM kills the app with no diagnosis anybody can read.
///
/// **4 MiB, deliberately the same number as `SPEC_READ_CAP`, because it is
/// the same tree.** The biggest thing that crosses here is a `{"params":…}`
/// update, and the seed for that very tree arrives on a spec disk capped at
/// 4 MiB. The previous value was 1 MiB — a quarter of it — which is not
/// reachable today only because the host sends deltas; the moment it sends a
/// whole tree instead, the guest would stop at `LineTooLong` and `pump`
/// RETURNS on that, so the instance would lose every later signal and params
/// update for the rest of its life. A cap that is smaller than the thing it
/// is sized for is a trap with a fuse in it, and the fuse is somebody else's
/// future commit. `the_wire_cap_is_never_below_the_spec_disk_cap` in
/// `main.rs` fails if the two drift apart again.
pub const MAX_LINE: usize = 4 * 1024 * 1024;

/// Why `pump` stopped. It always stops for one of these, and every one of
/// them means the same thing operationally — **the host can no longer signal
/// this VM** — which is why the caller logs it rather than letting the thread
/// disappear.
#[derive(Debug)]
pub enum PumpEnd {
    /// The channel closed cleanly. Normal at shutdown; before that it means
    /// the host went away.
    Eof,
    /// A read failed, or a line was not UTF-8.
    Error(std::io::Error),
    /// `MAX_LINE` bytes arrived with no newline among them. The rest of that
    /// line would be read as if it were the start of a new one, so the loop
    /// stops instead of resynchronising onto a frame boundary it cannot find.
    LineTooLong,
    /// The channel closed part-way through a line: bytes arrived, then EOF,
    /// and no newline between them. Operationally this is `Eof` — the host
    /// went away — but it is a separate variant because reporting it as
    /// `LineTooLong` sends a reader after a megabyte-long line that does not
    /// exist, and because a host that closes mid-frame is a host-side bug
    /// worth naming. The partial line is discarded: an unterminated line is
    /// not a frame.
    EofMidLine { bytes: usize },
}

impl std::fmt::Display for PumpEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PumpEnd::Eof => write!(f, "the host closed the control channel"),
            PumpEnd::Error(e) => write!(f, "read: {e}"),
            PumpEnd::LineTooLong => {
                write!(f, "a host line exceeded {MAX_LINE} bytes with no newline")
            }
            PumpEnd::EofMidLine { bytes } => write!(
                f,
                "the host closed the control channel after {bytes} bytes with no newline; \
                 that partial line was discarded"
            ),
        }
    }
}

/// Read host→guest lines until the channel ends, handing each to `on_line`.
/// Unknown lines are skipped: the two sides version independently, and a line
/// this build does not understand must never end an instance.
///
/// Generic over `BufRead` rather than taking the `BufReader<File>` it is
/// always called with, so the framing, the cap and the reason it stops are
/// all testable over a `Cursor` on every platform CI runs — this file has no
/// Linux in it and should need no VM to be checked.
pub fn pump(mut inbound: impl BufRead, mut on_line: impl FnMut(HostLine)) -> PumpEnd {
    let mut line = String::new();
    loop {
        line.clear();
        // A fresh `take` per line, so the cap bounds ONE line rather than the
        // whole conversation.
        let read = (&mut inbound).take(MAX_LINE as u64).read_line(&mut line);
        match read {
            Ok(0) => return PumpEnd::Eof,
            Err(e) => return PumpEnd::Error(e),
            Ok(n) => {
                if !line.ends_with('\n') {
                    // Two different bugs share this branch and the remedies
                    // differ: `n == MAX_LINE` is the cap biting, anything
                    // less is EOF part-way through a line. The one case they
                    // cannot be told apart in is a close at exactly the cap,
                    // which reads as `LineTooLong` — the rarer of the two and
                    // the one whose message still points at real bytes.
                    return if n >= MAX_LINE {
                        PumpEnd::LineTooLong
                    } else {
                        PumpEnd::EofMidLine { bytes: n }
                    };
                }
                if let Some(parsed) = parse_host_line(&line) {
                    on_line(parsed);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn the_pump_says_why_it_stopped_instead_of_vanishing() {
        // Both background loops used to return on EOF with no log at all,
        // and "the host can no longer signal this VM" is indistinguishable
        // from "the app is busy" unless somebody says so. The reason is
        // returned rather than logged here because this half of the crate is
        // portable and `log` is PID 1's.
        let mut seen = Vec::new();
        let end = pump(
            Cursor::new(b"{\"signal\":\"TERM\"}\n{\"nonsense\":1}\n".to_vec()),
            |line| seen.push(line),
        );
        assert!(matches!(end, PumpEnd::Eof), "clean close is Eof");
        assert_eq!(
            seen,
            vec![HostLine::Signal {
                name: "TERM".into()
            }],
            "an unknown line is skipped, not fatal, and not delivered"
        );
    }

    #[test]
    fn a_host_that_never_sends_a_newline_cannot_grow_the_guest_heap_forever() {
        // `read_line` on its own is unbounded: a host — or a wedged VMM
        // device — that writes and never terminates a line would grow this
        // string until the guest OOMs, and an OOM inside a VM kills the app
        // with no diagnosis. The cap turns that into one line on the console.
        let flood = vec![b'x'; MAX_LINE + 4096];
        let end = pump(Cursor::new(flood), |_| {
            panic!("a line that never ended must never be delivered")
        });
        assert!(matches!(end, PumpEnd::LineTooLong));
    }

    /// A valid `{"params":…}` line of exactly `total` bytes, newline
    /// included, padded in the value so the JSON stays well-formed.
    fn params_line_of(total: usize) -> String {
        let fixed = "{\"params\":[[\"web\",[[\"k\",\"\"]]]]}\n".len();
        format!(
            "{{\"params\":[[\"web\",[[\"k\",\"{}\"]]]]}}\n",
            "v".repeat(total - fixed)
        )
    }

    #[test]
    fn a_line_that_is_exactly_at_the_cap_and_properly_ended_still_arrives() {
        // The cap must bound the damage, not clip legitimate traffic: a
        // params update for a large stack is a long line and a valid one.
        //
        // The name used to promise the boundary and the body tested a line
        // half the cap, which every off-by-one in the `take` survives. Both
        // edges are checked here instead: MAX_LINE - 1 and MAX_LINE exactly,
        // the largest line the framing can ever deliver.
        for total in [MAX_LINE - 1, MAX_LINE] {
            let line = params_line_of(total);
            assert_eq!(line.len(), total);
            let mut seen = 0;
            let end = pump(Cursor::new(line.into_bytes()), |_| seen += 1);
            assert!(
                matches!(end, PumpEnd::Eof),
                "a {total}-byte line ending in a newline is a whole line, got {end}"
            );
            assert_eq!(seen, 1, "and it must be delivered, not clipped");
        }
        // One byte more and it is the cap biting, not a delivery: the
        // newline lands outside the window `take` allows.
        let over = params_line_of(MAX_LINE + 1);
        let end = pump(Cursor::new(over.into_bytes()), |_| {
            panic!("a line past the cap must never be delivered")
        });
        assert!(matches!(end, PumpEnd::LineTooLong), "got {end}");
    }

    #[test]
    fn a_host_that_closes_mid_line_is_reported_as_a_close_not_as_an_overlong_line() {
        // A host that writes a line and closes without the newline used to
        // produce "a host line exceeded 1048576 bytes with no newline",
        // which sends whoever reads the console after a flood that never
        // happened. The bytes are the diagnosis worth printing.
        let end = pump(Cursor::new(b"{\"signal\":\"TERM\"}".to_vec()), |_| {
            panic!("an unterminated line is not a frame and must not be delivered")
        });
        let PumpEnd::EofMidLine { bytes } = end else {
            panic!("a close mid-line is not a cap violation: got {end}");
        };
        assert_eq!(bytes, 17);
        assert!(
            format!("{}", PumpEnd::EofMidLine { bytes }).contains("closed"),
            "the message must name the close, not a line length"
        );
        // A clean close between lines stays plain `Eof`, so the two really
        // are distinguishable and not just two names for one branch.
        let mut seen = 0;
        let end = pump(Cursor::new(b"{\"signal\":\"TERM\"}\n".to_vec()), |_| {
            seen += 1
        });
        assert!(matches!(end, PumpEnd::Eof), "got {end}");
        assert_eq!(seen, 1);
    }
}
