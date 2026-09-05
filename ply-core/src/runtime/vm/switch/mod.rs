//! One virtual L2 network per stack, in the `ply up` (or standalone
//! `ply run`) parent. No daemon, no entitlement, no tap device: macOS gives
//! us no tap without `com.apple.vm.networking`, which is restricted, so the
//! network lives in userspace and dies with the parent — exactly as the
//! stack's netns does on Linux.
//!
//! Members speak Ethernet. The switch answers ARP, hands out fixed addresses
//! from the same `10.77.0.0/16` range the Linux bridge uses, answers
//! `<name>.ply`, terminates guest TCP to anywhere (smoltcp in `any_ip` mode)
//! and bridges it to real host sockets, and dials INTO a guest so a
//! published host port and a health probe can reach it.
//!
//! # Portable on purpose
//!
//! Nothing here touches Hypervisor.framework, so this module compiles and
//! runs its tests on Linux, which is where this project's CI is. That
//! includes the end-to-end tests: [`tests`] stands a second smoltcp stack up
//! as a fake guest and drives real TCP through the switch in both
//! directions, with no VM anywhere.
//!
//! # The four layers, and which file each lives in
//!
//! * [`frame`] — the 4-byte big-endian length framing (ruling R0-2), which
//!   [`unix`] puts on the wire.
//! * [`Fabric`] (here) — L2: ARP, learning, unicast, flood. Pure, and the
//!   only part with a byte layout of its own.
//! * [`nat`] — L3/L4: smoltcp, the NAT to host sockets, the DNS pump.
//! * [`unix`] — the `--vswitch` seam: the switch in the `ply up` parent,
//!   every member's `ply run` a client of it over a unix socket. [`Net`] is
//!   the one type the backend holds, whichever side of that seam it is on.

pub mod dns;
pub mod frame;
mod nat;
pub mod unix;

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use smoltcp::wire::{
    ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol,
    EthernetRepr,
};

use crate::runtime::publish::{self, Connector, Upstream};

/// The switch's own address: the gateway every member routes through, the
/// resolver every member asks, and the source of every connection the switch
/// makes into a guest.
///
/// The same constant the Linux bridge uses (`publish::GATEWAY`), and
/// deliberately so: one `10.77.0.0/16` means one set of addresses a person
/// has to recognise, whichever backend they are on.
pub const GATEWAY: Ipv4Addr = publish::GATEWAY;

/// `/16`, as on the Linux bridge. Members are on-link with each other, so a
/// stack's peers reach one another without going through the switch's own
/// TCP/IP stack at all.
pub const PREFIX_LEN: u8 = 16;

/// IP MTU. 1500 because that is what the guest's virtio-net gets by default
/// and there is no path here that can carry more; the Ethernet frame is this
/// plus the 14-byte header.
pub const MTU: usize = 1500;

/// Frames queued towards one member before the switch starts dropping them.
///
/// A NIC with a full RX ring drops, and so does this: the alternative is
/// growing a queue in the parent for a guest that has stopped reading, which
/// turns a stalled app into a dead Mac. TCP retransmits.
const MEMBER_QUEUE: usize = 256;

/// Frames queued towards the switch before a member's device starts
/// dropping them, counted by [`FrameSink`] so the device can see the depth
/// without owning the queue.
pub const UPLINK_QUEUE: usize = 512;

/// The MAC of the address `ip` on this switch.
///
/// `52:54:00` is the QEMU/KVM OUI — locally administered (bit 1 of the first
/// octet), never multicast (bit 0 clear), and instantly recognisable in a
/// packet capture as "a virtual machine". `77` then names ply's own range,
/// and the last two octets are the host part of the address, so a MAC and an
/// address can be read off each other with no table.
pub fn member_mac(ip: Ipv4Addr) -> EthernetAddress {
    let o = ip.octets();
    EthernetAddress([0x52, 0x54, 0x00, 0x77, o[2], o[3]])
}

/// The switch's own MAC — [`member_mac`] of [`GATEWAY`], so the rule above
/// has no exception.
pub fn gateway_mac() -> EthernetAddress {
    member_mac(GATEWAY)
}

// ------------------------------------------------------------- addressing

/// Which name has which address on this switch.
///
/// Allocation is by NAME, not by join order, and it is stable for the life
/// of the switch: a member that dies and is relaunched (a `restart: always`
/// app, a rolled instance) comes back on the address its peers already have
/// in their `/etc/hosts`. That is the property `/etc/hosts` staleness
/// depends on, and the plan records the gap it leaves when it does not hold.
#[derive(Debug, Default)]
pub struct Names {
    map: BTreeMap<String, Ipv4Addr>,
    /// Host part of the next address to hand out. Starts at 2: `.0` is the
    /// network, `.1` is the switch.
    next: u32,
}

impl Names {
    pub fn new() -> Names {
        Names {
            map: BTreeMap::new(),
            next: 2,
        }
    }

    /// This name's address, allocating one the first time it is asked for.
    ///
    /// Runs out at `10.77.255.254`; a stack of 65,000 members is not a thing
    /// this runtime supports, and wrapping round to hand two members one
    /// address would be worse than the panic-free saturation below (which
    /// hands the last address out repeatedly and is at least visible as two
    /// members fighting over one IP).
    pub fn allocate(&mut self, name: &str) -> Ipv4Addr {
        if let Some(ip) = self.map.get(name) {
            return *ip;
        }
        let host = self.next.min(0xfffe);
        self.next = (self.next + 1).min(0xfffe);
        let base = u32::from(GATEWAY) & 0xffff_0000;
        let ip = Ipv4Addr::from(base | host);
        self.map.insert(name.to_string(), ip);
        ip
    }

