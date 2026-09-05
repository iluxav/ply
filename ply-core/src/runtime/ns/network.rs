//! Mode B networking: netns per instance, veth pair onto a ply0 bridge,
//! 10.77.0.0/16. Each instance really binds its declared port on its own IP.
//!
//! v1 shells out to iproute2 (`ip`, `nsenter`) — present on every server
//! Linux; native netlink is a later swap that changes nothing user-visible.

use std::net::Ipv4Addr;
use std::process::Command;

use crate::error::{Error, Result};
// The gateway address is a portable fact `publish::bind_addr` also needs
// (`internal` binds/connects here, rootful) — defined once there so this
// module and that one can never drift apart.
use crate::runtime::publish::GATEWAY;

pub const BRIDGE: &str = "ply0";
pub const SUBNET: &str = "10.77.0.0/16";

fn sh(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| Error::Runtime(format!("{program} not found: {e} (iproute2 required)")))?;
    if !output.status.success() {
        return Err(Error::Runtime(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Cross-process lock serializing IP allocation + veth setup. Held by the
/// returned file; drops (and unlocks) when it goes out of scope.
pub fn lock() -> Result<std::fs::File> {
    let dir = crate::paths::run_dir();
    std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join("network.lock");
    let file = std::fs::File::create(&path).map_err(|source| Error::Io { path, source })?;
    let rc =
        unsafe { nix::libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), nix::libc::LOCK_EX) };
    if rc != 0 {
        return Err(Error::Runtime("network lock: flock failed".into()));
    }
    Ok(file)
}

/// Create ply0 bridge with the gateway address. Idempotent.
pub fn ensure_bridge() -> Result<()> {
    if !std::path::Path::new("/sys/class/net").join(BRIDGE).exists() {
        sh("ip", &["link", "add", BRIDGE, "type", "bridge"])?;
    }
    // addr add fails with EEXIST if already there — tolerate
    let _ = Command::new("ip")
        .args(["addr", "add", &format!("{GATEWAY}/16"), "dev", BRIDGE])
        .output();
    sh("ip", &["link", "set", BRIDGE, "up"])?;
    ensure_egress();
    Ok(())
}

/// nft program that source-NATs the bridge subnet on its way out. Applied
/// once; `nft list table ip ply` succeeding means it is in place.
pub fn nft_egress_script() -> String {
    format!(
        "table ip ply {{\n  chain postrouting {{\n    type nat hook postrouting priority srcnat; policy accept;\n    ip saddr {SUBNET} oifname != \"{BRIDGE}\" masquerade\n  }}\n}}\n"
    )
}

/// The same rule for hosts that only have iptables (checked with -C, added with -A).
pub fn iptables_egress_rule() -> [&'static str; 8] {
    [
        "POSTROUTING",
        "-s",
        SUBNET,
        "!",
        "-o",
        BRIDGE,
        "-j",
        "MASQUERADE",
    ]
}

/// FORWARD accepts for the bridge, in iptables terms. Docker and ufw both
/// set that chain's policy to DROP, and the masquerade alone then leaves
/// the bridge with no way out. It has to be iptables: a packet accepted in
/// one nft base chain is still dropped by the next, so a chain of ours
/// could not override the policy — and only hosts with iptables have it.
/// Same two rules Docker installs for docker0.
pub fn iptables_forward_rules() -> [Vec<&'static str>; 2] {
    [
        vec!["FORWARD", "-i", BRIDGE, "-j", "ACCEPT"],
        vec![
            "FORWARD",
            "-o",
            BRIDGE,
            "-m",
            "conntrack",
            "--ctstate",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ],
    ]
}

/// `iptables -C <chain> <rule>`: is it already there?
pub fn iptables_check_args<'a>(rule: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["-C"];
    v.extend_from_slice(rule);
    v
}

