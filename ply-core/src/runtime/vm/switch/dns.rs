//! The switch's resolver: `<name>.ply` answered from the switch's own
//! name→address table, everything else forwarded to the host's resolver.
//!
//! Portable and hand-rolled rather than pulled from a DNS crate, because
//! what is needed is small and exactly specified: read one question, and
//! either write one A record or hand the query, unchanged, to a real
//! resolver and hand the answer back. Every offset below is from RFC 1035
//! §4.1 and each is pinned by a test on a byte string, not on this module's
//! own encoder.
//!
//! # Why the guest cannot talk to a resolver directly
//!
//! It has no route to one: the switch is the only thing on its network, and
//! the switch is the only thing that has host sockets. So a query for
//! `deb.debian.org` arrives here on UDP 53 and leaves this process as a
//! query from the Mac. `<name>.ply` never leaves at all.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// The suffix a ply stack member answers to. `<name>.ply` is resolved from
/// the switch's table and is never forwarded — a name that is ours and
/// unknown is `NXDOMAIN`, not a question for the internet, which would
/// otherwise leak a stack's member names to the host's resolver.
pub const PLY_SUFFIX: &str = ".ply";

/// TTL on the answers the switch makes up. One minute: long enough that a
/// busy app is not re-asking constantly, short enough that a member which
/// restarts on a new address is not shadowed for the rest of the run.
const TTL: u32 = 60;

/// The only question type the switch answers with a record of its own —
/// nothing on this network has an IPv6 address, so AAAA and everything else
/// get an empty NOERROR.
const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

/// One parsed question, and where the question section ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Lower-cased, dot-joined, no trailing dot: `db.ply`.
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// Offset one past the question, i.e. where an answer section starts.
    pub end: usize,
}

/// Read the single question out of a query.
///
/// `None` for anything this switch will not answer: a response rather than a
/// query, no question or more than one, a compressed name (a client never
/// compresses the question, so a pointer here is a malformed query), or a
/// truncated message. Those are forwarded upstream untouched rather than
/// guessed at.
pub fn parse_question(msg: &[u8]) -> Option<Question> {
    if msg.len() < 12 {
        return None;
    }
    // QR (bit 15 of the flags word) set means this is an answer.
    if msg[2] & 0x80 != 0 {
        return None;
    }
    if u16::from_be_bytes([msg[4], msg[5]]) != 1 {
        return None; // exactly one question, which is all any resolver sends
    }
    let mut at = 12usize;
    let mut labels: Vec<String> = Vec::new();
    loop {
        let len = *msg.get(at)? as usize;
        at += 1;
        if len == 0 {
            break;
        }
        // 0b11xxxxxx is a compression pointer. A question is the first thing
        // in the message, so there is nothing before it to point at.
        if len & 0xc0 != 0 {
            return None;
        }
        let label = msg.get(at..at + len)?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        at += len;
    }
    let qtype = u16::from_be_bytes([*msg.get(at)?, *msg.get(at + 1)?]);
    let qclass = u16::from_be_bytes([*msg.get(at + 2)?, *msg.get(at + 3)?]);
    Some(Question {
        name: labels.join("."),
        qtype,
        qclass,
        end: at + 4,
    })
}

/// Is this a name the switch owns?
pub fn is_ply_name(name: &str) -> bool {
    name.len() > PLY_SUFFIX.len() && name.ends_with(PLY_SUFFIX)
}

/// The member name a `<name>.ply` question is asking about.
pub fn ply_member(name: &str) -> Option<&str> {
    is_ply_name(name).then(|| &name[..name.len() - PLY_SUFFIX.len()])
}