    /// Make `name` resolve to an address something else already has.
    ///
    /// The FIRST claimant wins, and that is the point: instances are
    /// allocated under `<app>.<n>` because two instances of one app are two
    /// machines, but `<app>.ply` has to answer with one of them. A later
    /// instance must not take the name off the one its peers are already
    /// talking to. Answers whether the alias was taken.
    ///
    /// # Why `<app>.ply` is not round-robined across `--scale N`
    ///
    /// Deliberate, and it is not a matter of effort. Three things would all
    /// have to change together for a rotating answer to reach anyone:
    ///
    /// * `/etc/hosts` beats DNS, and every stack member now boots with a
    ///   line per peer (`spec_disk::hosts_lines`). A resolver that rotated
    ///   would simply never be asked by the members it is for.
    /// * Removing those lines to make it live would give back the problem
    ///   they were added to solve — a peer that has not booted yet is
    ///   nameable today only because `ply up` reserved its address before
    ///   spawning anything.
    /// * On Linux `<app>.ply` is `127.0.0.1` inside the stack's shared
    ///   namespace, where scaled instances differ by PORT, not by address.
    ///   A rotating answer here would make the two backends disagree about
    ///   what the name means.
    ///
    /// So a scaled app is reached by name at its first instance, on both
    /// backends. Spreading load across instances is `--publish`'s pool,
    /// which already round-robins (`publish::Pool::rotated`) and already
    /// works through the switch.
    pub fn alias(&mut self, name: &str, ip: Ipv4Addr) -> bool {
        if self.map.contains_key(name) {
            return false;
        }
        self.map.insert(name.to_string(), ip);
        true
    }

    /// This name's address if it has one. Never allocates: a DNS question
    /// for a name nobody has joined under is `NXDOMAIN`, not a new member.
    pub fn lookup(&self, name: &str) -> Option<Ipv4Addr> {
        self.map.get(name).copied()
    }
}

// ------------------------------------------------------------------- L2

/// A member of the switch, as the fabric knows it: an identity, an address
/// and a MAC. The channel to it lives in the run loop, so this half is pure
/// and its behaviour is tested on synthetic frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberInfo {
    pub id: MemberId,
    pub ip: Ipv4Addr,
    pub mac: EthernetAddress,
}

/// Identifies one attached member for the life of the switch. Never reused,
/// so a frame for a member that has left is dropped rather than delivered to
/// whoever took its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemberId(pub u64);

/// Where one frame goes after the fabric has looked at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// To exactly one member.
    To(MemberId, Vec<u8>),
    /// To every member except `except` — a broadcast or a multicast.
    Flood {
        except: Option<MemberId>,
        frame: Vec<u8>,
    },
    /// Into the switch's own TCP/IP stack: this frame is addressed to the
    /// gateway's MAC, which means the guest is talking to us or routing
    /// through us.
    Uplink(Vec<u8>),
}

/// The L2 half of the switch: who is on it, and where a frame goes.
#[derive(Debug, Default)]
pub struct Fabric {
    members: Vec<MemberInfo>,
}

impl Fabric {
    pub fn new() -> Fabric {
        Fabric {
            members: Vec::new(),
        }
    }

    /// Put a member on the fabric, displacing whatever held its identity or
    /// its address, and answer with the ids that were displaced.
    ///
    /// The address half is the load-bearing one. A member that dies and
    /// rejoins comes back under the same NAME, so [`Names`] hands it the same
    /// address — that stability is what keeps a peer's `/etc/hosts`, copied
    /// once at boot, correct. But its connection is new, so its
    /// [`MemberId`] is new, and without this line the fabric would hold two
    /// members with one MAC: `forward` finds the first, which is the corpse,
    /// and every frame for that address is delivered to a channel nobody is
    /// reading. No error, no log line — just a peer that has silently stopped
    /// answering.
    pub fn join(&mut self, info: MemberInfo) -> Vec<MemberId> {
        let displaced: Vec<MemberId> = self
            .members
            .iter()
            .filter(|m| m.id != info.id && (m.ip == info.ip || m.mac == info.mac))
            .map(|m| m.id)
            .collect();
        self.members
            .retain(|m| m.id != info.id && m.ip != info.ip && m.mac != info.mac);
        self.members.push(info);
        displaced
    }

    pub fn leave(&mut self, id: MemberId) {
        self.members.retain(|m| m.id != id);
    }

    pub fn members(&self) -> &[MemberInfo] {
        &self.members
    }

    /// The address this switch answers ARP for, or `None`.
    fn owner_of(&self, ip: Ipv4Addr) -> Option<EthernetAddress> {
        if ip == GATEWAY {
            return Some(gateway_mac());
        }
        self.members.iter().find(|m| m.ip == ip).map(|m| m.mac)
    }

    /// One frame a member transmitted.
    ///
    /// ARP requests are answered here rather than passed to smoltcp, so
    /// there is exactly ONE thing on this network that answers them and it
    /// answers for every address the switch knows — including a peer that
    /// has joined but has not yet sent a packet of its own. smoltcp still
    /// sees ARP *replies*, which is how its neighbour cache learns a guest's
    /// MAC when the switch is the one dialling in.
    pub fn from_member(&self, from: MemberId, frame: &[u8]) -> Vec<Delivery> {
        let Ok(eth) = EthernetFrame::new_checked(frame) else {
            return Vec::new();
        };
        if eth.ethertype() == EthernetProtocol::Arp {
            if let Some(reply) = self.arp_reply(eth.payload()) {
                return vec![Delivery::To(from, reply)];
            }
        }
        self.forward(eth.dst_addr(), Some(from), frame)
    }

    /// One frame the switch's own stack emitted — an ARP request it needs
    /// answered, or a segment for a guest. Never inspected: the stack does
    /// not talk to itself.
    pub fn from_gateway(&self, frame: &[u8]) -> Vec<Delivery> {
        let Ok(eth) = EthernetFrame::new_checked(frame) else {
            return Vec::new();
        };
        self.forward(eth.dst_addr(), None, frame)
    }

    fn forward(&self, dst: EthernetAddress, from: Option<MemberId>, frame: &[u8]) -> Vec<Delivery> {
        if dst.is_broadcast() || dst.is_multicast() {
            let mut out = vec![Delivery::Flood {
                except: from,
                frame: frame.to_vec(),
            }];
            // The switch is on this network too: an ARP request broadcast by
            // a member is how it learns of a peer, and dropping broadcasts
            // here would make the switch the only host that never hears one.
            if from.is_some() {
                out.push(Delivery::Uplink(frame.to_vec()));
            }
            return out;
        }
        if dst == gateway_mac() {
            return vec![Delivery::Uplink(frame.to_vec())];
        }
        match self
            .members
            .iter()
            .find(|m| m.mac == dst && Some(m.id) != from)
        {
            Some(m) => vec![Delivery::To(m.id, frame.to_vec())],
            // Every MAC on this network was handed out by this switch, so an
            // unknown unicast destination is not "somewhere we have not
            // learned yet" — it is nowhere. A real switch floods; flooding
            // here would only leak one member's traffic to another.
            None => Vec::new(),
        }
    }

