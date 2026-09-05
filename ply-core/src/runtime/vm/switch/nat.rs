//! The switch's L3/L4 half: one smoltcp stack, and the bridge between it
//! and real host sockets.
//!
//! Everything below runs on ONE thread — the switch's — and every blocking
//! call (a `connect` to the internet, a `read` from a host socket, a DNS
//! round trip) happens on a worker thread that reports back through
//! [`Event`]. That is the rule the whole file is arranged around: a switch
//! that blocks is a guest whose network has stopped, and there is no way for
//! the guest to notice or complain.
//!
//! # Why smoltcp rather than the spike's hand-written TCP
//!
//! The plyvm spike terminated the guest's TCP itself, and its own comment
//! explains why it could: "the wire is lossless and ordered, so a toy TCP is
//! a correct TCP here". That is true of the wire between the device and the
//! switch. It is not true of the wire between the switch and `deb.debian.org`
//! — the moment egress is bridged to the real internet there are
//! retransmits, windows, MSS clamping and delayed ACKs to get right, and a
//! toy TCP gets them wrong in ways that look like a hung build.
//!
//! # How a guest reaches anything (`any_ip`)
//!
//! The guest routes everything through the gateway, so every egress packet
//! arrives here addressed to the gateway's MAC and to some address the
//! switch does not own. `Interface::set_any_ip(true)` makes the interface
//! accept those — `InterfaceInner::has_ip_addr` returns true unconditionally
//! when it is set (smoltcp 0.14, `iface/interface/mod.rs`) — and a TCP
//! socket LISTENING on that exact destination then completes the handshake.
//! The listener is created when the SYN is seen, one per 4-tuple, and a host
//! socket to the real destination is dialled in parallel.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium};
use smoltcp::socket::udp::UdpMetadata;
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, HardwareAddress, IpAddress, IpCidr, IpEndpoint,
    IpListenEndpoint, IpProtocol, Ipv4Packet, TcpPacket,
};

use super::{dns, Delivery, Fabric, MemberId, MemberInfo, Names, GATEWAY, MTU, PREFIX_LEN};

/// Per-socket buffers. 64 KiB each way is what lets a single TCP connection
/// use a fast link: the window can never exceed the receive buffer, and a
/// 4 KiB buffer caps one connection at a few megabits over any real RTT.
const SOCKET_BUFFER: usize = 64 * 1024;

/// Bytes moved between a host socket and the switch in one go.
const CHUNK: usize = 16 * 1024;

/// Chunks in flight per direction, per connection, before the thread on the
/// far side blocks. This IS the backpressure: a guest that stops reading
/// makes the switch stop draining, which makes the reader thread block,
/// which closes the real TCP window to the far end.
const PIPE_DEPTH: usize = 4;

/// Longest the loop will sleep with nothing to do. Only a bound on how late
/// a smoltcp timer may fire; every real wakeup arrives as an [`Event`].
const IDLE: Duration = Duration::from_millis(100);

/// How long a host dial may take before the guest's connection is reset.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

/// How long an accepted-but-never-completed guest handshake is kept.
/// Without this a SYN that is never followed up leaks a socket and a host
/// connection for the life of the run.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Concurrent DNS forwards. A guest in a resolver loop must not be able to
/// spawn threads in the parent without limit.
const MAX_DNS_INFLIGHT: usize = 32;

type ConnId = u64;

/// A guest connection, by its full 4-tuple: what a retransmitted SYN is
/// matched against so it does not open a second host socket.
type Tuple = (Ipv4Addr, u16, Ipv4Addr, u16);

/// Everything that reaches the switch's thread.
pub(super) enum Event {
    /// One Ethernet frame a member transmitted.
    Frame {
        from: MemberId,
        bytes: Vec<u8>,
    },
    Join {
        info: MemberInfo,
        tx: mpsc::SyncSender<Vec<u8>>,
    },
    /// The host wants a TCP connection INTO a guest (a published port, a
    /// health probe, an `--after` gate).
    Connect {
        ip: Ipv4Addr,
        port: u16,
        deadline: Instant,
        reply: mpsc::Sender<std::io::Result<UnixStream>>,
    },
    /// An egress dial finished, well or badly.
    Dialed {
        id: ConnId,
        result: std::io::Result<TcpStream>,
    },
    /// A bridged connection's host side moved bytes. It carries no
    /// identity: one pass of the loop looks at every connection anyway, and
    /// a marker that names one would only be a second way to say the same
    /// thing.
    Nudge,
    /// The host side reached EOF or died.
    HostEof(ConnId),
    /// A forwarded DNS query came back.
    Dns {
        reply: Vec<u8>,
        to: IpEndpoint,
        from: Option<IpAddress>,
    },
    Stop,
}

