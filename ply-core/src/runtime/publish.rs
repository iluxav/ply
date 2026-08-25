//! `ply run --publish` — opt-in L4 pool load balancer in the run parent.
//!
//! The parent binds the host port and TCP-splices connections across its
//! instances. It uniquely owns pool truth (it forked the instances and
//! orchestrates rolls), so backend selection needs no discovery or reload:
//! the pool map is updated as instances launch and die. Scope is a hard
//! line: TCP only — no TLS, no HTTP, no hostnames (that's the edge's job;
//! this is port *exposure*, not a proxy).

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// Who can reach a published port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindScope {
    /// `0.0.0.0` — anyone who can reach this host. What a public web port
    /// wants, and what a database never does.
    Public,
    /// Other ply apps on this host only: loopback rootless (instances share
    /// the host netns), the bridge gateway rootful (instances reach the host
    /// there). Same address the depending app is told to use.
    Internal,
    /// An explicit address, for anything the two presets do not cover.
    Addr(Ipv4Addr),
}

impl BindScope {
    /// The address to bind, once rootless-vs-rootful is known.
    pub fn bind_addr(&self, rootless: bool) -> Ipv4Addr {
        match self {
            BindScope::Public => Ipv4Addr::UNSPECIFIED,
            BindScope::Internal if rootless => Ipv4Addr::LOCALHOST,
            BindScope::Internal => crate::runtime::network::GATEWAY,
            BindScope::Addr(a) => *a,
        }
    }

    /// The address a *depending* app should connect to. Identical to the bind
    /// address except for Public, which binds the wildcard but is reached at a
    /// concrete one.
    pub fn connect_addr(&self, rootless: bool) -> Ipv4Addr {
        match self {
            BindScope::Public if rootless => Ipv4Addr::LOCALHOST,
            BindScope::Public => crate::runtime::network::GATEWAY,
            other => other.bind_addr(rootless),
        }
    }
}

/// Parsed `--publish` spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publish {
    /// Port the parent binds on the host.
    pub host_port: u16,
    /// Port instances serve on (rootful: on their bridge IP; rootless: the
    /// parent allocates a distinct loopback port per instance instead).
    pub instance_port: u16,
    /// The spec named the instance port outright (`5434:5432`) rather than it
    /// defaulting to the host port. Rootless that is a promise about where the
    /// app listens, and ply keeps it instead of injecting `PORT` — an imported
    /// Docker image binds a fixed port and ignores `PORT` entirely, so
    /// injecting would aim the pool at a socket nobody ever opened.
    pub instance_port_explicit: bool,
    /// Who may reach `host_port`. Defaults to Public for backwards
    /// compatibility — `--publish internal:5432` is how a database opts out
    /// of being on the internet.
    pub scope: BindScope,
}

/// `PORT` | `HOST_PORT:INSTANCE_PORT` | `ADDR:PORT` | `ADDR:HOST_PORT:INSTANCE_PORT`
/// where ADDR is `internal`, `public`, or an IPv4 address. A leading segment
/// that parses as a port number is a port, never an address — so the existing
/// `80:3100` grammar is untouched.
pub fn parse_publish(s: &str) -> Result<Publish> {
    let bad = || {
        Error::Runtime(format!(
            "--publish `{s}`: expected PORT, HOST_PORT:INSTANCE_PORT, or ADDR:PORT[:INSTANCE_PORT] \
             where ADDR is `internal`, `public` or an IPv4 address \
             (e.g. 3100, 80:3100, internal:5432, 127.0.0.1:8080:3000)"
        ))
    };
    let parse_port = |p: &str| p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad);
    let parse_scope = |a: &str| match a {
        "internal" => Some(BindScope::Internal),
        "public" => Some(BindScope::Public),
        other => other.parse::<Ipv4Addr>().ok().map(BindScope::Addr),
    };

    let parts: Vec<&str> = s.split(':').collect();
    // An address may only lead, and only when it is not itself a port.
    let (scope, ports): (BindScope, &[&str]) = match parts.first() {
        Some(first) if first.parse::<u16>().is_err() => {
            (parse_scope(first).ok_or_else(bad)?, &parts[1..])
        }
        _ => (BindScope::Public, &parts[..]),
    };

    match ports {
        [port] => {
            let port = parse_port(port)?;
            Ok(Publish {
                host_port: port,
                instance_port: port,
                scope,
                instance_port_explicit: false,
            })
        }
        [host, instance] => Ok(Publish {
            host_port: parse_port(host)?,
            instance_port: parse_port(instance)?,
            scope,
            instance_port_explicit: true,
        }),
        _ => Err(bad()),
    }
}

