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
use std::time::Duration;

use crate::error::{Error, Result};

/// The rootful bridge gateway's address — instances reach the host there,
/// so it is also where a depending app (or this pool, binding `internal`)
/// finds it. A plain fact about the bridge's addressing, not a bridge
/// operation, so it lives here rather than behind the Linux-only seam:
/// `runtime::ns::network` imports this constant rather than defining its
/// own, and this portable pool stays able to name it on every platform.
pub const GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);

/// Who can reach a published port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindScope {
    /// `0.0.0.0` — anyone who can reach this host. What a public web port
    /// wants, and what a database never does.
    Public,
    /// Other ply apps on this host only: loopback rootless, the bridge
    /// gateway rootful (instances reach the host there). Same address the
    /// depending app is told to use.
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
            BindScope::Internal => GATEWAY,
            BindScope::Addr(a) => *a,
        }
    }

    /// The address a *depending* app should connect to. Identical to the bind
    /// address except for Public, which binds the wildcard but is reached at a
    /// concrete one.
    pub fn connect_addr(&self, rootless: bool) -> Ipv4Addr {
        match self {
            BindScope::Public if rootless => Ipv4Addr::LOCALHOST,
            BindScope::Public => GATEWAY,
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

/// One end of a connection to a backend, as the proxy uses it.
///
/// `TcpStream` is not enough on its own. A microVM instance lives on the
/// parent's userspace switch and has no socket the host can name, so its
/// connections arrive as one end of a `UnixStream` pair — private by
/// construction, which a loopback listener standing in for the guest would
/// not be. Everything the proxy does to a backend connection is here, and
/// nothing else is: read, write, clone the handle, half-close.
pub trait Upstream: std::io::Read + std::io::Write + Send {
    /// A second handle to the same connection, for the other direction of
    /// the copy.
    fn dup(&self) -> std::io::Result<Box<dyn Upstream>>;
    /// End this side of the conversation, so the far end sees EOF rather
    /// than a connection that merely stops.
    fn shutdown_write(&self);
}

impl Upstream for TcpStream {
    fn dup(&self) -> std::io::Result<Box<dyn Upstream>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

impl Upstream for std::os::unix::net::UnixStream {
    fn dup(&self) -> std::io::Result<Box<dyn Upstream>> {
        Ok(Box::new(self.try_clone()?))
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

/// How to reach one instance's port.
///
/// Namespaces hand back an address the host can dial; a VM hands back a
/// connector that goes through the switch. `Instance::tcp_open` covered only
/// the health and `--after` probes — the published pool needs the same
/// indirection, or every `--publish` on macOS dials an address that means
/// nothing on the host and the parent resets a connection it accepted.
pub trait Connector: Send + Sync {
    fn connect(&self, timeout: Duration) -> std::io::Result<Box<dyn Upstream>>;
    /// For messages, and for `serve`'s self-connection guard.
    fn addr(&self) -> SocketAddr;
}

/// The connector every address-based backend uses: dial the address.
struct AddrConnector(SocketAddr);

impl Connector for AddrConnector {
    fn connect(&self, timeout: Duration) -> std::io::Result<Box<dyn Upstream>> {
        Ok(Box::new(connect_either_family(self.0, timeout)?))
    }

    fn addr(&self) -> SocketAddr {
        self.0
    }
}

/// "Reach this backend by dialling this address" — what a namespace
/// instance, and any port ply itself forwarded onto loopback, hands the
/// pool.
pub fn connector_for(addr: SocketAddr) -> Arc<dyn Connector> {
    Arc::new(AddrConnector(addr))
}

/// The live backend set, shared between the run loop (writer) and the
/// accept loop (reader). Keyed by slot so removal is exact.
#[derive(Clone, Default)]
pub struct Pool {
    backends: Arc<Mutex<BTreeMap<u32, Arc<dyn Connector>>>>,
    counter: Arc<AtomicUsize>,
}

impl Pool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, slot: u32, backend: Arc<dyn Connector>) {
        self.backends.lock().unwrap().insert(slot, backend);
    }

    pub fn remove(&self, slot: u32) {
        self.backends.lock().unwrap().remove(&slot);
    }

    /// Backends in round-robin order: each call starts one position later.
    /// The whole list is returned so the caller can fail over down it.
    pub fn rotated(&self) -> Vec<Arc<dyn Connector>> {
        let backends: Vec<Arc<dyn Connector>> =
            self.backends.lock().unwrap().values().cloned().collect();
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

/// Connect to `addr`, falling back to the other loopback family.
///
/// Runtimes bind what they consider "everywhere", and for node, Next, Java
/// and plenty of others that is `[::]`. When the kernel hands that socket
/// out IPv6-only, `127.0.0.1` is refused — so a probe or a proxy that knows
/// one family calls a perfectly healthy app dead. Ply picks the instance's
/// address itself, so this is ply's problem to absorb, not something every
/// app author should have to learn (and work around with `-H 0.0.0.0`).
///
/// Only loopback is retried: a bridge IP is a real address the app was told
/// to bind, and guessing there would hide a genuine misconfiguration.
pub fn connect_either_family(
    addr: SocketAddr,
    timeout: std::time::Duration,
) -> std::io::Result<TcpStream> {
    let first = TcpStream::connect_timeout(&addr, timeout);
    if first.is_ok() || !addr.ip().is_loopback() {
        return first;
    }
    let other = match addr {
        SocketAddr::V4(_) => SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, addr.port())),
        SocketAddr::V6(_) => SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port())),
    };
    TcpStream::connect_timeout(&other, timeout).or(first)
}

/// Reserve a free loopback port (rootless: one per instance, injected as
/// PORT). Bind-then-drop has a theoretical reuse race; in practice the
/// instance rebinds it within milliseconds.
///
/// A free loopback port is not proof the app can bind it. Apps bind
/// wildcards — node's `listen(port)` takes `[::]` — and a socket already on
/// `[::]:port` (an editor's port forwarder mirroring an earlier run is the
/// usual culprit, and it outlives the run it copied) does not stop the
/// kernel handing us the same number on `127.0.0.1`. The app then fails to
/// bind IPv6, serves nothing on IPv4 either, and ply waits on a port that
/// answers for nobody — an instance that looks up and is unreachable.
///
/// So prefer a port free on every address the app might choose — but only
/// prefer it. Whether *this* process can bind a wildcard is a property of
/// where ply happens to be running (a namespace without IPv6 refuses `[::]`
/// outright), and that must never decide whether an app may start. The
/// probe filters candidates; a loopback port is still the answer.
pub fn allocate_loopback_port() -> Result<u16> {
    let err = |e: std::io::Error| Error::Runtime(format!("allocating a loopback port: {e}"));
    let mut fallback = None;
    for _ in 0..64 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(err)?;
        let port = listener.local_addr().map_err(err)?.port();
        drop(listener);
        // A wildcard this process cannot bind AT ALL (EAFNOSUPPORT,
        // EADDRNOTAVAIL — no IPv6 here) says nothing about the port, so
        // only a genuine conflict disqualifies it.
        let taken = |addr: &str| {
            matches!(
                TcpListener::bind((addr, port)),
                Err(e) if e.kind() == std::io::ErrorKind::AddrInUse
            )
        };
        if !taken("::") && !taken("0.0.0.0") {
            return Ok(port);
        }
        fallback.get_or_insert(port);
    }
    // Every candidate was claimed on some wildcard. Hand back the first one
    // anyway: the app may well bind loopback and work, and refusing to start
    // is certainly worse than a port that might be shadowed.
    fallback.ok_or_else(|| Error::Runtime("no loopback port available".into()))
}

