//! The per-instance nftables table: rendered as text for `nft -f -`, pins
//! for `allow_dns`, and the JSON readers for the two audit sets and the pin
//! set. Pure: the Linux thread (`runtime/ns/egress.rs`) is the only thing
//! that runs nft.

use std::net::Ipv4Addr;

use crate::egress::{EgressEntry, Mode, Policy};
use crate::error::{Error, Result};

pub fn table_name(app: &str) -> String {
    let safe: String = app
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("egress_{safe}")
}

fn static_elements(policy: &Policy) -> Vec<String> {
    if policy.unrestricted() {
        return vec!["0.0.0.0/0".into()];
    }
    policy
        .allow
        .iter()
        .filter_map(|e| match e {
            EgressEntry::Addr(a) => Some(a.to_string()),
            EgressEntry::Cidr(a, p) => Some(format!("{a}/{p}")),
            _ => None,
        })
        .collect()
}

fn set_line(name: &str, flags: &str, elements: &[String]) -> String {
    if elements.is_empty() {
        format!("  set {name} {{ type ipv4_addr; flags {flags}; }}\n")
    } else {
        format!(
            "  set {name} {{ type ipv4_addr; flags {flags}; elements = {{ {} }} }}\n",
            elements.join(", ")
        )
    }
}

/// Bound on the two audit sets. Without one the kernel's default (unbounded
/// for dynamic sets) lets a container that sprays destinations grow kernel
/// memory without limit; with one, `update` starts failing once the set is
/// full — which is why the verdict must never ride on that update (the
/// two-rule shape below).
pub const AUDIT_SET_SIZE: u32 = 65_535;
/// Bound on the pinned-answers set. Pins expire on their TTL, so this only
/// has to hold what one instance resolves inside one TTL window.
pub const PIN_SET_SIZE: u32 = 4_096;

/// The instance's whole table, ready for `nft -f -`.
///
/// `upstreams` are the resolvers the forwarder itself is allowed to ask —
/// and only IT: the DNS rule keys on `udp sport UPSTREAM_SPORT`, the port
/// the forwarder binds for the life of the instance, so the app's own
/// queries to a resolver are ordinary traffic and are counted and (under
/// `enforce`) dropped like anything else it did not declare.
pub fn nft_script(app: &str, policy: &Policy, upstreams: &[Ipv4Addr]) -> String {
    let verdict = match policy.mode {
        Mode::Enforce => "drop",
        Mode::Audit | Mode::Off => "accept",
    };
    let table = table_name(app);
    let ups: Vec<String> = upstreams.iter().map(|u| u.to_string()).collect();
    let mut s = format!("table inet {table} {{\n");
    s.push_str(&set_line(
        "allow_static",
        "interval",
        &static_elements(policy),
    ));
    s.push_str(&format!(
        "  set allow_dns {{ type ipv4_addr; flags timeout; size {PIN_SET_SIZE}; }}\n"
    ));
    if ups.is_empty() {
        s.push_str("  set upstream { type ipv4_addr; }\n");
    } else {
        s.push_str(&format!(
            "  set upstream {{ type ipv4_addr; elements = {{ {} }} }}\n",
            ups.join(", ")
        ));
    }
    for name in ["allowed", "blocked"] {
        s.push_str(&format!("  set {name} {{ type ipv4_addr . inet_proto . inet_service; flags dynamic,timeout; timeout 24h; size {AUDIT_SET_SIZE}; counter; }}\n"));
    }
    s.push_str("  chain output {\n");
    s.push_str("    type filter hook output priority filter; policy accept;\n");
    s.push_str("    oif \"lo\" accept\n");
    s.push_str("    ip daddr 10.77.0.0/16 accept\n");
    s.push_str("    ct state established,related accept\n");
    // The FORWARDER's queries only: it holds `UPSTREAM_SPORT` for the life
    // of the instance and never speaks TCP to an upstream, so an app that
    // dials a resolver itself falls through to @allowed/@blocked like any
    // other destination instead of getting unfiltered, unlogged DNS.
    s.push_str(&format!(
        "    ip daddr @upstream udp sport {} udp dport 53 accept\n",
        crate::egress::dns::UPSTREAM_SPORT
    ));
    s.push_str(&format!("    meta nfproto ipv6 {verdict}\n"));
    // Each verdict counts in its OWN set: the accept path updates @allowed
    // and the fall-through updates @blocked, so neither counter has to be
    // derived from the other and a packet is never counted twice.
    for allow in ["allow_static", "allow_dns"] {
        s.push_str(&format!(
            "    ip daddr @{allow} ct state new update @allowed {{ ip daddr . meta l4proto . th dport }}\n"
        ));
        s.push_str(&format!("    ip daddr @{allow} accept\n"));
    }
    // Two rules, never one: `update` on a FULL set returns break, and a
    // combined `update … drop` would then fall through to `policy accept`.
    // The verdict stands on its own line, so an exhausted @blocked costs
    // the record and never the enforcement.
    s.push_str("    ct state new update @blocked { ip daddr . meta l4proto . th dport }\n");
    s.push_str(&format!("    {verdict}\n"));
    s.push_str("  }\n}\n");
    s
}