/// The live backend set, shared between the run loop (writer) and the
/// accept loop (reader). Keyed by slot so removal is exact.
#[derive(Clone, Default)]
pub struct Pool {
    backends: Arc<Mutex<BTreeMap<u32, SocketAddr>>>,
    counter: Arc<AtomicUsize>,
}

impl Pool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, slot: u32, addr: SocketAddr) {
        self.backends.lock().unwrap().insert(slot, addr);
    }

    pub fn remove(&self, slot: u32) {
        self.backends.lock().unwrap().remove(&slot);
    }

    /// Backends in round-robin order: each call starts one position later.
    /// The whole list is returned so the caller can fail over down it.
    pub fn rotated(&self) -> Vec<SocketAddr> {
        let backends: Vec<SocketAddr> = self.backends.lock().unwrap().values().copied().collect();
        if backends.is_empty() {
            return backends;
        }
        let start = self.counter.fetch_add(1, Ordering::Relaxed) % backends.len();
        let mut out = Vec::with_capacity(backends.len());
        out.extend_from_slice(&backends[start..]);
        out.extend_from_slice(&backends[..start]);
        out
    }
}

/// Reserve a free loopback port (rootless: one per instance, injected as
/// PORT). Bind-then-drop has a theoretical reuse race; in practice the
/// instance rebinds it within milliseconds.
pub fn allocate_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| Error::Runtime(format!("allocating a loopback port: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Runtime(format!("allocating a loopback port: {e}")))?
        .port();
    Ok(port)
}

/// Bind the published host port. Separate from `serve` so the claim fails
/// fast (before any instance starts) with a clear message.
pub fn bind(spec: Publish, rootless: bool) -> Result<TcpListener> {
    let addr = spec.scope.bind_addr(rootless);
    let port = spec.host_port;
    TcpListener::bind((addr, port)).map_err(|e| {
        Error::Runtime(format!(
            "--publish {port}: cannot bind {addr}:{port}: {e} — published ports are real host ports (one owner per port)"
        ))
    })
}

/// Accept loop — runs on its own thread for the parent's lifetime. Each
/// connection tries the pool in round-robin order and splices bytes; an
/// unreachable backend (booting, dying, mid-roll) is skipped.
pub fn serve(listener: TcpListener, pool: Pool) {
    for conn in listener.incoming() {
        let Ok(client) = conn else { continue };
        let pool = pool.clone();
        std::thread::spawn(move || {
            let _ = client.set_nodelay(true);
            for addr in pool.rotated() {
                match TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)) {
                    Ok(upstream) => {
                        let _ = upstream.set_nodelay(true);
                        relay(client, upstream);
                        return;
                    }
                    Err(_) => continue, // next backend
                }
            }
            // No reachable backend: the connection drops (client sees EOF).
        });
    }
}