/// Bind the published host port. Separate from `serve` so the claim fails
/// fast (before any instance starts) with a clear message.
pub fn bind(spec: Publish, rootless: bool) -> Result<TcpListener> {
    let addr = spec.scope.bind_addr(rootless);
    let port = spec.host_port;
    // `internal` binds the bridge gateway, and on a freshly prepared host
    // the bridge does not exist until the first instance would create it —
    // the listener claim comes first, so create it here (idempotent).
    if !rootless && addr == GATEWAY {
        ensure_bridge_for_publish()?;
    }
    TcpListener::bind((addr, port)).map_err(|e| {
        Error::Runtime(format!(
            "--publish {port}: cannot bind {addr}:{port}: {e} — published ports are real host ports (one owner per port)"
        ))
    })
}

/// The bridge itself only exists on Linux (`runtime::ns::network`); `rootless`
/// is always true elsewhere (there is no rootful backend yet), so `bind`
/// never reaches here off Linux — this stub exists only so the crate
/// compiles for a platform that hasn't got a bridge to create.
#[cfg(target_os = "linux")]
fn ensure_bridge_for_publish() -> Result<()> {
    crate::runtime::ns::network::ensure_bridge()
}

#[cfg(not(target_os = "linux"))]
fn ensure_bridge_for_publish() -> Result<()> {
    Ok(())
}