pub fn pin_script(app: &str, addrs: &[Ipv4Addr], ttl_secs: u32) -> String {
    let elems: Vec<String> = addrs
        .iter()
        .map(|a| format!("{a} timeout {ttl_secs}s"))
        .collect();
    format!(
        "add element inet {} allow_dns {{ {} }}\n",
        table_name(app),
        elems.join(", ")
    )
}

/// The idempotent pin: one `nft -f -` batch that removes the addresses the
/// set already holds and adds every address back with a fresh timeout.
///
/// `add element … timeout` does NOT refresh an existing element's expiry
/// before kernel 6.10 / nft 1.1 — it either errors (`File exists`) or is a
/// no-op — so a long-lived instance that keeps resolving a declared name
/// would watch its own pin decay to zero and then be blocked for a name it
/// declared. Delete-then-add refreshes it on every kernel, and being one
/// batch means the address is never absent from the set in between.
///
/// Only addresses in BOTH `present` and `addrs` are deleted: `present` is
/// the whole set, and the other names' pins are none of this answer's
/// business.
pub fn repin_script(app: &str, present: &[Ipv4Addr], addrs: &[Ipv4Addr], ttl_secs: u32) -> String {
    let table = table_name(app);
    let mut s = String::new();
    let stale: Vec<String> = addrs
        .iter()
        .filter(|a| present.contains(a))
        .map(|a| a.to_string())
        .collect();
    if !stale.is_empty() {
        s.push_str(&format!(
            "delete element inet {table} allow_dns {{ {} }}\n",
            stale.join(", ")
        ));
    }
    s.push_str(&pin_script(app, addrs, ttl_secs));
    s
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetElement {
    pub addr: Ipv4Addr,
    pub proto: String,
    pub port: u16,
    pub packets: u64,
}

fn proto_name(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.to_ascii_lowercase()),
        serde_json::Value::Number(n) => match n.as_u64()? {
            6 => Some("tcp".into()),
            17 => Some("udp".into()),
            other => Some(other.to_string()),
        },
        _ => None,
    }
}

/// Elements of one `nft -j list set` result. Lenient: a missing `elem`
/// key is an empty set; unrecognised elements are skipped, not errors.
pub fn parse_set_elements(json: &str) -> Result<Vec<SetElement>> {
    let root: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error::Runtime(format!("nft -j: {e}")))?;
    let mut out = Vec::new();
    let items = root
        .get("nftables")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for item in items {
        let Some(set) = item.get("set") else { continue };
        let Some(elems) = set.get("elem").and_then(|v| v.as_array()) else {
            continue;
        };
        for e in elems {
            let inner = e.get("elem").unwrap_or(e);
            let val = inner.get("val").unwrap_or(inner);
            let parts = match val.get("concat").and_then(|c| c.as_array()) {
                Some(parts) => parts.clone(),
                None => vec![val.clone()],
            };
            let addr = parts
                .first()
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Ipv4Addr>().ok());
            let proto = parts.get(1).and_then(proto_name);
            let port = parts
                .get(2)
                .and_then(|v| v.as_u64())
                .and_then(|p| u16::try_from(p).ok());
            let packets = inner
                .get("counter")
                .and_then(|c| c.get("packets"))
                .and_then(|p| p.as_u64())
                .unwrap_or(0);
            if let (Some(addr), Some(proto), Some(port)) = (addr, proto, port) {
                out.push(SetElement {
                    addr,
                    proto,
                    port,
                    packets,
                });
            }
        }
    }
    Ok(out)
}

