//! The per-instance egress thread: lives in the instance's network
//! namespace, serves DNS on 127.0.0.53, owns the `egress_<app>` table,
//! pins what the app resolves, polls what it connected to, writes the
//! audit log and emits events. Started before the child is released;
//! ends when its handle drops.
//!
//! Everything it decides comes from `crate::egress` — the policy (Task 1),
//! the table text (Task 4), the DNS wire (Task 5), the log (Task 6). This
//! file is the only place that runs `nft` or holds a socket, and the only
//! part of the feature that a unit test on a laptop cannot reach: it needs
//! a namespace, a nftables kernel and a child to watch.
//!
//! Everything it tracks is fed by the container, so every map here is
//! capped and swept, every child process is time-bounded, and every turn of
//! the loop is short enough that the app cannot keep it away from its own
//! bookkeeping.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::egress::{dns, log, nft, Mode, Policy};
use crate::error::{Error, Result};
use crate::runtime::events;
use crate::runtime::ns::netns;

/// How often the counters are read back out of the two audit sets.
const POLL: Duration = Duration::from_secs(2);
/// The forwarder's UDP read timeout — also the longest the loop can go
/// without noticing the stop channel.
const WAKE: Duration = Duration::from_millis(200);
/// One event per destination per hour: a blocked app retries hard, and the
/// journal is a record of what happened, not of how often.
const EVENT_EVERY: Duration = Duration::from_secs(3600);
/// One `refused`/`resolved` record a minute per name — the same damping the
/// connection records have, for the same reason: the log is evidence, and
/// the app decides how often it resolves.
const DNS_RECORD_EVERY: Duration = Duration::from_secs(60);
/// How long an address keeps the name it was resolved from, for the
/// connection records.
const NAME_TTL: Duration = Duration::from_secs(3600);
/// An entry no poll has touched for this long is forgotten — the same
/// lifetime the `allowed`/`blocked` set elements have in the kernel.
const TRACK_TTL: Duration = Duration::from_secs(24 * 3600);
/// Cap on every map the container can grow. Past it the map stops taking
/// new destinations and says so once, rather than growing without bound.
const MAX_TRACKED: usize = 10_000;
/// Per-upstream patience for one forwarded query.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);
/// Patience for a DNS-over-TCP client that opened a connection. Short: a
/// client that connects and says nothing costs the loop this much.
const TCP_TIMEOUT: Duration = Duration::from_millis(500);
/// TCP clients served per turn, so a container that opens connections in a
/// tight loop cannot keep the thread away from the poll below.
const MAX_ACCEPTS: usize = 8;
/// How long any one `nft` may take. A wedged one would otherwise hold the
/// thread — and with it the namespace and its veth — for ever.
const NFT_TIMEOUT: Duration = Duration::from_secs(5);
/// The largest DNS message we will read from a client.
const MAX_MSG: usize = 65_535;

/// Where the forwarder listens inside the instance's netns — the same
/// address `container.rs` writes into the instance's `resolv.conf`, named
/// once so the two can never drift.
const LISTEN: (&str, u16) = (crate::runtime::ns::container::EGRESS_FORWARDER, 53);

/// The three sockets the thread owns for the life of the instance: the two
/// the container resolves through, and the one it asks upstreams from.
struct Sockets {
    udp: UdpSocket,
    tcp: TcpListener,
    upstream: UdpSocket,
}

pub struct EgressHandle {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for EgressHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(t) = self.thread.take() {
            // The loop wakes every 200 ms; a join bound keeps a wedged nft
            // from holding the supervisor's teardown.
            let deadline = Instant::now() + Duration::from_secs(3);
            while !t.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if t.is_finished() {
                let _ = t.join();
            }
        }
    }
}

/// One destination as the audit sets key it.
type Key = (Ipv4Addr, u16, String);

/// Said once per map, the first time it fills.
fn warn_full(warned: &mut bool, what: &str) {
    if !*warned {
        *warned = true;
        eprintln!(
            "ply: warning: egress: {what} is full ({MAX_TRACKED} entries) — further destinations are not tracked"
        );
    }
}

pub(crate) struct Throttle {
    every: Duration,
    last: HashMap<Key, Instant>,
    full_warned: bool,
}

impl Throttle {
    pub(crate) fn new(every: Duration) -> Self {
        Throttle {
            every,
            last: HashMap::new(),
            full_warned: false,
        }
    }
    pub(crate) fn allow(&mut self, key: &Key, now: Instant) -> bool {
        if let Some(t) = self.last.get(key) {
            if now.duration_since(*t) < self.every {
                return false;
            }
            self.last.insert(key.clone(), now);
            return true;
        }
        if self.last.len() >= MAX_TRACKED {
            warn_full(&mut self.full_warned, "the event throttle");
            return false;
        }
        self.last.insert(key.clone(), now);
        true
    }
    /// Forget destinations nothing has mentioned in a day.
    pub(crate) fn sweep(&mut self, now: Instant) {
        self.last.retain(|_, t| now.duration_since(*t) < TRACK_TTL);
    }
}

/// The two DNS records the forwarder writes. Damped per `(name, kind)`:
/// the log is evidence, and an app that asks for the same name in a loop
/// must not be able to rotate everything else out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DnsKind {
    Resolved,
    Refused,
}

/// One record a minute per `(name, kind)`, carrying how many queries it
/// stands for.
///
/// Written per query, `refused` and `resolved` are the two records a
/// container controls the rate of completely: a `getent` loop over made-up
/// names rotates the 512 KiB log in seconds and takes the connection
/// evidence with it. Damping keeps the first sighting instant (which is
/// what an operator actually needs) and turns the flood into a count.
pub(crate) struct DnsDamp {
    every: Duration,
    /// name+kind → (when a record was last written, queries since then).
    last: HashMap<(String, DnsKind), (Instant, u64)>,
    full_warned: bool,
}

impl DnsDamp {
    pub(crate) fn new(every: Duration) -> Self {
        DnsDamp {
            every,
            last: HashMap::new(),
            full_warned: false,
        }
    }

    /// `Some(count)` when this query is to be written — `count` being the
    /// queries it stands for, this one included — and `None` when it falls
    /// inside the minute.
    pub(crate) fn allow(&mut self, name: &str, kind: DnsKind, now: Instant) -> Option<u64> {
        let key = (name.to_string(), kind);
        if let Some((at, pending)) = self.last.get_mut(&key) {
            *pending += 1;
            if now.duration_since(*at) < self.every {
                return None;
            }
            let count = *pending;
            *at = now;
            *pending = 0;
            return Some(count);
        }
        if self.last.len() >= MAX_TRACKED {
            // Past the cap a name has no history, so a record for it could
            // not carry an honest count — and the flood that filled the map
            // is exactly what the damping exists to stop.
            warn_full(&mut self.full_warned, "the resolved-name log damping");
            return None;
        }
        self.last.insert(key, (now, 0));
        Some(1)
    }

