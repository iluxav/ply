//! `ply run --publish` — opt-in L4 pool load balancer in the run parent.
//!
//! The parent binds the host port and TCP-splices connections across its
//! instances. It uniquely owns pool truth (it forked the instances and
//! orchestrates rolls), so backend selection needs no discovery or reload:
//! the pool map is updated as instances launch and die. Scope is a hard
//! line: TCP only — no TLS, no HTTP, no hostnames (that's the edge's job;
//! this is port *exposure*, not a proxy).

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};

/// Parsed `--publish` spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Publish {
    /// Port the parent binds on the host (0.0.0.0).
    pub host_port: u16,
    /// Port instances serve on (rootful: on their bridge IP; rootless: the
    /// parent allocates a distinct loopback port per instance instead).
    pub instance_port: u16,
}

/// `"3100"` = same port both sides; `"80:3100"` = host:instance.
pub fn parse_publish(s: &str) -> Result<Publish> {
    let bad = || {
        Error::Runtime(format!(
            "--publish `{s}`: expected PORT or HOST_PORT:INSTANCE_PORT, e.g. 3100 or 80:3100"
        ))
    };
    let parse_port = |p: &str| p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad);
    match s.split_once(':') {
        None => {
            let port = parse_port(s)?;
            Ok(Publish {
                host_port: port,
                instance_port: port,
            })
        }
        Some((host, instance)) => Ok(Publish {
            host_port: parse_port(host)?,
            instance_port: parse_port(instance)?,
        }),
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
pub fn bind(host_port: u16) -> Result<TcpListener> {
    TcpListener::bind(("0.0.0.0", host_port)).map_err(|e| {
        Error::Runtime(format!(
            "--publish {host_port}: cannot bind 0.0.0.0:{host_port}: {e} — published ports are real host ports (one owner per port)"
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
    fn parses_publish_specs() {
        assert_eq!(
            parse_publish("3100").unwrap(),
            Publish {
                host_port: 3100,
                instance_port: 3100
            }
        );
        assert_eq!(
            parse_publish("80:3100").unwrap(),
            Publish {
                host_port: 80,
                instance_port: 3100
            }
        );
        assert!(parse_publish("0").is_err());
        assert!(parse_publish("nope").is_err());
        assert!(parse_publish("80:").is_err());
    }

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