// ------------------------------------------------------------------ phy

/// The switch's own network device: frames in from the fabric, frames out
/// to it. Both queues are drained by the loop on every pass, so neither
/// grows.
struct Uplink {
    rx: VecDeque<Vec<u8>>,
    tx: Vec<Vec<u8>>,
}

impl Device for Uplink {
    type RxToken<'a> = UplinkRx;
    type TxToken<'a> = UplinkTx<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        // Ethernet MTU, not IP MTU: smoltcp subtracts the 14-byte header
        // itself (`DeviceCapabilities::ip_mtu`), and getting this wrong by
        // 14 bytes shows up only as a large transfer stalling.
        caps.max_transmission_unit = MTU + 14;
        caps
    }

    fn receive(&mut self, _now: SmolInstant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((UplinkRx(frame), UplinkTx(&mut self.tx)))
    }

    fn transmit(&mut self, _now: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(UplinkTx(&mut self.tx))
    }
}

struct UplinkRx(Vec<u8>);

impl smoltcp::phy::RxToken for UplinkRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

struct UplinkTx<'a>(&'a mut Vec<Vec<u8>>);

impl smoltcp::phy::TxToken for UplinkTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let out = f(&mut buf);
        self.0.push(buf);
        out
    }
}

// --------------------------------------------------------- host sockets

/// A host socket the switch bridges to. Two kinds and no more: a
/// `TcpStream` for egress, and one end of a `UnixStream` pair for a
/// connection dialled INTO a guest.
///
/// # Why inbound is a socketpair and not a loopback listener
///
/// `Connector::connect` has to hand the pool something it can `read`,
/// `write` and half-close. The obvious way to make one — bind `127.0.0.1:0`
/// and connect to it — would put every guest port on the host's loopback
/// for anything on the Mac to reach, which is exactly the exposure
/// `--publish internal` exists to prevent. A socketpair has no address at
/// all, so the only way to the guest is through a handle the switch gave
/// out.
trait HostSocket: Read + Write + Send + Sync + Sized + 'static {
    fn dup(&self) -> std::io::Result<Self>;
    fn stop(&self, how: Shutdown) -> std::io::Result<()>;
}

impl HostSocket for TcpStream {
    fn dup(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn stop(&self, how: Shutdown) -> std::io::Result<()> {
        self.shutdown(how)
    }
}

impl HostSocket for UnixStream {
    fn dup(&self) -> std::io::Result<Self> {
        self.try_clone()
    }
    fn stop(&self, how: Shutdown) -> std::io::Result<()> {
        self.shutdown(how)
    }
}

/// The switch's end of one bridged connection.
struct Pipe {
    /// Host → guest.
    inbox: mpsc::Receiver<Vec<u8>>,
    pending: VecDeque<Vec<u8>>,
    /// Guest → host. Dropped when the guest half-closes, which is what
    /// makes the writer thread shut the host side's write half.
    outbox: Option<mpsc::SyncSender<Vec<u8>>>,
    /// One chunk read out of smoltcp that `outbox` had no room for. Held
    /// here rather than dropped: there is no way to put it back.
    staged: Option<Vec<u8>>,
    host_eof: bool,
}

/// Bridge one host socket: a thread each way, and the channels between them
/// and the switch.
fn bridge<S: HostSocket>(
    id: ConnId,
    stream: S,
    events: &mpsc::Sender<Event>,
) -> std::io::Result<Pipe> {
    let mut reader = stream.dup()?;
    let mut writer = stream.dup()?;
    let (in_tx, inbox) = mpsc::sync_channel::<Vec<u8>>(PIPE_DEPTH);
    let (outbox, out_rx) = mpsc::sync_channel::<Vec<u8>>(PIPE_DEPTH);

    let read_events = events.clone();
    std::thread::Builder::new()
        .name("ply-switch-rx".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut buf = vec![0u8; CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // The data goes first and the nudge second, so the
                        // switch never wakes to an empty channel.
                        if in_tx.send(buf[..n].to_vec()).is_err() {
                            return; // the switch dropped this connection
                        }
                        let _ = read_events.send(Event::Nudge);
                    }
                }
            }
            let _ = read_events.send(Event::HostEof(id));
        })?;

    let write_events = events.clone();
    std::thread::Builder::new()
        .name("ply-switch-tx".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            while let Ok(chunk) = out_rx.recv() {
                if writer.write_all(&chunk).is_err() {
                    return;
                }
                let _ = write_events.send(Event::Nudge);
            }
            // The guest half-closed (or the connection is over): pass the
            // half-close on, so the far end sees an orderly EOF rather than
            // a connection that simply stops.
            let _ = writer.stop(Shutdown::Write);
        })?;

    Ok(Pipe {
        inbox,
        pending: VecDeque::new(),
        outbox: Some(outbox),
        staged: None,
        host_eof: false,
    })
}