    /// Forget names nothing has asked for in a day.
    pub(crate) fn sweep(&mut self, now: Instant) {
        self.last
            .retain(|_, (at, _)| now.duration_since(*at) < TRACK_TTL);
    }
}

/// What one poll decided about one destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Blocked,
    Allowed,
}

/// One line the poll wants written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PollRecord {
    pub(crate) key: Key,
    pub(crate) verdict: Verdict,
    /// The destination's counter in the set that made this verdict —
    /// `@allowed` for `Allowed`, `@blocked` for `Blocked`. Each verdict has
    /// a counter of its own in the kernel, so nothing here is a difference
    /// of two numbers read at two different instants.
    pub(crate) count: u64,
}

/// What the previous poll saw for one destination.
struct Tracked {
    /// Counters as of the last poll. The two sets count DISJOINT packets:
    /// the allow rules update `@allowed` and only the fall-through updates
    /// `@blocked`, so each delta stands on its own.
    allowed: u64,
    blocked: u64,
    /// When a line of each kind was last written, for the minute damping.
    blocked_at: Option<Instant>,
    allowed_at: Option<Instant>,
    /// The last poll that touched this key — what the sweep ages out.
    at: Instant,
}

/// The connection records, decided from per-poll deltas.
///
/// Presence in a set says nothing: `allowed` and `blocked` elements live 24 h
/// in the kernel, long after the traffic that made them. Only MOVEMENT since
/// the previous poll does — which is also why a destination cannot be
/// reported `allowed` because its `blocked` element went quiet, the way a
/// presence-based rule would.
#[derive(Default)]
pub(crate) struct Tracker {
    keys: HashMap<Key, Tracked>,
    full_warned: bool,
}

/// What a counter did since the last poll. A counter that went BACKWARDS is
/// an element the kernel flushed and recreated, so everything it holds now
/// is new traffic — never a wrapped, enormous delta, and never a zero that
/// would hide the traffic that recreated it.
fn growth(before: u64, now: u64) -> u64 {
    if now >= before {
        now - before
    } else {
        now
    }
}

/// A line of this kind is written at most once a minute per destination.
fn damped(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(60))
}

impl Tracker {
    /// The records two fresh snapshots call for, oldest state updated in
    /// place.
    ///
    /// Each verdict has its own counter in the kernel, so each is read
    /// straight out of its own set: `@allowed` grew → something got through,
    /// `@blocked` grew → something did not. Nothing is inferred by
    /// subtracting one set from the other, so the order the two are read in
    /// cannot invent a verdict.
    ///
    /// Pure but for the state and the once-only "full" warning: everything
    /// the poll decides is decided here.
    pub(crate) fn poll(
        &mut self,
        allowed: &[nft::SetElement],
        blocked: &[nft::SetElement],
        now: Instant,
    ) -> Vec<PollRecord> {
        // Sorted, so the log's order does not depend on a hash seed.
        let mut counts: BTreeMap<Key, (u64, u64)> = BTreeMap::new();
        for e in allowed {
            counts
                .entry((e.addr, e.port, e.proto.clone()))
                .or_default()
                .0 = e.packets;
        }
        for e in blocked {
            counts
                .entry((e.addr, e.port, e.proto.clone()))
                .or_default()
                .1 = e.packets;
        }

        let mut out = Vec::new();
        for (key, (allowed_now, blocked_now)) in counts {
            if !self.keys.contains_key(&key) && self.keys.len() >= MAX_TRACKED {
                // Past the cap this destination has no history, so anything
                // said about it would be a guess. Silence, never a lie.
                warn_full(&mut self.full_warned, "the connection tracker");
                continue;
            }
            let t = self.keys.entry(key.clone()).or_insert(Tracked {
                allowed: 0,
                blocked: 0,
                blocked_at: None,
                allowed_at: None,
                at: now,
            });
            // First sight starts from zero, so a destination's whole count
            // is its first delta.
            let ad = growth(t.allowed, allowed_now);
            let bd = growth(t.blocked, blocked_now);
            if bd > 0 && damped(t.blocked_at, now) {
                t.blocked_at = Some(now);
                out.push(PollRecord {
                    key: key.clone(),
                    verdict: Verdict::Blocked,
                    count: blocked_now,
                });
            }
            if ad > 0 && damped(t.allowed_at, now) {
                t.allowed_at = Some(now);
                out.push(PollRecord {
                    key: key.clone(),
                    verdict: Verdict::Allowed,
                    count: allowed_now,
                });
            }
            // Always, whatever was written: the next poll compares against
            // what the kernel says NOW, not against the last line logged.
            t.allowed = allowed_now;
            t.blocked = blocked_now;
            t.at = now;
        }
        out
    }

    /// Forget destinations no poll has seen in a day — the kernel has
    /// dropped their elements by then too.
    pub(crate) fn sweep(&mut self, now: Instant) {
        self.keys
            .retain(|_, t| now.duration_since(t.at) < TRACK_TTL);
    }
}

/// What an address was resolved from, for the connection records.
#[derive(Default)]
pub(crate) struct NameMap {
    last: HashMap<Ipv4Addr, (String, Instant)>,
    full_warned: bool,
}

impl NameMap {
    pub(crate) fn remember(&mut self, addr: Ipv4Addr, name: &str, now: Instant) {
        if !self.last.contains_key(&addr) && self.last.len() >= MAX_TRACKED {
            warn_full(&mut self.full_warned, "the resolved-name map");
            return;
        }
        self.last.insert(addr, (name.to_string(), now));
    }
    pub(crate) fn name_of(&self, addr: Ipv4Addr) -> Option<String> {
        self.last.get(&addr).map(|(name, _)| name.clone())
    }
    pub(crate) fn sweep(&mut self, now: Instant) {
        self.last
            .retain(|_, (_, at)| now.duration_since(*at) < NAME_TTL);
    }
}

/// The shortest TTL in the answer, floored at five minutes: a CDN's 30 s
/// record would otherwise expire the pin between the resolve and the
/// connect, and the app would be blocked for a name it was allowed.
pub(crate) fn pin_ttl(records: &[(Ipv4Addr, u32)]) -> u32 {
    records
        .iter()
        .map(|(_, ttl)| *ttl)
        .min()
        .unwrap_or(300)
        .max(300)
}

