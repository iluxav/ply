//! DNS wire handling for the egress forwarder: enough of RFC 1035 to read the
//! question, pull the A records out of an answer, build an error reply, frame
//! TCP and ask an upstream once over UDP. Pure and panic-free: every read is
//! bounds-checked, and a malformed message yields `None` or an empty vec. The
//! policy (what is allowed) and the thread that serves 127.0.0.53 live
//! elsewhere.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

pub const A: u16 = 1;
pub const AAAA: u16 = 28;

/// The fixed DNS header: ID, flags, and the four section counts.
pub const HEADER: usize = 12;
/// A pointer chase this long is a loop, not a name.
const MAX_POINTER_HOPS: usize = 64;
/// RFC 1035's limit on a presentation-form name.
const MAX_NAME: usize = 255;
/// Replies with the wrong transaction id we will read past before giving up on
/// an upstream. Bounded so a chatty peer cannot hold the thread forever.
const MAX_STRAY_REPLIES: usize = 4;
/// The largest UDP payload we will accept from an upstream.
const MAX_UDP: usize = 65_535;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Lowercase, no trailing dot.
    pub name: String,
    pub qtype: u16,
}

fn be16(msg: &[u8], off: usize) -> Option<u16> {
    let hi = *msg.get(off)?;
    let lo = *msg.get(off.checked_add(1)?)?;
    Some(u16::from_be_bytes([hi, lo]))
}

fn be32(msg: &[u8], off: usize) -> Option<u32> {
    let hi = be16(msg, off)?;
    let lo = be16(msg, off.checked_add(2)?)?;
    Some((u32::from(hi) << 16) | u32::from(lo))
}

/// Read the name at `offset`, following compression pointers. Returns the
/// lowercase dotless name and the offset just past the name *in its own
/// record* — that is, past the first pointer when one was followed.
///
/// Terminates on any input: each pointer costs a hop and the hops are capped,
/// so a name pointing at itself unwinds instead of spinning.
fn read_name(msg: &[u8], offset: usize) -> Option<(String, usize)> {
    let mut name = String::new();
    let mut pos = offset;
    let mut after: Option<usize> = None;
    let mut hops = 0usize;
    loop {
        let len = *msg.get(pos)?;
        match len & 0xc0 {
            0x00 => {
                if len == 0 {
                    let end = match after {
                        Some(end) => end,
                        None => pos.checked_add(1)?,
                    };
                    return Some((name, end));
                }
                let start = pos.checked_add(1)?;
                let end = start.checked_add(usize::from(len))?;
                let label = msg.get(start..end)?;
                if !name.is_empty() {
                    name.push('.');
                }
                for b in label {
                    name.push(char::from(b.to_ascii_lowercase()));
                }
                if name.len() > MAX_NAME {
                    return None;
                }
                pos = end;
            }
            0xc0 => {
                let lo = *msg.get(pos.checked_add(1)?)?;
                let target = ((usize::from(len) & 0x3f) << 8) | usize::from(lo);
                if target >= msg.len() {
                    return None;
                }
                if after.is_none() {
                    after = Some(pos.checked_add(2)?);
                }
                hops += 1;
                if hops > MAX_POINTER_HOPS {
                    return None;
                }
                pos = target;
            }
            // 0x40 and 0x80 are reserved label types we do not speak.
            _ => return None,
        }
    }
}

/// End of an *uncompressed* name at `offset`, or `None` if it runs past the
/// message or uses a pointer. Used when copying a question section verbatim:
/// a copied pointer would dangle in the new message.
fn plain_name_end(msg: &[u8], offset: usize) -> Option<usize> {
    let mut pos = offset;
    loop {
        let len = *msg.get(pos)?;
        if len == 0 {
            return pos.checked_add(1);
        }
        if len & 0xc0 != 0 {
            return None;
        }
        pos = pos.checked_add(1)?.checked_add(usize::from(len))?;
    }
}

