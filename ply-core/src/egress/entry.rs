//! The egress entry grammar: what a `[network] egress` list or a stack
//! override may contain, and how an entry matches a name or an address.

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

use crate::error::{Error, Result};

/// One allowed destination. Names and wildcards match what the app RESOLVES
/// (the forwarder pins the answers); addresses and ranges match what it
/// CONNECTS to. `Any` is "unrestricted" and is always reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressEntry {
    Name(String),
    Wildcard(String),
    Addr(Ipv4Addr),
    Cidr(Ipv4Addr, u8),
    Any,
}

const FORMS: &str = "host.example, *.example, 1.2.3.4, 10.0.0.0/8, or *";

fn normalize_name(raw: &str) -> Option<String> {
    let name = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    let ok = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    ok.then_some(name)
}

impl FromStr for EgressEntry {
    type Err = Error;
    fn from_str(raw: &str) -> Result<Self> {
        let bad = || {
            Error::Manifest(format!(
                "egress entry `{raw}`: not a destination — expected {FORMS}"
            ))
        };
        let s = raw.trim();
        if s == "*" {
            return Ok(EgressEntry::Any);
        }
        if let Some(rest) = s.strip_prefix("*.") {
            return normalize_name(rest)
                .map(EgressEntry::Wildcard)
                .ok_or_else(bad);
        }
        if let Some((addr, prefix)) = s.split_once('/') {
            let addr: Ipv4Addr = addr.parse().map_err(|_| bad())?;
            let prefix: u8 = prefix.parse().map_err(|_| bad())?;
            if prefix > 32 {
                return Err(bad());
            }
            return Ok(EgressEntry::Cidr(addr, prefix));
        }
        if let Ok(addr) = s.parse::<Ipv4Addr>() {
            return Ok(EgressEntry::Addr(addr));
        }
        // A bare name must contain no characters an address or URL would.
        if s.contains(':') || s.contains('/') || s.contains(char::is_whitespace) {
            return Err(bad());
        }
        normalize_name(s).map(EgressEntry::Name).ok_or_else(bad)
    }
}

impl fmt::Display for EgressEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EgressEntry::Name(n) => f.write_str(n),
            EgressEntry::Wildcard(n) => write!(f, "*.{n}"),
            EgressEntry::Addr(a) => write!(f, "{a}"),
            EgressEntry::Cidr(a, p) => write!(f, "{a}/{p}"),
            EgressEntry::Any => f.write_str("*"),
        }
    }
}

fn mask(prefix: u8) -> u32 {
    let prefix = prefix.min(32);
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    }
}