/// Wait for a child, killing it if it outstays `NFT_TIMEOUT`. Every `nft`
/// this file runs goes through here.
fn wait_bounded(child: &mut std::process::Child) -> Result<std::process::ExitStatus> {
    let deadline = Instant::now() + NFT_TIMEOUT;
    // Backs off 2 ms → 50 ms: a pin sits in front of a DNS answer the app is
    // waiting on, and a flat 50 ms poll would add that to every resolution.
    let mut step = Duration::from_millis(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(e) => return Err(Error::Runtime(format!("egress policy: nft: {e}"))),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Runtime(
                "egress policy: nft did not finish within 5s".into(),
            ));
        }
        std::thread::sleep(step);
        step = (step * 2).min(Duration::from_millis(50));
    }
}

/// Drain a child's pipe from a thread of its own, so a big write cannot
/// deadlock against our `try_wait` loop and a killed child still ends it.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    })
}

fn run_nft(script: &str) -> Result<()> {
    let mut child = std::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime(format!("egress policy: nft: {e}")))?;
    let errors = drain(child.stderr.take());
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
    }
    let status = wait_bounded(&mut child)?;
    let errors = errors.join().unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(Error::Runtime(format!(
            "egress policy: nft: {}",
            errors.trim()
        )))
    }
}

fn list_set(app: &str, set: &str) -> Result<Vec<nft::SetElement>> {
    let mut child = std::process::Command::new("nft")
        .args(["-j", "list", "set", "inet", &nft::table_name(app), set])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime(format!("egress policy: nft -j: {e}")))?;
    let out = drain(child.stdout.take());
    let errors = drain(child.stderr.take());
    let status = wait_bounded(&mut child)?;
    let text = out.join().unwrap_or_default();
    if !status.success() {
        // Say what nft said — a missing table reads far better than the
        // "EOF while parsing" the empty stdout would produce below.
        let errors = errors.join().unwrap_or_default();
        let detail = match errors.trim() {
            "" => format!("exit {status}"),
            said => said.to_string(),
        };
        return Err(Error::Runtime(format!("egress policy: nft -j: {detail}")));
    }
    nft::parse_set_elements(&text)
}

/// The addresses a plain `ipv4_addr` set holds right now — `allow_dns`,
/// read back so a re-pin knows what to delete before it adds.
fn list_addr_set(app: &str, set: &str) -> Result<Vec<Ipv4Addr>> {
    let mut child = std::process::Command::new("nft")
        .args(["-j", "list", "set", "inet", &nft::table_name(app), set])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime(format!("egress policy: nft -j: {e}")))?;
    let out = drain(child.stdout.take());
    let errors = drain(child.stderr.take());
    let status = wait_bounded(&mut child)?;
    let text = out.join().unwrap_or_default();
    if !status.success() {
        let errors = errors.join().unwrap_or_default();
        let detail = match errors.trim() {
            "" => format!("exit {status}"),
            said => said.to_string(),
        };
        return Err(Error::Runtime(format!("egress policy: nft -j: {detail}")));
    }
    Ok(nft::parse_addr_set(&text))
}

/// Is this answer worth pinning into `allow_dns`?
///
/// A declared name that resolves to a private, loopback or link-local
/// address is a DNS-rebinding lever: `169.254.169.254.nip.io` under a
/// declared `*.nip.io` would pin the cloud metadata service, and a name
/// pointing at `10.77.0.x` would pin a neighbour instance. The bridge
/// subnet is accepted by its own rule anyway, and everything else here is
/// somewhere the operator never meant "the internet" to mean.
///
/// The escape hatch is the policy itself: an operator who writes the
/// address (or a range holding it) into the list has said it out loud, and
/// `allow_static` covers it — so the pin adds nothing, but refusing it
/// would contradict a declaration.
fn pinnable(addr: Ipv4Addr, policy: &Policy) -> bool {
    if policy.allow.iter().any(|e| e.matches_addr(addr)) {
        return true;
    }
    let o = addr.octets();
    let cgnat = o[0] == 100 && (64..128).contains(&o[1]);
    // 10.77.0.0/16 is inside is_private(), and named here because it is the
    // one range with a ply meaning: the bridge every instance sits on.
    let bridge = o[0] == 10 && o[1] == 77;
    !(addr.is_loopback()
        || addr.is_link_local()
        || addr.is_private()
        || addr.is_multicast()
        || addr.is_broadcast()
        || addr.is_unspecified()
        || o[0] == 0
        || cgnat
        || bridge)
}

/// Start the thread for `app.n` whose child is `child_pid`. Returns once
/// the forwarder listens and the table is installed, or with the reason it
/// could not; the caller decides what a failure means for its mode.
pub fn spawn(
    app: &str,
    n: u32,
    child_pid: i32,
    policy: &Policy,
    upstreams: Vec<Ipv4Addr>,
) -> Result<EgressHandle> {
    if !crate::runtime::ns::network::has_nft() {
        return Err(Error::Runtime(
            "egress policy: nft not found — install nftables or run with --egress audit".into(),
        ));
    }
    if upstreams.is_empty() {
        // The instance's resolv.conf now names the forwarder, so the
        // diagnostic `resolv_conf_for_instance` would have printed never
        // gets a chance — say it here instead of SERVFAILing in silence.
        eprintln!("ply: warning: egress: no upstream resolver found — the host's /etc/resolv.conf points at a loopback stub and no upstream was found; names will not resolve inside {app}");
    }
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let app = app.to_string();
    let policy = policy.clone();
    let thread = std::thread::Builder::new()
        .name(format!("egress-{app}.{n}"))
        .spawn(move || {
            let setup = (|| -> Result<Sockets> {
                let ns = netns::open_ns(child_pid)?;
                netns::enter(&ns)?;
                let (host, port) = LISTEN;
                let udp = UdpSocket::bind(LISTEN).map_err(|e| {
                    Error::Runtime(format!("egress policy: binding {host}:{port}: {e}"))
                })?;
                udp.set_read_timeout(Some(WAKE)).ok();
                let tcp = TcpListener::bind(LISTEN).map_err(|e| {
                    Error::Runtime(format!("egress policy: binding {host}:{port}/tcp: {e}"))
                })?;
                tcp.set_nonblocking(true).ok();
                // Held for the life of the instance: the table's DNS rule
                // keys on this source port, and holding it is also what
                // stops the app from binding it and speaking as us.
                let upstream_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, dns::UPSTREAM_SPORT))
                    .map_err(|e| {
                    Error::Runtime(format!(
                        "egress policy: binding the upstream port {}: {e}",
                        dns::UPSTREAM_SPORT
                    ))
                })?;
                run_nft(&nft::nft_script(&app, &policy, &upstreams))?;
                Ok(Sockets {
                    udp,
                    tcp,
                    upstream: upstream_sock,
                })
            })();
            let socks = match setup {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            serve(&app, n, &policy, &upstreams, socks, stop_rx);
        })
        .map_err(|e| Error::Runtime(format!("egress policy: thread: {e}")))?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(EgressHandle {
            stop: stop_tx,
            thread: Some(thread),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Runtime(
            "egress policy: the forwarder did not start within 5s".into(),
        )),
    }
}