// ------------------------------------------------------------- the loop

enum Stage {
    /// Egress: the guest's SYN has a listener; the host dial is in flight.
    Dialing {
        since: Instant,
    },
    /// Inbound: smoltcp is dialling the guest; the caller is waiting.
    Ringing {
        reply: mpsc::Sender<std::io::Result<UnixStream>>,
        deadline: Instant,
    },
    Open(Pipe),
}

struct Conn {
    handle: SocketHandle,
    stage: Stage,
    /// Egress only: the tuple this connection was opened for, so a
    /// retransmitted SYN finds it instead of opening a second one.
    key: Option<Tuple>,
    /// Where it goes: for messages, and for the handshake timeout.
    dest: SocketAddr,
    opened: Instant,
}

struct Loop {
    iface: Interface,
    phy: Uplink,
    sockets: SocketSet<'static>,
    fabric: Fabric,
    links: HashMap<MemberId, mpsc::SyncSender<Vec<u8>>>,
    conns: HashMap<ConnId, Conn>,
    egress: HashMap<Tuple, ConnId>,
    names: Arc<Mutex<Names>>,
    events: mpsc::Sender<Event>,
    depth: Arc<AtomicUsize>,
    dns_socket: SocketHandle,
    dns_inflight: Arc<AtomicUsize>,
    next_id: ConnId,
    next_port: u16,
    started: Instant,
}