/// `iptables -I <chain> 1 <rule>`: at the top, above the ufw/Docker jumps.
pub fn iptables_insert_args<'a>(rule: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec!["-I", rule[0], "1"];
    v.extend_from_slice(&rule[1..]);
    v
}

/// Idempotent: each rule is checked before it is inserted. A host without
/// iptables has no Docker or ufw either, so there is nothing to open.
fn ensure_forward_accept() {
    if !has("iptables") {
        return;
    }
    for rule in iptables_forward_rules() {
        if succeeds("iptables", &iptables_check_args(&rule)) {
            continue;
        }
        if let Err(e) = sh("iptables", &iptables_insert_args(&rule)) {
            eprintln!("ply: warning: iptables FORWARD accept for {BRIDGE} failed: {e}");
        }
    }
}

pub fn forwarding_needs_enable(current: &str) -> bool {
    current.trim() != "1"
}

fn has(program: &str) -> bool {
    Command::new(program)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Is `nft` on this host? The egress thread needs it to install a table;
/// without it there is no enforcement and nothing to observe.
pub fn has_nft() -> bool {
    has("nft")
}

fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Instances sit behind the bridge in their own netns; without IPv4
/// forwarding and source NAT on the host they can reach each other and
/// nothing else. Idempotent; warns instead of failing so a host without
/// nft/iptables still runs apps that need no egress.
pub fn ensure_egress() {
    const FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
    if forwarding_needs_enable(&std::fs::read_to_string(FORWARD).unwrap_or_default()) {
        if let Err(e) = std::fs::write(FORWARD, "1") {
            eprintln!(
                "ply: warning: cannot enable {FORWARD} ({e}) — instances have no internet egress"
            );
        }
    }
    ensure_forward_accept();
    if has("nft") {
        if succeeds("nft", &["list", "table", "ip", "ply"]) {
            return;
        }
        let script = nft_egress_script();
        let run = Command::new("nft")
            .args(["-f", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(script.as_bytes())?;
                }
                child.wait_with_output()
            });
        match run {
            Ok(o) if o.status.success() => return,
            Ok(o) => eprintln!(
                "ply: warning: nft NAT setup failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => eprintln!("ply: warning: nft NAT setup failed: {e}"),
        }
    }
    if has("iptables") {
        let rule = iptables_egress_rule();
        let mut check = vec!["-t", "nat", "-C"];
        check.extend(rule);
        if succeeds("iptables", &check) {
            return;
        }
        let mut add = vec!["-t", "nat", "-A"];
        add.extend(rule);
        if let Err(e) = sh("iptables", &add) {
            eprintln!("ply: warning: iptables NAT setup failed: {e}");
        }
        return;
    }
    eprintln!(
        "ply: warning: neither nft nor iptables found — instances have no internet egress (install nftables)"
    );
}

/// Lowest free 10.77.x.y outside {.0.0, .0.1}.
pub fn allocate_ip(used: &[Ipv4Addr]) -> Result<Ipv4Addr> {
    for third in 0..=255u8 {
        for fourth in 0..=255u8 {
            if third == 0 && fourth < 2 {
                continue;
            }
            let candidate = Ipv4Addr::new(10, 77, third, fourth);
            if !used.contains(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(Error::Runtime("10.77.0.0/16 exhausted".into()))
}

/// Wire the instance: veth pair, host end on the bridge, container end as
/// eth0 with `ip`/16 + default route via the gateway. The veth pair dies
/// with the netns — no teardown needed.
pub fn setup_instance(pid: i32, ip: Ipv4Addr) -> Result<()> {
    let octets = ip.octets();
    let host_if = format!("ply{:02x}{:02x}", octets[2], octets[3]);
    let cont_if = format!("{host_if}c");
    let pid_s = pid.to_string();

    // A crashed run can orphan a host-side veth (its peer never reached a
    // netns that died). The IP was allocated fresh under the lock, so any
    // interface with our derived name is stale by definition — remove it.
    if std::path::Path::new("/sys/class/net")
        .join(&host_if)
        .exists()
    {
        let _ = Command::new("ip").args(["link", "del", &host_if]).output();
    }
    sh(
        "ip",
        &[
            "link", "add", &host_if, "type", "veth", "peer", "name", &cont_if,
        ],
    )?;
    sh("ip", &["link", "set", &host_if, "master", BRIDGE, "up"])?;
    sh("ip", &["link", "set", &cont_if, "netns", &pid_s])?;

    let ns = |args: &[&str]| -> Result<()> {
        let mut full = vec!["-t", &pid_s, "-n", "ip"];
        full.extend_from_slice(args);
        sh("nsenter", &full)
    };
    ns(&["link", "set", "lo", "up"])?;
    ns(&["link", "set", &cont_if, "name", "eth0"])?;
    ns(&["addr", "add", &format!("{ip}/16"), "dev", "eth0"])?;
    ns(&["link", "set", "eth0", "up"])?;
    ns(&["route", "add", "default", "via", &GATEWAY.to_string()])?;

    // A respawn reuses a freed IP behind a NEW veth MAC. The host's stale
    // neighbor entry would blackhole traffic (health gates, LBs) until ARP
    // expires — drop it now.
    let _ = Command::new("ip")
        .args(["neigh", "del", &ip.to_string(), "dev", BRIDGE])
        .output();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nft_script_masquerades_the_bridge_subnet_only_when_leaving_the_bridge() {
        let s = nft_egress_script();
        assert!(s.contains("table ip ply"), "{s}");
        assert!(
            s.contains("type nat hook postrouting priority srcnat"),
            "{s}"
        );
        assert!(
            s.contains(&format!(
                "ip saddr {SUBNET} oifname != \"{BRIDGE}\" masquerade"
            )),
            "{s}"
        );
    }

    #[test]
    fn iptables_rule_matches_the_nft_semantics() {
        assert_eq!(
            iptables_egress_rule(),
            [
                "POSTROUTING",
                "-s",
                SUBNET,
                "!",
                "-o",
                BRIDGE,
                "-j",
                "MASQUERADE"
            ]
        );
    }

    /// Docker and ufw both set the FORWARD policy to DROP; the masquerade
    /// alone then leaves the bridge with no way out (seen 2026-09-05: SYNs
    /// counted as allowed by the egress table, no reply ever). Open it the
    /// way Docker opens docker0: out unconditionally, back only for
    /// conntrack replies.
    #[test]
    fn forward_accepts_open_the_bridge_out_and_its_replies_back() {
        let rules = iptables_forward_rules();
        assert_eq!(rules[0], ["FORWARD", "-i", BRIDGE, "-j", "ACCEPT"]);
        assert_eq!(
            rules[1],
            [
                "FORWARD",
                "-o",
                BRIDGE,
                "-m",
                "conntrack",
                "--ctstate",
                "RELATED,ESTABLISHED",
                "-j",
                "ACCEPT"
            ]
        );
    }

    /// Checked with -C first (idempotent across runs), then inserted at
    /// position 1 so it sits above the ufw/Docker jumps, not below the
    /// chain's DROP policy.
    #[test]
    fn forward_accepts_are_checked_then_inserted_at_the_top() {
        let rule = vec!["FORWARD", "-i", BRIDGE, "-j", "ACCEPT"];
        assert_eq!(
            iptables_check_args(&rule),
            ["-C", "FORWARD", "-i", BRIDGE, "-j", "ACCEPT"]
        );
        assert_eq!(
            iptables_insert_args(&rule),
            ["-I", "FORWARD", "1", "-i", BRIDGE, "-j", "ACCEPT"]
        );
    }

    #[test]
    fn forwarding_is_only_written_when_off() {
        assert!(forwarding_needs_enable("0\n"));
        assert!(forwarding_needs_enable(""));
        assert!(!forwarding_needs_enable("1\n"));
    }
}
