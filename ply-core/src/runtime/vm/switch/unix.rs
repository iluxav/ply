//! The `--vswitch` seam: one [`Switch`] in the `ply up` parent, one client
//! in each member's `ply run`, a unix socket between them.
//!
//! # Why the switch spans processes at all
//!
//! `ply up` spawns one `ply run` per member, and each of those boots its own
//! microVM. On Linux the members share a network because they share a
//! *namespace* — an object the kernel keeps alive and a child can join by
//! path. There is no such object on macOS: the network is userspace, it
//! lives in one process's memory, and the only way a sibling process joins
//! it is by talking to that process. So the parent listens, the children
//! connect, and the socket dies with the parent exactly as the netns does.
//!
//! # The wire
//!
//! One connection, one purpose, declared by a single ASCII line so that a
//! packet capture or a `nc` is enough to see what a member asked for:
//!
//! ```text
//!   PLYSW1 ping                        -> ok
//!   PLYSW1 member <slot> [<alias>]     -> ok <ip> <prefix> <gateway> <mac>
//!                                         then framed Ethernet, both ways
//!   PLYSW1 lookup <name>               -> ok <ip> | err <why>
//!   PLYSW1 dial <ip> <port> <ms>       -> ok | err <kind> <why>
//!                                         then raw bytes, both ways
//! ```
//!
//! `<kind>` is one of `refused`, `timeout`, `gone`, `other`, so that an
//! `io::ErrorKind` survives a wire that can carry only text — see
//! [`kind_token`].
//!
//! ```text
//! ```
//!
//! After a `member` handshake the connection carries nothing but
//! [`frame`]-framed Ethernet, which is ruling R0-2's format: a 4-byte
//! big-endian length and the raw frame, the same thing passt and gvproxy
//! speak.
//!
//! `dial` is the direction that surprises people. It is not egress — it is
//! the HOST reaching INTO a guest, which is what a `--publish` pool and an
//! `--after` port probe both need and what neither can do with
//! `TcpStream::connect`, because `10.77.0.2` names a machine that exists
//! only inside the parent's switch.

use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use smoltcp::wire::EthernetAddress;

use super::{frame, FrameSink, MemberLink, Switch, MEMBER_QUEUE, UPLINK_QUEUE};

/// The first word of every request. Versioned because the guest side of
/// this is a released binary and the host side is not: a future protocol
/// change must be refused with a readable line, not misread.
const HELLO: &str = "PLYSW1";

/// Longest handshake line either side will read. A `member` line is a name
/// and a verb; anything longer is a peer that is not speaking this protocol.
const MAX_LINE: usize = 512;

/// How long a connection may stay silent before its handshake is abandoned.
/// Without it a peer that connects and says nothing leaks a thread for the
/// life of the run.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a member's writer thread wakes to notice its socket has gone.
const WRITER_TICK: Duration = Duration::from_millis(200);

fn invalid(what: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.into())
}

/// The word an `err` reply leads with, so the KIND of a failure survives a
/// socket that can only carry text.
///
/// Refused has to stay refused. A health gate that reads "nothing is
/// listening" as "timed out" waits out its whole window on every failure
/// instead of retrying immediately, which turns a fast `--after` into a slow
/// one — and it is exactly the difference a message string alone loses,
/// because the switch's own words for the two cases ("nothing is listening
/// on …", "… did not answer") share no substring worth matching on.
fn kind_token(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::ConnectionRefused => "refused",
        io::ErrorKind::TimedOut => "timeout",
        io::ErrorKind::NotConnected => "gone",
        _ => "other",
    }
}

fn token_kind(token: &str) -> io::ErrorKind {
    match token {
        "refused" => io::ErrorKind::ConnectionRefused,
        "timeout" => io::ErrorKind::TimedOut,
        "gone" => io::ErrorKind::NotConnected,
        _ => io::ErrorKind::Other,
    }
}

/// Read one `\n`-terminated line, byte at a time.
///
/// Byte at a time on purpose: what follows the line on this socket is
/// framed Ethernet or raw application bytes, and a buffered reader would
/// swallow the first of it into a buffer the next reader does not own.
fn read_line(input: &mut impl Read, cap: usize) -> io::Result<String> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        input.read_exact(&mut byte)?;
        if byte[0] == b'\n' {
            break;
        }
        if out.len() >= cap {
            return Err(invalid("a handshake line longer than the protocol allows"));
        }
        out.push(byte[0]);
    }
    String::from_utf8(out).map_err(|_| invalid("a handshake line that is not UTF-8"))
}