/// The first question of a DNS message, or `None` if there isn't a whole one.
///
/// A caller that makes a policy decision on the result must refuse messages whose
/// QDCOUNT is not exactly 1: only the first question is vetted here, and the
/// forwarded bytes carry all of them.
pub fn question(msg: &[u8]) -> Option<Question> {
    if msg.len() < HEADER || be16(msg, 4)? == 0 {
        return None;
    }
    let (name, off) = read_name(msg, HEADER)?;
    let qtype = be16(msg, off)?;
    be16(msg, off.checked_add(2)?)?; // the class must be there too
    Some(Question { name, qtype })
}

/// How many questions the message claims, or `None` if it is too short to say.
pub fn qdcount(msg: &[u8]) -> Option<u16> {
    be16(msg, 4)
}

/// Every A record in the answer section, with its TTL, in wire order.
/// Compression-aware; empty for a malformed message or one with no answers.
pub fn a_records(msg: &[u8]) -> Vec<(Ipv4Addr, u32)> {
    read_a_records(msg).unwrap_or_default()
}

fn read_a_records(msg: &[u8]) -> Option<Vec<(Ipv4Addr, u32)>> {
    if msg.len() < HEADER {
        return None;
    }
    let questions = be16(msg, 4)?;
    let answers = be16(msg, 6)?;
    let mut pos = HEADER;
    for _ in 0..questions {
        let (_, off) = read_name(msg, pos)?;
        pos = off.checked_add(4)?; // QTYPE + QCLASS
        if pos > msg.len() {
            return None;
        }
    }
    let mut out = Vec::new();
    for _ in 0..answers {
        let (_, off) = read_name(msg, pos)?;
        let rtype = be16(msg, off)?;
        be16(msg, off.checked_add(2)?)?; // class
        let ttl = be32(msg, off.checked_add(4)?)?;
        let rdlen = usize::from(be16(msg, off.checked_add(8)?)?);
        let rdata = off.checked_add(10)?;
        let end = rdata.checked_add(rdlen)?;
        let body = msg.get(rdata..end)?;
        if rtype == A && body.len() == 4 {
            out.push((Ipv4Addr::new(body[0], body[1], body[2], body[3]), ttl));
        }
        pos = end;
    }
    Some(out)
}

/// A reply to `query` carrying nothing but `rcode`: the id and question are
/// kept so the resolver can match it, every section count but QD is zeroed.
/// `2` is SERVFAIL, `5` is REFUSED.
pub fn error_reply(query: &[u8], rcode: u8) -> Vec<u8> {
    let mut out = vec![0u8; HEADER];
    let copied = query.len().min(HEADER);
    out[..copied].copy_from_slice(&query[..copied]);
    // QR=1, RA=1; keep OPCODE and RD; clear AA, TC and Z.
    out[2] = 0x80 | (out[2] & 0x79);
    out[3] = 0x80 | (rcode & 0x0f);
    let question = plain_name_end(query, HEADER)
        .and_then(|off| off.checked_add(4))
        .and_then(|end| query.get(HEADER..end));
    let qdcount: u16 = if question.is_some() { 1 } else { 0 };
    out[4..6].copy_from_slice(&qdcount.to_be_bytes());
    out[6..12].fill(0); // AN, NS, AR
    if let Some(q) = question {
        out.extend_from_slice(q);
    }
    out
}

/// The reply for a name the policy does not allow.
pub fn refused_reply(query: &[u8]) -> Vec<u8> {
    error_reply(query, 5)
}

/// A DNS-over-TCP frame: two big-endian length bytes, then the message.
pub fn tcp_frame(msg: &[u8]) -> Vec<u8> {
    let len = msg.len().min(usize::from(u16::MAX));
    let mut out = Vec::with_capacity(2 + len);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(&msg[..len]);
    out
}

/// Split one whole frame off the front of `buf`, or `None` while it is still
/// arriving. Returns the message and whatever followed it.
pub fn tcp_unframe(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = usize::from(be16(buf, 0)?);
    let end = len.checked_add(2)?;
    if buf.len() < end {
        return None;
    }
    Some((&buf[2..end], &buf[end..]))
}