    /// An ARP reply for a request we own the answer to, or `None` for
    /// anything else (a reply, a request for an address that is not on this
    /// switch, a malformed packet).
    fn arp_reply(&self, payload: &[u8]) -> Option<Vec<u8>> {
        let packet = ArpPacket::new_checked(payload).ok()?;
        let ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_protocol_addr,
            ..
        } = ArpRepr::parse(&packet).ok()?
        else {
            return None;
        };
        if operation != ArpOperation::Request {
            return None;
        }
        let mac = self.owner_of(target_protocol_addr)?;
        let arp = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: mac,
            source_protocol_addr: target_protocol_addr,
            target_hardware_addr: source_hardware_addr,
            target_protocol_addr: source_protocol_addr,
        };
        let eth = EthernetRepr {
            src_addr: mac,
            dst_addr: source_hardware_addr,
            ethertype: EthernetProtocol::Arp,
        };
        let mut out = vec![0u8; eth.buffer_len() + arp.buffer_len()];
        let mut frame = EthernetFrame::new_unchecked(&mut out[..]);
        eth.emit(&mut frame);
        arp.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
        Some(out)
    }
}

// --------------------------------------------------------------- the handle

/// A member's end of the wire: its address, and the two directions of
/// frames.
pub struct MemberLink {
    pub ip: Ipv4Addr,
    pub mac: EthernetAddress,
    pub gateway: Ipv4Addr,
    pub prefix_len: u8,
    /// Frames this member transmits.
    pub tx: FrameSink,
    /// Frames the switch has for this member.
    pub rx: mpsc::Receiver<Vec<u8>>,
}

/// Where a member's device puts the frames it transmits.
///
/// Two shapes, one interface, because a device must not know whether the
/// switch is in this process (a standalone `ply run`) or on the far end of a
/// unix socket (`ply up`'s stack switch, [`unix`]): the virtio-net model in
/// `vm/net.rs` calls [`FrameSink::send`] and nothing else.
#[derive(Clone)]
pub struct FrameSink {
    inner: SinkKind,
}

#[derive(Clone)]
enum SinkKind {
    /// The switch runs on a thread of this process: the frame goes straight
    /// into its event queue.
    ///
    /// Counted rather than bounded, because that queue carries control
    /// messages too and a bounded one a flooding guest could fill would
    /// block a `ply stop` behind a `wget`. The depth is visible to the
    /// device instead, which drops — which is what a NIC does when it
    /// cannot send.
    Local {
        id: MemberId,
        events: mpsc::Sender<nat::Event>,
        depth: Arc<AtomicUsize>,
    },
    /// The switch is another process's: the frame goes to the writer thread
    /// that owns this member's socket. Bounded, and a full queue drops —
    /// the same rule, enforced one hop earlier because the socket, not the
    /// switch, is what is behind.
    Remote(mpsc::SyncSender<Vec<u8>>),
}

impl FrameSink {
    /// Frames go to a switch reached over a unix socket; `tx` is drained by
    /// the writer thread [`unix::Client::attach`] spawned.
    pub(super) fn remote(tx: mpsc::SyncSender<Vec<u8>>) -> FrameSink {
        FrameSink {
            inner: SinkKind::Remote(tx),
        }
    }

    /// Hand one Ethernet frame to the switch. `false` when the switch is
    /// behind and the frame was dropped.
    pub fn send(&self, frame: Vec<u8>) -> bool {
        match &self.inner {
            SinkKind::Local { id, events, depth } => {
                if depth.load(Ordering::Relaxed) >= UPLINK_QUEUE {
                    return false;
                }
                depth.fetch_add(1, Ordering::Relaxed);
                if events
                    .send(nat::Event::Frame {
                        from: *id,
                        bytes: frame,
                    })
                    .is_err()
                {
                    depth.fetch_sub(1, Ordering::Relaxed);
                    return false;
                }
                true
            }
            SinkKind::Remote(tx) => tx.try_send(frame).is_ok(),
        }
    }
}

/// The switch itself: a thread, a name table, and a channel to it.
///
/// Cloning gives another handle to the same switch. The last handle to drop
/// stops the thread, which is what ties the network's lifetime to the run
/// parent's — the same lifetime the netns has on Linux, and for the same
/// reason.
#[derive(Clone)]
pub struct Switch {
    inner: Arc<Inner>,
}

struct Inner {
    events: mpsc::Sender<nat::Event>,
    names: Arc<Mutex<Names>>,
    next_member: AtomicUsize,
    depth: Arc<AtomicUsize>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.events.send(nat::Event::Stop);
        if let Ok(mut slot) = self.thread.lock() {
            if let Some(t) = slot.take() {
                let _ = t.join();
            }
        }
    }
}

impl Switch {
    /// Start a switch on its own thread.
    ///
    /// Fails only for the reasons a thread fails to spawn: everything the
    /// network needs is allocated inside the thread, and a switch with no
    /// members is a perfectly valid one.
    pub fn start() -> std::io::Result<Switch> {
        let (tx, rx) = mpsc::channel();
        let names = Arc::new(Mutex::new(Names::new()));
        let thread_names = names.clone();
        let thread_events = tx.clone();
        let depth = Arc::new(AtomicUsize::new(0));
        let thread_depth = depth.clone();
        let thread = std::thread::Builder::new()
            .name("ply-switch".into())
            .spawn(move || nat::run(rx, thread_events, thread_names, thread_depth))?;
        Ok(Switch {
            inner: Arc::new(Inner {
                events: tx,
                names,
                next_member: AtomicUsize::new(0),
                depth,
                thread: Mutex::new(Some(thread)),
            }),
        })
    }

    /// This name's address on the switch, allocated on first ask and stable
    /// afterwards.
    pub fn allocate(&self, name: &str) -> Ipv4Addr {
        let mut names = match self.inner.names.lock() {
            Ok(names) => names,
            // A poisoned name table means a panic on the switch thread. The
            // run is over either way; hand back the gateway rather than
            // panicking a second time in the supervisor.
            Err(poisoned) => poisoned.into_inner(),
        };
        names.allocate(name)
    }