/// Bidirectional byte copy; each direction's EOF shuts down the paired
/// write side so the counterpart copy terminates.
fn relay(client: TcpStream, upstream: TcpStream) {
    let (Ok(mut c_read), Ok(mut u_read)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let (mut c_write, mut u_write) = (client, upstream);
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_read, &mut u_write);
        let _ = u_write.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut u_read, &mut c_write);
    let _ = c_write.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn rotation_starts_one_later_each_call() {
        let pool = Pool::new();
        let a: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:1".parse().unwrap();
        pool.insert(1, a);
        pool.insert(2, b);
        assert_eq!(pool.rotated()[0], a);
        assert_eq!(pool.rotated()[0], b);
        assert_eq!(pool.rotated()[0], a);
        pool.remove(1);
        assert_eq!(pool.rotated(), vec![b]);
    }

    /// One echo backend: accepts connections forever, echoes one read back.
    fn echo_backend() -> (SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                counter.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut buf = [0u8; 256];
                    if let Ok(n) = conn.read(&mut buf) {
                        let _ = conn.write_all(&buf[..n]);
                    }
                });
            }
        });
        (addr, hits)
    }

    fn roundtrip(via: SocketAddr, msg: &[u8]) -> Vec<u8> {
        let mut c = TcpStream::connect(via).unwrap();
        c.write_all(msg).unwrap();
        c.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = Vec::new();
        c.read_to_end(&mut out).unwrap();
        out
    }

    #[test]
    fn the_existing_port_grammar_is_untouched() {
        assert_eq!(
            parse_publish("3100").unwrap(),
            Publish {
                host_port: 3100,
                instance_port: 3100,
                scope: BindScope::Public,
                instance_port_explicit: false,
            }
        );
        assert_eq!(
            parse_publish("80:3100").unwrap(),
            Publish {
                host_port: 80,
                instance_port: 3100,
                scope: BindScope::Public,
                instance_port_explicit: true,
            }
        );
    }

    #[test]
    fn a_database_can_opt_out_of_the_internet() {
        // `--publish 5432` puts postgres on 0.0.0.0. This is the way out.
        assert_eq!(
            parse_publish("internal:5432").unwrap(),
            Publish {
                host_port: 5432,
                instance_port: 5432,
                scope: BindScope::Internal,
                instance_port_explicit: false,
            }
        );
        assert_eq!(
            parse_publish("internal:5432:5432").unwrap(),
            Publish {
                host_port: 5432,
                instance_port: 5432,
                scope: BindScope::Internal,
                instance_port_explicit: true,
            }
        );
    }

    #[test]
    fn an_explicit_address_binds_exactly_it() {
        let p = parse_publish("127.0.0.1:8080:3000").unwrap();
        assert_eq!(p.scope, BindScope::Addr(Ipv4Addr::LOCALHOST));
        assert_eq!((p.host_port, p.instance_port), (8080, 3000));
        assert_eq!(parse_publish("public:80").unwrap().scope, BindScope::Public);
    }

    #[test]
    fn internal_resolves_to_whatever_the_mode_can_reach() {
        // rootless shares the host netns; rootful instances reach the host
        // at the bridge gateway. Binding the wrong one silently isolates.
        assert_eq!(BindScope::Internal.bind_addr(true), Ipv4Addr::LOCALHOST);
        assert_eq!(
            BindScope::Internal.bind_addr(false),
            crate::runtime::network::GATEWAY
        );
        // Public binds the wildcard but is *reached* at a concrete address
        assert_eq!(BindScope::Public.bind_addr(true), Ipv4Addr::UNSPECIFIED);
        assert_eq!(BindScope::Public.connect_addr(true), Ipv4Addr::LOCALHOST);
        assert_eq!(
            BindScope::Public.connect_addr(false),
            crate::runtime::network::GATEWAY
        );
    }

    #[test]
    fn naming_the_instance_port_is_a_promise_ply_keeps() {
        // `--publish internal:5434` leaves ply free to inject PORT and move
        // the app; `internal:5434:5432` says postgres is on 5432 and always
        // will be, because an imported image ignores PORT entirely.
        assert!(
            !parse_publish("internal:5434")
                .unwrap()
                .instance_port_explicit
        );
        assert!(
            parse_publish("internal:5434:5432")
                .unwrap()
                .instance_port_explicit
        );
        assert!(parse_publish("80:3000").unwrap().instance_port_explicit);
        assert!(!parse_publish("3100").unwrap().instance_port_explicit);
    }

    #[test]
    fn nonsense_specs_are_refused_with_the_grammar() {
        for bad in [
            "0",
            "nope",
            "80:",
            "internal:",
            "internal:0",
            "1.2.3",
            "a:b:c",
            "80:90:100",
        ] {
            assert!(parse_publish(bad).is_err(), "`{bad}` should not parse");
        }
        let err = parse_publish("nope").unwrap_err().to_string();
        assert!(err.contains("internal"), "error names the new forms: {err}");
    }

    #[test]
    fn balances_and_fails_over() {
        let (backend_a, hits_a) = echo_backend();
        let (backend_b, hits_b) = echo_backend();
        let pool = Pool::new();
        pool.insert(1, backend_a);
        pool.insert(2, backend_b);

        let front = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let front_addr = front.local_addr().unwrap();
        let serve_pool = pool.clone();
        std::thread::spawn(move || serve(front, serve_pool));

        for i in 0..4 {
            assert_eq!(roundtrip(front_addr, b"ping"), b"ping", "conn {i}");
        }
        assert_eq!(hits_a.load(Ordering::SeqCst), 2, "round-robin split");
        assert_eq!(hits_b.load(Ordering::SeqCst), 2, "round-robin split");

        // Kill backend A (simulate a dead instance still in the pool for a
        // moment): connections must fail over to B.
        pool.remove(1);
        pool.insert(1, "127.0.0.1:1".parse().unwrap()); // nothing listens here
        for _ in 0..4 {
            assert_eq!(roundtrip(front_addr, b"pong"), b"pong");
        }
        assert_eq!(hits_b.load(Ordering::SeqCst), 6, "all traffic on B");

        // Empty pool: the connection is dropped without a byte served — as
        // clean EOF or RST (close with unread data resets), never a hang.
        pool.remove(1);
        pool.remove(2);
        let mut c = TcpStream::connect(front_addr).unwrap();
        let _ = c.write_all(b"x");
        let mut out = Vec::new();
        match c.read_to_end(&mut out) {
            Ok(_) => assert!(out.is_empty()),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
        }
    }
}