/// Accept loop — runs on its own thread for the parent's lifetime. Each
/// connection tries the pool in round-robin order and splices bytes; an
/// unreachable backend (booting, dying, mid-roll) is skipped.
/// `same_network`: do the listener and the backends live in ONE network? On
/// the host's, yes — and then a backend equal to this listener's address is
/// a loop that spawns a thread per hop until the process dies, so it must be
/// refused. With the instances in their own namespace the very same numbers
/// name different sockets, and refusing would break every publish.
pub fn serve(listener: TcpListener, pool: Pool, same_network: bool) {
    let own = same_network.then(|| listener.local_addr().ok()).flatten();
    for conn in listener.incoming() {
        let Ok(client) = conn else { continue };
        let pool = pool.clone();
        std::thread::spawn(move || {
            let _ = client.set_nodelay(true);
            for backend in pool.rotated() {
                let addr = backend.addr();
                if Some(addr) == own {
                    eprintln!(
                        "ply: refusing to proxy {addr} to itself — the instance is not \
                         where the pool thinks it is"
                    );
                    continue;
                }
                match backend.connect(std::time::Duration::from_millis(500)) {
                    Ok(upstream) => {
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
fn relay(client: TcpStream, upstream: Box<dyn Upstream>) {
    let (Ok(mut c_read), Ok(mut u_read)) = (client.try_clone(), upstream.dup()) else {
        return;
    };
    let (mut c_write, mut u_write) = (client, upstream);
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut c_read, &mut u_write);
        u_write.shutdown_write();
    });
    let _ = std::io::copy(&mut u_read, &mut c_write);
    let _ = c_write.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// The allocator must skip a port an app could not bind.
    ///
    /// The squatter here is IPv6-ONLY on purpose: that is the shape seen in
    /// the wild (an editor's port forwarder mirroring an earlier run). It
    /// leaves 127.0.0.1 bindable, so the loopback probe calls the port free,
    /// and it refuses IPv4 connections, so a connect check misses it too —
    /// the instance then comes up bound to nothing reachable. A dual-stack
    /// listener would block loopback and never reproduce the bug.
    #[test]
    fn allocated_port_is_free_on_every_address_an_app_might_bind() {
        use nix::sys::socket::{
            bind, listen, setsockopt, socket, sockopt, AddressFamily, Backlog, SockFlag,
            SockProtocol, SockType, SockaddrIn6,
        };
        use std::net::{Ipv6Addr, SocketAddrV6};

        let sock = socket(
            AddressFamily::Inet6,
            SockType::Stream,
            SockFlag::empty(),
            SockProtocol::Tcp,
        )
        .expect("ipv6 socket");
        setsockopt(&sock, sockopt::Ipv6V6Only, &true).expect("v6only");
        let any = SockaddrIn6::from(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0));
        bind(std::os::fd::AsRawFd::as_raw_fd(&sock), &any).expect("bind [::]:0");
        listen(&sock, Backlog::new(1).unwrap()).expect("listen");
        let taken =
            nix::sys::socket::getsockname::<SockaddrIn6>(std::os::fd::AsRawFd::as_raw_fd(&sock))
                .expect("port")
                .port();

        // The hole: `bind(127.0.0.1, 0)` searches only the IPv4 space, so the
        // kernel may hand back a port an existing `[::]` listener holds — while
        // an EXPLICIT bind to that port fails. Allocation must therefore prove
        // the port with real binds, which is what this asserts.
        for _ in 0..32 {
            let port = allocate_loopback_port().expect("allocates");
            assert_ne!(port, taken, "handed out a port an app could not bind");
        }
    }

    /// A pool whose backend IS the listener must not be dialled: that loop
    /// spawns a thread per hop and takes the process down with EAGAIN.
    #[test]
    fn a_listener_is_never_its_own_backend() {
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = front.local_addr().unwrap();
        let pool = Pool::new();
        pool.insert(0, connector_for(addr)); // the shape a bad address derivation makes
        std::thread::spawn(move || serve(front, pool, true));

        // the connection is refused service rather than looping: it closes
        let mut c = TcpStream::connect(addr).expect("connect");
        let mut got = Vec::new();
        c.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();
        let _ = c.read_to_end(&mut got);
        assert!(got.is_empty(), "no backend served it");
    }

    /// A backend inside a network namespace is reachable from a thread that
    /// is also inside it — which is what the run parent arranges by joining
    /// before it spawns anything. From outside, the same address is dead.
    #[test]
    // The asymmetry it asserts — one address, alive from inside and dead
    // from outside — is a property of network namespaces themselves, so
    // there has to be one to enter. Linux-only by nature, like its subject:
    // `runtime::ns` is not compiled anywhere else.
    #[cfg(target_os = "linux")]
    fn a_namespace_backend_is_reachable_only_from_inside() {
        use crate::runtime::ns::netns::NetNs;

        let ns = match NetNs::create() {
            Ok(ns) => ns,
            Err(e) if std::env::var("PLY_NETNS_TESTS").is_ok() => {
                panic!("PLY_NETNS_TESTS is set but namespaces are unavailable: {e}")
            }
            // skips where unprivileged user namespaces are restricted; see
            // netns::tests::namespace_for_test
            Err(_) => return,
        };

        // a listener that exists only inside the namespace
        let fd = ns.open().expect("ns fd");
        let (port_tx, port_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            crate::runtime::ns::netns::enter(&fd).expect("enter");
            let l = TcpListener::bind("127.0.0.1:0").expect("bind inside");
            port_tx.send(l.local_addr().unwrap().port()).unwrap();
            for c in l.incoming().flatten() {
                let mut c = c;
                let _ = c.write_all(b"inside");
            }
        });
        let port = port_rx.recv().expect("port");
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

        // from the caller's network the address is dead…
        assert!(
            connect_either_family(addr, std::time::Duration::from_millis(200)).is_err(),
            "premise: the backend is invisible from the caller's network"
        );

        // …and alive from a thread that joined the namespace
        let fd = ns.open().expect("ns fd");
        let got = std::thread::spawn(move || {
            crate::runtime::ns::netns::enter(&fd).expect("enter");
            let mut conn = connect_either_family(addr, std::time::Duration::from_millis(500))
                .expect("reaches the backend from inside");
            let mut got = String::new();
            conn.read_to_string(&mut got).expect("read");
            got
        })
        .join()
        .expect("thread");
        assert_eq!(got, "inside");
    }

    /// An app that binds `[::]` IPv6-only must still look alive to a probe
    /// aimed at IPv4 loopback — node and Next default to exactly that, and
    /// ply chose the address, so ply absorbs the mismatch.
    #[test]
    fn loopback_connect_crosses_families() {
        use nix::sys::socket::{
            bind, listen, setsockopt, socket, sockopt, AddressFamily, Backlog, SockFlag,
            SockProtocol, SockType, SockaddrIn6,
        };
        use std::net::{Ipv6Addr, SocketAddrV6};
        use std::os::fd::AsRawFd;

        let sock = socket(
            AddressFamily::Inet6,
            SockType::Stream,
            SockFlag::empty(),
            SockProtocol::Tcp,
        )
        .expect("ipv6 socket");
        setsockopt(&sock, sockopt::Ipv6V6Only, &true).expect("v6only");
        bind(
            sock.as_raw_fd(),
            &SockaddrIn6::from(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0)),
        )
        .expect("bind");
        listen(&sock, Backlog::new(8).unwrap()).expect("listen");
        let port = nix::sys::socket::getsockname::<SockaddrIn6>(sock.as_raw_fd())
            .unwrap()
            .port();

        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        assert!(
            TcpStream::connect_timeout(&v4, std::time::Duration::from_millis(300)).is_err(),
            "premise: IPv4 alone cannot reach an IPv6-only listener"
        );
        assert!(
            connect_either_family(v4, std::time::Duration::from_millis(300)).is_ok(),
            "the fallback finds it on ::1"
        );
    }

    /// The fallback is loopback-only: a bridge address is what the app was
    /// told to bind, and retrying elsewhere would mask a real misconfiguration.
    #[test]
    fn non_loopback_addresses_are_not_retried() {
        let addr = SocketAddr::from(([10, 77, 0, 99], 9));
        assert!(connect_either_family(addr, std::time::Duration::from_millis(80)).is_err());
    }

    /// Filtering is a preference, never a gate: if no candidate is clean the
    /// allocator still returns one. An app that fails to start because ply
    /// could not find its ideal port is a worse outcome than a port that
    /// might be shadowed.
    #[test]
    fn allocation_never_fails_closed() {
        assert!(allocate_loopback_port().is_ok());
    }

    #[test]
    fn rotation_starts_one_later_each_call() {
        let pool = Pool::new();
        let a: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:1".parse().unwrap();
        pool.insert(1, connector_for(a));
        pool.insert(2, connector_for(b));
        let first = |pool: &Pool| pool.rotated()[0].addr();
        assert_eq!(first(&pool), a);
        assert_eq!(first(&pool), b);
        assert_eq!(first(&pool), a);
        pool.remove(1);
        assert_eq!(
            pool.rotated().iter().map(|c| c.addr()).collect::<Vec<_>>(),
            vec![b]
        );
    }

    /// **The property that makes `--publish` work on a backend with no host
    /// address.** `serve` must never assume it can dial a backend itself:
    /// a microVM's port exists only on the parent's userspace switch, and
    /// the pool reaches it through a connector or not at all.
    #[test]
    fn a_pool_backed_by_a_connector_reaches_a_backend_with_no_host_address() {
        use std::os::unix::net::UnixStream;

        /// Hands back one end of a socketpair whose other end echoes — the
        /// shape of the switch's connector, with no address anywhere.
        struct Pair;
        impl Connector for Pair {
            fn connect(&self, _t: std::time::Duration) -> std::io::Result<Box<dyn Upstream>> {
                let (theirs, ours) = UnixStream::pair()?;
                std::thread::spawn(move || {
                    let mut ours = ours;
                    let mut got = Vec::new();
                    let _ = ours.read_to_end(&mut got);
                    let _ = ours.write_all(&got);
                    let _ = ours.shutdown(std::net::Shutdown::Write);
                });
                Ok(Box::new(theirs))
            }
            fn addr(&self) -> SocketAddr {
                // Deliberately an address nothing on this host can dial: if
                // `serve` ever fell back to dialling it, this test would
                // fail rather than pass by accident.
                SocketAddr::from(([10, 77, 0, 2], 5432))
            }
        }

        let pool = Pool::new();
        pool.insert(0, Arc::new(Pair));
        let front = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let front_addr = front.local_addr().unwrap();
        let serve_pool = pool.clone();
        std::thread::spawn(move || serve(front, serve_pool, true));
        assert_eq!(roundtrip(front_addr, b"select 1"), b"select 1");
    }

    /// The loop guard in `serve` — a backend equal to the listener's own
    /// address — must survive the indirection. It is what stops the proxy
    /// spawning a thread per hop until the process dies.
    #[test]
    fn the_self_connection_guard_still_fires_for_address_backends() {
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = front.local_addr().unwrap();
        let pool = Pool::new();
        pool.insert(0, connector_for(addr));
        std::thread::spawn(move || serve(front, pool, true));

        let mut c = TcpStream::connect(addr).expect("connect");
        c.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();
        let mut got = Vec::new();
        let _ = c.read_to_end(&mut got);
        assert!(got.is_empty(), "no backend served it");
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
        assert_eq!(BindScope::Internal.bind_addr(false), GATEWAY);
        // Public binds the wildcard but is *reached* at a concrete address
        assert_eq!(BindScope::Public.bind_addr(true), Ipv4Addr::UNSPECIFIED);
        assert_eq!(BindScope::Public.connect_addr(true), Ipv4Addr::LOCALHOST);
        assert_eq!(BindScope::Public.connect_addr(false), GATEWAY);
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
        pool.insert(1, connector_for(backend_a));
        pool.insert(2, connector_for(backend_b));

        let front = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let front_addr = front.local_addr().unwrap();
        let serve_pool = pool.clone();
        std::thread::spawn(move || serve(front, serve_pool, true));

        for i in 0..4 {
            assert_eq!(roundtrip(front_addr, b"ping"), b"ping", "conn {i}");
        }
        assert_eq!(hits_a.load(Ordering::SeqCst), 2, "round-robin split");
        assert_eq!(hits_b.load(Ordering::SeqCst), 2, "round-robin split");

        // Kill backend A (simulate a dead instance still in the pool for a
        // moment): connections must fail over to B.
        pool.remove(1);
        pool.insert(1, connector_for("127.0.0.1:1".parse().unwrap())); // nothing listens
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