/// `52:54:00:77:00:02` → the address. `None` for anything else.
fn parse_mac(text: &str) -> Option<EthernetAddress> {
    let mut out = [0u8; 6];
    let mut octets = text.split(':');
    for slot in out.iter_mut() {
        *slot = u8::from_str_radix(octets.next()?, 16).ok()?;
    }
    octets.next().is_none().then_some(EthernetAddress(out))
}

fn format_mac(mac: EthernetAddress) -> String {
    mac.0
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

// ------------------------------------------------------------------ server

/// A [`Switch`] plus the socket other processes reach it on.
///
/// The socket is optional and its absence is not fatal: a switch with no
/// listener is exactly what a standalone `ply run` had before this module
/// existed — one member, reachable from nothing but its own parent. `ply up`
/// checks [`Server::path`] and says so when it is `None`, because for a
/// stack that IS the whole feature.
pub struct Server {
    switch: Switch,
    socket: Option<Socket>,
}

struct Socket {
    path: PathBuf,
    stopping: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Start a switch and listen on `path`.
    ///
    /// Errors only when the switch's own thread cannot be spawned. A socket
    /// that cannot be bound is reported by `path()` being `None` and by the
    /// returned warning, because a run whose network merely cannot be
    /// SHARED should still boot.
    pub fn start(path: &Path) -> io::Result<(Server, Option<String>)> {
        let switch = Switch::start()?;
        match listen(path, &switch) {
            Ok(socket) => Ok((
                Server {
                    switch,
                    socket: Some(socket),
                },
                None,
            )),
            Err(e) => Ok((
                Server {
                    switch,
                    socket: None,
                },
                Some(format!("{}: {e}", path.display())),
            )),
        }
    }

    pub fn switch(&self) -> &Switch {
        &self.switch
    }

    pub fn path(&self) -> Option<&Path> {
        self.socket.as_ref().map(|s| s.path.as_path())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let Some(socket) = &mut self.socket else {
            return;
        };
        socket.stopping.store(true, Ordering::SeqCst);
        // `accept` is blocking, so the flag alone would not be seen until
        // the next member connected — which, at teardown, is never. One
        // connection of our own wakes it to read the flag and return.
        let _ = UnixStream::connect(&socket.path);
        let _ = std::fs::remove_file(&socket.path);
        if let Some(thread) = socket.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Unlink sockets in `dir` whose owning process is gone.
///
/// A `ply up` or `ply run` that is SIGKILLed never runs a destructor, so its
/// socket file outlives it. Nothing breaks — every path carries the pid that
/// made it, so a new run never collides with one — but without this the
/// directory grows a file per crash for as long as the machine is up.
///
/// A name whose pid cannot be read, or whose pid is alive, is left alone:
/// this must never be able to unlink a socket somebody is still listening on.
fn prune_dead(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str().and_then(|n| n.strip_suffix(".sock")) else {
            continue;
        };
        // `<pid>.sock` (a standalone run) or `up-<pid>.sock` (a stack).
        let Ok(pid) = name.trim_start_matches("up-").parse::<i32>() else {
            continue;
        };
        if unsafe { nix::libc::kill(pid, 0) } == 0 {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

fn listen(path: &Path, switch: &Switch) -> io::Result<Socket> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        prune_dead(dir);
    }
    // A socket file left by a `ply up` that was killed: `bind` fails with
    // EADDRINUSE on an existing path whether or not anything is listening,
    // so a stale one has to go. Ours is named for this process, so removing
    // it cannot take a live switch out from under another run.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // The switch is a way into every instance of this run; the state files
    // beside it are already 0600 and this is no less sensitive.
    let _ = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600));

    let stopping = Arc::new(AtomicBool::new(false));
    let thread_stop = stopping.clone();
    let thread_switch = switch.clone();
    let thread = std::thread::Builder::new()
        .name("ply-switch-accept".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(conn) = conn else { continue };
                let switch = thread_switch.clone();
                let _ = std::thread::Builder::new()
                    .name("ply-switch-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_one(conn, &switch) {
                            // Never fatal: one member speaking badly must not
                            // take the stack's network down. Quiet on a clean
                            // close, which is what every member does at exit.
                            if e.kind() != io::ErrorKind::UnexpectedEof {
                                eprintln!("ply: switch: {e}");
                            }
                        }
                    });
            }
        })?;
    Ok(Socket {
        path: path.to_path_buf(),
        stopping,
        thread: Some(thread),
    })
}