/// The addresses of a plain `ipv4_addr` set (`allow_dns`), as `nft -j list
/// set` prints them: a bare string per element, or — with `flags timeout` —
/// an `{"elem": {"val": "1.2.3.4", "timeout": …, "expires": …}}` wrapper.
/// Lenient in the same way `parse_set_elements` is: unreadable JSON and
/// unrecognised elements yield nothing rather than an error, and the caller
/// treats "nothing" exactly as it treats a set it could not read.
pub fn parse_addr_set(json: &str) -> Vec<Ipv4Addr> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let items = root
        .get("nftables")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for item in items {
        let Some(set) = item.get("set") else { continue };
        let Some(elems) = set.get("elem").and_then(|v| v.as_array()) else {
            continue;
        };
        for e in elems {
            let inner = e.get("elem").unwrap_or(e);
            let val = inner.get("val").unwrap_or(inner);
            if let Some(addr) = val.as_str().and_then(|s| s.parse::<Ipv4Addr>().ok()) {
                out.push(addr);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{effective, EgressOverride, Mode};
    use std::net::Ipv4Addr;

    fn policy(mode: Mode, raw: &[&str]) -> Policy {
        let list: Vec<crate::egress::EgressEntry> =
            raw.iter().map(|s| s.parse().unwrap()).collect();
        effective(
            Some(&list),
            Some(&EgressOverride {
                mode: Some(mode),
                allow: None,
            }),
        )
    }
    const UP: [Ipv4Addr; 2] = [Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(1, 0, 0, 1)];

    #[test]
    fn the_enforce_table_matches_the_spec_verbatim() {
        let script = nft_script(
            "web",
            &policy(
                Mode::Enforce,
                &["api.stripe.com", "140.82.112.0/20", "1.1.1.1"],
            ),
            &UP,
        );
        let want = "\
table inet egress_web {
  set allow_static { type ipv4_addr; flags interval; elements = { 140.82.112.0/20, 1.1.1.1 } }
  set allow_dns { type ipv4_addr; flags timeout; size 4096; }
  set upstream { type ipv4_addr; elements = { 8.8.8.8, 1.0.0.1 } }
  set allowed { type ipv4_addr . inet_proto . inet_service; flags dynamic,timeout; timeout 24h; size 65535; counter; }
  set blocked { type ipv4_addr . inet_proto . inet_service; flags dynamic,timeout; timeout 24h; size 65535; counter; }
  chain output {
    type filter hook output priority filter; policy accept;
    oif \"lo\" accept
    ip daddr 10.77.0.0/16 accept
    ct state established,related accept
    ip daddr @upstream udp sport 35353 udp dport 53 accept
    meta nfproto ipv6 drop
    ip daddr @allow_static ct state new update @allowed { ip daddr . meta l4proto . th dport }
    ip daddr @allow_static accept
    ip daddr @allow_dns ct state new update @allowed { ip daddr . meta l4proto . th dport }
    ip daddr @allow_dns accept
    ct state new update @blocked { ip daddr . meta l4proto . th dport }
    drop
  }
}
";
        assert_eq!(script, want);
    }

    /// The verdict is its own rule, so a full `@blocked` cannot turn a drop
    /// into the chain's `policy accept`.
    #[test]
    fn the_verdict_never_rides_on_the_blocked_update() {
        for mode in [Mode::Enforce, Mode::Audit] {
            let script = nft_script("web", &policy(mode, &["api.stripe.com"]), &UP);
            let verdict = if mode == Mode::Enforce {
                "drop"
            } else {
                "accept"
            };
            assert!(
                script.ends_with(&format!(
                    "    ct state new update @blocked {{ ip daddr . meta l4proto . th dport }}\n    {verdict}\n  }}\n}}\n"
                )),
                "{script}"
            );
        }
    }

    #[test]
    fn audit_accepts_where_enforce_drops_and_still_records_blocked() {
        let script = nft_script("web", &policy(Mode::Audit, &["api.stripe.com"]), &UP);
        assert!(script.contains("meta nfproto ipv6 accept\n"));
        assert!(script.contains("    ct state new update @blocked { ip daddr . meta l4proto . th dport }\n    accept\n"), "{script}");
        assert!(!script.contains(" drop"));
    }

    /// Both verdicts have a counter of their own: nothing is derived by
    /// subtracting one set from the other.
    #[test]
    fn every_allow_rule_counts_into_allowed_before_it_accepts() {
        let script = nft_script("web", &policy(Mode::Enforce, &["1.1.1.1"]), &UP);
        assert!(!script.contains("@seen"), "the seen set is gone: {script}");
        for allow in ["allow_static", "allow_dns"] {
            assert!(
                script.contains(&format!(
                    "    ip daddr @{allow} ct state new update @allowed {{ ip daddr . meta l4proto . th dport }}\n    ip daddr @{allow} accept\n"
                )),
                "{script}"
            );
        }
        // and both dynamic sets are bounded
        assert_eq!(script.matches("size 65535;").count(), 2, "{script}");
        assert!(script.contains("size 4096;"), "{script}");
    }

    #[test]
    fn no_static_entries_renders_an_empty_interval_set_and_any_renders_the_whole_internet() {
        let script = nft_script("web", &policy(Mode::Enforce, &["api.stripe.com"]), &UP);
        assert!(
            script.contains("  set allow_static { type ipv4_addr; flags interval; }\n"),
            "{script}"
        );
        let script = nft_script("web", &policy(Mode::Enforce, &["*"]), &UP);
        assert!(script.contains("elements = { 0.0.0.0/0 }"), "{script}");
    }

    #[test]
    fn table_names_are_safe_and_pins_carry_their_ttl() {
        assert_eq!(table_name("my-app.v2"), "egress_my_app_v2");
        assert_eq!(
            pin_script("web", &[Ipv4Addr::new(54, 187, 174, 169), Ipv4Addr::new(54, 187, 174, 170)], 300),
            "add element inet egress_web allow_dns { 54.187.174.169 timeout 300s, 54.187.174.170 timeout 300s }\n"
        );
    }

    /// Re-pinning deletes what the set already holds and adds everything
    /// back, in one batch — `add` alone does not refresh a timeout on a
    /// kernel older than 6.10.
    #[test]
    fn a_repin_deletes_only_the_addresses_it_is_about_to_add_back() {
        let a = Ipv4Addr::new(1, 2, 3, 4);
        let b = Ipv4Addr::new(5, 6, 7, 8);
        let elsewhere = Ipv4Addr::new(9, 9, 9, 9);
        assert_eq!(
            repin_script("web", &[a, elsewhere], &[a, b], 300),
            "delete element inet egress_web allow_dns { 1.2.3.4 }\n\
             add element inet egress_web allow_dns { 1.2.3.4 timeout 300s, 5.6.7.8 timeout 300s }\n",
            "another name's pin is not this answer's business"
        );
        // Nothing pinned yet: a plain add, no delete to fail on.
        assert_eq!(
            repin_script("web", &[], &[a], 300),
            "add element inet egress_web allow_dns { 1.2.3.4 timeout 300s }\n"
        );
    }

    #[test]
    fn the_pinned_set_reads_back_as_plain_addresses() {
        let json = r#"{"nftables": [{"metainfo": {"version": "1.0.9", "release_name": "Old Doc Yak #3", "json_schema_version": 1}}, {"set": {"family": "inet", "name": "allow_dns", "table": "egress_web", "type": "ipv4_addr", "handle": 3, "flags": ["timeout"], "elem": [{"elem": {"val": "54.187.174.169", "timeout": 300, "expires": 287}}, {"elem": {"val": "1.1.1.1", "timeout": 300, "expires": 12}}]}}]}"#;
        assert_eq!(
            parse_addr_set(json),
            vec![Ipv4Addr::new(54, 187, 174, 169), Ipv4Addr::new(1, 1, 1, 1)]
        );
        // a set with no timeout prints bare strings
        let bare = r#"{"nftables": [{"set": {"name": "allow_dns", "table": "t", "type": "ipv4_addr", "elem": ["8.8.8.8"]}}]}"#;
        assert_eq!(parse_addr_set(bare), vec![Ipv4Addr::new(8, 8, 8, 8)]);
        // empty, and unreadable, both mean "nothing known" — never an error
        let empty =
            r#"{"nftables": [{"set": {"name": "allow_dns", "table": "t", "type": "ipv4_addr"}}]}"#;
        assert!(parse_addr_set(empty).is_empty());
        assert!(parse_addr_set("not json").is_empty());
    }

    #[test]
    fn set_elements_parse_from_nft_json_concats_and_plain_values() {
        let json = r#"{"nftables": [{"metainfo": {"version": "1.0.9", "release_name": "Old Doc Yak #3", "json_schema_version": 1}}, {"set": {"family": "inet", "name": "allowed", "table": "egress_web", "type": ["ipv4_addr", "inet_proto", "inet_service"], "handle": 4, "flags": ["dynamic", "timeout"], "timeout": 86400, "elem": [{"elem": {"val": {"concat": ["54.187.174.169", "tcp", 443]}, "timeout": 86400, "expires": 86390, "counter": {"packets": 12, "bytes": 720}}}, {"elem": {"val": {"concat": ["203.0.113.9", "udp", 8443]}, "timeout": 86400, "expires": 100, "counter": {"packets": 1, "bytes": 60}}}]}}]}"#;
        let got = parse_set_elements(json).unwrap();
        assert_eq!(
            got,
            vec![
                SetElement {
                    addr: Ipv4Addr::new(54, 187, 174, 169),
                    proto: "tcp".into(),
                    port: 443,
                    packets: 12
                },
                SetElement {
                    addr: Ipv4Addr::new(203, 0, 113, 9),
                    proto: "udp".into(),
                    port: 8443,
                    packets: 1
                },
            ]
        );
        // an empty set has no "elem" key at all
        let empty = r#"{"nftables": [{"metainfo": {"version": "1.0.9", "release_name": "x", "json_schema_version": 1}}, {"set": {"family": "inet", "name": "allowed", "table": "egress_web", "type": ["ipv4_addr", "inet_proto", "inet_service"], "handle": 4, "flags": ["dynamic", "timeout"], "timeout": 86400}}]}"#;
        assert!(parse_set_elements(empty).unwrap().is_empty());
        // a protocol may come back as a number on some builds
        let numeric = r#"{"nftables": [{"set": {"family": "inet", "name": "allowed", "table": "t", "type": ["ipv4_addr", "inet_proto", "inet_service"], "elem": [{"elem": {"val": {"concat": ["1.2.3.4", 6, 80]}, "counter": {"packets": 2, "bytes": 1}}}]}}]}"#;
        assert_eq!(parse_set_elements(numeric).unwrap()[0].proto, "tcp");
    }

    /// Not run by `make check`: writes the rendered script so the owner can
    /// validate it against a real kernel — `cargo test -p ply-core
    /// write_sample_script -- --ignored && sudo nft -c -f target/egress-sample.nft`.
    #[test]
    #[ignore]
    fn write_sample_script_for_nft_check() {
        // Unit tests run with the crate directory as cwd, so anchor on the
        // workspace `target/` the command above names, not a relative path.
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../target"));
        std::fs::create_dir_all(&target).unwrap();
        let script = nft_script(
            "sample",
            &policy(
                Mode::Enforce,
                &["api.stripe.com", "140.82.112.0/20", "1.1.1.1"],
            ),
            &[Ipv4Addr::new(8, 8, 8, 8)],
        );
        std::fs::write(target.join("egress-sample.nft"), script).unwrap();
    }
}