/// What one query gets. `declared` is the audit log's word, not the
/// verdict: in `audit` an undeclared name is forwarded AND recorded as
/// undeclared, which is the whole point of the mode.
enum Decision {
    Forward { name: String, declared: bool },
    Refuse { name: String, declared: bool },
}

/// `Any` or a matching entry → declared; otherwise `enforce` refuses and
/// `audit` forwards. A message that does not carry exactly one question is
/// refused whatever the mode: only the first question is vetted here and
/// the forwarded bytes would carry all of them.
fn decide(policy: &Policy, query: &[u8]) -> Decision {
    let question = dns::question(query);
    let name = question
        .as_ref()
        .map(|q| q.name.clone())
        .unwrap_or_else(|| "?".into());
    if dns::qdcount(query) != Some(1) {
        return Decision::Refuse {
            name,
            declared: false,
        };
    }
    // `allows_name` rejects the root name (""), so `*` is checked first:
    // an unrestricted policy declares everything, including a root query.
    let declared = question
        .as_ref()
        .is_some_and(|q| policy.unrestricted() || policy.allows_name(&q.name));
    if declared || policy.mode != Mode::Enforce {
        Decision::Forward { name, declared }
    } else {
        Decision::Refuse { name, declared }
    }
}

/// Everything the loop carries between turns.
struct Serve<'a> {
    app: &'a str,
    n: u32,
    policy: &'a Policy,
    /// The resolvers the forwarder may ask, on port 53.
    upstreams: Vec<(Ipv4Addr, u16)>,
    /// Every upstream query goes out of this one socket: the table accepts
    /// its source port and nothing else.
    upstream_sock: UdpSocket,
    /// `None` once the audit log could not be opened — said once, then the
    /// thread goes on enforcing, which matters more than the record.
    writer: Option<log::Writer>,
    names: NameMap,
    tracker: Tracker,
    events: Throttle,
    /// One `refused`/`resolved` line a minute per name (I2): the container
    /// decides how often these happen, so it must not decide how fast the
    /// log rotates.
    dns: DnsDamp,
    pin_warned: bool,
    /// `allow_dns` could not be listed — said once, then pins go in with a
    /// plain `add` (which on kernels before 6.10 does not refresh a
    /// timeout).
    repin_warned: bool,
    /// The audit sets could not be read last poll — said once, and once
    /// again when they come back.
    poll_warned: bool,
}