/// One accepted connection, from its first line to its last byte.
fn serve_one(mut conn: UnixStream, switch: &Switch) -> io::Result<()> {
    conn.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let line = read_line(&mut conn, MAX_LINE)?;
    let mut words = line.split_whitespace();
    if words.next() != Some(HELLO) {
        let _ = writeln!(conn, "err this is a ply switch socket, speaking {HELLO}");
        return Err(invalid(format!("a peer that greeted with {line:?}")));
    }
    match words.next() {
        Some("ping") => {
            writeln!(conn, "ok")?;
            Ok(())
        }
        Some("lookup") => {
            let name = words.next().unwrap_or_default();
            match switch.lookup(name) {
                Some(ip) => writeln!(conn, "ok {ip}")?,
                None => writeln!(conn, "err no member named {name}")?,
            }
            Ok(())
        }
        Some("member") => {
            let Some(slot) = words.next() else {
                let _ = writeln!(conn, "err member needs a name");
                return Err(invalid("a member handshake with no name"));
            };
            let link = switch.attach(slot);
            if let Some(alias) = words.next() {
                switch.alias(alias, link.ip);
            }
            writeln!(
                conn,
                "ok {} {} {} {}",
                link.ip,
                link.prefix_len,
                link.gateway,
                format_mac(link.mac)
            )?;
            // The handshake budget was for the handshake. A member is
            // silent for as long as its guest has nothing to send.
            conn.set_read_timeout(None)?;
            pump_member(conn, link)
        }
        Some("dial") => {
            let target = (|| {
                let ip: Ipv4Addr = words.next()?.parse().ok()?;
                let port: u16 = words.next()?.parse().ok()?;
                let ms: u64 = words.next().unwrap_or("5000").parse().ok()?;
                Some((ip, port, Duration::from_millis(ms)))
            })();
            let Some((ip, port, timeout)) = target else {
                let _ = writeln!(conn, "err dial needs <ip> <port> [timeout_ms]");
                return Err(invalid(format!("a malformed dial: {line:?}")));
            };
            match switch.connect(ip, port, timeout) {
                Ok(guest) => {
                    writeln!(conn, "ok")?;
                    conn.set_read_timeout(None)?;
                    bridge(conn, guest)
                }
                Err(e) => {
                    writeln!(conn, "err {} {e}", kind_token(e.kind()))?;
                    Ok(())
                }
            }
        }
        other => {
            let _ = writeln!(conn, "err unknown request");
            Err(invalid(format!("an unknown request: {other:?}")))
        }
    }
}