/// Header + the question copied verbatim, with the response bits set.
///
/// `rcode` is the low nibble of the second flags byte; `answers` goes in
/// ANCOUNT. Everything else is zeroed, so a query that arrived with an EDNS
/// OPT record in its additional section gets an answer without one — legal,
/// and it keeps the reply inside 512 bytes.
fn respond(query: &[u8], question: &Question, rcode: u8, answers: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(question.end + 16);
    out.extend_from_slice(&query[..question.end]);
    // QR=1, AA=1, RD copied from the query, RA=1.
    out[2] = 0x84 | (query[2] & 0x01);
    out[3] = 0x80 | (rcode & 0x0f);
    out[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    out[6..8].copy_from_slice(&answers.to_be_bytes()); // ANCOUNT
    out[8..12].copy_from_slice(&[0, 0, 0, 0]); // NSCOUNT, ARCOUNT
    out
}

/// One A record for `ip`, answering `question`.
pub fn answer_a(query: &[u8], question: &Question, ip: Ipv4Addr) -> Vec<u8> {
    let mut out = respond(query, question, 0, 1);
    // A pointer to offset 12 — where the question's name starts, because
    // `respond` copied the header and the question and nothing else.
    out.extend_from_slice(&[0xc0, 0x0c]);
    out.extend_from_slice(&TYPE_A.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.extend_from_slice(&TTL.to_be_bytes());
    out.extend_from_slice(&4u16.to_be_bytes());
    out.extend_from_slice(&ip.octets());
    out
}

/// The name exists but has no record of this type — what a `<name>.ply`
/// AAAA question gets, since nothing on this switch has an IPv6 address.
/// NOERROR with no answers, which is what stops a resolver retrying.
pub fn answer_empty(query: &[u8], question: &Question) -> Vec<u8> {
    respond(query, question, 0, 0)
}

/// No such name. Reserved for `<name>.ply` names the switch does not know:
/// forwarding one upstream would leak a stack's member names to the host's
/// resolver and could not be answered anyway.
pub fn nxdomain(query: &[u8], question: &Question) -> Vec<u8> {
    respond(query, question, 3, 0)
}

/// What the switch does with one query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Send these bytes back to the guest.
    Reply(Vec<u8>),
    /// Ask the host's resolver and relay whatever it says.
    Forward,
}

/// Decide one query against the switch's name table.
///
/// `lookup` is the switch's `<name>` → address map. Everything that is not a
/// `.ply` name is forwarded, including a name this switch could not parse:
/// the resolver on the other side is a real one and will do better than a
/// guess made here.
pub fn decide(query: &[u8], lookup: impl Fn(&str) -> Option<Ipv4Addr>) -> Answer {
    let Some(question) = parse_question(query) else {
        return Answer::Forward;
    };
    let Some(member) = ply_member(&question.name) else {
        return Answer::Forward;
    };
    if question.qclass != CLASS_IN {
        return Answer::Reply(nxdomain(query, &question));
    }
    match (lookup(member), question.qtype) {
        (Some(ip), TYPE_A) => Answer::Reply(answer_a(query, &question, ip)),
        // The name is ours and it exists; it just has no AAAA. Saying
        // NXDOMAIN here would make a dual-stack resolver believe the name
        // does not exist at all.
        (Some(_), _) => Answer::Reply(answer_empty(query, &question)),
        (None, _) => Answer::Reply(nxdomain(query, &question)),
    }
}

/// The resolvers this host uses, in order.
///
/// `/etc/resolv.conf` first, because it is the one file both platforms
/// agree on and on this Mac it is generated from the live configuration.
/// `scutil --dns` is the macOS fallback for the case where it is not (a
/// machine whose only resolver arrived through a VPN's split DNS, say).
pub fn upstream_resolvers() -> Vec<SocketAddr> {
    let configured = std::fs::read_to_string("/etc/resolv.conf")
        .map(|text| parse_resolv_conf(&text))
        .unwrap_or_default();
    if !configured.is_empty() {
        return configured;
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("/usr/sbin/scutil")
        .arg("--dns")
        .output()
    {
        return parse_scutil(&String::from_utf8_lossy(&out.stdout));
    }
    // Empty: this host declares no resolver, so the switch has nowhere to
    // forward to and says so by answering nothing. Inventing a public
    // resolver here would send a guest's queries somewhere its operator
    // never chose.
    configured
}

/// `nameserver <ip>` lines, IPv4 only — the guest has no IPv6 address, so a
/// v6 resolver is unreachable from here even though the host can use it.
pub fn parse_resolv_conf(text: &str) -> Vec<SocketAddr> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter_map(|line| line.strip_prefix("nameserver"))
        .filter_map(|rest| rest.trim().parse::<Ipv4Addr>().ok())
        .map(|ip| SocketAddr::from((ip, 53)))
        .collect()
}

/// `nameserver[0] : 1.2.3.4` lines out of `scutil --dns`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn parse_scutil(text: &str) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("nameserver[") {
            continue;
        }
        let Some((_, value)) = line.split_once(':') else {
            continue;
        };
        if let Ok(ip) = value.trim().parse::<Ipv4Addr>() {
            let addr = SocketAddr::from((ip, 53));
            if !out.contains(&addr) {
                out.push(addr);
            }
        }
    }
    out
}