impl Serve<'_> {
    fn write(&mut self, record: log::Record) {
        if let Some(w) = self.writer.as_mut() {
            w.write(&record);
        }
    }

    /// The reply for one query, and everything it teaches us.
    fn answer(&mut self, query: &[u8]) -> Vec<u8> {
        match decide(self.policy, query) {
            Decision::Refuse { name, declared } => {
                if let Some(count) = self.dns.allow(&name, DnsKind::Refused, Instant::now()) {
                    self.write(log::Record::Refused {
                        t: log::now_rfc3339(),
                        app: self.app.to_string(),
                        n: self.n,
                        name,
                        declared,
                        count,
                    });
                }
                dns::refused_reply(query)
            }
            Decision::Forward { name, declared } => {
                match dns::forward_via(
                    &self.upstream_sock,
                    query,
                    &self.upstreams,
                    UPSTREAM_TIMEOUT,
                ) {
                    Ok(reply) => {
                        self.learn(&name, declared, &reply);
                        reply
                    }
                    // No upstream answered: SERVFAIL, so the resolver in the
                    // container retries or gives up instead of caching a lie.
                    Err(_) => dns::error_reply(query, 2),
                }
            }
        }
    }

    /// Pin what a declared name resolved to, remember the name for every
    /// answer (declared or not — an undeclared connection deserves a name
    /// in the log too), and record the resolution.
    fn learn(&mut self, name: &str, declared: bool, reply: &[u8]) {
        let records = dns::a_records(reply);
        let now = Instant::now();
        if records.is_empty() {
            // An AAAA-only answer, an NXDOMAIN, a CNAME-only reply: nothing
            // to pin and nothing the instance can connect to over v4 — but
            // the lookup HAPPENED, and an audit trail that omits it cannot
            // show what an app was reaching for over IPv6.
            if let Some(count) = self.dns.allow(name, DnsKind::Resolved, now) {
                self.write(log::Record::Resolved {
                    t: log::now_rfc3339(),
                    app: self.app.to_string(),
                    n: self.n,
                    name: name.to_string(),
                    declared,
                    addrs: Vec::new(),
                    ttl: 0,
                    count,
                });
            }
            return;
        }
        let ttl = pin_ttl(&records);
        let addrs: Vec<Ipv4Addr> = records.iter().map(|(a, _)| *a).collect();
        for addr in &addrs {
            self.names.remember(*addr, name, now);
        }
        // Only DECLARED names are pinned, in both modes: `audit` accepts
        // anyway, and pinning an undeclared name would have it show up as
        // allowed the moment the operator flips to enforce. And only
        // GLOBAL answers: see `pinnable`.
        if declared {
            let pins: Vec<Ipv4Addr> = addrs
                .iter()
                .copied()
                .filter(|a| pinnable(*a, self.policy))
                .collect();
            if !pins.is_empty() {
                self.pin(&pins, ttl);
            }
        }
        if let Some(count) = self.dns.allow(name, DnsKind::Resolved, now) {
            self.write(log::Record::Resolved {
                t: log::now_rfc3339(),
                app: self.app.to_string(),
                n: self.n,
                name: name.to_string(),
                declared,
                addrs,
                ttl,
                count,
            });
        }
    }

    /// Put `addrs` into `allow_dns` with a FRESH timeout.
    ///
    /// `add element … timeout` does not refresh an existing element's
    /// expiry before kernel 6.10 / nft 1.1, so a long-lived instance that
    /// keeps resolving a declared name would watch its own pin decay and
    /// then be blocked for a name it declared. Reading the set first and
    /// issuing delete+add as ONE batch refreshes it on every kernel, and
    /// the address is never absent from the set in between.
    ///
    /// Both fallbacks land on the plain `add`, which is what this did
    /// before: a listing we could not read, and a batch the kernel refused
    /// (an element that expired between the read and the write would fail
    /// its `delete`, and one transaction fails whole).
    fn pin(&mut self, addrs: &[Ipv4Addr], ttl: u32) {
        let plain = nft::pin_script(self.app, addrs, ttl);
        let script = match list_addr_set(self.app, "allow_dns") {
            Ok(present) => nft::repin_script(self.app, &present, addrs, ttl),
            Err(e) => {
                if !self.repin_warned {
                    self.repin_warned = true;
                    eprintln!("ply: warning: egress: cannot read the pinned set ({e}) — pins are added without refreshing their timeout");
                }
                plain.clone()
            }
        };
        let outcome = match run_nft(&script) {
            Ok(()) => Ok(()),
            Err(e) if script != plain => {
                // The batch was refused as a whole (a racing expiry, most
                // likely). Losing the pin would block a declared name, so
                // fall back to what always worked.
                run_nft(&plain).map_err(|_| e)
            }
            Err(e) => Err(e),
        };
        if let Err(e) = outcome {
            if !self.pin_warned {
                self.pin_warned = true;
                eprintln!("ply: warning: {e}");
            }
        }
    }

    /// One DNS-over-TCP client: one frame in, one frame out, then closed.
    ///
    /// The whole read has ONE deadline, not one per `read`: a client that
    /// dribbles a byte every 400 ms would otherwise hold this thread — and
    /// the instance's teardown behind it — for as long as it liked.
    fn answer_tcp(&mut self, mut stream: TcpStream) {
        let deadline = Instant::now() + TCP_TIMEOUT;
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_write_timeout(Some(TCP_TIMEOUT));
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 2048];
        let query = loop {
            if let Some((msg, _)) = dns::tcp_unframe(&buf) {
                break Some(msg.to_vec());
            }
            if buf.len() > MAX_MSG + 2 {
                break None;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break None;
            }
            // A zero timeout means "block for ever" to the kernel, so the
            // last sliver of the deadline is rounded up to a millisecond.
            if stream
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .is_err()
            {
                break None;
            }
            match stream.read(&mut chunk) {
                Ok(0) => break None,
                Ok(k) => buf.extend_from_slice(&chunk[..k]),
                Err(_) => break None,
            }
        };
        if let Some(query) = query {
            let reply = self.answer(&query);
            let _ = stream.write_all(&dns::tcp_frame(&reply));
        }
    }

    /// Read the two audit sets back out of the kernel and turn what MOVED
    /// since the last poll into records and events.
    fn poll(&mut self, now: Instant) {
        self.names.sweep(now);
        self.tracker.sweep(now);
        self.events.sweep(now);
        self.dns.sweep(now);

        // The two sets count disjoint packets — the allow rules update
        // `@allowed`, the fall-through updates `@blocked` — so a burst
        // landing between the two reads is simply reported by the next
        // poll, and no read order can turn a drop into an accept.
        let sets = list_set(self.app, "allowed")
            .and_then(|allowed| list_set(self.app, "blocked").map(|blocked| (allowed, blocked)));
        let (allowed, blocked) = match sets {
            Ok(sets) => {
                if self.poll_warned {
                    self.poll_warned = false;
                    eprintln!("ply: egress: audit sets readable again");
                }
                sets
            }
            Err(e) => {
                // Enforcement is in the kernel and unaffected; only the
                // observation stops, and silence about it would be worse.
                if !self.poll_warned {
                    self.poll_warned = true;
                    eprintln!("ply: warning: egress: cannot read the audit sets ({e}) — connection records paused");
                }
                return;
            }
        };

        for record in self.tracker.poll(&allowed, &blocked, now) {
            let (dst, port, proto) = record.key.clone();
            let t = log::now_rfc3339();
            let app = self.app.to_string();
            let n = self.n;
            let name = self.names.name_of(dst);
            match record.verdict {
                Verdict::Blocked => {
                    self.write(log::Record::Blocked {
                        t,
                        app,
                        n,
                        proto: proto.clone(),
                        dst,
                        port,
                        name,
                        count: record.count,
                    });
                    // Only alongside a record, so the journal cannot fill
                    // with events for a destination that has not moved.
                    if self.events.allow(&record.key, now) {
                        // In audit nothing was actually blocked — the same
                        // rule counted what WOULD have been.
                        let event = if self.policy.mode == Mode::Enforce {
                            "egress-blocked"
                        } else {
                            "egress-undeclared"
                        };
                        events::emit(self.app, event, &format!("{proto} {dst}:{port}"));
                    }
                }
                Verdict::Allowed => self.write(log::Record::Allowed {
                    t,
                    app,
                    n,
                    proto,
                    dst,
                    port,
                    name,
                    count: record.count,
                }),
            }
        }
    }
}

/// The handle is gone, or asked us to stop.
fn stopped(stop_rx: &mpsc::Receiver<()>) -> bool {
    !matches!(stop_rx.try_recv(), Err(mpsc::TryRecvError::Empty))
}