/// Frames both ways for one member, until the socket closes.
///
/// The writer runs on a thread of its own and the reader on this one, so
/// neither direction can stall the other — a guest that stops reading must
/// not stop the switch from hearing what its peers send.
fn pump_member(conn: UnixStream, link: MemberLink) -> io::Result<()> {
    let gone = Arc::new(AtomicBool::new(false));
    let writer_gone = gone.clone();
    let mut out = conn.try_clone()?;
    let writer = std::thread::Builder::new()
        .name("ply-switch-tx".into())
        .spawn(move || {
            loop {
                match link.rx.recv_timeout(WRITER_TICK) {
                    Ok(bytes) => {
                        if frame::write_frame(&mut out, &bytes).is_err() {
                            break;
                        }
                    }
                    // Ticks exist only so a dead socket is noticed by a
                    // member whose guest has gone quiet.
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if writer_gone.load(Ordering::SeqCst) {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            // Dropping `link` here is what takes this member off the fabric:
            // its receiver goes, the switch's next send to it fails as
            // disconnected, and `Loop::send_to` removes it.
            let _ = out.shutdown(std::net::Shutdown::Both);
        })?;

    let mut input = conn;
    let mut buf = Vec::new();
    let result = loop {
        match frame::read_frame(&mut input, &mut buf) {
            Ok(_) => {
                link.tx.send(std::mem::take(&mut buf));
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    gone.store(true, Ordering::SeqCst);
    let _ = input.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
    result
}

/// Copy bytes both ways between two streams until each side has said its
/// last word, then let both drop.
fn bridge(a: UnixStream, b: UnixStream) -> io::Result<()> {
    let (a_in, mut a_out) = (a.try_clone()?, a);
    let (b_in, mut b_out) = (b.try_clone()?, b);
    let up = std::thread::Builder::new()
        .name("ply-switch-dial".into())
        .spawn(move || {
            let mut a_in = a_in;
            let _ = io::copy(&mut a_in, &mut b_out);
            // A half close, not a close: the far end may still have an
            // answer to finish sending, and a full shutdown here would cut
            // a reply short. This is the same discipline `publish::serve`
            // uses between a client and a backend.
            let _ = b_out.shutdown(std::net::Shutdown::Write);
        })?;
    let mut b_in = b_in;
    let _ = io::copy(&mut b_in, &mut a_out);
    let _ = a_out.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
    Ok(())
}

// ------------------------------------------------------------------ client

/// The far side of a `--vswitch` socket: a member's `ply run`.
///
/// Holds a path and nothing else. Every operation opens its own connection,
/// which is what lets a `dial` block for the life of a published connection
/// while a `lookup` beside it answers immediately, with no multiplexing and
/// no framing to get wrong.
#[derive(Clone, Debug)]
pub struct Client {
    path: PathBuf,
}

impl Client {
    /// Check that a switch is actually listening on `path`, and hand back a
    /// handle to it.
    ///
    /// The `ping` is the point. A `--vswitch` that is not answering means
    /// the parent died or the path is wrong, and a member that quietly ran
    /// on a private network instead would look wired up and talk only to
    /// itself — with `<peer>.ply` resolving to nothing and no error
    /// anywhere.
    pub fn connect(path: &Path) -> io::Result<Client> {
        let client = Client {
            path: path.to_path_buf(),
        };
        let (mut conn, reply) = client.request("ping")?;
        let _ = &mut conn;
        match reply.as_str() {
            "ok" => Ok(client),
            other => Err(invalid(format!(
                "the switch at {} answered a ping with {other:?}",
                path.display()
            ))),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Open a connection and send one request line; hand back the socket
    /// and the reply line.
    fn request(&self, request: &str) -> io::Result<(UnixStream, String)> {
        let mut conn = UnixStream::connect(&self.path)?;
        conn.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        writeln!(conn, "{HELLO} {request}")?;
        let reply = read_line(&mut conn, MAX_LINE)?;
        Ok((conn, reply))
    }

    /// This name's address on the switch, or `None` when nothing has joined
    /// under it.
    pub fn lookup(&self, name: &str) -> io::Result<Option<Ipv4Addr>> {
        let (_, reply) = self.request(&format!("lookup {name}"))?;
        Ok(reply
            .strip_prefix("ok ")
            .and_then(|rest| rest.trim().parse().ok()))
    }

    /// Join the switch as `slot`, with `alias` pointing at the same address.
    pub fn attach(&self, slot: &str, alias: &str) -> io::Result<MemberLink> {
        let (conn, reply) = self.request(&format!("member {slot} {alias}"))?;
        let rest = reply
            .strip_prefix("ok ")
            .ok_or_else(|| invalid(format!("the switch refused a member: {reply}")))?;
        let mut fields = rest.split_whitespace();
        let parsed = (|| {
            let ip: Ipv4Addr = fields.next()?.parse().ok()?;
            let prefix_len: u8 = fields.next()?.parse().ok()?;
            let gateway: Ipv4Addr = fields.next()?.parse().ok()?;
            let mac = parse_mac(fields.next()?)?;
            Some((ip, prefix_len, gateway, mac))
        })();
        let Some((ip, prefix_len, gateway, mac)) = parsed else {
            return Err(invalid(format!(
                "the switch answered a member handshake with {reply:?}"
            )));
        };
        conn.set_read_timeout(None)?;

        // Uplink: frames the device sends, drained by the writer thread.
        // Bounded, and the sink drops when it is full — the same rule the
        // in-process sink applies, one hop earlier.
        let (uplink, to_switch) = mpsc::sync_channel::<Vec<u8>>(UPLINK_QUEUE);
        // Downlink: frames the switch sends this member. Bounded for the
        // same reason `Switch::attach`'s is — a guest with a full ring
        // drops, it does not grow a queue in its parent.
        let (from_switch, downlink) = mpsc::sync_channel::<Vec<u8>>(MEMBER_QUEUE);

        let mut out = conn.try_clone()?;
        std::thread::Builder::new()
            .name("ply-vswitch-tx".into())
            .spawn(move || {
                while let Ok(bytes) = to_switch.recv() {
                    if frame::write_frame(&mut out, &bytes).is_err() {
                        break;
                    }
                }
                let _ = out.shutdown(std::net::Shutdown::Both);
            })?;
        let mut input = conn;
        std::thread::Builder::new()
            .name("ply-vswitch-rx".into())
            .spawn(move || {
                let mut buf = Vec::new();
                while frame::read_frame(&mut input, &mut buf).is_ok() {
                    match from_switch.try_send(std::mem::take(&mut buf)) {
                        Ok(()) => {}
                        // A device that is behind. Drop the frame and carry
                        // on — what a NIC does with a full ring, and what
                        // the switch itself does. Ending the thread here
                        // instead would take the guest's network down for
                        // good on one busy moment, with TCP left retrying
                        // into a link that will never come back.
                        Err(mpsc::TrySendError::Full(_)) => {}
                        // The instance is gone; nothing will read again.
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
                let _ = input.shutdown(std::net::Shutdown::Both);
            })?;

        Ok(MemberLink {
            ip,
            mac,
            gateway,
            prefix_len,
            tx: FrameSink::remote(uplink),
            rx: downlink,
        })
    }

    /// Ask the switch to open a TCP connection to `ip:port` inside the
    /// network, and hand back this end of it.
    pub fn dial(&self, ip: Ipv4Addr, port: u16, timeout: Duration) -> io::Result<UnixStream> {
        let (conn, reply) = self.request(&format!(
            "dial {ip} {port} {}",
            timeout.as_millis().min(u128::from(u32::MAX))
        ))?;
        if reply != "ok" {
            let rest = reply.strip_prefix("err ").unwrap_or(&reply);
            let (token, why) = rest.split_once(' ').unwrap_or((rest, rest));
            return Err(io::Error::new(token_kind(token), why.to_string()));
        }
        conn.set_read_timeout(None)?;
        Ok(conn)
    }
}

/// Open a TCP connection to `ip:port` on the switch listening at `socket`.
///
/// The free-function form, for callers that have an address and a socket
/// path out of an instance state file and no [`Client`]: `--after`'s port
/// probe, which runs in a `ply run` parent that is not even on the same
/// switch.
pub fn dial(socket: &Path, ip: Ipv4Addr, port: u16, timeout: Duration) -> io::Result<UnixStream> {
    Client {
        path: socket.to_path_buf(),
    }
    .dial(ip, port, timeout)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{start_guest, GuestCommand};
    use super::*;
    use std::sync::mpsc;

    /// A socket path under this test's own temporary directory, removed
    /// with it.
    ///
    /// Deliberately SHORT, and in `/tmp` rather than under `temp_dir()`. A
    /// unix socket path has to fit `sockaddr_un.sun_path` — 104 bytes on
    /// macOS, 108 on Linux — and macOS's `TMPDIR` is a ~50-character
    /// per-user path before a test has added a word of its own.
    struct Sock {
        dir: PathBuf,
    }

    /// Distinct per socket, because two tests running side by side must not
    /// pick the same directory and a thread id is not short enough to spell.
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    impl Sock {
        fn new(_name: &str) -> Sock {
            let dir = PathBuf::from(format!(
                "/tmp/plysw{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a scratch directory");
            Sock { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("s")
        }
    }

    impl Drop for Sock {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn server(sock: &Sock) -> Server {
        let (server, warning) = Server::start(&sock.path()).expect("a switch");
        assert!(warning.is_none(), "the socket must bind: {warning:?}");
        assert_eq!(server.path(), Some(sock.path().as_path()));
        server
    }

    #[test]
    fn a_mac_survives_the_wire_unchanged() {
        let mac = super::super::member_mac(Ipv4Addr::new(10, 77, 1, 9));
        assert_eq!(format_mac(mac), "52:54:00:77:01:09");
        assert_eq!(parse_mac(&format_mac(mac)), Some(mac));
        // The failures that matter are the ones that would otherwise be read
        // as a valid address: too few octets, too many, and not hex.
        assert_eq!(parse_mac("52:54:00:77:01"), None);
        assert_eq!(parse_mac("52:54:00:77:01:09:11"), None);
        assert_eq!(parse_mac("52:54:00:77:01:zz"), None);
    }

    #[test]
    fn a_peer_that_is_not_speaking_the_protocol_is_refused_and_told_so() {
        let sock = Sock::new("greeting");
        let _server = server(&sock);
        let mut conn = UnixStream::connect(sock.path()).expect("connect");
        writeln!(conn, "GET / HTTP/1.1").expect("write");
        let reply = read_line(&mut conn, MAX_LINE).expect("a reply");
        assert!(reply.starts_with("err "), "{reply}");
        assert!(
            reply.contains(HELLO),
            "the error names the protocol: {reply}"
        );
    }

    /// A client that cannot reach a switch must SAY so. A member that fell
    /// back to a private network instead would boot, look healthy, resolve
    /// none of its peers, and report nothing at all.
    #[test]
    fn connecting_to_a_socket_with_no_switch_behind_it_fails() {
        let sock = Sock::new("absent");
        assert!(Client::connect(&sock.path()).is_err());
    }

    /// Addresses are the whole point of joining someone else's switch: a
    /// member must get the address the switch already reserved for its
    /// name, not a fresh one.
    #[test]
    fn a_member_joins_over_the_socket_at_the_address_the_switch_reserved() {
        let sock = Sock::new("join");
        let server = server(&sock);
        // What `ply up` does before it spawns anything: reserve every
        // member's address so a peer's `/etc/hosts` line is right even when
        // that peer has not started yet.
        let db = server.switch().allocate("db.1");
        server.switch().alias("db", db);

        let client = Client::connect(&sock.path()).expect("a client");
        assert_eq!(client.lookup("db").expect("lookup"), Some(db));
        assert_eq!(client.lookup("nobody").expect("lookup"), None);

        let link = client.attach("db.1", "db").expect("attach");
        assert_eq!(link.ip, db, "the reserved address, not a new one");
        assert_eq!(link.gateway, super::super::GATEWAY);
        assert_eq!(link.prefix_len, super::super::PREFIX_LEN);
        assert_eq!(link.mac, super::super::member_mac(db));
    }

    /// **The property `<name>.ply` between two members rests on.** Two
    /// members, each on its own socket connection, and a frame one sends
    /// arrives at the other — through the framing, the fabric, and back out
    /// of a second socket.
    #[test]
    fn a_frame_crosses_from_one_socket_member_to_another() {
        let sock = Sock::new("l2");
        let _server = server(&sock);
        let client = Client::connect(&sock.path()).expect("a client");
        let db = client.attach("db.1", "db").expect("db joins");
        let web = client.attach("web.1", "web").expect("web joins");
        assert_ne!(db.ip, web.ip, "two members are two machines");

        // 1. web asks who has db's address. The switch answers ARP for
        //    every address it handed out, so the reply comes back over web's
        //    own socket without db having said a word — which is what lets a
        //    peer be named before it has finished booting.
        let request = super::super::tests::arp_request(web.mac, web.ip, db.ip);
        assert!(web.tx.send(request), "the switch took the frame");
        let reply = super::super::tests::wait_for_frame(&web.rx, Duration::from_secs(5))
            .expect("an ARP reply over web's socket");
        let eth = smoltcp::wire::EthernetFrame::new_checked(&reply[..]).expect("a frame");
        assert_eq!(eth.ethertype(), smoltcp::wire::EthernetProtocol::Arp);
        assert_eq!(eth.src_addr(), db.mac, "answered for db's own MAC");

        // 2. And a frame web then sends to that MAC arrives at db — out of
        //    one socket, through the fabric, into another. This is the hop
        //    `<peer>.ply` traffic actually takes.
        let payload = super::super::tests::unicast(web.mac, db.mac);
        assert!(web.tx.send(payload.clone()), "the switch took the frame");
        assert_eq!(
            super::super::tests::wait_for_frame(&db.rx, Duration::from_secs(5)),
            Some(payload),
            "unchanged on the way"
        );
    }

    /// **The property `--publish` and `--after` rest on.** The switch is in
    /// this process, the guest is behind a socket, and a THIRD party — a
    /// client with nothing but the socket path — opens a TCP connection to
    /// it and gets bytes back.
    #[test]
    fn a_dial_over_the_socket_reaches_a_guest_that_joined_over_the_socket() {
        let sock = Sock::new("dial");
        let _server = server(&sock);
        let client = Client::connect(&sock.path()).expect("a client");
        let link = client.attach("db.1", "db").expect("db joins");
        let ip = link.ip;
        let (done, echoed) = mpsc::channel();
        let guest = start_guest(link);
        guest
            .commands
            .send(GuestCommand::Echo { port: 5432, done })
            .expect("order the echo server up");

        // `dial`, not `Client::dial`, because this is the shape `--after`
        // uses: a socket path out of a state file and nothing else.
        let mut stream = dial(&sock.path(), ip, 5432, Duration::from_secs(10))
            .expect("the switch dials the guest for us");
        stream.write_all(b"select 1").expect("write");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("half close");
        let mut back = Vec::new();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream.read_to_end(&mut back).expect("read the echo");
        assert_eq!(back, b"select 1");
        assert_eq!(
            echoed.recv_timeout(Duration::from_secs(5)).expect("guest"),
            b"select 1"
        );
    }

    /// A closed port has to stay CLOSED across the socket. A probe that
    /// cannot tell "nothing is listening" from "still connecting" waits out
    /// its whole window on every failure, which is what turns a fast
    /// `--after` into a slow one.
    #[test]
    fn a_dial_to_a_port_nothing_is_listening_on_is_refused_across_the_socket() {
        let sock = Sock::new("refused");
        let _server = server(&sock);
        let client = Client::connect(&sock.path()).expect("a client");
        let link = client.attach("db.1", "db").expect("db joins");
        let ip = link.ip;
        let _guest = start_guest(link);
        let err = dial(&sock.path(), ip, 5999, Duration::from_secs(3))
            .expect_err("nothing is listening there");
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err:?}");
    }

    /// **`--after`'s port probe, end to end, with no VM in sight.**
    ///
    /// The gate runs in a `ply run` that is not on this switch and holds
    /// nothing but what the dependency's state file said: an address and a
    /// socket path. Reaching the guest is only possible through the second
    /// of those, and the third assertion is the one that matters — with the
    /// socket path dropped, the very same live instance reads as "not
    /// answering", which is exactly how a `[health] port` dependency behaved
    /// on macOS before `InstanceState::network` existed.
    #[test]
    fn an_after_gates_port_probe_reaches_a_guest_through_the_switch() {
        use crate::runtime::after::{readiness_of, Endpoint, Readiness};

        let sock = Sock::new("after");
        let _server = server(&sock);
        let client = Client::connect(&sock.path()).expect("a client");
        let link = client.attach("db.1", "db").expect("db joins");
        let ip = link.ip;
        let (done, _echoed) = mpsc::channel();
        let guest = start_guest(link);
        guest
            .commands
            .send(GuestCommand::Echo { port: 5432, done })
            .expect("order the echo server up");

        let me = std::process::id() as i32;
        let through = Endpoint {
            pid: me,
            ip,
            via: Some(sock.path()),
        };
        // The probe's budget is 300 ms and the guest is still bringing its
        // listener up, so retry rather than sleep. A probe that fails
        // consumes nothing; only the one that succeeds takes the connection.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let ready = loop {
            if matches!(
                readiness_of(std::slice::from_ref(&through), Some(5432)),
                Readiness::Ready
            ) {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(
            ready,
            "the probe never reached the guest through the switch"
        );

        // A port nothing is listening on stays unhealthy — the gate must not
        // be reporting "the switch answered" as "the app answered".
        assert!(matches!(
            readiness_of(std::slice::from_ref(&through), Some(5999)),
            Readiness::Unhealthy(_)
        ));

        // And the load-bearing half: the same instance, the same address,
        // no socket to route through.
        assert!(
            matches!(
                readiness_of(&[Endpoint { pid: me, ip, via: None }], Some(5432)),
                Readiness::Unhealthy(_)
            ),
            "a switch address is not dialable from the host; if this passes,              the probe is reaching something other than the guest"
        );
    }

    /// A member that leaves and comes back — a `restart: always` app, or a
    /// slot the supervisor relaunched — must return to the SAME address, or
    /// every peer that copied it into `/etc/hosts` at boot is now dialling
    /// nowhere.
    #[test]
    fn a_member_that_rejoins_comes_back_on_the_address_its_peers_know() {
        let sock = Sock::new("rejoin");
        let _server = server(&sock);
        let client = Client::connect(&sock.path()).expect("a client");
        let first = client.attach("db.1", "db").expect("db joins");
        let ip = first.ip;
        drop(first);
        // Another member joining in between must not be handed the address
        // the departed one is coming back to.
        let other = client.attach("web.1", "web").expect("web joins");
        assert_ne!(other.ip, ip);
        let again = client.attach("db.1", "db").expect("db rejoins");
        assert_eq!(again.ip, ip);
    }
}