/// The source port the per-instance forwarder asks its upstreams from.
///
/// A fixed port is what lets the policy table tell the forwarder's queries
/// from the app's: the table accepts `udp sport 35353` to an upstream and
/// nothing else, and the forwarder holds the port for its whole life, so an
/// app that ignores `resolv.conf` and dials an upstream itself falls through
/// to the ordinary rules and is counted and dropped like any other
/// destination.
pub const UPSTREAM_SPORT: u16 = 35_353;

/// Send `query` to each upstream in turn over `sock` and return the first
/// reply that came FROM the upstream just asked and carries the query's
/// transaction id. Gives up on an upstream after `timeout` with no answer and
/// moves to the next; reports the last error if none answers.
///
/// The whole call is bounded by `timeout` per upstream — a stray-reply storm
/// on a known port cannot stretch it further, because each wait is the
/// shorter of the upstream's own patience and what is left overall.
///
/// The socket is not connected — one socket serves every query the forwarder
/// makes, so the kernel cannot filter senders for us and the source check
/// here is what a connected socket would have given us for free.
pub fn forward_via(
    sock: &UdpSocket,
    query: &[u8],
    upstreams: &[(Ipv4Addr, u16)],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let mut last = io::Error::new(io::ErrorKind::TimedOut, "no DNS upstream answered");
    let budget = timeout.saturating_mul(u32::try_from(upstreams.len()).unwrap_or(u32::MAX));
    let deadline = Instant::now() + budget;
    for (addr, port) in upstreams {
        match ask(
            sock,
            query,
            SocketAddrV4::new(*addr, *port),
            timeout,
            deadline,
        ) {
            Ok(reply) => return Ok(reply),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// `forward_via` on a socket of its own, bound to an ephemeral port.
pub fn forward_once_to(
    query: &[u8],
    upstreams: &[(Ipv4Addr, u16)],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    forward_via(&sock, query, upstreams, timeout)
}

/// `forward_once_to` on the standard DNS port.
pub fn forward_once(
    query: &[u8],
    upstreams: &[Ipv4Addr],
    timeout: Duration,
) -> io::Result<Vec<u8>> {
    let with_port: Vec<(Ipv4Addr, u16)> = upstreams.iter().map(|a| (*a, 53)).collect();
    forward_once_to(query, &with_port, timeout)
}

fn ask(
    sock: &UdpSocket,
    query: &[u8],
    upstream: SocketAddrV4,
    timeout: Duration,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    sock.send_to(query, upstream)?;
    let want = query.get(..2);
    let mut buf = vec![0u8; MAX_UDP];
    for _ in 0..MAX_STRAY_REPLIES {
        // This upstream's own patience, never past the call's deadline —
        // and never zero, which the kernel reads as "wait for ever".
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{upstream}: no answer within the deadline"),
            ));
        }
        sock.set_read_timeout(Some(timeout.min(left).max(Duration::from_millis(1))))?;
        let (n, from) = sock.recv_from(&mut buf)?;
        // A reply is an answer only if it came from the upstream we just
        // asked AND carries our transaction id: anyone inside the namespace
        // can send this port a datagram.
        let from_upstream = matches!(from, SocketAddr::V4(v4) if v4 == upstream);
        if from_upstream && n >= HEADER && want.is_none_or(|id| buf[..2] == *id) {
            buf.truncate(n);
            return Ok(buf);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{upstream}: no reply with a matching transaction id"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn name_bytes(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in name.split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }
    fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut m = vec![
            (id >> 8) as u8,
            id as u8,
            0x01,
            0x00,
            0,
            1,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        m.extend(name_bytes(name));
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        m
    }
    fn answer_with_a(name: &str, records: &[(Ipv4Addr, u32)]) -> Vec<u8> {
        let mut m = query(0xbeef, name, A);
        m[2] = 0x81;
        m[3] = 0x80; // QR, RD, RA
        m[7] = records.len() as u8; // ANCOUNT
        for (addr, ttl) in records {
            m.extend_from_slice(&[0xc0, 0x0c]); // pointer to the question name
            m.extend_from_slice(&[0, 1, 0, 1]); // A, IN
            m.extend_from_slice(&ttl.to_be_bytes());
            m.extend_from_slice(&[0, 4]);
            m.extend_from_slice(&addr.octets());
        }
        m
    }

    #[test]
    fn the_question_is_read_lowercase_without_the_dot() {
        let q = question(&query(7, "API.Stripe.COM", A)).unwrap();
        assert_eq!(q.name, "api.stripe.com");
        assert_eq!(q.qtype, A);
        assert!(question(&[0, 1, 2]).is_none());
    }

    #[test]
    fn qdcount_is_readable_on_its_own_so_a_caller_can_refuse_multi_question_queries() {
        assert_eq!(qdcount(&query(7, "api.stripe.com", A)), Some(1));
        assert_eq!(qdcount(&[0, 1, 2]), None);
    }

    #[test]
    fn a_records_follow_compression_pointers_and_keep_ttls() {
        let msg = answer_with_a(
            "api.stripe.com",
            &[
                (Ipv4Addr::new(54, 187, 174, 169), 60),
                (Ipv4Addr::new(54, 187, 174, 170), 3600),
            ],
        );
        assert_eq!(
            a_records(&msg),
            vec![
                (Ipv4Addr::new(54, 187, 174, 169), 60),
                (Ipv4Addr::new(54, 187, 174, 170), 3600)
            ]
        );
        assert!(a_records(&query(1, "x.example", A)).is_empty());
    }

    #[test]
    fn a_refused_reply_keeps_the_id_and_the_question_and_says_refused() {
        let q = query(0x1234, "evil.example", A);
        let r = refused_reply(&q);
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80, "QR");
        assert_eq!(r[3] & 0x0f, 5, "RCODE REFUSED");
        assert_eq!(&r[4..12], &[0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&r[12..], &q[12..]);
        assert_eq!(question(&r).unwrap().name, "evil.example");
        // the thread also needs SERVFAIL out of the same builder
        assert_eq!(error_reply(&q, 2)[3] & 0x0f, 2, "RCODE SERVFAIL");
    }

    #[test]
    fn tcp_framing_round_trips_and_waits_for_a_whole_frame() {
        let msg = query(1, "a.example", A);
        let framed = tcp_frame(&msg);
        assert_eq!(&framed[..2], &(msg.len() as u16).to_be_bytes());
        let (got, rest) = tcp_unframe(&framed).unwrap();
        assert_eq!(got, &msg[..]);
        assert!(rest.is_empty());
        assert!(tcp_unframe(&framed[..framed.len() - 1]).is_none());
    }

    #[test]
    fn forward_once_talks_to_the_first_upstream_that_answers() {
        // a fake upstream on loopback that echoes the query with QR set
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = sock.recv_from(&mut buf).unwrap();
            buf[2] |= 0x80;
            sock.send_to(&buf[..n], from).unwrap();
        });
        let q = query(9, "a.example", A);
        let reply = forward_once_to(
            &q,
            &[("127.0.0.1".parse().unwrap(), port)],
            std::time::Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(reply[2] & 0x80, 0x80);
        assert_eq!(&reply[..2], &q[..2]);
    }

    #[test]
    fn a_pointer_loop_and_a_runaway_label_are_refused_not_a_panic() {
        // A name at offset 12 that is nothing but a pointer to itself.
        let mut looped = query(1, "a.example", A);
        looped.truncate(HEADER);
        looped.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1]);
        assert!(question(&looped).is_none());
        assert!(a_records(&looped).is_empty());

        // Two names that point at each other.
        let mut pingpong = query(1, "a.example", A);
        pingpong.truncate(HEADER);
        pingpong.extend_from_slice(&[0xc0, 0x0e, 0xc0, 0x0c]);
        assert!(question(&pingpong).is_none());

        // A label whose length runs past the end of the message.
        let mut runaway = query(1, "a.example", A);
        runaway.truncate(HEADER);
        runaway.extend_from_slice(&[0x05, b'a', b'b']);
        assert!(question(&runaway).is_none());
        assert!(a_records(&runaway).is_empty());
        assert_eq!(refused_reply(&runaway).len(), HEADER, "no question copied");

        // A reserved label type (0x40, 0x80) is not a name we speak.
        let mut reserved = query(1, "a.example", A);
        reserved.truncate(HEADER);
        reserved.extend_from_slice(&[0x40, b'a', 0, 0, 1, 0, 1]);
        assert!(question(&reserved).is_none());

        // A pointer past the end, and a truncated one.
        let mut off_end = query(1, "a.example", A);
        off_end.truncate(HEADER);
        off_end.extend_from_slice(&[0xc0, 0xff]);
        assert!(question(&off_end).is_none());
        assert!(question(&[0u8; HEADER]).is_none());

        // An answer whose RDLENGTH claims more than is there.
        let mut short_rdata = answer_with_a("a.example", &[(Ipv4Addr::new(1, 2, 3, 4), 60)]);
        let n = short_rdata.len();
        short_rdata[n - 6] = 0xff; // RDLENGTH high byte
        assert!(a_records(&short_rdata).is_empty());
    }

    #[test]
    fn an_error_reply_survives_a_query_that_is_not_one() {
        assert_eq!(refused_reply(&[]).len(), HEADER);
        assert_eq!(refused_reply(&[0x12, 0x34])[..2], [0x12, 0x34]);
        // A truncated question is dropped rather than copied half-way.
        let mut half = query(3, "a.example", A);
        half.truncate(HEADER + 4);
        let r = refused_reply(&half);
        assert_eq!(r.len(), HEADER);
        assert_eq!(&r[4..12], &[0, 0, 0, 0, 0, 0, 0, 0], "QDCOUNT 0");
    }

    #[test]
    fn tcp_unframe_returns_the_rest_of_the_stream() {
        let a = query(1, "a.example", A);
        let b = query(2, "b.example", AAAA);
        let mut stream = tcp_frame(&a);
        stream.extend_from_slice(&tcp_frame(&b));
        let (first, rest) = tcp_unframe(&stream).unwrap();
        assert_eq!(first, &a[..]);
        let (second, rest) = tcp_unframe(rest).unwrap();
        assert_eq!(second, &b[..]);
        assert!(rest.is_empty());
        assert!(tcp_unframe(&[0x00]).is_none());
    }

    /// The forwarder asks from ONE socket on a fixed port for its whole
    /// life (the table keys on that source port); a reply to it is an answer
    /// like any other.
    #[test]
    fn forward_via_answers_on_a_socket_of_the_callers_own() {
        let up = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = up.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            // one stray from a stranger first, then the real answer
            let (n, from) = up.recv_from(&mut buf).unwrap();
            let stray = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            stray.send_to(&buf[..n], from).unwrap();
            buf[2] |= 0x80;
            up.send_to(&buf[..n], from).unwrap();
        });
        let mine = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let q = query(7, "a.example", A);
        let reply = forward_via(
            &mine,
            &q,
            &[(Ipv4Addr::LOCALHOST, port)],
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(&reply[..2], &q[..2]);
        assert_eq!(reply[2] & 0x80, 0x80, "the stranger's copy was not taken");
        // the same socket serves the next query too
        assert!(forward_via(&mine, &q, &[], Duration::from_millis(50)).is_err());
    }

    #[test]
    fn forward_once_to_gives_up_when_nobody_answers() {
        let err =
            forward_once_to(&query(1, "a.example", A), &[], Duration::from_millis(50)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        // A closed loopback port: the send is fine, the wait is not.
        let dead = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = dead.local_addr().unwrap().port();
        drop(dead);
        assert!(forward_once_to(
            &query(1, "a.example", A),
            &[(Ipv4Addr::LOCALHOST, port)],
            Duration::from_millis(200)
        )
        .is_err());
    }
}
