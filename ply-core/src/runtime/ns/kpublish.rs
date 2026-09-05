//! The kernel path for `--publish` on rootful Linux.
//!
//! The run parent still binds the port and still owns the pool; it just
//! stops copying bytes. On every pool change it rewrites one nftables DNAT
//! rule — per published port, its own pair of base chains in the `ip ply`
//! table — so new connections are round-robined by the kernel and the relay
//! threads sit idle. Rootless, the macOS switch, and traffic to loopback
//! keep the relay: the same split Docker makes with docker-proxy.
//!
//! Everything that renders a script is pure and unit-tested here; the three
//! functions that talk to `nft` are thin.

use std::net::Ipv4Addr;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::runtime::ns::network::SUBNET;
use crate::runtime::publish::{BindScope, PoolMirror, GATEWAY};

const TABLE: &str = "ip ply";

/// Whether this port gets a kernel path at all: rootful (netfilter needs
/// CAP_NET_ADMIN), `nft` present, and a scope that is not loopback.
pub fn eligible(rootless: bool, has_nft: bool, scope: BindScope) -> bool {
    !rootless && has_nft && !matches!(scope, BindScope::Addr(a) if a.is_loopback())
}

/// The pool speaks socket addresses; an `ip` table takes IPv4 only.
pub fn ipv4_of(backends: &[std::net::SocketAddr]) -> Vec<Ipv4Addr> {
    backends
        .iter()
        .filter_map(|a| match a {
            std::net::SocketAddr::V4(v4) => Some(*v4.ip()),
            _ => None,
        })
        .collect()
}

/// `KernelPublish` as the pool sees it. A failed `nft` is said once and the
/// relay carries on: the listener never stopped, so nothing is lost but the
/// saving.
pub struct KernelMirror {
    kp: KernelPublish,
    warned: std::sync::atomic::AtomicBool,
}