/// The loop: answer DNS, drain TCP, poll the counters, until the handle
/// drops. Runs inside the instance's netns, on its own thread, forever —
/// there is no other way out.
fn serve(
    app: &str,
    n: u32,
    policy: &Policy,
    upstreams: &[Ipv4Addr],
    socks: Sockets,
    stop_rx: mpsc::Receiver<()>,
) {
    let Sockets {
        udp,
        tcp,
        upstream: upstream_sock,
    } = socks;
    let writer = match log::Writer::open(app, n) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("ply: warning: egress log for {app}.{n}: {e}");
            None
        }
    };
    let mut state = Serve {
        app,
        n,
        policy,
        upstreams: upstreams.iter().map(|a| (*a, 53)).collect(),
        upstream_sock,
        writer,
        names: NameMap::default(),
        tracker: Tracker::default(),
        events: Throttle::new(EVENT_EVERY),
        dns: DnsDamp::new(DNS_RECORD_EVERY),
        pin_warned: false,
        repin_warned: false,
        poll_warned: false,
    };
    let mut buf = vec![0u8; MAX_MSG];
    let mut next_poll = Instant::now() + POLL;
    loop {
        if stopped(&stop_rx) {
            return;
        }
        // Blocks at most WAKE, which is what paces the whole loop when the
        // instance is quiet.
        if let Ok((len, from)) = udp.recv_from(&mut buf) {
            let reply = state.answer(&buf[..len]);
            let _ = udp.send_to(&reply, from);
        }
        for _ in 0..MAX_ACCEPTS {
            if stopped(&stop_rx) {
                return;
            }
            match tcp.accept() {
                Ok((stream, _)) => state.answer_tcp(stream),
                Err(_) => break,
            }
        }
        let now = Instant::now();
        if now >= next_poll {
            state.poll(now);
            next_poll = now + POLL;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_throttle_fires_once_per_destination_per_hour() {
        let mut t = Throttle::new(std::time::Duration::from_secs(3600));
        let key = (
            "203.0.113.9".parse::<std::net::Ipv4Addr>().unwrap(),
            8443u16,
            "tcp".to_string(),
        );
        let now = std::time::Instant::now();
        assert!(t.allow(&key, now));
        assert!(!t.allow(&key, now + std::time::Duration::from_secs(10)));
        assert!(t.allow(&key, now + std::time::Duration::from_secs(3601)));
    }

    #[test]
    fn pin_ttl_has_a_floor_of_five_minutes() {
        assert_eq!(pin_ttl(&[(std::net::Ipv4Addr::new(1, 1, 1, 1), 60)]), 300);
        assert_eq!(
            pin_ttl(&[
                (std::net::Ipv4Addr::new(1, 1, 1, 1), 3600),
                (std::net::Ipv4Addr::new(1, 1, 1, 2), 900)
            ]),
            900
        );
    }

    fn element(addr: Ipv4Addr, port: u16, packets: u64) -> crate::egress::nft::SetElement {
        crate::egress::nft::SetElement {
            addr,
            proto: "tcp".into(),
            port,
            packets,
        }
    }

    const D: Ipv4Addr = Ipv4Addr::new(9, 9, 9, 9);

    fn key(addr: Ipv4Addr, port: u16) -> Key {
        (addr, port, "tcp".to_string())
    }

    fn blocked(addr: Ipv4Addr, port: u16, count: u64) -> PollRecord {
        PollRecord {
            key: key(addr, port),
            verdict: Verdict::Blocked,
            count,
        }
    }

    fn allowed(addr: Ipv4Addr, port: u16, count: u64) -> PollRecord {
        PollRecord {
            key: key(addr, port),
            verdict: Verdict::Allowed,
            count,
        }
    }

    /// The poll's whole decision, as transitions of the two snapshots.
    ///
    /// Each verdict has its OWN counter in the kernel: `@allowed` is updated
    /// by the allow rules, `@blocked` by the fall-through, and a packet is
    /// counted in exactly one of them. So the rule is as small as it gets —
    /// `@blocked` grew → a blocked line, `@allowed` grew → an allowed line,
    /// neither → silence. Presence proves nothing: elements live 24 h in
    /// the kernel, long after the traffic that made them.
    #[test]
    fn each_verdict_is_reported_from_its_own_counter() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        // Dropped: it appears in @blocked ONLY — the allow rules never ran.
        assert_eq!(
            t.poll(&[], &[element(D, 443, 1)], t0),
            vec![blocked(D, 443, 1)],
            "first sight of a dropped packet: blocked, and nothing else"
        );
        // Nothing moved: the element is still there, and says nothing.
        assert_eq!(
            t.poll(&[], &[element(D, 443, 1)], t0 + POLL),
            vec![],
            "a frozen counter is not news — and above all not `allowed`"
        );
        // Hours later, still frozen: still nothing.
        assert_eq!(
            t.poll(&[], &[element(D, 443, 1)], t0 + Duration::from_secs(7200)),
            vec![]
        );
        // The pin lands: @allowed appears and grows, @blocked does not.
        assert_eq!(
            t.poll(
                &[element(D, 443, 4)],
                &[element(D, 443, 1)],
                t0 + Duration::from_secs(7300)
            ),
            vec![allowed(D, 443, 4)],
            "traffic now passing: allowed, counted from its own set"
        );
        // Both move — a second destination behind the same name, say, or a
        // policy narrowed mid-run: both lines, blocked first.
        assert_eq!(
            t.poll(
                &[element(D, 443, 7)],
                &[element(D, 443, 2)],
                t0 + Duration::from_secs(7400)
            ),
            vec![blocked(D, 443, 2), allowed(D, 443, 7)],
            "mixed growth says both, blocked first"
        );
        // The policy tightened: only @blocked climbs now.
        assert_eq!(
            t.poll(
                &[element(D, 443, 7)],
                &[element(D, 443, 5)],
                t0 + Duration::from_secs(7500)
            ),
            vec![blocked(D, 443, 5)],
            "only the blocked counter grew: no allowed line"
        );
    }

    /// The other direction: allowed first, then the policy (or the pin's
    /// expiry) turns the same destination into a drop.
    #[test]
    fn a_destination_that_passes_and_then_stops_reports_both_in_turn() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert_eq!(
            t.poll(&[element(D, 443, 3)], &[], t0),
            vec![allowed(D, 443, 3)]
        );
        assert_eq!(
            t.poll(
                &[element(D, 443, 3)],
                &[element(D, 443, 2)],
                t0 + Duration::from_secs(61)
            ),
            vec![blocked(D, 443, 2)],
            "the pin expired: the allowed counter froze and blocked started"
        );
    }

    /// A destination that only ever appears in `allowed` is the ordinary
    /// case.
    #[test]
    fn connection_records_are_emitted_on_first_sight_and_on_growth_at_most_once_a_minute() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert_eq!(
            t.poll(&[element(D, 80, 1)], &[], t0),
            vec![allowed(D, 80, 1)],
            "first sight"
        );
        assert_eq!(
            t.poll(&[element(D, 80, 1)], &[], t0 + Duration::from_secs(5)),
            vec![],
            "no growth"
        );
        assert_eq!(
            t.poll(&[element(D, 80, 3)], &[], t0 + Duration::from_secs(30)),
            vec![],
            "growth, but inside the minute"
        );
        assert_eq!(
            t.poll(&[element(D, 80, 4)], &[], t0 + Duration::from_secs(61)),
            vec![allowed(D, 80, 4)],
            "growth, minute passed — counted from the kernel, not from the last line"
        );
        // The damping is per destination, not global.
        assert_eq!(
            t.poll(
                &[element(D, 80, 4), element(Ipv4Addr::new(1, 2, 3, 4), 80, 2)],
                &[],
                t0 + Duration::from_secs(62)
            ),
            vec![allowed(Ipv4Addr::new(1, 2, 3, 4), 80, 2)]
        );
    }

    /// The two sets are read one after the other, so a burst can land
    /// between the reads. With a counter per verdict that is simply news
    /// the NEXT poll carries — it can never be read as traffic that got
    /// through, which is what the old seen-minus-blocked subtraction did.
    #[test]
    fn read_skew_between_the_two_sets_never_becomes_an_allowed_line() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        // The destination is in @allowed at zero (the element exists — the
        // rule matched once with a different port, say) while @blocked
        // climbs. `allowed` never moves, so no allowed line, ever.
        assert_eq!(
            t.poll(&[element(D, 443, 0)], &[element(D, 443, 3)], t0),
            vec![blocked(D, 443, 3)],
            "an allowed counter sitting at zero is not traffic"
        );
        assert_eq!(
            t.poll(
                &[element(D, 443, 0)],
                &[element(D, 443, 6)],
                t0 + Duration::from_secs(61)
            ),
            vec![blocked(D, 443, 6)],
            "still blocked only"
        );
        assert_eq!(
            t.poll(
                &[element(D, 443, 0)],
                &[element(D, 443, 6)],
                t0 + Duration::from_secs(122)
            ),
            vec![],
            "and a quiet pair says nothing at all"
        );
    }

    /// An element the kernel flushed and recreated restarts from a lower
    /// count. Everything it holds is then NEW traffic — reporting it as a
    /// huge delta would be a lie, and reporting nothing would hide the
    /// packets that recreated the element.
    #[test]
    fn a_counter_that_went_backwards_counts_from_what_it_holds_now() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert_eq!(
            t.poll(&[element(D, 80, 900)], &[], t0),
            vec![allowed(D, 80, 900)]
        );
        assert_eq!(
            t.poll(&[element(D, 80, 3)], &[], t0 + Duration::from_secs(61)),
            vec![allowed(D, 80, 3)],
            "a reset element's whole count is new traffic, not a 900-packet jump"
        );
        assert_eq!(
            t.poll(&[element(D, 80, 9)], &[], t0 + Duration::from_secs(122)),
            vec![allowed(D, 80, 9)],
            "and it counts up from there"
        );
        assert_eq!(growth(900, 3), 3);
        assert_eq!(growth(3, 9), 6);
        assert_eq!(growth(0, 7), 7, "first sight is the whole count");
    }

    #[test]
    fn tracked_destinations_are_forgotten_after_a_day_idle() {
        let mut t = Tracker::default();
        let mut events = Throttle::new(EVENT_EVERY);
        let t0 = Instant::now();
        let k = key(Ipv4Addr::new(1, 2, 3, 4), 80);
        assert_eq!(
            t.poll(&[element(Ipv4Addr::new(1, 2, 3, 4), 80, 7)], &[], t0)
                .len(),
            1
        );
        assert!(events.allow(&k, t0));
        // A day less a second: both still remembered, so an unchanged
        // counter is still not news and the event stays throttled.
        t.sweep(t0 + TRACK_TTL - Duration::from_secs(1));
        events.sweep(t0 + TRACK_TTL - Duration::from_secs(1));
        assert_eq!(t.keys.len(), 1);
        assert_eq!(events.last.len(), 1);
        // Past the day both are forgotten, and the next sighting is a first
        // one again — the kernel's own elements are gone by then too.
        t.sweep(t0 + TRACK_TTL);
        events.sweep(t0 + TRACK_TTL);
        assert!(t.keys.is_empty());
        assert!(events.last.is_empty());
        assert_eq!(
            t.poll(
                &[element(Ipv4Addr::new(1, 2, 3, 4), 80, 7)],
                &[],
                t0 + TRACK_TTL
            ),
            vec![allowed(Ipv4Addr::new(1, 2, 3, 4), 80, 7)]
        );
    }

    #[test]
    fn the_tracker_stops_taking_new_destinations_at_the_cap() {
        let mut t = Tracker::default();
        let now = Instant::now();
        let full: Vec<_> = (0..MAX_TRACKED)
            .map(|i| element(Ipv4Addr::from(i as u32), 443, 1))
            .collect();
        assert_eq!(t.poll(&full, &[], now).len(), MAX_TRACKED);
        assert_eq!(t.keys.len(), MAX_TRACKED);
        // One past the cap: no state for it, so NOTHING is said about it —
        // an `allowed` line here would be a guess, and a blocked one a lie.
        let over = element(Ipv4Addr::new(203, 0, 113, 7), 9, 4);
        assert_eq!(
            t.poll(
                std::slice::from_ref(&over),
                std::slice::from_ref(&over),
                now + Duration::from_secs(61)
            ),
            vec![]
        );
        assert_eq!(t.keys.len(), MAX_TRACKED);
        // Destinations already tracked keep being reported.
        assert_eq!(
            t.poll(
                &[element(Ipv4Addr::from(0u32), 443, 99)],
                &[],
                now + Duration::from_secs(62)
            ),
            vec![allowed(Ipv4Addr::from(0u32), 443, 99)]
        );
    }

    #[test]
    fn the_event_throttle_stops_taking_new_destinations_at_the_cap() {
        let mut t = Throttle::new(EVENT_EVERY);
        let now = Instant::now();
        for i in 0..MAX_TRACKED {
            assert!(t.allow(&(Ipv4Addr::from(i as u32), 443, "tcp".into()), now));
        }
        assert!(!t.allow(&(Ipv4Addr::new(203, 0, 113, 7), 443, "tcp".into()), now));
        assert_eq!(t.last.len(), MAX_TRACKED);
        // and a known destination still fires once its hour is up
        assert!(t.allow(
            &(Ipv4Addr::from(0u32), 443, "tcp".into()),
            now + EVENT_EVERY
        ));
    }

    #[test]
    fn the_name_map_stops_taking_new_addresses_at_the_cap() {
        let mut names = NameMap::default();
        let now = Instant::now();
        for i in 0..MAX_TRACKED {
            names.remember(Ipv4Addr::from(i as u32), "api.example", now);
        }
        assert_eq!(
            names.name_of(Ipv4Addr::from(0u32)).as_deref(),
            Some("api.example")
        );
        names.remember(Ipv4Addr::new(203, 0, 113, 7), "late.example", now);
        assert_eq!(names.name_of(Ipv4Addr::new(203, 0, 113, 7)), None);
        assert_eq!(names.last.len(), MAX_TRACKED);
        // a known address can still be refreshed, and ages out on its own
        names.remember(Ipv4Addr::from(0u32), "other.example", now);
        assert_eq!(
            names.name_of(Ipv4Addr::from(0u32)).as_deref(),
            Some("other.example")
        );
        names.sweep(now + NAME_TTL);
        assert!(names.last.is_empty());
    }

    /// The DNS records are damped exactly like the connection records, and
    /// carry what the damping swallowed.
    #[test]
    fn dns_records_are_written_once_a_minute_per_name_and_kind_with_their_count() {
        let mut d = DnsDamp::new(Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(d.allow("evil.example", DnsKind::Refused, t0), Some(1));
        // …400 more queries inside the minute, all silent
        for i in 1..400 {
            assert_eq!(
                d.allow(
                    "evil.example",
                    DnsKind::Refused,
                    t0 + Duration::from_millis(i)
                ),
                None
            );
        }
        assert_eq!(
            d.allow(
                "evil.example",
                DnsKind::Refused,
                t0 + Duration::from_secs(61)
            ),
            Some(400),
            "one line, standing for every query since the last one"
        );
        assert_eq!(
            d.allow(
                "evil.example",
                DnsKind::Refused,
                t0 + Duration::from_secs(62)
            ),
            None,
            "and the counter restarts from that line"
        );
        // Per name AND per kind: a second name, and the same name resolved
        // rather than refused, are their own first sightings.
        assert_eq!(
            d.allow(
                "other.example",
                DnsKind::Refused,
                t0 + Duration::from_secs(62)
            ),
            Some(1)
        );
        assert_eq!(
            d.allow(
                "evil.example",
                DnsKind::Resolved,
                t0 + Duration::from_secs(62)
            ),
            Some(1)
        );
    }

    #[test]
    fn the_dns_damping_stops_taking_new_names_at_the_cap_and_ages_out() {
        let mut d = DnsDamp::new(DNS_RECORD_EVERY);
        let now = Instant::now();
        for i in 0..MAX_TRACKED {
            assert_eq!(
                d.allow(&format!("n{i}.example"), DnsKind::Refused, now),
                Some(1)
            );
        }
        assert_eq!(d.allow("late.example", DnsKind::Refused, now), None);
        assert_eq!(d.last.len(), MAX_TRACKED);
        // a known name still gets its line once the minute is up
        assert_eq!(
            d.allow("n0.example", DnsKind::Refused, now + DNS_RECORD_EVERY),
            Some(1)
        );
        d.sweep(now + TRACK_TTL + DNS_RECORD_EVERY);
        assert!(d.last.is_empty());
    }

    /// A declared name that resolves somewhere private is a rebinding
    /// lever, not a destination: log it, never pin it.
    #[test]
    fn only_global_answers_are_pinned() {
        let open = policy(Mode::Enforce, &["*.nip.io"]);
        for bad in [
            "127.0.0.1",       // loopback
            "169.254.169.254", // link-local: the cloud metadata service
            "10.1.2.3",        // RFC 1918
            "172.16.9.9",
            "192.168.1.1",
            "10.77.0.5",  // a neighbour instance on the ply bridge
            "100.64.0.1", // CGNAT
            "224.0.0.1",  // multicast
            "255.255.255.255",
            "0.0.0.0",
            "0.1.2.3", // 0.0.0.0/8
        ] {
            assert!(
                !pinnable(bad.parse().unwrap(), &open),
                "{bad} must not be pinned"
            );
        }
        for good in ["93.184.216.34", "8.8.8.8", "54.187.174.169", "9.9.9.9"] {
            assert!(pinnable(good.parse().unwrap(), &open), "{good}");
        }
        // The operator's own word overrides it: an address (or a range
        // holding it) written into the list has been said out loud.
        let declared = policy(Mode::Enforce, &["169.254.169.254", "192.168.0.0/16"]);
        assert!(pinnable("169.254.169.254".parse().unwrap(), &declared));
        assert!(pinnable("192.168.7.7".parse().unwrap(), &declared));
        assert!(!pinnable("10.0.0.1".parse().unwrap(), &declared));
        // …and `*` declares everything, including the private ranges.
        assert!(pinnable(
            "10.0.0.1".parse().unwrap(),
            &policy(Mode::Enforce, &["*"])
        ));
    }

    fn policy(mode: Mode, raw: &[&str]) -> Policy {
        let list: Vec<crate::egress::EgressEntry> =
            raw.iter().map(|s| s.parse().unwrap()).collect();
        crate::egress::effective(
            Some(&list),
            Some(&crate::egress::EgressOverride {
                mode: Some(mode),
                allow: None,
            }),
        )
    }

    fn query(name: &str, qdcount: u16) -> Vec<u8> {
        let mut m = vec![0x12, 0x34, 0x01, 0x00];
        m.extend_from_slice(&qdcount.to_be_bytes());
        m.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        for label in name.split('.').filter(|l| !l.is_empty()) {
            m.push(label.len() as u8);
            m.extend_from_slice(label.as_bytes());
        }
        m.push(0);
        m.extend_from_slice(&[0, 1, 0, 1]); // A IN
        m
    }

    #[test]
    fn enforce_refuses_what_it_does_not_declare_and_audit_forwards_it_undeclared() {
        let allowed = query("api.stripe.com", 1);
        let other = query("evil.example", 1);
        assert!(matches!(
            decide(&policy(Mode::Enforce, &["api.stripe.com"]), &allowed),
            Decision::Forward { declared: true, .. }
        ));
        assert!(matches!(
            decide(&policy(Mode::Enforce, &["api.stripe.com"]), &other),
            Decision::Refuse {
                declared: false,
                ..
            }
        ));
        assert!(matches!(
            decide(&policy(Mode::Audit, &["api.stripe.com"]), &other),
            Decision::Forward {
                declared: false,
                ..
            }
        ));
        // `*` declares everything, including the root name a malformed or
        // empty question leaves behind.
        assert!(matches!(
            decide(&policy(Mode::Enforce, &["*"]), &query("", 1)),
            Decision::Forward { declared: true, .. }
        ));
        assert!(matches!(
            decide(&policy(Mode::Enforce, &["api.stripe.com"]), &query("", 1)),
            Decision::Refuse {
                declared: false,
                ..
            }
        ));
    }

    #[test]
    fn a_multi_question_query_is_refused_by_either_mode_with_the_first_name() {
        for mode in [Mode::Audit, Mode::Enforce] {
            // QDCOUNT says two; the vetted name is only the first.
            let msg = query("api.stripe.com", 2);
            match decide(&policy(mode, &["api.stripe.com", "*"]), &msg) {
                Decision::Refuse { name, declared } => {
                    assert_eq!(name, "api.stripe.com");
                    assert!(!declared);
                }
                Decision::Forward { .. } => panic!("{mode} forwarded a multi-question query"),
            }
            // Unreadable, and QDCOUNT cannot even be trusted: still refused,
            // and the log gets a name it can print.
            match decide(&policy(mode, &["*"]), &[0u8; 4]) {
                Decision::Refuse { name, .. } => assert_eq!(name, "?"),
                Decision::Forward { .. } => panic!("{mode} forwarded an unreadable query"),
            }
        }
    }
}