/// The QUERIED name, normalised for comparison only: lowercased, one
/// trailing dot removed. Deliberately NOT `normalize_name` — that is the
/// grammar an operator's entry must satisfy, and applying it to what the
/// app asked for would refuse perfectly ordinary lookups under a wildcard
/// the operator did declare: `_acme-challenge.plybox.sh` (ACME DNS-01),
/// `_postgresql._tcp.db.example` (SRV), any underscore label at all.
/// Empty is still nothing: a root query matches no entry but `*`, which is
/// answered before this is ever asked.
fn query_name(raw: &str) -> Option<String> {
    let name = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

impl EgressEntry {
    pub fn matches_name(&self, name: &str) -> bool {
        let Some(name) = query_name(name) else {
            return false;
        };
        match self {
            EgressEntry::Name(n) => *n == name,
            EgressEntry::Wildcard(suffix) => name
                .strip_suffix(suffix.as_str())
                .is_some_and(|head| head.ends_with('.')),
            EgressEntry::Any => true,
            EgressEntry::Addr(_) | EgressEntry::Cidr(..) => false,
        }
    }

    pub fn matches_addr(&self, addr: Ipv4Addr) -> bool {
        match self {
            EgressEntry::Addr(a) => *a == addr,
            EgressEntry::Cidr(net, prefix) => {
                let m = mask(*prefix);
                (u32::from(*net) & m) == (u32::from(addr) & m)
            }
            EgressEntry::Any => true,
            EgressEntry::Name(_) | EgressEntry::Wildcard(_) => false,
        }
    }
}

/// Parse a whole list, failing on the first bad entry.
pub fn parse_list(raw: &[String]) -> Result<Vec<EgressEntry>> {
    raw.iter().map(|s| s.parse()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn the_five_forms_parse_and_round_trip() {
        for (raw, want) in [
            ("api.stripe.com", EgressEntry::Name("api.stripe.com".into())),
            (
                "API.Stripe.COM.",
                EgressEntry::Name("api.stripe.com".into()),
            ),
            (
                "*.amazonaws.com",
                EgressEntry::Wildcard("amazonaws.com".into()),
            ),
            ("1.1.1.1", EgressEntry::Addr(Ipv4Addr::new(1, 1, 1, 1))),
            (
                "140.82.112.0/20",
                EgressEntry::Cidr(Ipv4Addr::new(140, 82, 112, 0), 20),
            ),
            ("*", EgressEntry::Any),
        ] {
            let e: EgressEntry = raw.parse().unwrap();
            assert_eq!(e, want, "{raw}");
            assert_eq!(
                e.to_string().parse::<EgressEntry>().unwrap(),
                e,
                "round trip {raw}"
            );
        }
    }

    #[test]
    fn rejected_forms_name_the_entry_and_the_accepted_forms() {
        for raw in [
            "https://api.stripe.com",
            "api.stripe.com:443",
            "two words",
            "2001:db8::1",
            "10.0.0.0/33",
            "",
            "*.",
            "-bad.example",
        ] {
            let err = raw.parse::<EgressEntry>().unwrap_err().to_string();
            assert!(err.contains(&format!("`{raw}`")), "{raw}: {err}");
            assert!(err.contains("host.example"), "{raw}: {err}");
        }
    }

    #[test]
    fn names_match_exactly_and_wildcards_match_suffixes_only() {
        let name: EgressEntry = "api.stripe.com".parse().unwrap();
        assert!(name.matches_name("api.stripe.com"));
        assert!(name.matches_name("API.STRIPE.COM."));
        assert!(!name.matches_name("stripe.com"));
        assert!(!name.matches_name("x.api.stripe.com"));
        let wild: EgressEntry = "*.amazonaws.com".parse().unwrap();
        assert!(wild.matches_name("s3.amazonaws.com"));
        assert!(wild.matches_name("a.b.amazonaws.com"));
        assert!(!wild.matches_name("amazonaws.com"));
        assert!(!wild.matches_name("evilamazonaws.com"));
        assert!(EgressEntry::Any.matches_name("anything.example"));
        assert!(!EgressEntry::Any.matches_name(""));
    }

    /// The entry grammar constrains what an OPERATOR may write, never what
    /// the app may ask for. A wildcard covers every subdomain of its suffix,
    /// underscore labels (ACME, SRV) included.
    #[test]
    fn a_wildcard_covers_names_the_entry_grammar_would_refuse() {
        let wild: EgressEntry = "*.plybox.sh".parse().unwrap();
        assert!(wild.matches_name("_acme-challenge.plybox.sh"));
        assert!(wild.matches_name("_ACME-Challenge.plybox.sh."));
        let db: EgressEntry = "*.db.example".parse().unwrap();
        assert!(db.matches_name("_postgresql._tcp.db.example"));
        // …and the entry side stays strict
        assert!("_acme-challenge.plybox.sh".parse::<EgressEntry>().is_err());
        // the negatives are unchanged: a suffix is not a substring
        assert!(!wild.matches_name("plybox.sh"));
        assert!(!wild.matches_name("evilplybox.sh"));
        assert!(!wild.matches_name("plybox.sh.evil.example"));
        let exact: EgressEntry = "api.stripe.com".parse().unwrap();
        assert!(!exact.matches_name("_dmarc.api.stripe.com"));
    }

    #[test]
    fn addresses_and_ranges_match_and_names_never_match_addresses() {
        let a: EgressEntry = "1.1.1.1".parse().unwrap();
        assert!(a.matches_addr(Ipv4Addr::new(1, 1, 1, 1)));
        assert!(!a.matches_addr(Ipv4Addr::new(1, 1, 1, 2)));
        let c: EgressEntry = "140.82.112.0/20".parse().unwrap();
        assert!(c.matches_addr(Ipv4Addr::new(140, 82, 127, 255)));
        assert!(!c.matches_addr(Ipv4Addr::new(140, 82, 128, 0)));
        let any_range: EgressEntry = "0.0.0.0/0".parse().unwrap();
        assert!(any_range.matches_addr(Ipv4Addr::new(203, 0, 113, 9)));
        let n: EgressEntry = "api.stripe.com".parse().unwrap();
        assert!(!n.matches_addr(Ipv4Addr::new(1, 1, 1, 1)));
        assert!(EgressEntry::Any.matches_addr(Ipv4Addr::new(9, 9, 9, 9)));
    }

    #[test]
    fn parse_list_reports_the_first_bad_entry() {
        let ok = parse_list(&["a.example".into(), "*".into()]).unwrap();
        assert_eq!(ok.len(), 2);
        let err = parse_list(&["a.example".into(), "nope:1".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("`nope:1`"), "{err}");
    }

    #[test]
    fn an_out_of_range_prefix_built_by_hand_behaves_as_a_single_address() {
        let e = EgressEntry::Cidr(Ipv4Addr::new(10, 1, 2, 3), 200);
        assert!(e.matches_addr(Ipv4Addr::new(10, 1, 2, 3)));
        assert!(!e.matches_addr(Ipv4Addr::new(10, 1, 2, 4)));
    }
}