impl KernelMirror {
    pub fn new(kp: KernelPublish) -> Self {
        KernelMirror {
            kp,
            warned: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl PoolMirror for KernelMirror {
    fn sync(&self, backends: &[std::net::SocketAddr]) {
        if let Err(e) = self.kp.apply(&ipv4_of(backends)) {
            if !self.warned.swap(true, std::sync::atomic::Ordering::SeqCst) {
                eprintln!(
                    "ply: warning: kernel path for port {} unavailable ({e}) — relaying in user space",
                    self.kp.host_port
                );
            }
        }
    }
    fn teardown(&self) {
        let _ = self.kp.teardown();
    }
}

/// One published port's presence in the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelPublish {
    host_port: u16,
    instance_port: u16,
    scope: BindScope,
    /// The owning run parent — in the chain names, so a later parent can tell
    /// a live neighbour's chains from a crashed predecessor's.
    pid: u32,
}

impl KernelPublish {
    pub fn new(host_port: u16, instance_port: u16, scope: BindScope, pid: u32) -> Self {
        KernelPublish {
            host_port,
            instance_port,
            scope,
            pid,
        }
    }

    pub fn chain_pre(&self) -> String {
        format!("pub_{}_p{}_pre", self.host_port, self.pid)
    }
    pub fn chain_out(&self) -> String {
        format!("pub_{}_p{}_out", self.host_port, self.pid)
    }

    /// What this port's rule matches, or `None` when the scope has no kernel
    /// path (a loopback address: DNAT from lo needs `route_localnet`, and the
    /// relay is right there).
    pub fn match_expr(&self) -> Option<String> {
        let p = self.host_port;
        match self.scope {
            BindScope::Public => Some(format!(
                "ip daddr != 127.0.0.0/8 fib daddr type local tcp dport {p}"
            )),
            BindScope::Internal => Some(format!("ip daddr {GATEWAY} tcp dport {p}")),
            BindScope::Addr(a) if a.is_loopback() => None,
            BindScope::Addr(a) => Some(format!("ip daddr {a} tcp dport {p}")),
        }
    }

    /// The whole batch for one pool state. `add` lines are idempotent (an
    /// existing table/chain of the same shape is fine), the flush+add pair
    /// is atomic under `nft -f`, so there is never a moment with two rules
    /// or none where one was.
    pub fn sync_script(&self, backends: &[Ipv4Addr]) -> String {
        let (pre, out) = (self.chain_pre(), self.chain_out());
        let mut s = format!(
            "add table {TABLE}\n\
             add chain {TABLE} {pre} {{ type nat hook prerouting priority dstnat; policy accept; }}\n\
             add chain {TABLE} {out} {{ type nat hook output priority dstnat; policy accept; }}\n\
             flush chain {TABLE} {pre}\n\
             flush chain {TABLE} {out}\n"
        );
        if let (Some(m), Some(d)) = (self.match_expr(), dnat_expr(backends, self.instance_port)) {
            s.push_str(&format!("add rule {TABLE} {pre} {m} {d}\n"));
            s.push_str(&format!("add rule {TABLE} {out} {m} {d}\n"));
        }
        s
    }

    pub fn teardown_script(&self) -> String {
        format!(
            "delete chain {TABLE} {}\ndelete chain {TABLE} {}\n",
            self.chain_pre(),
            self.chain_out()
        )
    }

    /// Put `backends` in the kernel (and the hairpin chain, cheaply, every
    /// time — it is idempotent and the pool changes rarely).
    pub fn apply(&self, backends: &[Ipv4Addr]) -> Result<()> {
        nft_batch(&format!(
            "{}{}",
            hairpin_script(),
            self.sync_script(backends)
        ))
    }

    pub fn teardown(&self) -> Result<()> {
        nft_batch(&self.teardown_script())
    }
}

/// The verdict for a pool: nothing for an empty one (the packet goes on to
/// the parent's listener), a plain rewrite for one backend, a per-connection
/// round-robin over the rest.
pub fn dnat_expr(backends: &[Ipv4Addr], instance_port: u16) -> Option<String> {
    match backends {
        [] => None,
        [one] => Some(format!("dnat to {one}:{instance_port}")),
        many => {
            let elems: Vec<String> = many
                .iter()
                .enumerate()
                .map(|(i, a)| format!("{i} : {a} . {instance_port}"))
                .collect();
            Some(format!(
                "ip protocol tcp dnat ip addr . port to numgen inc mod {} map {{ {} }}",
                many.len(),
                elems.join(", ")
            ))
        }
    }
}

/// Bridge→bridge flows that were DNATed get the host as their visible
/// source, so the reply comes back through conntrack instead of straight to
/// the client. Idempotent (flush + add).
pub fn hairpin_script() -> String {
    format!(
        "add table {TABLE}\n\
         add chain {TABLE} pub_hairpin {{ type nat hook postrouting priority srcnat; policy accept; }}\n\
         flush chain {TABLE} pub_hairpin\n\
         add rule {TABLE} pub_hairpin ip saddr {SUBNET} ip daddr {SUBNET} ct status dnat masquerade\n"
    )
}

/// From `nft -j list table ip ply`: the `pub_<port>_p<pid>_*` chains that a
/// starting parent for `own_port` should delete — a dead owner's, or any
/// for its own port (their owner cannot be serving it: our bind succeeded).
pub fn stale_chains(list_json: &str, own_port: u16, alive: impl Fn(u32) -> bool) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(list_json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in v
        .get("nftables")
        .and_then(|n| n.as_array())
        .into_iter()
        .flatten()
    {
        let Some(name) = item
            .get("chain")
            .and_then(|c| c.get("name"))
            .and_then(|n| n.as_str())
        else {
            continue;
        };
        let Some((port, pid)) = parse_chain_name(name) else {
            continue;
        };
        if port == own_port || !alive(pid) {
            out.push(name.to_string());
        }
    }
    out
}

/// `pub_18080_p4242_pre` → (18080, 4242).
fn parse_chain_name(name: &str) -> Option<(u16, u32)> {
    let rest = name.strip_prefix("pub_")?;
    let mut parts = rest.split('_');
    let port = parts.next()?.parse().ok()?;
    let pid = parts.next()?.strip_prefix('p')?.parse().ok()?;
    matches!(parts.next(), Some("pre") | Some("out")).then_some((port, pid))
}

/// Delete what `stale_chains` names. Best effort: a host without the table
/// yet has nothing to clean.
pub fn gc_stale(own_port: u16) {
    let Ok(out) = Command::new("nft")
        .args(["-j", "list", "table", "ip", "ply"])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let alive = |pid: u32| std::path::Path::new(&format!("/proc/{pid}")).exists();
    let stale = stale_chains(&String::from_utf8_lossy(&out.stdout), own_port, alive);
    if stale.is_empty() {
        return;
    }
    let script: String = stale
        .iter()
        .map(|c| format!("delete chain {TABLE} {c}\n"))
        .collect();
    if let Err(e) = nft_batch(&script) {
        eprintln!("ply: warning: could not remove stale publish chains ({e})");
    }
}

fn nft_batch(script: &str) -> Result<()> {
    let run = Command::new("nft")
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(script.as_bytes())?;
            }
            child.wait_with_output()
        })
        .map_err(|e| Error::Runtime(format!("nft: {e}")))?;
    if run.status.success() {
        Ok(())
    } else {
        Err(Error::Runtime(format!(
            "nft: {}",
            String::from_utf8_lossy(&run.stderr).trim()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::publish::BindScope;
    use std::net::Ipv4Addr;

    fn kp(scope: BindScope) -> KernelPublish {
        KernelPublish::new(18080, 8080, scope, 4242)
    }
    fn a(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(10, 77, 0, last)
    }

    #[test]
    fn chain_names_carry_port_and_owner_pid() {
        let k = kp(BindScope::Public);
        assert_eq!(k.chain_pre(), "pub_18080_p4242_pre");
        assert_eq!(k.chain_out(), "pub_18080_p4242_out");
    }

    /// Public binds the wildcard, so the kernel must only catch traffic
    /// addressed to THIS host (not transit, not an instance's own port), and
    /// never loopback — that stays on the relay, as Docker's does.
    #[test]
    fn match_follows_the_scope() {
        assert_eq!(
            kp(BindScope::Public).match_expr(),
            Some("ip daddr != 127.0.0.0/8 fib daddr type local tcp dport 18080".to_string())
        );
        assert_eq!(
            kp(BindScope::Internal).match_expr(),
            Some("ip daddr 10.77.0.1 tcp dport 18080".to_string())
        );
        assert_eq!(
            kp(BindScope::Addr(Ipv4Addr::new(192, 168, 1, 5))).match_expr(),
            Some("ip daddr 192.168.1.5 tcp dport 18080".to_string())
        );
        assert_eq!(kp(BindScope::Addr(Ipv4Addr::LOCALHOST)).match_expr(), None);
    }

    #[test]
    fn dnat_is_absent_for_no_backends_plain_for_one_and_round_robin_for_many() {
        assert_eq!(dnat_expr(&[], 8080), None);
        assert_eq!(
            dnat_expr(&[a(2)], 8080),
            Some("dnat to 10.77.0.2:8080".to_string())
        );
        assert_eq!(
            dnat_expr(&[a(2), a(3), a(9)], 8080),
            Some(
                "ip protocol tcp dnat ip addr . port to numgen inc mod 3 map { 0 : 10.77.0.2 . 8080, 1 : 10.77.0.3 . 8080, 2 : 10.77.0.9 . 8080 }"
                    .to_string()
            )
        );
    }

    /// One batch: create-if-missing, flush, add — atomic under `nft -f`, so
    /// a pool change never leaves a moment with no rule and no old rule.
    #[test]
    fn sync_script_is_one_atomic_batch_per_chain_pair() {
        let s = kp(BindScope::Public).sync_script(&[a(2), a(3)]);
        let want = "\
add table ip ply
add chain ip ply pub_18080_p4242_pre { type nat hook prerouting priority dstnat; policy accept; }
add chain ip ply pub_18080_p4242_out { type nat hook output priority dstnat; policy accept; }
flush chain ip ply pub_18080_p4242_pre
flush chain ip ply pub_18080_p4242_out
add rule ip ply pub_18080_p4242_pre ip daddr != 127.0.0.0/8 fib daddr type local tcp dport 18080 ip protocol tcp dnat ip addr . port to numgen inc mod 2 map { 0 : 10.77.0.2 . 8080, 1 : 10.77.0.3 . 8080 }
add rule ip ply pub_18080_p4242_out ip daddr != 127.0.0.0/8 fib daddr type local tcp dport 18080 ip protocol tcp dnat ip addr . port to numgen inc mod 2 map { 0 : 10.77.0.2 . 8080, 1 : 10.77.0.3 . 8080 }
";
        assert_eq!(s, want);
    }

    /// An empty pool flushes and adds nothing: traffic falls through to the
    /// parent's listener, which answers exactly as before (EOF).
    #[test]
    fn sync_with_no_backends_leaves_the_chains_empty() {
        let s = kp(BindScope::Internal).sync_script(&[]);
        assert!(
            s.contains("flush chain ip ply pub_18080_p4242_pre\n"),
            "{s}"
        );
        assert!(!s.contains("add rule"), "{s}");
    }

    #[test]
    fn teardown_deletes_both_chains() {
        assert_eq!(
            kp(BindScope::Public).teardown_script(),
            "delete chain ip ply pub_18080_p4242_pre\ndelete chain ip ply pub_18080_p4242_out\n"
        );
    }

    /// A bridge client DNATed back onto the bridge would get its reply
    /// directly from the backend and conntrack would never see it; the
    /// hairpin masquerade makes the host the visible peer — which is also
    /// what the relay showed backends.
    #[test]
    fn hairpin_masquerades_only_dnated_bridge_to_bridge_flows() {
        let s = hairpin_script();
        assert!(s.contains("add chain ip ply pub_hairpin { type nat hook postrouting priority srcnat; policy accept; }"), "{s}");
        assert!(s.contains("flush chain ip ply pub_hairpin\n"), "{s}");
        assert!(
            s.contains("add rule ip ply pub_hairpin ip saddr 10.77.0.0/16 ip daddr 10.77.0.0/16 ct status dnat masquerade\n"),
            "{s}"
        );
    }

    /// GC on start: chains of a dead parent, and any chain for our own port
    /// (a crashed predecessor holding the port we are about to publish).
    #[test]
    fn stale_chains_are_dead_owners_or_our_own_port() {
        let listing = r#"{"nftables":[{"metainfo":{}},{"table":{"family":"ip","name":"ply"}},
            {"chain":{"family":"ip","table":"ply","name":"postrouting"}},
            {"chain":{"family":"ip","table":"ply","name":"pub_hairpin"}},
            {"chain":{"family":"ip","table":"ply","name":"pub_18080_p111_pre"}},
            {"chain":{"family":"ip","table":"ply","name":"pub_18080_p111_out"}},
            {"chain":{"family":"ip","table":"ply","name":"pub_5432_p222_pre"}},
            {"chain":{"family":"ip","table":"ply","name":"pub_5432_p222_out"}},
            {"chain":{"family":"ip","table":"ply","name":"pub_9000_p333_pre"}}]}"#;
        // 111 is dead; 222 alive and another port; 333 alive but holds OUR port? no — 18080 is ours, held by dead 111.
        let alive = |pid: u32| pid != 111;
        let mut got = stale_chains(listing, 18080, alive);
        got.sort();
        assert_eq!(got, vec!["pub_18080_p111_out", "pub_18080_p111_pre"]);
        // Our port held by a LIVE pid is still ours to reclaim: the bind
        // would have failed if that parent were really serving it.
        let mut got = stale_chains(listing, 9000, |_| true);
        got.sort();
        assert_eq!(got, vec!["pub_9000_p333_pre"]);
        assert!(stale_chains("not json", 1, |_| true).is_empty());
    }

    /// Rootful, nft on the host, and a scope the kernel can serve. Rootless
    /// has no netfilter; a loopback address has no kernel path.
    #[test]
    fn eligibility_is_rootful_with_nft_and_a_non_loopback_scope() {
        assert!(eligible(false, true, BindScope::Public));
        assert!(eligible(false, true, BindScope::Internal));
        assert!(eligible(
            false,
            true,
            BindScope::Addr(Ipv4Addr::new(192, 168, 1, 5))
        ));
        assert!(!eligible(false, true, BindScope::Addr(Ipv4Addr::LOCALHOST)));
        assert!(!eligible(true, true, BindScope::Public));
        assert!(!eligible(false, false, BindScope::Public));
    }

    /// The pool hands over socket addresses; only IPv4 ones can be DNAT
    /// targets in an `ip` table (a loopback-forwarded rootless port never
    /// reaches here, but the filter is where the type changes).
    #[test]
    fn the_mirror_keeps_only_ipv4_backends_in_pool_order() {
        let addrs: Vec<std::net::SocketAddr> = vec![
            "10.77.0.3:8080".parse().unwrap(),
            "[::1]:8080".parse().unwrap(),
            "10.77.0.2:8080".parse().unwrap(),
        ];
        assert_eq!(ipv4_of(&addrs), vec![a(3), a(2)]);
    }

    /// For the owner's `sudo nft -c -f target/publish-sample.nft`: the
    /// hairpin chain plus a two-backend public port, exactly as `apply`
    /// would send it.
    #[test]
    #[ignore]
    fn write_publish_sample_script() {
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"));
        std::fs::create_dir_all(&target).unwrap();
        let script = format!(
            "{}{}",
            hairpin_script(),
            kp(BindScope::Public).sync_script(&[a(2), a(3)])
        );
        std::fs::write(target.join("publish-sample.nft"), script).unwrap();
    }
}