/// Ask the host's resolvers and return the first answer.
///
/// Blocking, and therefore called on a worker thread, never on the switch's
/// own: a resolver that is slow must not stop the switch forwarding frames.
pub fn forward(
    query: &[u8],
    resolvers: &[SocketAddr],
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    for resolver in resolvers {
        let bind: SocketAddr = if resolver.is_ipv4() {
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
        } else {
            SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
        };
        let Ok(sock) = UdpSocket::bind(bind) else {
            continue;
        };
        if sock.set_read_timeout(Some(timeout)).is_err() {
            continue;
        }
        if sock.send_to(query, resolver).is_err() {
            continue;
        }
        // Up to 512 bytes is the classic limit; ask for a bigger buffer so an
        // EDNS answer is not silently truncated by us as well as by the
        // resolver.
        let mut buf = vec![0u8; 4096];
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from.ip() == resolver.ip() && n >= 12 => {
                buf.truncate(n);
                return Some(buf);
            }
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not in the module above because the switch never answers one; it is
    /// here because the *behaviour* for an AAAA question is under test.
    const TYPE_AAAA: u16 = 28;

    /// `db.ply` A IN, id 0x1234, RD set — the exact bytes a musl resolver
    /// puts on the wire, written out by hand rather than by this module's
    /// own encoder, which would only prove it agrees with itself.
    fn query_db_ply(qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.push(2);
        q.extend_from_slice(b"db");
        q.push(3);
        q.extend_from_slice(b"ply");
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    #[test]
    fn a_question_is_read_out_of_the_bytes_a_resolver_actually_sends() {
        let q = parse_question(&query_db_ply(TYPE_A)).expect("a question");
        assert_eq!(q.name, "db.ply");
        assert_eq!(q.qtype, TYPE_A);
        assert_eq!(q.qclass, CLASS_IN);
        assert_eq!(q.end, query_db_ply(TYPE_A).len());
    }

    #[test]
    fn a_name_is_matched_case_insensitively() {
        // 0x20 randomisation ("dNs-0x20") is real: some resolvers vary the
        // case of the question to make spoofing harder, and the answer must
        // still be ours.
        let mut q = query_db_ply(TYPE_A);
        q[13] = b'D';
        assert_eq!(parse_question(&q).expect("question").name, "db.ply");
    }

    #[test]
    fn a_ply_name_is_answered_from_the_switchs_own_table() {
        let query = query_db_ply(TYPE_A);
        let Answer::Reply(reply) = decide(&query, |n| {
            (n == "db").then_some(Ipv4Addr::new(10, 77, 0, 2))
        }) else {
            panic!("a .ply name must never be forwarded");
        };
        // Same transaction id, QR set, one answer.
        assert_eq!(&reply[..2], &[0x12, 0x34]);
        assert_eq!(reply[2] & 0x80, 0x80, "QR");
        assert_eq!(reply[3] & 0x0f, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 1, "ANCOUNT");
        // The record: a pointer to the question's name, A/IN, then 4 bytes
        // of address.
        let rr = &reply[query.len()..];
        assert_eq!(&rr[..2], &[0xc0, 0x0c]);
        assert_eq!(u16::from_be_bytes([rr[2], rr[3]]), TYPE_A);
        assert_eq!(u16::from_be_bytes([rr[4], rr[5]]), CLASS_IN);
        assert_eq!(u16::from_be_bytes([rr[10], rr[11]]), 4, "RDLENGTH");
        assert_eq!(&rr[12..16], &[10, 77, 0, 2]);
    }

    #[test]
    fn an_unknown_ply_name_is_nxdomain_and_is_never_forwarded() {
        // Forwarding it would leak a stack's member names to the host's
        // resolver, and no resolver out there could answer it anyway.
        let query = query_db_ply(TYPE_A);
        let Answer::Reply(reply) = decide(&query, |_| None) else {
            panic!("a .ply name must never be forwarded");
        };
        assert_eq!(reply[3] & 0x0f, 3, "NXDOMAIN");
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 0, "no answers");
    }

    #[test]
    fn a_known_ply_name_with_no_aaaa_is_empty_not_nxdomain() {
        // NXDOMAIN for AAAA tells a dual-stack resolver the name does not
        // exist at all, and some of them then stop asking for the A too.
        let query = query_db_ply(TYPE_AAAA);
        let Answer::Reply(reply) = decide(&query, |_| Some(Ipv4Addr::new(10, 77, 0, 2))) else {
            panic!("a .ply name must never be forwarded");
        };
        assert_eq!(reply[3] & 0x0f, 0, "NOERROR");
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 0, "no answers");
    }

    #[test]
    fn anything_that_is_not_a_ply_name_is_forwarded() {
        let mut q = vec![0x00, 0x01, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        q.push(11);
        q.extend_from_slice(b"deb.debian");
        q.push(3);
        q.extend_from_slice(b"org");
        q.push(0);
        q.extend_from_slice(&TYPE_A.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        assert_eq!(decide(&q, |_| None), Answer::Forward);
        // And so is something this module cannot read at all: a real
        // resolver will do better with it than a guess made here.
        assert_eq!(decide(b"\x00\x01", |_| None), Answer::Forward);
    }

    #[test]
    fn resolvers_come_from_the_hosts_own_configuration() {
        let text = "# generated\nnameserver 8.8.8.8\nsearch example.com\n\
                    nameserver fe80::1\nnameserver 1.1.1.1 # trailing\n";
        assert_eq!(
            parse_resolv_conf(text),
            vec![
                SocketAddr::from(([8, 8, 8, 8], 53)),
                SocketAddr::from(([1, 1, 1, 1], 53)),
            ],
            "IPv6 resolvers are dropped: the guest has no IPv6 address"
        );
        assert_eq!(
            parse_scutil(
                "resolver #1\n  nameserver[0] : 192.168.1.1\n  nameserver[0] : 192.168.1.1\n"
            ),
            vec![SocketAddr::from(([192, 168, 1, 1], 53))],
            "scutil lists a resolver once per scope; the duplicates are one server"
        );
    }
}