    /// Point `name` at an address a member already has, unless something
    /// already answers to it. See [`Names::alias`].
    pub fn alias(&self, name: &str, ip: Ipv4Addr) -> bool {
        let mut names = match self.inner.names.lock() {
            Ok(names) => names,
            Err(poisoned) => poisoned.into_inner(),
        };
        names.alias(name, ip)
    }

    /// Address the switch answers `<name>.ply` with, if any.
    pub fn lookup(&self, name: &str) -> Option<Ipv4Addr> {
        let names = match self.inner.names.lock() {
            Ok(names) => names,
            Err(poisoned) => poisoned.into_inner(),
        };
        names.lookup(name)
    }

    pub fn gateway(&self) -> Ipv4Addr {
        GATEWAY
    }

    /// Attach one member and hand back its end of the wire.
    pub fn attach(&self, name: &str) -> MemberLink {
        let ip = self.allocate(name);
        let mac = member_mac(ip);
        let id = MemberId(self.inner.next_member.fetch_add(1, Ordering::Relaxed) as u64);
        let (tx, rx) = mpsc::sync_channel(MEMBER_QUEUE);
        let _ = self.inner.events.send(nat::Event::Join {
            info: MemberInfo { id, ip, mac },
            tx,
        });
        MemberLink {
            ip,
            mac,
            gateway: GATEWAY,
            prefix_len: PREFIX_LEN,
            tx: FrameSink {
                inner: SinkKind::Local {
                    id,
                    events: self.inner.events.clone(),
                    depth: self.inner.depth.clone(),
                },
            },
            rx,
        }
    }

    /// Open a TCP connection from the switch to `ip:port` inside the
    /// network, and hand back the host's end of it.
    ///
    /// This is the ONLY way anything on the Mac reaches a guest: the address
    /// exists on the switch and nowhere else, so a `TcpStream::connect` to
    /// it from the host would find nothing. `--publish` goes through here
    /// (via [`SwitchConnector`]), and so do the health gate and `--after`.
    pub fn connect(
        &self,
        ip: Ipv4Addr,
        port: u16,
        timeout: Duration,
    ) -> std::io::Result<UnixStream> {
        let (reply, answer) = mpsc::channel();
        let deadline = Instant::now() + timeout;
        self.inner
            .events
            .send(nat::Event::Connect {
                ip,
                port,
                deadline,
                reply,
            })
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "the switch is no longer running",
                )
            })?;
        // A little past the switch's own deadline, so the answer that comes
        // back is the switch's verdict ("refused", "timed out") rather than
        // this side giving up first and reporting something vaguer.
        match answer.recv_timeout(timeout + Duration::from_millis(250)) {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("the switch did not answer a connection to {ip}:{port} in time"),
            )),
        }
    }
}

// ---------------------------------------------------------------- the run's net

/// One run's network, from the run's point of view.
///
/// A `ply run` either OWNS the switch — a standalone run, one member on it —
/// or has JOINED one a `ply up` parent owns, over the unix socket that
/// parent passed on `--vswitch`. Everything above this line is written for
/// the first case and everything in [`unix`] for the second; this is the one
/// type the backend holds, so `launch`, `tcp_open` and `connector` do not
/// each have to know which they got.
///
/// Cloning is cheap and gives another handle to the same network. The last
/// handle of an owned one to drop stops the switch, which is what ties the
/// network's lifetime to the run parent's.
#[derive(Clone)]
pub enum Net {
    /// This process runs the switch.
    Own(Arc<unix::Server>),
    /// Another process runs it; we are a member and a client.
    Joined(unix::Client),
}

impl Net {
    /// Attach this instance under `slot` (`<app>.<n>`) and point `alias`
    /// (`<app>`) at the same address, unless something already answers to it.
    ///
    /// Two names in one call because they are one decision: the slot is the
    /// machine and the alias is what `<app>.ply` means, and a client that
    /// set them in two round trips could be interleaved with another
    /// member's between them.
    pub fn attach(&self, slot: &str, alias: &str) -> std::io::Result<MemberLink> {
        match self {
            Net::Own(server) => {
                let link = server.switch().attach(slot);
                server.switch().alias(alias, link.ip);
                Ok(link)
            }
            Net::Joined(client) => client.attach(slot, alias),
        }
    }

    /// The address the switch answers `<name>.ply` with, if any. Never
    /// allocates: a name nobody has joined under has no address, and the
    /// caller must write no `/etc/hosts` line for it.
    pub fn lookup(&self, name: &str) -> Option<Ipv4Addr> {
        match self {
            Net::Own(server) => server.switch().lookup(name),
            Net::Joined(client) => client.lookup(name).ok().flatten(),
        }
    }

    /// Open a TCP connection to `ip:port` INSIDE the network.
    ///
    /// The only way anything on the Mac reaches a guest: the address exists
    /// on the switch and nowhere else, so `TcpStream::connect` to it from
    /// the host finds nothing.
    pub fn connect(
        &self,
        ip: Ipv4Addr,
        port: u16,
        timeout: Duration,
    ) -> std::io::Result<UnixStream> {
        match self {
            Net::Own(server) => server.switch().connect(ip, port, timeout),
            Net::Joined(client) => client.dial(ip, port, timeout),
        }
    }

    /// How the published pool reaches one instance's port.
    pub fn connector(&self, ip: Ipv4Addr, port: u16) -> Arc<dyn Connector> {
        Arc::new(NetConnector {
            net: self.clone(),
            ip,
            port,
        })
    }

    /// The unix socket this network can be reached on from another process,
    /// when it has one.
    ///
    /// Recorded in the instance state file, because an address on a switch
    /// means nothing without it: `--after`'s port probe runs in a DIFFERENT
    /// `ply run` parent, and this is the only thing that tells it which
    /// switch to ask.
    pub fn socket(&self) -> Option<&std::path::Path> {
        match self {
            Net::Own(server) => server.path(),
            Net::Joined(client) => Some(client.path()),
        }
    }
}

/// A [`Connector`] that dials through the run's network.
///
/// `addr()` is the guest's real address on the switch — the honest answer,
/// and the one `serve`'s self-connection guard needs: it is never equal to a
/// host listener's address, because it names a machine that exists only on a
/// userspace network.
struct NetConnector {
    net: Net,
    ip: Ipv4Addr,
    port: u16,
}