/// The switch's whole life. Returns when the last [`Switch`] handle is
/// dropped (which sends [`Event::Stop`]) or when the channel closes.
///
/// [`Switch`]: super::Switch
pub(super) fn run(
    inbox: mpsc::Receiver<Event>,
    events: mpsc::Sender<Event>,
    names: Arc<Mutex<Names>>,
    depth: Arc<AtomicUsize>,
) {
    let mut phy = Uplink {
        rx: VecDeque::new(),
        tx: Vec::new(),
    };
    let started = Instant::now();
    let mut config = Config::new(HardwareAddress::Ethernet(super::gateway_mac()));
    // Seeded from the clock: smoltcp's own advice is that the seed differ
    // between boots, because it is what keeps two runs from picking the same
    // TCP sequence numbers for the same 4-tuple.
    config.random_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x50_4c_59);
    let mut iface = Interface::new(config, &mut phy, SmolInstant::from_micros(0));
    iface.update_ip_addrs(|addrs| {
        let _ = addrs.push(IpCidr::new(GATEWAY.into(), PREFIX_LEN));
    });
    // Accept packets addressed to anything, not just to 10.77.0.1: that is
    // what makes a guest's connection to the real internet land in a socket
    // here instead of being dropped as "not for us".
    iface.set_any_ip(true);
    // smoltcp's own documentation for AnyIP asks for a route whose gateway
    // is one of the interface's addresses. Nothing this switch sends is ever
    // off-link (every peer is inside 10.77.0.0/16), so the route is never
    // consulted; it is here to satisfy the documented contract rather than
    // the current implementation of it.
    let _ = iface.routes_mut().add_default_ipv4_route(GATEWAY);

    let mut sockets = SocketSet::new(Vec::new());
    // The resolver. Bound to port 53 on ANY address, so a guest that has
    // been told to use some other nameserver still gets the switch's — a
    // microVM has no other route to a resolver, and silently dropping those
    // queries would look like a network that is up and does not work.
    let mut dns_socket = udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 16 * 1024]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 16], vec![0u8; 16 * 1024]),
    );
    if let Err(e) = dns_socket.bind(IpListenEndpoint {
        addr: None,
        port: 53,
    }) {
        eprintln!("ply: warning: the switch could not open its resolver ({e:?}) — names will not resolve inside instances");
    }
    let dns_handle = sockets.add(dns_socket);

    let mut state = Loop {
        iface,
        phy,
        sockets,
        fabric: Fabric::new(),
        links: HashMap::new(),
        conns: HashMap::new(),
        egress: HashMap::new(),
        names,
        events,
        depth,
        dns_socket: dns_handle,
        dns_inflight: Arc::new(AtomicUsize::new(0)),
        next_id: 1,
        next_port: 49_152,
        started,
    };

    loop {
        let mut moved = false;
        loop {
            match inbox.try_recv() {
                Ok(event) => {
                    if !state.handle(event) {
                        return;
                    }
                    moved = true;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        moved |= state.step();
        if moved {
            continue;
        }
        let wait = state.wait();
        match inbox.recv_timeout(wait) {
            Ok(event) => {
                if !state.handle(event) {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

impl Loop {
    fn now(&self) -> SmolInstant {
        SmolInstant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    /// `false` to stop the switch.
    fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Stop => return false,
            Event::Frame { from, bytes } => {
                self.depth.fetch_sub(1, Ordering::Relaxed);
                let deliveries = self.fabric.from_member(from, &bytes);
                self.deliver(deliveries);
            }
            Event::Join { info, tx } => {
                // A rejoining member displaces its own corpse; its channel
                // has to go with it, or the map grows one dead entry per
                // restart for the life of the switch.
                for gone in self.fabric.join(info) {
                    self.links.remove(&gone);
                }
                self.links.insert(info.id, tx);
            }
            Event::Connect {
                ip,
                port,
                deadline,
                reply,
            } => self.dial_guest(ip, port, deadline, reply),
            Event::Dialed { id, result } => self.dialed(id, result),
            Event::Nudge => {} // the pumps below look at every connection
            Event::HostEof(id) => {
                if let Some(conn) = self.conns.get_mut(&id) {
                    if let Stage::Open(pipe) = &mut conn.stage {
                        pipe.host_eof = true;
                    }
                }
            }
            Event::Dns { reply, to, from } => {
                let socket = self.sockets.get_mut::<udp::Socket>(self.dns_socket);
                let _ = socket.send_slice(
                    &reply,
                    UdpMetadata {
                        endpoint: to,
                        local_address: from,
                        meta: Default::default(),
                    },
                );
            }
        }
        true
    }

    /// One pass: poll the stack, move bytes, retire what is finished.
    fn step(&mut self) -> bool {
        let mut moved = self.poll_stack();
        moved |= self.pump_dns();
        moved |= self.pump_conns();
        if moved {
            // Anything the pumps put in a socket goes on the wire now rather
            // than at the next wakeup.
            self.poll_stack();
        }
        moved
    }

    /// Run smoltcp and hand whatever it emitted to the fabric.
    fn poll_stack(&mut self) -> bool {
        let now = self.now();
        self.iface.poll(now, &mut self.phy, &mut self.sockets);
        if self.phy.tx.is_empty() {
            return false;
        }
        let frames: Vec<Vec<u8>> = std::mem::take(&mut self.phy.tx);
        for frame in frames {
            let deliveries = self.fabric.from_gateway(&frame);
            self.deliver(deliveries);
        }
        true
    }

    fn deliver(&mut self, deliveries: Vec<Delivery>) {
        for delivery in deliveries {
            match delivery {
                Delivery::To(id, frame) => self.send_to(id, frame),
                Delivery::Flood { except, frame } => {
                    let targets: Vec<MemberId> = self
                        .fabric
                        .members()
                        .iter()
                        .map(|m| m.id)
                        .filter(|id| Some(*id) != except)
                        .collect();
                    for id in targets {
                        self.send_to(id, frame.clone());
                    }
                }
                Delivery::Uplink(frame) => {
                    self.prepare_uplink(&frame);
                    self.phy.rx.push_back(frame);
                }
            }
        }
    }

    fn send_to(&mut self, id: MemberId, frame: Vec<u8>) {
        let gone = match self.links.get(&id) {
            // A full queue is a member that has stopped reading. Dropping is
            // what a NIC does with a full ring, and TCP recovers; growing the
            // queue in the parent instead does not.
            Some(tx) => matches!(tx.try_send(frame), Err(mpsc::TrySendError::Disconnected(_))),
            None => false,
        };
        if gone {
            self.links.remove(&id);
            self.fabric.leave(id);
        }
    }

    /// A guest's SYN needs a socket listening on its destination before
    /// smoltcp sees it, or smoltcp answers with a reset.
    fn prepare_uplink(&mut self, frame: &[u8]) {
        let Some((src, sport, dst, dport)) = syn_target(frame) else {
            return;
        };
        let key = (src, sport, dst, dport);
        if self.egress.contains_key(&key) {
            return; // a retransmitted SYN, not a second connection
        }
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; SOCKET_BUFFER]),
            tcp::SocketBuffer::new(vec![0u8; SOCKET_BUFFER]),
        );
        // A guest that vanishes mid-connection must not pin a socket
        // forever; smoltcp's own keepalive/timeout is the cheapest way to
        // notice, and it is the same thing a kernel does.
        socket.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
        socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
        if socket
            .listen(IpListenEndpoint {
                addr: Some(IpAddress::Ipv4(dst)),
                port: dport,
            })
            .is_err()
        {
            return;
        }
        let handle = self.sockets.add(socket);
        let id = self.next_id;
        self.next_id += 1;
        // Reaching the gateway's own address means reaching the Mac: the
        // switch IS the host from in there, so 10.77.0.1:N is the host's
        // 127.0.0.1:N. Anything else is dialled as written.
        let host_dst = if dst == GATEWAY {
            SocketAddr::from((Ipv4Addr::LOCALHOST, dport))
        } else {
            SocketAddr::from((dst, dport))
        };
        self.egress.insert(key, id);
        self.conns.insert(
            id,
            Conn {
                handle,
                stage: Stage::Dialing {
                    since: Instant::now(),
                },
                key: Some(key),
                dest: host_dst,
                opened: Instant::now(),
            },
        );
        let events = self.events.clone();
        let spawned = std::thread::Builder::new()
            .name("ply-switch-dial".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                let result = TcpStream::connect_timeout(&host_dst, DIAL_TIMEOUT);
                if let Ok(stream) = &result {
                    let _ = stream.set_nodelay(true);
                }
                let _ = events.send(Event::Dialed { id, result });
            });
        if spawned.is_err() {
            self.retire(id);
        }
    }

    fn dialed(&mut self, id: ConnId, result: std::io::Result<TcpStream>) {
        let Some(conn) = self.conns.get_mut(&id) else {
            return; // the guest gave up first
        };
        match result {
            Ok(stream) => match bridge(id, stream, &self.events) {
                Ok(pipe) => conn.stage = Stage::Open(pipe),
                Err(e) => {
                    eprintln!("ply: switch: bridging {}: {e}", conn.dest);
                    self.reset(id);
                }
            },
            Err(e) => {
                // The guest asked for somewhere it cannot get to. A reset is
                // the honest answer and the one that makes `connect(2)`
                // inside the guest fail immediately instead of timing out.
                if std::env::var_os("PLY_VM_DEBUG").is_some() {
                    eprintln!("ply: switch: {} is unreachable: {e}", conn.dest);
                }
                self.reset(id);
            }
        }
    }

    /// Open a connection from the switch INTO a guest.
    fn dial_guest(
        &mut self,
        ip: Ipv4Addr,
        port: u16,
        deadline: Instant,
        reply: mpsc::Sender<std::io::Result<UnixStream>>,
    ) {
        let mut socket = tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0u8; SOCKET_BUFFER]),
            tcp::SocketBuffer::new(vec![0u8; SOCKET_BUFFER]),
        );
        socket.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
        socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
        let local = self.next_port;
        // 49152..65535 is IANA's ephemeral range; wrapping inside it means
        // this can never collide with a port a guest is listening on.
        self.next_port = if self.next_port == u16::MAX {
            49_152
        } else {
            self.next_port + 1
        };
        let handle = self.sockets.add(socket);
        let socket = self.sockets.get_mut::<tcp::Socket>(handle);
        if let Err(e) = socket.connect(self.iface.context(), (IpAddress::Ipv4(ip), port), local) {
            self.sockets.remove(handle);
            let _ = reply.send(Err(std::io::Error::other(format!(
                "the switch cannot dial {ip}:{port}: {e:?}"
            ))));
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.conns.insert(
            id,
            Conn {
                handle,
                stage: Stage::Ringing { reply, deadline },
                key: None,
                dest: SocketAddr::from((ip, port)),
                opened: Instant::now(),
            },
        );
    }

    /// Move bytes on every open connection and retire the finished ones.
    fn pump_conns(&mut self) -> bool {
        let mut moved = false;
        let mut retire: Vec<ConnId> = Vec::new();
        let mut reset: Vec<ConnId> = Vec::new();
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for id in ids {
            let Some(conn) = self.conns.get_mut(&id) else {
                continue;
            };
            let socket = self.sockets.get_mut::<tcp::Socket>(conn.handle);
            match &mut conn.stage {
                Stage::Dialing { since } => {
                    if socket.state() == tcp::State::Closed
                        && since.elapsed() > Duration::from_secs(1)
                    {
                        retire.push(id);
                    } else if conn.opened.elapsed() > HANDSHAKE_TIMEOUT
                        && !matches!(socket.state(), tcp::State::Established)
                    {
                        reset.push(id);
                    }
                }
                Stage::Ringing { reply, deadline } => {
                    if socket.may_send() {
                        match UnixStream::pair() {
                            Ok((theirs, ours)) => {
                                let _ = reply.send(Ok(theirs));
                                match bridge(id, ours, &self.events) {
                                    Ok(pipe) => conn.stage = Stage::Open(pipe),
                                    Err(_) => reset.push(id),
                                }
                                moved = true;
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                                reset.push(id);
                            }
                        }
                    } else if socket.state() == tcp::State::Closed {
                        // smoltcp went straight back to Closed: the guest
                        // sent a reset, which means nothing is listening.
                        let _ = reply.send(Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            format!("nothing is listening on {}", conn.dest),
                        )));
                        retire.push(id);
                    } else if Instant::now() >= *deadline {
                        let _ = reply.send(Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!("{} did not answer", conn.dest),
                        )));
                        reset.push(id);
                    }
                }
                Stage::Open(pipe) => {
                    moved |= pump_pipe(socket, pipe);
                    if socket.state() == tcp::State::Closed {
                        retire.push(id);
                    }
                }
            }
        }
        for id in reset {
            self.reset(id);
            moved = true;
        }
        for id in retire {
            self.retire(id);
            moved = true;
        }
        moved
    }

    /// Reset the guest's side and forget the connection.
    fn reset(&mut self, id: ConnId) {
        if let Some(conn) = self.conns.get(&id) {
            self.sockets.get_mut::<tcp::Socket>(conn.handle).abort();
        }
        self.retire(id);
    }

    fn retire(&mut self, id: ConnId) {
        let Some(conn) = self.conns.remove(&id) else {
            return;
        };
        if let Some(key) = conn.key {
            self.egress.remove(&key);
        }
        self.sockets.remove(conn.handle);
        // Dropping `conn` drops the pipe, which drops both channels, which
        // ends both worker threads and closes the host socket.
    }

    /// Answer or forward whatever the guest asked the resolver.
    fn pump_dns(&mut self) -> bool {
        let mut queries: Vec<(Vec<u8>, UdpMetadata)> = Vec::new();
        {
            let socket = self.sockets.get_mut::<udp::Socket>(self.dns_socket);
            while let Ok((payload, meta)) = socket.recv() {
                queries.push((payload.to_vec(), meta));
            }
        }
        if queries.is_empty() {
            return false;
        }
        for (query, meta) in queries {
            let answer = {
                let names = match self.names.lock() {
                    Ok(names) => names,
                    Err(poisoned) => poisoned.into_inner(),
                };
                dns::decide(&query, |name| names.lookup(name))
            };
            match answer {
                dns::Answer::Reply(bytes) => {
                    let socket = self.sockets.get_mut::<udp::Socket>(self.dns_socket);
                    let _ = socket.send_slice(
                        &bytes,
                        UdpMetadata {
                            endpoint: meta.endpoint,
                            local_address: meta.local_address,
                            meta: Default::default(),
                        },
                    );
                }
                dns::Answer::Forward => self.forward_dns(query, meta),
            }
        }
        true
    }

    fn forward_dns(&mut self, query: Vec<u8>, meta: UdpMetadata) {
        if self.dns_inflight.load(Ordering::Relaxed) >= MAX_DNS_INFLIGHT {
            return; // the guest's resolver will retry; the parent will not fork bomb
        }
        self.dns_inflight.fetch_add(1, Ordering::Relaxed);
        let events = self.events.clone();
        let inflight = self.dns_inflight.clone();
        let spawned = std::thread::Builder::new()
            .name("ply-switch-dns".into())
            .stack_size(128 * 1024)
            .spawn(move || {
                // Read every time rather than caching: a laptop changes
                // networks mid-run, and one small file read next to a
                // network round trip costs nothing.
                let resolvers = dns::upstream_resolvers();
                if let Some(reply) = dns::forward(&query, &resolvers, Duration::from_secs(3)) {
                    let _ = events.send(Event::Dns {
                        reply,
                        to: meta.endpoint,
                        from: meta.local_address,
                    });
                }
                inflight.fetch_sub(1, Ordering::Relaxed);
            });
        if spawned.is_err() {
            self.dns_inflight.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// How long the loop may sleep: whatever smoltcp's timers allow, and
    /// never longer than [`IDLE`].
    fn wait(&mut self) -> Duration {
        let now = self.now();
        let delay = self
            .iface
            .poll_delay(now, &self.sockets)
            .map(|d| Duration::from_micros(d.total_micros()))
            .unwrap_or(IDLE);
        delay.clamp(Duration::from_millis(1), IDLE)
    }
}

/// Move bytes between one smoltcp socket and one host pipe.
fn pump_pipe(socket: &mut tcp::Socket<'_>, pipe: &mut Pipe) -> bool {
    let mut moved = false;

    // --- guest → host ----------------------------------------------------
    // Never more than one chunk out of smoltcp at a time: what the channel
    // cannot take stays in the socket's receive buffer, which closes the
    // window, which is the only backpressure a TCP endpoint understands.
    if pipe.staged.is_none() && socket.can_recv() {
        let mut buf = vec![0u8; CHUNK];
        if let Ok(n) = socket.recv_slice(&mut buf) {
            if n > 0 {
                buf.truncate(n);
                pipe.staged = Some(buf);
                moved = true;
            }
        }
    }
    if let (Some(chunk), Some(outbox)) = (pipe.staged.take(), pipe.outbox.as_ref()) {
        match outbox.try_send(chunk) {
            Ok(()) => moved = true,
            Err(mpsc::TrySendError::Full(chunk)) => pipe.staged = Some(chunk),
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }
    // The guest has said everything it is going to. Dropping the sender is
    // what makes the writer thread half-close the host side.
    // `may_recv` is false in SYN-RECEIVED too, so testing it alone would
    // half-close a connection in the middle of its own handshake.
    if !socket.may_recv()
        && pipe.staged.is_none()
        && socket.state() != tcp::State::SynReceived
        && pipe.outbox.take().is_some()
    {
        moved = true;
    }

    // --- host → guest ----------------------------------------------------
    if pipe.pending.is_empty() {
        while let Ok(chunk) = pipe.inbox.try_recv() {
            pipe.pending.push_back(chunk);
            moved = true;
            if pipe.pending.len() >= PIPE_DEPTH {
                break;
            }
        }
    }
    while let Some(front) = pipe.pending.front_mut() {
        if !socket.can_send() {
            break;
        }
        match socket.send_slice(front) {
            Ok(0) => break,
            Ok(n) => {
                moved = true;
                if n >= front.len() {
                    pipe.pending.pop_front();
                } else {
                    front.drain(..n);
                }
            }
            Err(_) => break,
        }
    }
    if pipe.host_eof && pipe.pending.is_empty() && socket.may_send() {
        socket.close();
        moved = true;
    }
    moved
}

/// The destination of a guest's TCP SYN, or `None` for anything that is not
/// one.
///
/// Every offset here comes from smoltcp's own parsers rather than from a
/// hand-written table, so a malformed packet is a `None` and never a panic
/// or a read past the end.
fn syn_target(frame: &[u8]) -> Option<Tuple> {
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.ethertype() != EthernetProtocol::Ipv4 {
        return None;
    }
    let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
    if ip.next_header() != IpProtocol::Tcp {
        return None;
    }
    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
    // A SYN with no ACK is a connection being opened. A SYN-ACK from a guest
    // belongs to a connection the switch opened and must not make a second.
    if !tcp.syn() || tcp.ack() {
        return None;
    }
    Some((ip.src_addr(), tcp.src_port(), ip.dst_addr(), tcp.dst_port()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::phy::ChecksumCapabilities;
    use smoltcp::wire::{EthernetAddress, EthernetRepr, Ipv4Repr, TcpRepr, TcpSeqNumber};

    fn tcp_frame(control: smoltcp::wire::TcpControl, ack: Option<u32>) -> Vec<u8> {
        let tcp = TcpRepr {
            src_port: 50_000,
            dst_port: 443,
            control,
            seq_number: TcpSeqNumber(1),
            ack_number: ack.map(|a| TcpSeqNumber(a as i32)),
            window_len: 64_000,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };
        let src = Ipv4Addr::new(10, 77, 0, 2);
        let dst = Ipv4Addr::new(93, 184, 216, 34);
        let ip = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Tcp,
            payload_len: tcp.buffer_len(),
            hop_limit: 64,
        };
        let eth = EthernetRepr {
            src_addr: EthernetAddress([0x52, 0x54, 0x00, 0x77, 0, 2]),
            dst_addr: super::super::gateway_mac(),
            ethertype: EthernetProtocol::Ipv4,
        };
        let mut out = vec![0u8; eth.buffer_len() + ip.buffer_len() + ip.payload_len];
        let mut frame = EthernetFrame::new_unchecked(&mut out[..]);
        eth.emit(&mut frame);
        let checksum = ChecksumCapabilities::default();
        let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
        ip.emit(&mut packet, &checksum);
        tcp.emit(
            &mut TcpPacket::new_unchecked(packet.payload_mut()),
            &src.into(),
            &dst.into(),
            &checksum,
        );
        out
    }

    /// The one thing that decides whether egress works at all: a guest's SYN
    /// has to be recognised BEFORE smoltcp sees it, or smoltcp answers with
    /// a reset and the guest's `connect` fails.
    #[test]
    fn a_syn_names_the_destination_the_switch_must_dial() {
        let frame = tcp_frame(smoltcp::wire::TcpControl::Syn, None);
        assert_eq!(
            syn_target(&frame),
            Some((
                Ipv4Addr::new(10, 77, 0, 2),
                50_000,
                Ipv4Addr::new(93, 184, 216, 34),
                443
            ))
        );
    }

    #[test]
    fn nothing_else_is_mistaken_for_a_connection_being_opened() {
        // A SYN-ACK belongs to a connection the SWITCH opened into a guest;
        // treating it as a new one would dial the guest's own address from
        // the host on every inbound connection.
        assert_eq!(
            syn_target(&tcp_frame(smoltcp::wire::TcpControl::Syn, Some(7))),
            None
        );
        assert_eq!(
            syn_target(&tcp_frame(smoltcp::wire::TcpControl::None, Some(7))),
            None
        );
        assert_eq!(syn_target(b"too short"), None);
        assert_eq!(syn_target(&[0u8; 64]), None);
    }
}