impl Connector for NetConnector {
    fn connect(&self, timeout: Duration) -> std::io::Result<Box<dyn Upstream>> {
        let stream = self.net.connect(self.ip, self.port, timeout)?;
        Ok(Box::new(stream))
    }

    fn addr(&self) -> SocketAddr {
        SocketAddr::from((self.ip, self.port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    pub(super) fn arp_request(from: EthernetAddress, from_ip: Ipv4Addr, want: Ipv4Addr) -> Vec<u8> {
        let arp = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Request,
            source_hardware_addr: from,
            source_protocol_addr: from_ip,
            target_hardware_addr: EthernetAddress([0; 6]),
            target_protocol_addr: want,
        };
        let eth = EthernetRepr {
            src_addr: from,
            dst_addr: EthernetAddress::BROADCAST,
            ethertype: EthernetProtocol::Arp,
        };
        let mut out = vec![0u8; eth.buffer_len() + arp.buffer_len()];
        let mut frame = EthernetFrame::new_unchecked(&mut out[..]);
        eth.emit(&mut frame);
        arp.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
        out
    }

    pub(super) fn unicast(src: EthernetAddress, dst: EthernetAddress) -> Vec<u8> {
        let eth = EthernetRepr {
            src_addr: src,
            dst_addr: dst,
            ethertype: EthernetProtocol::Ipv4,
        };
        let mut out = vec![0u8; eth.buffer_len() + 20];
        eth.emit(&mut EthernetFrame::new_unchecked(&mut out[..]));
        out
    }

    fn fabric_of(n: usize) -> (Fabric, Vec<MemberInfo>) {
        let mut names = Names::new();
        let mut fabric = Fabric::new();
        let mut infos = Vec::new();
        for i in 0..n {
            let ip = names.allocate(&format!("m{i}"));
            let info = MemberInfo {
                id: MemberId(i as u64),
                ip,
                mac: member_mac(ip),
            };
            let _ = fabric.join(info);
            infos.push(info);
        }
        (fabric, infos)
    }

    #[test]
    fn a_member_gets_an_address_in_the_same_range_the_linux_bridge_uses() {
        let mut names = Names::new();
        let ip = names.allocate("db");
        assert_eq!(&ip.octets()[..2], &[10, 77]);
        assert_ne!(ip, GATEWAY, "the gateway is the switch itself");
        assert_eq!(ip, Ipv4Addr::new(10, 77, 0, 2), "the first member is .2");
    }

    #[test]
    fn the_same_name_always_gets_the_same_address_within_one_stack() {
        let mut names = Names::new();
        assert_eq!(names.allocate("db"), names.allocate("db"));
        assert_ne!(names.allocate("db"), names.allocate("web"));
        // And a name that never joined has no address at all: a DNS question
        // must not be able to conjure a member into existence.
        assert!(names.lookup("cache").is_none());
        assert_eq!(names.lookup("db"), Some(Ipv4Addr::new(10, 77, 0, 2)));
    }

    /// Instances are allocated under `<app>.<n>`, so `<app>.ply` is an
    /// ALIAS onto the first of them — and it has to stay pointing there,
    /// because a second instance taking the name would silently redirect
    /// every peer that is already talking to the first.
    #[test]
    fn the_app_name_follows_the_first_instance_and_no_later_one() {
        let mut names = Names::new();
        let first = names.allocate("db.1");
        let second = names.allocate("db.2");
        assert_ne!(first, second, "two instances are two machines");
        assert!(names.alias("db", first));
        assert!(!names.alias("db", second), "the first claimant keeps it");
        assert_eq!(names.lookup("db"), Some(first));
        // An alias never invents an address: `<app>.<n>` still resolves to
        // the instance's own, and nothing else moved.
        assert_eq!(names.lookup("db.1"), Some(first));
        assert_eq!(names.lookup("db.2"), Some(second));
    }

    #[test]
    fn a_mac_is_locally_administered_and_carries_its_own_address() {
        let mac = member_mac(Ipv4Addr::new(10, 77, 1, 9));
        assert_eq!(mac.0, [0x52, 0x54, 0x00, 0x77, 1, 9]);
        assert_eq!(mac.0[0] & 0x02, 0x02, "locally administered");
        assert_eq!(mac.0[0] & 0x01, 0x00, "not a multicast address");
        assert_ne!(mac, gateway_mac());
    }

    #[test]
    fn an_arp_request_for_the_gateway_is_answered_with_the_switchs_mac() {
        let (fabric, m) = fabric_of(1);
        let request = arp_request(m[0].mac, m[0].ip, GATEWAY);
        let out = fabric.from_member(m[0].id, &request);
        let [Delivery::To(to, reply)] = &out[..] else {
            panic!("one reply, to the asker: {out:?}");
        };
        assert_eq!(*to, m[0].id);
        let eth = EthernetFrame::new_checked(&reply[..]).expect("an ethernet frame");
        assert_eq!(eth.ethertype(), EthernetProtocol::Arp);
        assert_eq!(eth.dst_addr(), m[0].mac);
        let arp = ArpRepr::parse(&ArpPacket::new_checked(eth.payload()).expect("arp"))
            .expect("a well-formed arp packet");
        let ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            ..
        } = arp
        else {
            panic!("an Ethernet/IPv4 ARP reply");
        };
        assert_eq!(
            operation,
            ArpOperation::Reply,
            "an ARP reply, not a second request"
        );
        assert_eq!(source_protocol_addr, GATEWAY);
        assert_eq!(source_hardware_addr, gateway_mac());
    }

    #[test]
    fn an_arp_request_for_an_address_nobody_holds_is_not_answered() {
        let (fabric, m) = fabric_of(1);
        let request = arp_request(m[0].mac, m[0].ip, Ipv4Addr::new(10, 77, 9, 9));
        // It is flooded (there might be someone) and shown to the switch's
        // own stack, but the switch does not answer for an address it has
        // not handed out — proxy-ARP for the whole subnet would make every
        // peer look alive.
        let out = fabric.from_member(m[0].id, &request);
        assert!(
            !out.iter().any(|d| matches!(d, Delivery::To(..))),
            "no reply: {out:?}"
        );
    }

    #[test]
    fn a_frame_for_a_peer_is_delivered_to_that_peer_and_to_nobody_else() {
        let (fabric, m) = fabric_of(3);
        let frame = unicast(m[0].mac, m[1].mac);
        let out = fabric.from_member(m[0].id, &frame);
        let [Delivery::To(to, got)] = &out[..] else {
            panic!("exactly one delivery: {out:?}");
        };
        assert_eq!(*to, m[1].id);
        assert_eq!(got, &frame);
    }

    #[test]
    fn a_broadcast_reaches_every_member_except_the_sender() {
        let (fabric, m) = fabric_of(3);
        let frame = unicast(m[0].mac, EthernetAddress::BROADCAST);
        let out = fabric.from_member(m[0].id, &frame);
        let flood = out
            .iter()
            .find_map(|d| match d {
                Delivery::Flood { except, frame } => Some((*except, frame)),
                _ => None,
            })
            .expect("a flood");
        assert_eq!(flood.0, Some(m[0].id), "never back to the sender");
        assert_eq!(flood.1, &frame);
        assert!(
            out.iter().any(|d| matches!(d, Delivery::Uplink(_))),
            "the switch is on this network too and must see broadcasts"
        );
    }

    #[test]
    fn a_frame_addressed_to_the_gateway_goes_to_the_switchs_own_stack() {
        let (fabric, m) = fabric_of(2);
        let frame = unicast(m[0].mac, gateway_mac());
        assert_eq!(
            fabric.from_member(m[0].id, &frame),
            vec![Delivery::Uplink(frame.clone())]
        );
        // And a frame the stack emits for a member goes to that member, with
        // no inspection: `from_gateway` must not try to answer its own ARP.
        let down = unicast(gateway_mac(), m[1].mac);
        assert_eq!(
            fabric.from_gateway(&down),
            vec![Delivery::To(m[1].id, down.clone())]
        );
    }

    /// **A restart must not leave a corpse holding the address.**
    ///
    /// A member that dies and comes back is a NEW connection — a new
    /// [`MemberId`] — at the SAME name, so [`Names`] hands it the same
    /// address and the same MAC. Without the displacement in [`Fabric::join`]
    /// the fabric holds both, `forward` finds the older one first, and every
    /// frame for that address is delivered into a channel nobody reads: a
    /// peer that stops answering with no error anywhere.
    #[test]
    fn a_member_that_rejoins_takes_its_address_back_from_its_own_corpse() {
        let (mut fabric, m) = fabric_of(2);
        let reborn = MemberInfo {
            id: MemberId(99),
            ip: m[1].ip,
            mac: m[1].mac,
        };
        assert_eq!(
            fabric.join(reborn),
            vec![m[1].id],
            "the old holder of that address is displaced, and named so its \
             channel can be dropped"
        );
        assert_eq!(fabric.members().len(), 2, "still two machines, not three");
        let frame = unicast(m[0].mac, m[1].mac);
        assert_eq!(
            fabric.from_member(m[0].id, &frame),
            vec![Delivery::To(MemberId(99), frame.clone())],
            "the live member gets the frame, not the corpse"
        );
    }

    /// The other side of the same coin: joining does not disturb a member
    /// that shares nothing with the newcomer.
    #[test]
    fn a_new_address_displaces_nobody() {
        let (mut fabric, _) = fabric_of(2);
        let mut names = Names::new();
        names.allocate("m0");
        names.allocate("m1");
        let ip = names.allocate("m2");
        let fresh = MemberInfo {
            id: MemberId(7),
            ip,
            mac: member_mac(ip),
        };
        assert!(fabric.join(fresh).is_empty());
        assert_eq!(fabric.members().len(), 3);
    }

    #[test]
    fn a_member_that_left_receives_nothing() {
        let (mut fabric, m) = fabric_of(2);
        fabric.leave(m[1].id);
        assert!(fabric
            .from_member(m[0].id, &unicast(m[0].mac, m[1].mac))
            .is_empty());
    }

    // ------------------------------------------------------- end to end

    /// A guest, as far as the switch is concerned: a second smoltcp stack on
    /// the other end of a [`MemberLink`], on its own thread.
    ///
    /// This is what makes the NAT half testable at all on a machine with no
    /// Hypervisor.framework. It is the same code path a real guest drives —
    /// ARP, a handshake, windows, FINs — with a virtio-net device replaced
    /// by two channels.
    pub(super) struct FakeGuest {
        pub(super) commands: mpsc::Sender<GuestCommand>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    pub(super) enum GuestCommand {
        /// Accept one connection on `port`, echo every byte back, then stop.
        Echo {
            port: u16,
            done: mpsc::Sender<Vec<u8>>,
        },
        /// Dial `addr` through the gateway, send `send`, read to EOF.
        Dial {
            addr: SocketAddr,
            send: Vec<u8>,
            done: mpsc::Sender<Vec<u8>>,
        },
        Stop,
    }

    impl Drop for FakeGuest {
        fn drop(&mut self) {
            let _ = self.commands.send(GuestCommand::Stop);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// The guest's phy: frames in from the switch, frames out to it.
    struct ChannelPhy {
        rx: std::collections::VecDeque<Vec<u8>>,
        tx: Vec<Vec<u8>>,
    }

    impl smoltcp::phy::Device for ChannelPhy {
        type RxToken<'a> = PhyRx;
        type TxToken<'a> = PhyTx<'a>;

        fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
            let mut caps = smoltcp::phy::DeviceCapabilities::default();
            caps.medium = smoltcp::phy::Medium::Ethernet;
            caps.max_transmission_unit = MTU + 14;
            caps
        }

        fn receive(
            &mut self,
            _now: smoltcp::time::Instant,
        ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            let frame = self.rx.pop_front()?;
            Some((PhyRx(frame), PhyTx(&mut self.tx)))
        }

        fn transmit(&mut self, _now: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
            Some(PhyTx(&mut self.tx))
        }
    }

    struct PhyRx(Vec<u8>);
    impl smoltcp::phy::RxToken for PhyRx {
        fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
            f(&self.0)
        }
    }

    struct PhyTx<'a>(&'a mut Vec<Vec<u8>>);
    impl smoltcp::phy::TxToken for PhyTx<'_> {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            let mut buf = vec![0u8; len];
            let out = f(&mut buf);
            self.0.push(buf);
            out
        }
    }

    pub(super) fn start_guest(link: MemberLink) -> FakeGuest {
        use smoltcp::iface::{Config, Interface, SocketSet};
        use smoltcp::socket::tcp;
        use smoltcp::time::Instant as SmolInstant;
        use smoltcp::wire::{HardwareAddress, IpCidr};

        let (commands, orders) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("fake-guest".into())
            .spawn(move || {
                let mut phy = ChannelPhy {
                    rx: Default::default(),
                    tx: Vec::new(),
                };
                let started = Instant::now();
                let now = || SmolInstant::from_micros(started.elapsed().as_micros() as i64);
                let mut config = Config::new(HardwareAddress::Ethernet(link.mac));
                config.random_seed = 0x5eed;
                let mut iface = Interface::new(config, &mut phy, now());
                iface.update_ip_addrs(|addrs| {
                    let _ = addrs.push(IpCidr::new(link.ip.into(), link.prefix_len));
                });
                let _ = iface.routes_mut().add_default_ipv4_route(link.gateway);
                let mut sockets = SocketSet::new(Vec::new());

                let mut order: Option<GuestCommand> = None;
                let mut handle = None;
                let mut collected: Vec<u8> = Vec::new();
                let mut sent = false;
                loop {
                    if order.is_none() {
                        match orders.try_recv() {
                            Ok(GuestCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                                return
                            }
                            Ok(cmd) => {
                                let socket = tcp::Socket::new(
                                    tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
                                    tcp::SocketBuffer::new(vec![0u8; 64 * 1024]),
                                );
                                let h = sockets.add(socket);
                                let s = sockets.get_mut::<tcp::Socket>(h);
                                match &cmd {
                                    GuestCommand::Echo { port, .. } => {
                                        s.listen(*port).expect("listen")
                                    }
                                    GuestCommand::Dial { addr, .. } => s
                                        .connect(
                                            iface.context(),
                                            (
                                                smoltcp::wire::IpAddress::from(match addr.ip() {
                                                    std::net::IpAddr::V4(v4) => v4,
                                                    _ => unreachable!("ipv4 only"),
                                                }),
                                                addr.port(),
                                            ),
                                            49_152,
                                        )
                                        .expect("connect"),
                                    GuestCommand::Stop => unreachable!(),
                                }
                                handle = Some(h);
                                collected.clear();
                                sent = false;
                                order = Some(cmd);
                            }
                            Err(mpsc::TryRecvError::Empty) => {}
                        }
                    }

                    while let Ok(frame) = link.rx.try_recv() {
                        phy.rx.push_back(frame);
                    }
                    iface.poll(now(), &mut phy, &mut sockets);
                    for frame in phy.tx.drain(..) {
                        link.tx.send(frame);
                    }

                    if let (Some(h), Some(cmd)) = (handle, order.as_ref()) {
                        let s = sockets.get_mut::<tcp::Socket>(h);
                        let mut finished = false;
                        match cmd {
                            GuestCommand::Echo { done, .. } => {
                                let mut buf = [0u8; 4096];
                                while s.can_recv() {
                                    match s.recv_slice(&mut buf) {
                                        Ok(n) if n > 0 => {
                                            collected.extend_from_slice(&buf[..n]);
                                            let _ = s.send_slice(&buf[..n]);
                                        }
                                        _ => break,
                                    }
                                }
                                // `may_recv` is false in SYN-RECEIVED too, so
                                // testing it alone would close the connection
                                // in the middle of its own handshake.
                                let up = !matches!(
                                    s.state(),
                                    tcp::State::Listen
                                        | tcp::State::SynSent
                                        | tcp::State::SynReceived
                                );
                                if up && !s.may_recv() && !s.can_recv() {
                                    s.close();
                                }
                                if up && !s.is_open() {
                                    let _ = done.send(std::mem::take(&mut collected));
                                    finished = true;
                                }
                            }
                            GuestCommand::Dial { send, done, .. } => {
                                if !sent && s.can_send() {
                                    let _ = s.send_slice(send);
                                    s.close();
                                    sent = true;
                                }
                                let mut buf = [0u8; 4096];
                                if s.can_recv() {
                                    if let Ok(n) = s.recv_slice(&mut buf) {
                                        collected.extend_from_slice(&buf[..n]);
                                    }
                                }
                                if sent && !s.is_open() {
                                    let _ = done.send(std::mem::take(&mut collected));
                                    finished = true;
                                }
                            }
                            GuestCommand::Stop => unreachable!(),
                        }
                        if finished {
                            sockets.remove(h);
                            handle = None;
                            order = None;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
            .expect("the fake guest's thread");
        FakeGuest {
            commands,
            thread: Some(thread),
        }
    }

    /// **The property the acceptance test rests on.** The host has no route
    /// to `10.77.0.2`; the switch is the only thing that does, and this is
    /// how a published port reaches a guest.
    #[test]
    fn the_switch_dials_into_a_guest_and_carries_bytes_both_ways() {
        let switch = Switch::start().expect("a switch");
        let link = switch.attach("db");
        let ip = link.ip;
        let (done, echoed) = mpsc::channel();
        let guest = start_guest(link);
        guest
            .commands
            .send(GuestCommand::Echo { port: 5432, done })
            .expect("order the echo server up");

        let mut stream = switch
            .connect(ip, 5432, Duration::from_secs(5))
            .expect("the switch dials the guest");
        stream.write_all(b"select 1").expect("write");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("half close");
        let mut back = Vec::new();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        stream.read_to_end(&mut back).expect("read the echo");
        assert_eq!(back, b"select 1");
        assert_eq!(
            echoed.recv_timeout(Duration::from_secs(5)).expect("guest"),
            b"select 1"
        );
    }

    /// A closed port inside the guest must be refused, promptly and by the
    /// switch — a health gate that cannot tell "not listening" from "still
    /// booting" waits out its whole window on every failure.
    #[test]
    fn a_port_nothing_is_listening_on_is_refused() {
        let switch = Switch::start().expect("a switch");
        let link = switch.attach("db");
        let ip = link.ip;
        let _guest = start_guest(link);
        let err = switch
            .connect(ip, 5999, Duration::from_secs(3))
            .expect_err("nothing is listening there");
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
            ),
            "unexpected error: {err:?}"
        );
    }

    /// **Egress.** A guest dialling an address on the real network gets a
    /// real host socket, which is what makes a microVM usable for builds.
    #[test]
    fn a_guest_reaches_a_host_socket_through_the_switch() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("a host listener");
        let addr = listener.local_addr().expect("its address");
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let mut conn = conn;
                let mut got = Vec::new();
                let _ = conn.read_to_end(&mut got);
                let _ = conn.write_all(b"HTTP/1.0 200 OK\r\n\r\n");
                let _ = conn.shutdown(std::net::Shutdown::Write);
            }
        });

        let switch = Switch::start().expect("a switch");
        let link = switch.attach("builder");
        let (done, answered) = mpsc::channel();
        let guest = start_guest(link);
        guest
            .commands
            .send(GuestCommand::Dial {
                addr,
                send: b"GET / HTTP/1.0\r\n\r\n".to_vec(),
                done,
            })
            .expect("order the dial");
        let got = answered
            .recv_timeout(Duration::from_secs(10))
            .expect("the guest got an answer");
        assert_eq!(got, b"HTTP/1.0 200 OK\r\n\r\n");
    }

    /// A `<name>.ply` question is answered by the switch's own resolver,
    /// over the guest's own UDP — the whole path, not just [`dns::decide`].
    #[test]
    fn a_ply_name_resolves_through_the_switchs_own_resolver() {
        let switch = Switch::start().expect("a switch");
        let db = switch.allocate("db");
        let link = switch.attach("web");
        let mac = link.mac;
        let ip = link.ip;

        // One DNS query, built by hand and put on the wire as the guest
        // would: an IPv4/UDP datagram to the gateway's port 53.
        let mut query = vec![0xab, 0xcd, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        query.push(2);
        query.extend_from_slice(b"db");
        query.push(3);
        query.extend_from_slice(b"ply");
        query.extend_from_slice(&[0, 0, 1, 0, 1]);
        let frame = udp_frame(mac, ip, GATEWAY, 40_000, 53, &query);
        assert!(link.tx.send(frame), "the switch accepted the query");

        let reply = wait_for_udp(&link, Duration::from_secs(5)).expect("an answer");
        assert_eq!(&reply[..2], &[0xab, 0xcd], "the same transaction");
        assert_eq!(u16::from_be_bytes([reply[6], reply[7]]), 1, "one answer");
        assert_eq!(
            &reply[reply.len() - 4..],
            &db.octets(),
            "the address the switch allocated for db"
        );
    }

    /// The next frame the switch sends this member, or `None`.
    pub(super) fn wait_for_frame(
        rx: &mpsc::Receiver<Vec<u8>>,
        within: Duration,
    ) -> Option<Vec<u8>> {
        rx.recv_timeout(within).ok()
    }

    /// An ARP reply for `ip`, if `payload` is a request for it.
    fn arp_answer(payload: &[u8], ip: Ipv4Addr, mac: EthernetAddress) -> Option<Vec<u8>> {
        let packet = ArpPacket::new_checked(payload).ok()?;
        let ArpRepr::EthernetIpv4 {
            operation,
            source_hardware_addr,
            source_protocol_addr,
            target_protocol_addr,
            ..
        } = ArpRepr::parse(&packet).ok()?
        else {
            return None;
        };
        if operation != ArpOperation::Request || target_protocol_addr != ip {
            return None;
        }
        let arp = ArpRepr::EthernetIpv4 {
            operation: ArpOperation::Reply,
            source_hardware_addr: mac,
            source_protocol_addr: ip,
            target_hardware_addr: source_hardware_addr,
            target_protocol_addr: source_protocol_addr,
        };
        let eth = EthernetRepr {
            src_addr: mac,
            dst_addr: source_hardware_addr,
            ethertype: EthernetProtocol::Arp,
        };
        let mut out = vec![0u8; eth.buffer_len() + arp.buffer_len()];
        let mut frame = EthernetFrame::new_unchecked(&mut out[..]);
        eth.emit(&mut frame);
        arp.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()));
        Some(out)
    }

    /// One IPv4/UDP datagram in an Ethernet frame, addressed to the gateway.
    fn udp_frame(
        src_mac: EthernetAddress,
        src: Ipv4Addr,
        dst: Ipv4Addr,
        sport: u16,
        dport: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        use smoltcp::phy::ChecksumCapabilities;
        use smoltcp::wire::{IpProtocol, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr};
        let udp = UdpRepr {
            src_port: sport,
            dst_port: dport,
        };
        let ip = Ipv4Repr {
            src_addr: src,
            dst_addr: dst,
            next_header: IpProtocol::Udp,
            payload_len: udp.header_len() + payload.len(),
            hop_limit: 64,
        };
        let eth = EthernetRepr {
            src_addr: src_mac,
            dst_addr: gateway_mac(),
            ethertype: EthernetProtocol::Ipv4,
        };
        let mut out = vec![0u8; eth.buffer_len() + ip.buffer_len() + ip.payload_len];
        let mut frame = EthernetFrame::new_unchecked(&mut out[..]);
        eth.emit(&mut frame);
        let checksum = ChecksumCapabilities::default();
        let mut packet = Ipv4Packet::new_unchecked(frame.payload_mut());
        ip.emit(&mut packet, &checksum);
        udp.emit(
            &mut UdpPacket::new_unchecked(packet.payload_mut()),
            &src.into(),
            &dst.into(),
            payload.len(),
            |buf| buf.copy_from_slice(payload),
            &checksum,
        );
        out
    }

    /// The UDP payload of the next frame the switch sends this member.
    ///
    /// Answers ARP on the way, because there is no guest here to do it: the
    /// switch has to resolve this member's MAC before it can send the reply,
    /// exactly as it would to a real one.
    fn wait_for_udp(link: &MemberLink, within: Duration) -> Option<Vec<u8>> {
        use smoltcp::wire::{Ipv4Packet, UdpPacket};
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            let Ok(frame) = link.rx.recv_timeout(Duration::from_millis(100)) else {
                continue;
            };
            let Ok(eth) = EthernetFrame::new_checked(&frame[..]) else {
                continue;
            };
            if eth.ethertype() == EthernetProtocol::Arp {
                if let Some(reply) = arp_answer(eth.payload(), link.ip, link.mac) {
                    link.tx.send(reply);
                }
                continue;
            }
            if eth.ethertype() != EthernetProtocol::Ipv4 {
                continue;
            }
            let Ok(ip) = Ipv4Packet::new_checked(eth.payload()) else {
                continue;
            };
            let Ok(udp) = UdpPacket::new_checked(ip.payload()) else {
                continue;
            };
            return Some(udp.payload().to_vec());
        }
        None
    }
}
