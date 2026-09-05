# Egress Contract Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An instance may only reach what its author declared and its operator allowed, enforced inside the instance's own network namespace, with a per-instance audit log, `ply egress`, and events.

**Architecture:** A new pure module `ply-core/src/egress/` (entry grammar, effective policy, nft script rendering and set parsing, DNS message handling, log records) with no platform code; the manifest gains `[network] egress`, the stack member and `ply run` gain an override; the supervisor computes the effective policy and hands it to the backend in `InstanceSpec.egress`; the Linux backend runs one thread per instance that enters the namespace, serves DNS on `127.0.0.53`, installs one nftables table, pins resolved addresses, polls two dynamic sets, writes the log and emits events. Rootless ignores the policy with a warning.

**Tech Stack:** Rust 2021; `nft` (nftables ≥ 1.0) spawned from the thread; `serde_json` for the log and for `nft -j`; `nix` (`setns`, sockets); no new crates.

**Spec:** `docs/superpowers/specs/2026-09-04-egress-contract-design.md` — every table in it (entries, effective policy, forwarder decisions) is a test in this plan.

## Global Constraints

- **Zero behaviour change without a policy.** With no `[network]` and no override the effective mode is `off`: no thread, no nft, `resolv.conf` exactly as today, no new output. `make check` (fmt, clippy `-D warnings`, `cargo test --workspace`) stays green after every task; `make check-darwin` stays clean (the egress module is pure and compiles everywhere; only `runtime/ns/egress.rs` is Linux).
- **Entry grammar** (spec "Manifest: the claim"): `host.example` exact name (case-insensitive, trailing dot ignored); `*.example` suffix wildcard (not `example` itself); `1.2.3.4` IPv4; `10.0.0.0/8` IPv4 CIDR; `*` unrestricted. Anything else is a manifest error naming the entry and the accepted forms. IPv6 entries are rejected in this version.
- **Effective policy** (spec table): manifest absent + override absent → `off`, none; manifest present + override absent → `audit`, manifest list; override with `mode` only → that mode with the manifest list (or none); override with `allow` → that list replaces the manifest's.
- **Always allowed:** `lo`; `10.77.0.0/16`; `ct state established,related`; TCP/UDP 53 to the forwarder's upstreams.
- **Exact messages:** unrestricted → `ply: <app> declares unrestricted egress`; rootless → `ply: egress policy needs a network per instance — rootless runs unenforced and unobserved (use a rootful host to audit or enforce)`; missing nft in enforce → `egress policy: nft not found — install nftables or run with --egress audit`; start line `ply: egress ` + `Policy::describe()`, e.g. `ply: egress enforce, 3 entries (manifest)`; plan line `egress: enforce, 3 entries (manifest)` / `egress: audit, 0 entries (override)` / `egress: off`; inspect line `egress:` + entries or `not declared`.
- **Forwarder listens on `127.0.0.53:53`** UDP and TCP; in `enforce` an undeclared name gets `REFUSED` (RCODE 5) without forwarding; pins use the answer's minimum A-record TTL with a 300 s floor; AAAA passes through unpinned.
- **Log:** `data_dir()/egress/<app>.<n>.log`, JSON lines, 512 KiB cap with one `.1` rotation; record kinds `resolved`, `refused`, `allowed`, `blocked` with the fields the spec shows; connection records at most once per minute per element; events `egress-blocked` / `egress-undeclared` at most once per hour per destination.
- **Table name** `inet egress_<app>`; sets `allow_static` (interval), `allow_dns` (timeout), `upstream`, `seen`, `blocked` (dynamic, timeout 24h, counter); chain `output` hook output priority filter policy accept; rules exactly as the spec lists them; `enforce` verdict `drop`, `audit` verdict `accept`, also for `meta nfproto ipv6`.
- **Owner commits.** Implementers never run `git commit`/`git add`; each task ends with `make check` green and the task's tests passing. Where a step says "commit", stop and report.
- **Nothing runs containers on this box.** The thread and the table are verified on the droplet by the owner (Task 8's checklist); everything below it is unit-tested here.

---

## File structure

```
ply-core/src/egress/
  mod.rs        pub mod entry, nft, dns, log; Mode, Policy, PolicySource, EgressOverride, effective(), unrestricted()
  entry.rs      EgressEntry { Name, Wildcard, Addr, Cidr, Any }, FromStr/Display, matches_name(), matches_addr(), parse_list()
  nft.rs        nft_script(app, policy, upstreams), pin_script(app, addrs, ttl), SetElement, parse_set_elements(json)
  dns.rs        Message parsing: question(), a_records(), refused_reply(); tcp_frame()/tcp_unframe(); forward_once()
  log.rs        Record (serde), Writer (cap+rotate), read_app(app) -> Vec<Record>, path()
ply-core/src/lib.rs               pub mod egress;
ply-core/src/manifest.rs          Network { egress }, Manifest.network, validation
ply-core/src/record.rs            egress_entries(manifest_json) for `ply inspect`
ply-core/src/stack.rs             Member.egress: Option<EgressOverride>; MEMBER_KEYS + parse_member
ply-core/src/runtime/backend.rs   InstanceSpec.egress: Option<egress::Policy>
ply-core/src/runtime/run.rs       RunOptions.egress: Option<EgressOverride>; effective policy after prepare_app; start line; rootless warning
ply-core/src/runtime/ns/egress.rs NEW  spawn(): the per-instance thread; EgressHandle
ply-core/src/runtime/ns/mod.rs    launch(): spawn the thread before release; NsInstance holds the handle
ply-core/src/runtime/ns/container.rs  resolv.conf → 127.0.0.53 when spec.egress is Some; upstream_resolvers()
ply-cli/src/cli.rs                RunArgs --egress/--egress-allow; Command::Egress(EgressArgs)
ply-cli/src/commands/run.rs       flags → RunOptions.egress
ply-cli/src/commands/up.rs        Prepared.egress; child flags; --plan line
ply-cli/src/commands/egress.rs    NEW  ply egress
ply-cli/src/commands/images.rs    inspect egress line
docs/{security,manifest,stacks,cli,ply-vs-docker}.md; services/postgres/ply.toml; services/redis/runnable.toml; TASKS.md
```

---

### Task 1: The pure core — entries, modes, policy, `effective`

**Files:**
- Create: `ply-core/src/egress/mod.rs`, `ply-core/src/egress/entry.rs`
- Modify: `ply-core/src/lib.rs` (add `pub mod egress;` alphabetically after `dev`)

**Interfaces:**
- Consumes: nothing.
- Produces (used verbatim by every later task):

```rust
// egress/entry.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EgressEntry { Name(String), Wildcard(String), Addr(Ipv4Addr), Cidr(Ipv4Addr, u8), Any }
impl FromStr for EgressEntry { type Err = crate::Error; … }   // Error::Manifest with the exact message below
impl Display for EgressEntry { … }                             // round-trips the TOML spelling
impl EgressEntry {
    pub fn matches_name(&self, name: &str) -> bool;            // Name/Wildcard/Any only
    pub fn matches_addr(&self, addr: Ipv4Addr) -> bool;        // Addr/Cidr/Any only
}
pub fn parse_list(raw: &[String]) -> crate::Result<Vec<EgressEntry>>;
// egress/mod.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Mode { Off, Audit, Enforce }
impl FromStr for Mode { … }  // "off" | "audit" | "enforce"; Error::Manifest("egress mode `x`: expected off, audit or enforce")
impl Display for Mode { … }
#[derive(Debug, Clone, PartialEq, Eq)] pub enum PolicySource { Manifest, Override, None }
#[derive(Debug, Clone, PartialEq, Eq)] pub struct Policy { pub mode: Mode, pub allow: Vec<EgressEntry>, pub source: PolicySource }
#[derive(Debug, Clone, Default, PartialEq, Eq)] pub struct EgressOverride { pub mode: Option<Mode>, pub allow: Option<Vec<EgressEntry>> }
pub fn effective(manifest: Option<&[EgressEntry]>, over: Option<&EgressOverride>) -> Policy;
impl Policy {
    pub fn unrestricted(&self) -> bool;          // any entry is Any
    pub fn allows_name(&self, name: &str) -> bool;
    pub fn describe(&self) -> String;            // "enforce, 3 entries (manifest)" | "audit, 0 entries (override)" | "off"
}
```

- [ ] **Step 1: Write the failing tests** (`egress/entry.rs` and `egress/mod.rs`, in `#[cfg(test)] mod tests`):

```rust
// entry.rs tests
use super::*;
use std::net::Ipv4Addr;

#[test]
fn the_five_forms_parse_and_round_trip() {
    for (raw, want) in [
        ("api.stripe.com", EgressEntry::Name("api.stripe.com".into())),
        ("API.Stripe.COM.", EgressEntry::Name("api.stripe.com".into())),
        ("*.amazonaws.com", EgressEntry::Wildcard("amazonaws.com".into())),
        ("1.1.1.1", EgressEntry::Addr(Ipv4Addr::new(1, 1, 1, 1))),
        ("140.82.112.0/20", EgressEntry::Cidr(Ipv4Addr::new(140, 82, 112, 0), 20)),
        ("*", EgressEntry::Any),
    ] {
        let e: EgressEntry = raw.parse().unwrap();
        assert_eq!(e, want, "{raw}");
        assert_eq!(e.to_string().parse::<EgressEntry>().unwrap(), e, "round trip {raw}");
    }
}

#[test]
fn rejected_forms_name_the_entry_and_the_accepted_forms() {
    for raw in ["https://api.stripe.com", "api.stripe.com:443", "two words", "2001:db8::1", "10.0.0.0/33", "", "*.", "-bad.example"] {
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
    assert!(!EgressEntry::Any.matches_name("") );
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
    let err = parse_list(&["a.example".into(), "nope:1".into()]).unwrap_err().to_string();
    assert!(err.contains("`nope:1`"), "{err}");
}
```

```rust
// mod.rs tests
use super::*;
use crate::egress::entry::EgressEntry;

fn entries(raw: &[&str]) -> Vec<EgressEntry> {
    raw.iter().map(|s| s.parse().unwrap()).collect()
}

#[test]
fn the_effective_policy_table_from_the_spec() {
    let m = entries(&["api.stripe.com"]);
    let over_mode = EgressOverride { mode: Some(Mode::Enforce), allow: None };
    let over_list = EgressOverride { mode: Some(Mode::Audit), allow: Some(entries(&["*.stripe.com"])) };
    // row 1: nothing anywhere
    assert_eq!(effective(None, None), Policy { mode: Mode::Off, allow: vec![], source: PolicySource::None });
    // row 2: manifest only → audit with the manifest's list
    assert_eq!(effective(Some(&m), None), Policy { mode: Mode::Audit, allow: m.clone(), source: PolicySource::Manifest });
    // row 3: override mode, no manifest → that mode, no list
    assert_eq!(effective(None, Some(&over_mode)), Policy { mode: Mode::Enforce, allow: vec![], source: PolicySource::None });
    // row 4: override mode + manifest → that mode, manifest's list
    assert_eq!(effective(Some(&m), Some(&over_mode)), Policy { mode: Mode::Enforce, allow: m.clone(), source: PolicySource::Manifest });
    // row 5: override list replaces the manifest's
    assert_eq!(effective(Some(&m), Some(&over_list)), Policy { mode: Mode::Audit, allow: entries(&["*.stripe.com"]), source: PolicySource::Override });
    // an override list with no mode keeps the default (audit)
    let list_only = EgressOverride { mode: None, allow: Some(vec![]) };
    assert_eq!(effective(Some(&m), Some(&list_only)).mode, Mode::Audit);
    assert_eq!(effective(Some(&m), Some(&list_only)).source, PolicySource::Override);
}

#[test]
fn modes_parse_and_print() {
    assert_eq!("enforce".parse::<Mode>().unwrap(), Mode::Enforce);
    assert_eq!(Mode::Audit.to_string(), "audit");
    let err = "strict".parse::<Mode>().unwrap_err().to_string();
    assert!(err.contains("expected off, audit or enforce"), "{err}");
}

#[test]
fn describe_and_unrestricted() {
    let p = effective(Some(&entries(&["a.example", "b.example", "1.1.1.1"])), Some(&EgressOverride { mode: Some(Mode::Enforce), allow: None }));
    assert_eq!(p.describe(), "enforce, 3 entries (manifest)");
    assert!(!p.unrestricted());
    let any = effective(Some(&entries(&["*"])), None);
    assert!(any.unrestricted());
    assert!(any.allows_name("whatever.example"));
    assert_eq!(effective(None, None).describe(), "off");
    let over = effective(None, Some(&EgressOverride { mode: Some(Mode::Audit), allow: Some(vec![]) }));
    assert_eq!(over.describe(), "audit, 0 entries (override)");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ply-core egress::`
Expected: compile errors — module and types missing.

- [ ] **Step 3: Implement `entry.rs`**

```rust
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
            return normalize_name(rest).map(EgressEntry::Wildcard).ok_or_else(bad);
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
    if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) }
}

impl EgressEntry {
    pub fn matches_name(&self, name: &str) -> bool {
        let Some(name) = normalize_name(name) else { return false };
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
```

- [ ] **Step 4: Implement `mod.rs`**

```rust
//! The egress contract: what an instance may reach. Pure types and the
//! effective-policy rule; the enforcement lives in the platform backend.

pub mod dns;
pub mod entry;
pub mod log;
pub mod nft;

use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};
pub use entry::EgressEntry;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Audit,
    Enforce,
}

impl FromStr for Mode {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "off" => Ok(Mode::Off),
            "audit" => Ok(Mode::Audit),
            "enforce" => Ok(Mode::Enforce),
            other => Err(Error::Manifest(format!(
                "egress mode `{other}`: expected off, audit or enforce"
            ))),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Off => "off",
            Mode::Audit => "audit",
            Mode::Enforce => "enforce",
        })
    }
}

/// Where the effective list came from — shown in `ply up --plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicySource {
    Manifest,
    Override,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub mode: Mode,
    pub allow: Vec<EgressEntry>,
    pub source: PolicySource,
}

/// The operator's word: a stack member's `egress = …` or `ply run
/// --egress/--egress-allow`. `allow: Some(list)` REPLACES the manifest's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressOverride {
    pub mode: Option<Mode>,
    pub allow: Option<Vec<EgressEntry>>,
}

/// The spec's effective-policy table. The operator's word wins, the
/// author's claim fills in; a claim alone means `audit`.
pub fn effective(manifest: Option<&[EgressEntry]>, over: Option<&EgressOverride>) -> Policy {
    let (allow, source) = match over.and_then(|o| o.allow.as_ref()) {
        Some(list) => (list.clone(), PolicySource::Override),
        None => match manifest {
            Some(list) => (list.to_vec(), PolicySource::Manifest),
            None => (Vec::new(), PolicySource::None),
        },
    };
    let mode = match over.and_then(|o| o.mode) {
        Some(mode) => mode,
        None => match (manifest, over) {
            (None, None) => Mode::Off,
            _ => Mode::Audit,
        },
    };
    Policy { mode, allow, source }
}

impl Policy {
    pub fn unrestricted(&self) -> bool {
        self.allow.iter().any(|e| *e == EgressEntry::Any)
    }
    pub fn allows_name(&self, name: &str) -> bool {
        self.allow.iter().any(|e| e.matches_name(name))
    }
    pub fn describe(&self) -> String {
        if self.mode == Mode::Off {
            return "off".into();
        }
        let source = match self.source {
            PolicySource::Manifest => " (manifest)",
            PolicySource::Override => " (override)",
            PolicySource::None => "",
        };
        let n = self.allow.len();
        format!("{}, {n} {}{source}", self.mode, if n == 1 { "entry" } else { "entries" })
    }
}
```

For this task, create `dns.rs`, `log.rs` and `nft.rs` as empty files with only a `//!` doc line each so the module tree compiles; Tasks 4–6 fill them.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p ply-core egress::`
Expected: 8 passed.

- [ ] **Step 6: `make check`** green. Report.

---

### Task 2: The claim — `[network] egress` in the manifest, `ply inspect`, `ply check`

**Files:**
- Modify: `ply-core/src/manifest.rs` (struct `Manifest` fields after `health` at `:39-40`; `Manifest::validate` — find it with `grep -n 'pub fn validate' ply-core/src/manifest.rs`; tests module)
- Modify: `ply-core/src/record.rs` (a helper next to `params_rows`)
- Modify: `ply-cli/src/commands/images.rs:74-90` (`render`)
- Verify: `ply-cli/src/commands/lifecycle.rs` `check` reaches `Manifest::validate` (grep `validate(`); if it does not, call it there.

**Interfaces:**
- Consumes: Task 1 `egress::entry::{EgressEntry, parse_list}`.
- Produces:

```rust
// manifest.rs
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// `[network] egress`: the destinations this package needs, as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<Vec<String>>,
}
// on Manifest, after `health`:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub network: Option<Network>,
impl Manifest {
    /// The declared list, parsed; `None` when `[network] egress` is absent.
    pub fn egress_entries(&self) -> Result<Option<Vec<EgressEntry>>>;
}
// record.rs
pub fn egress_entries(manifest: &serde_json::Value) -> Option<Vec<String>>;   // raw strings from manifest["network"]["egress"]
```

- [ ] **Step 1: Failing tests** (manifest.rs `mod tests`; images.rs tests next to `render_prints_the_params_block_and_the_footer_verbatim`):

```rust
#[test]
fn a_network_egress_list_parses_and_validates() {
    let m: Manifest = toml::from_str(
        "[package]\nname = \"web\"\nversion = \"1.0.0\"\nentrypoint = [\"/bin/true\"]\n[network]\negress = [\"api.stripe.com\", \"*.amazonaws.com\", \"140.82.112.0/20\"]\n",
    ).unwrap();
    m.validate().unwrap();
    let entries = m.egress_entries().unwrap().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1].to_string(), "*.amazonaws.com");
}

#[test]
fn an_empty_egress_list_is_a_claim_and_absence_is_not() {
    let empty: Manifest = toml::from_str(
        "[package]\nname = \"db\"\nversion = \"1.0.0\"\nentrypoint = [\"/bin/true\"]\n[network]\negress = []\n",
    ).unwrap();
    assert_eq!(empty.egress_entries().unwrap(), Some(vec![]));
    let none: Manifest = toml::from_str(
        "[package]\nname = \"db\"\nversion = \"1.0.0\"\nentrypoint = [\"/bin/true\"]\n",
    ).unwrap();
    assert_eq!(none.egress_entries().unwrap(), None);
}

#[test]
fn a_bad_egress_entry_fails_validation_with_the_entry_named() {
    let m: Manifest = toml::from_str(
        "[package]\nname = \"web\"\nversion = \"1.0.0\"\nentrypoint = [\"/bin/true\"]\n[network]\negress = [\"https://api.stripe.com\"]\n",
    ).unwrap();
    let err = m.validate().unwrap_err().to_string();
    assert!(err.contains("`https://api.stripe.com`"), "{err}");
    assert!(err.contains("host.example"), "{err}");
}

#[test]
fn an_unknown_network_key_is_rejected() {
    let err = toml::from_str::<Manifest>(
        "[package]\nname = \"web\"\nversion = \"1.0.0\"\nentrypoint = [\"/bin/true\"]\n[network]\ningress = []\n",
    ).unwrap_err().to_string();
    assert!(err.contains("ingress"), "{err}");
}
```

```rust
// images.rs tests
#[test]
fn render_shows_the_declared_egress_or_not_declared() {
    let with = "[package]\nname = \"web\"\nversion = \"1.0.0\"\nentrypoint = [\"/bin/true\"]\n[network]\negress = [\"api.stripe.com\", \"*\"]\n";
    let record = ply_core::record::record_for_toml(with, Path::new("ply.toml")).unwrap();
    let out = render(&record);
    assert!(out.contains("egress:       api.stripe.com, *"), "{out}");
    let record = ply_core::record::record_for_toml(PG, Path::new("ply.toml")).unwrap();
    assert!(render(&record).contains("egress:       not declared"));
}
```

(`PG` is the existing postgres fixture in that test module; `label_line` pads labels to the column the other lines use — match it exactly.)

- [ ] **Step 2: Run to verify they fail** — `cargo test -p ply-core manifest::` and `cargo test -p ply-cli images::` → compile errors.

- [ ] **Step 3: Implement** — the `Network` struct and field above; in `Manifest::validate` add, after the existing capability check:

```rust
        if let Some(network) = &self.network {
            if let Some(raw) = &network.egress {
                crate::egress::entry::parse_list(raw)?;
            }
        }
```

`egress_entries`:

```rust
    pub fn egress_entries(&self) -> Result<Option<Vec<crate::egress::EgressEntry>>> {
        match self.network.as_ref().and_then(|n| n.egress.as_ref()) {
            Some(raw) => crate::egress::entry::parse_list(raw).map(Some),
            None => Ok(None),
        }
    }
```

`record.rs`:

```rust
/// The `[network] egress` list as written, from a record's manifest JSON.
pub fn egress_entries(manifest: &serde_json::Value) -> Option<Vec<String>> {
    manifest
        .get("network")?
        .get("egress")?
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
}
```

`images.rs` `render`, after the `dependencies` line:

```rust
        label_line(
            "egress",
            &match ply_core::record::egress_entries(&record.manifest) {
                Some(list) if list.is_empty() => "none (declared)".to_string(),
                Some(list) => list.join(", "),
                None => "not declared".to_string(),
            },
        ),
```

- [ ] **Step 4: Run the tests** — both packages pass; then `make check`. Confirm `ply check <img>` runs `Manifest::validate` (name the line in your report). Report.

---

### Task 3: The policy — stack override, `ply run` flags, the supervisor, `--plan`

**Files:**
- Modify: `ply-core/src/stack.rs` (`Member` `:42-64`, `MEMBER_KEYS` `:513`, `parse_member` `:530-600`, tests)
- Modify: `ply-core/src/runtime/backend.rs` (`InstanceSpec`: add `pub egress: Option<crate::egress::Policy>`)
- Modify: `ply-core/src/runtime/run.rs` (`RunOptions`: add `pub egress: Option<crate::egress::EgressOverride>`; `run()` after `prepare_app`; `launch_instance`'s `InstanceSpec` literal at `:1607`)
- Modify: `ply-cli/src/cli.rs` (`RunArgs` after `publish`), `ply-cli/src/commands/run.rs` (`exec`), `ply-cli/src/commands/up.rs` (`Prepared` `:35`, its construction `:286`, the spawn loop `:340-385`, `render_plan` `:229`), `ply-cli/src/commands/reconcile.rs:1301` (the `RunOptions` literal there gains `egress: None`… see Step 3)

**Interfaces:**
- Consumes: Task 1 (`egress::{Mode, Policy, EgressOverride, effective, entry::parse_list}`), Task 2 (`Manifest::egress_entries`).
- Produces: `Member.egress: Option<EgressOverride>`; `RunOptions.egress`; `InstanceSpec.egress: Option<Policy>` (`None` when the effective mode is `Off` or the host is rootless); the messages in Global Constraints; `ply run --egress MODE --egress-allow ENTRY` (repeatable; `--egress-allow ""` = an empty list; any occurrence replaces the manifest list); the `--plan` line.

- [ ] **Step 1: Failing tests**

`stack.rs` tests (next to the existing `parse` tests):

```rust
#[test]
fn a_member_egress_override_parses_its_three_spellings() {
    let text = "[[app]]\nrun = \"postgres@17\"\nname = \"db\"\negress = { mode = \"enforce\" }\n\n[[app]]\nrun = \"redis@8\"\nname = \"cache\"\negress = { mode = \"audit\", allow = [\"*.stripe.com\", \"1.1.1.1\"] }\n\n[[app]]\nrun = \"nginx@1\"\nname = \"edge\"\negress = \"off\"\n";
    let stack = parse(text, Path::new("stack.toml")).unwrap().unwrap();
    let by_name = |n: &str| stack.members.iter().find(|m| m.name == n).unwrap().egress.clone().unwrap();
    assert_eq!(by_name("db"), crate::egress::EgressOverride { mode: Some(crate::egress::Mode::Enforce), allow: None });
    let cache = by_name("cache");
    assert_eq!(cache.mode, Some(crate::egress::Mode::Audit));
    assert_eq!(cache.allow.unwrap().len(), 2);
    assert_eq!(by_name("edge"), crate::egress::EgressOverride { mode: Some(crate::egress::Mode::Off), allow: None });
}

#[test]
fn a_bad_member_egress_names_the_member() {
    let text = "[[app]]\nrun = \"postgres@17\"\nname = \"db\"\negress = { mode = \"strict\" }\n";
    let err = parse(text, Path::new("stack.toml")).unwrap_err().to_string();
    assert!(err.contains("member `db`"), "{err}");
    assert!(err.contains("expected off, audit or enforce"), "{err}");
    let text = "[[app]]\nrun = \"postgres@17\"\nname = \"db\"\negress = 5\n";
    let err = parse(text, Path::new("stack.toml")).unwrap_err().to_string();
    assert!(err.contains("`egress`"), "{err}");
}
```

`up.rs` tests (next to `render_plan_masks_secrets_and_annotates_derived_waits`, reusing its `Prepared` fixture shape): a policies map with `db → enforce/1 entry/manifest`, `server → off`; assert the rendered plan contains `  egress: enforce, 1 entry (manifest)` on the line after `db`'s last env line, and `  egress: off` for `server`, and that a member with `Any` also prints `ply: server declares unrestricted egress` (assert on a returned warnings vec, see Step 3).

`run.rs` (ply-cli) tests: `egress_flags_to_override`:

```rust
#[test]
fn egress_flags_become_an_override() {
    assert_eq!(egress_override(None, &[]).unwrap(), None);
    let o = egress_override(Some("enforce"), &[]).unwrap().unwrap();
    assert_eq!(o.mode, Some(ply_core::egress::Mode::Enforce));
    assert_eq!(o.allow, None);
    let o = egress_override(None, &["a.example".to_string(), "1.1.1.1".to_string()]).unwrap().unwrap();
    assert_eq!(o.allow.unwrap().len(), 2);
    let o = egress_override(Some("audit"), &["".to_string()]).unwrap().unwrap();
    assert_eq!(o.allow, Some(vec![]));
    assert!(egress_override(Some("strict"), &[]).is_err());
}
```

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement**

`stack.rs`: add `"egress"` to `MEMBER_KEYS`; `Member.egress: Option<crate::egress::EgressOverride>`; in `parse_member`, after `scale`:

```rust
    let egress = match table.get("egress") {
        None => None,
        Some(toml::Value::String(mode)) => Some(crate::egress::EgressOverride {
            mode: Some(mode.parse().map_err(|e| Error::Manifest(format!("{}: member `{name}`: {e}", path.display())))?),
            allow: None,
        }),
        Some(toml::Value::Table(t)) => {
            for key in t.keys() {
                if key != "mode" && key != "allow" {
                    return Err(Error::Manifest(format!(
                        "{}: member `{name}`: `egress` accepts `mode` and `allow`, not `{key}`",
                        path.display()
                    )));
                }
            }
            let mode = match t.get("mode") {
                None => None,
                Some(toml::Value::String(m)) => Some(m.parse().map_err(|e| Error::Manifest(format!("{}: member `{name}`: {e}", path.display())))?),
                Some(_) => return Err(Error::Manifest(format!("{}: member `{name}`: `egress.mode` must be a string", path.display()))),
            };
            let allow = match t.get("allow") {
                None => None,
                Some(v) => {
                    let raw = string_list(Some(v), "egress.allow", &name, path)?;
                    Some(crate::egress::entry::parse_list(&raw).map_err(|e| Error::Manifest(format!("{}: member `{name}`: {e}", path.display())))?)
                }
            };
            Some(crate::egress::EgressOverride { mode, allow })
        }
        Some(_) => {
            return Err(Error::Manifest(format!(
                "{}: member `{name}`: `egress` must be a mode string (\"off\" | \"audit\" | \"enforce\") or a table {{ mode = …, allow = [...] }}",
                path.display()
            )))
        }
    };
```

and `egress` in the `Member { … }` literal. Every other place that constructs a `Member` (tests, deployments.rs) gets `egress: None`.

`cli.rs` `RunArgs`, after `publish`:

```rust
    /// Outbound policy: `off`, `audit` (log everything, mark what the
    /// manifest did not declare) or `enforce` (block it). Defaults to
    /// `audit` when the manifest declares `[network] egress`, else `off`.
    #[arg(long, value_name = "MODE")]
    pub egress: Option<String>,

    /// Replace the manifest's declared egress list with these entries
    /// (repeatable: a hostname, `*.suffix`, an IPv4 address or CIDR, or
    /// `*`). Pass `--egress-allow ""` for an empty list.
    #[arg(long = "egress-allow", value_name = "ENTRY")]
    pub egress_allow: Vec<String>,
```

`commands/run.rs`:

```rust
/// `--egress` / `--egress-allow` → the operator's override. Any occurrence
/// of `--egress-allow` replaces the manifest's list; `""` contributes no
/// entry, so `--egress-allow ""` is "allow nothing".
fn egress_override(mode: Option<&str>, allow: &[String]) -> Result<Option<ply_core::egress::EgressOverride>> {
    if mode.is_none() && allow.is_empty() {
        return Ok(None);
    }
    let mode = mode.map(|m| m.parse::<ply_core::egress::Mode>()).transpose()?;
    let allow = if allow.is_empty() {
        None
    } else {
        let raw: Vec<String> = allow.iter().filter(|s| !s.is_empty()).cloned().collect();
        Some(ply_core::egress::entry::parse_list(&raw)?)
    };
    Ok(Some(ply_core::egress::EgressOverride { mode, allow }))
}
```

and in `exec`, `egress: egress_override(args.egress.as_deref(), &args.egress_allow)?` in the `RunOptions` literal. `reconcile.rs:1301` is the BUILD container's `RunOptions` (a builder, not a member): it gets `egress: None`. Reconcile launches deployment members through argv it builds for `ply run` (grep `"--after"` in `reconcile.rs` to find the loop); add the same `--egress`/`--egress-allow` arguments there as in `up.rs` below, from the member's `egress` override, so a deployment file's member table drives the policy exactly like a stack file's.

`run.rs` (core), in `run()` right after `let identity = …` and `backend.admit(...)`:

```rust
    // The egress contract: the author's claim, the operator's word.
    // Order: off → rootless (one line, nothing else) → unrestricted warning
    // + start line → start line.
    let egress = {
        let declared = ctx.manifest.egress_entries()?;
        let policy = crate::egress::effective(declared.as_deref(), opts.egress.as_ref());
        if policy.mode == crate::egress::Mode::Off {
            None
        } else if !facts.own_addresses {
            eprintln!("ply: egress policy needs a network per instance — rootless runs unenforced and unobserved (use a rootful host to audit or enforce)");
            None
        } else {
            if policy.unrestricted() {
                eprintln!("ply: {identity} declares unrestricted egress");
            }
            eprintln!("ply: egress {}", policy.describe());
            Some(policy)
        }
    };
```

Then `launch_instance` receives `egress: &Option<Policy>` — do not add an eighth parameter: put it in `AppContext` as `pub egress: Option<crate::egress::Policy>` set right after `prepare_app` (both call sites: the initial one and the deploy path, where the new manifest is re-evaluated with the same override) and read `ctx.egress.clone()` into `InstanceSpec { egress: … }`.

`up.rs`: `Prepared.egress: Option<EgressOverride>` (from `member.egress.clone()` at `:286`); in the spawn loop after `--domain`:

```rust
        if let Some(over) = &p.egress {
            if let Some(mode) = over.mode {
                cmd.arg("--egress").arg(mode.to_string());
            }
            if let Some(allow) = &over.allow {
                if allow.is_empty() {
                    cmd.arg("--egress-allow").arg("");
                }
                for entry in allow {
                    cmd.arg("--egress-allow").arg(entry.to_string());
                }
            }
        }
```

`--plan`: add `fn member_policies(prepared: &[Prepared]) -> Result<BTreeMap<String, ply_core::egress::Policy>>` that loads each member's manifest the same way `resolve_members` does (find the call that produces `MemberInput.manifest` and reuse that loader; for a `MemberSource::Dir` member without a built image, read `dir/ply.toml` with `Manifest::load`), then `effective(manifest.egress_entries()?.as_deref(), p.egress.as_ref())`. `render_plan(resolution, prepared, policies: &BTreeMap<String, Policy>) -> (String, Vec<String>)` returns the text and the warnings: after a member's env lines (or its header when it has none) push `  egress: {policy.describe()}`; when `policy.unrestricted()` push `ply: {member} declares unrestricted egress` to the warnings, which `exec` prints to stderr before the plan. A member absent from `policies` prints no egress line, so the existing `render_plan` tests pass unchanged when called with an empty map; `exec` always fills the map for every member. Every `Prepared { … }` literal gains `egress: None` (the real one at `:286` uses `member.egress.clone()`; the test fixtures at `:779`, `:795`, `:938`, `:979`, `:995`, `:1008` use `None`).

- [ ] **Step 4: Tests pass; `make check`; `make check-darwin`** (the new code is portable). Report.

---

### Task 4: `egress::nft` — the table, the pin, and reading the sets back

**Files:**
- Modify: `ply-core/src/egress/nft.rs`

**Interfaces:**
- Consumes: Task 1 `Policy`, `Mode`, `EgressEntry`.
- Produces:

```rust
pub fn table_name(app: &str) -> String;                                   // "egress_" + app with non-[a-z0-9_] → '_'
pub fn nft_script(app: &str, policy: &Policy, upstreams: &[Ipv4Addr]) -> String;   // the whole `nft -f -` script
pub fn pin_script(app: &str, addrs: &[Ipv4Addr], ttl_secs: u32) -> String;        // "add element inet <t> allow_dns { a timeout Ns, … }\n"
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetElement { pub addr: Ipv4Addr, pub proto: String, pub port: u16, pub packets: u64 }
pub fn parse_set_elements(json: &str) -> Result<Vec<SetElement>>;
```

- [ ] **Step 1: Failing golden tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{effective, EgressOverride, Mode};
    use std::net::Ipv4Addr;

    fn policy(mode: Mode, raw: &[&str]) -> Policy {
        let list: Vec<crate::egress::EgressEntry> = raw.iter().map(|s| s.parse().unwrap()).collect();
        effective(Some(&list), Some(&EgressOverride { mode: Some(mode), allow: None }))
    }
    const UP: [Ipv4Addr; 2] = [Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(1, 0, 0, 1)];

    #[test]
    fn the_enforce_table_matches_the_spec_verbatim() {
        let script = nft_script("web", &policy(Mode::Enforce, &["api.stripe.com", "140.82.112.0/20", "1.1.1.1"]), &UP);
        let want = "\
table inet egress_web {
  set allow_static { type ipv4_addr; flags interval; elements = { 140.82.112.0/20, 1.1.1.1 } }
  set allow_dns { type ipv4_addr; flags timeout; }
  set upstream { type ipv4_addr; elements = { 8.8.8.8, 1.0.0.1 } }
  set seen { type ipv4_addr . inet_proto . inet_service; flags dynamic,timeout; timeout 24h; counter; }
  set blocked { type ipv4_addr . inet_proto . inet_service; flags dynamic,timeout; timeout 24h; counter; }
  chain output {
    type filter hook output priority filter; policy accept;
    oif \"lo\" accept
    ip daddr 10.77.0.0/16 accept
    ct state established,related accept
    ip daddr @upstream meta l4proto { tcp, udp } th dport 53 accept
    meta nfproto ipv6 drop
    ct state new update @seen { ip daddr . meta l4proto . th dport }
    ip daddr @allow_static accept
    ip daddr @allow_dns accept
    ct state new update @blocked { ip daddr . meta l4proto . th dport } drop
  }
}
";
        assert_eq!(script, want);
    }

    #[test]
    fn audit_accepts_where_enforce_drops_and_still_records_blocked() {
        let script = nft_script("web", &policy(Mode::Audit, &["api.stripe.com"]), &UP);
        assert!(script.contains("meta nfproto ipv6 accept\n"));
        assert!(script.ends_with("    ct state new update @blocked { ip daddr . meta l4proto . th dport } accept\n  }\n}\n"), "{script}");
        assert!(!script.contains(" drop"));
    }

    #[test]
    fn no_static_entries_renders_an_empty_interval_set_and_any_renders_the_whole_internet() {
        let script = nft_script("web", &policy(Mode::Enforce, &["api.stripe.com"]), &UP);
        assert!(script.contains("  set allow_static { type ipv4_addr; flags interval; }\n"), "{script}");
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

    #[test]
    fn set_elements_parse_from_nft_json_concats_and_plain_values() {
        let json = r#"{"nftables": [{"metainfo": {"version": "1.0.9", "release_name": "Old Doc Yak #3", "json_schema_version": 1}}, {"set": {"family": "inet", "name": "seen", "table": "egress_web", "type": ["ipv4_addr", "inet_proto", "inet_service"], "handle": 4, "flags": ["dynamic", "timeout"], "timeout": 86400, "elem": [{"elem": {"val": {"concat": ["54.187.174.169", "tcp", 443]}, "timeout": 86400, "expires": 86390, "counter": {"packets": 12, "bytes": 720}}}, {"elem": {"val": {"concat": ["203.0.113.9", "udp", 8443]}, "timeout": 86400, "expires": 100, "counter": {"packets": 1, "bytes": 60}}}]}}]}"#;
        let got = parse_set_elements(json).unwrap();
        assert_eq!(got, vec![
            SetElement { addr: Ipv4Addr::new(54, 187, 174, 169), proto: "tcp".into(), port: 443, packets: 12 },
            SetElement { addr: Ipv4Addr::new(203, 0, 113, 9), proto: "udp".into(), port: 8443, packets: 1 },
        ]);
        // an empty set has no "elem" key at all
        let empty = r#"{"nftables": [{"metainfo": {"version": "1.0.9", "release_name": "x", "json_schema_version": 1}}, {"set": {"family": "inet", "name": "seen", "table": "egress_web", "type": ["ipv4_addr", "inet_proto", "inet_service"], "handle": 4, "flags": ["dynamic", "timeout"], "timeout": 86400}}]}"#;
        assert!(parse_set_elements(empty).unwrap().is_empty());
        // a protocol may come back as a number on some builds
        let numeric = r#"{"nftables": [{"set": {"family": "inet", "name": "seen", "table": "t", "type": ["ipv4_addr", "inet_proto", "inet_service"], "elem": [{"elem": {"val": {"concat": ["1.2.3.4", 6, 80]}, "counter": {"packets": 2, "bytes": 1}}}]}}]}"#;
        assert_eq!(parse_set_elements(numeric).unwrap()[0].proto, "tcp");
    }
}
```

- [ ] **Step 2: Run to verify they fail.** `cargo test -p ply-core egress::nft::`

- [ ] **Step 3: Implement**

```rust
//! The per-instance nftables table: rendered as text for `nft -f -`, pins
//! for `allow_dns`, and the JSON reader for the two audit sets. Pure: the
//! Linux thread (`runtime/ns/egress.rs`) is the only thing that runs nft.

use std::net::Ipv4Addr;

use crate::egress::{EgressEntry, Mode, Policy};
use crate::error::{Error, Result};

pub fn table_name(app: &str) -> String {
    let safe: String = app
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
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
        format!("  set {name} {{ type ipv4_addr; flags {flags}; elements = {{ {} }} }}\n", elements.join(", "))
    }
}

pub fn nft_script(app: &str, policy: &Policy, upstreams: &[Ipv4Addr]) -> String {
    let verdict = match policy.mode {
        Mode::Enforce => "drop",
        Mode::Audit | Mode::Off => "accept",
    };
    let table = table_name(app);
    let ups: Vec<String> = upstreams.iter().map(|u| u.to_string()).collect();
    let mut s = format!("table inet {table} {{\n");
    s.push_str(&set_line("allow_static", "interval", &static_elements(policy)));
    s.push_str("  set allow_dns { type ipv4_addr; flags timeout; }\n");
    if ups.is_empty() {
        s.push_str("  set upstream { type ipv4_addr; }\n");
    } else {
        s.push_str(&format!("  set upstream {{ type ipv4_addr; elements = {{ {} }} }}\n", ups.join(", ")));
    }
    for name in ["seen", "blocked"] {
        s.push_str(&format!("  set {name} {{ type ipv4_addr . inet_proto . inet_service; flags dynamic,timeout; timeout 24h; counter; }}\n"));
    }
    s.push_str("  chain output {\n");
    s.push_str("    type filter hook output priority filter; policy accept;\n");
    s.push_str("    oif \"lo\" accept\n");
    s.push_str("    ip daddr 10.77.0.0/16 accept\n");
    s.push_str("    ct state established,related accept\n");
    s.push_str("    ip daddr @upstream meta l4proto { tcp, udp } th dport 53 accept\n");
    s.push_str(&format!("    meta nfproto ipv6 {verdict}\n"));
    s.push_str("    ct state new update @seen { ip daddr . meta l4proto . th dport }\n");
    s.push_str("    ip daddr @allow_static accept\n");
    s.push_str("    ip daddr @allow_dns accept\n");
    s.push_str(&format!("    ct state new update @blocked {{ ip daddr . meta l4proto . th dport }} {verdict}\n"));
    s.push_str("  }\n}\n");
    s
}

pub fn pin_script(app: &str, addrs: &[Ipv4Addr], ttl_secs: u32) -> String {
    let elems: Vec<String> = addrs.iter().map(|a| format!("{a} timeout {ttl_secs}s")).collect();
    format!("add element inet {} allow_dns {{ {} }}\n", table_name(app), elems.join(", "))
}

```

```rust
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
    let root: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Runtime(format!("nft -j: {e}")))?;
    let mut out = Vec::new();
    let items = root.get("nftables").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for item in items {
        let Some(set) = item.get("set") else { continue };
        let Some(elems) = set.get("elem").and_then(|v| v.as_array()) else { continue };
        for e in elems {
            let inner = e.get("elem").unwrap_or(e);
            let val = inner.get("val").unwrap_or(inner);
            let parts = match val.get("concat").and_then(|c| c.as_array()) {
                Some(parts) => parts.clone(),
                None => vec![val.clone()],
            };
            let addr = parts.first().and_then(|v| v.as_str()).and_then(|s| s.parse::<Ipv4Addr>().ok());
            let proto = parts.get(1).and_then(proto_name);
            let port = parts.get(2).and_then(|v| v.as_u64()).and_then(|p| u16::try_from(p).ok());
            let packets = inner.get("counter").and_then(|c| c.get("packets")).and_then(|p| p.as_u64()).unwrap_or(0);
            if let (Some(addr), Some(proto), Some(port)) = (addr, proto, port) {
                out.push(SetElement { addr, proto, port, packets });
            }
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Tests pass; `make check`.** Report. (The owner validates the rendered script's syntax against a real kernel: `cargo test` writes nothing, so add a `#[test] #[ignore]` named `write_sample_script_for_nft_check` that writes `nft_script("sample", …)` to `target/egress-sample.nft`; the owner runs `cargo test -p ply-core write_sample_script -- --ignored && sudo nft -c -f target/egress-sample.nft`.)

---

### Task 5: `egress::dns` — the forwarder's wire handling

**Files:**
- Modify: `ply-core/src/egress/dns.rs`

**Interfaces:**
- Consumes: nothing beyond std.
- Produces:

```rust
pub struct Question { pub name: String, pub qtype: u16 }          // name lowercase, no trailing dot
pub fn question(msg: &[u8]) -> Option<Question>;                   // first question of a DNS message
pub fn a_records(msg: &[u8]) -> Vec<(Ipv4Addr, u32)>;              // (addr, ttl) of every A record in the answer section, compression-aware
pub fn refused_reply(query: &[u8]) -> Vec<u8>;                     // header copied (QR=1, RCODE=5, AN/NS/AR=0), question section copied
pub fn tcp_frame(msg: &[u8]) -> Vec<u8>;                           // 2-byte big-endian length + msg
pub fn tcp_unframe(buf: &[u8]) -> Option<(&[u8], &[u8])>;          // (message, rest) once a whole frame is present
pub fn forward_once_to(query: &[u8], upstreams: &[(Ipv4Addr, u16)], timeout: Duration) -> std::io::Result<Vec<u8>>;  // UDP to each (addr, port) in turn; first reply with a matching ID wins
pub fn forward_once(query: &[u8], upstreams: &[Ipv4Addr], timeout: Duration) -> std::io::Result<Vec<u8>>;  // the same with port 53
pub const A: u16 = 1; pub const AAAA: u16 = 28;
```

- [ ] **Step 1: Failing tests** — build packets by hand:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn name_bytes(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for label in name.split('.') {
            out.push(label.len() as u8);
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out
    }
    fn query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
        let mut m = vec![(id >> 8) as u8, id as u8, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        m.extend(name_bytes(name));
        m.extend_from_slice(&qtype.to_be_bytes());
        m.extend_from_slice(&[0, 1]);
        m
    }
    fn answer_with_a(name: &str, records: &[(Ipv4Addr, u32)]) -> Vec<u8> {
        let mut m = query(0xbeef, name, A);
        m[2] = 0x81; m[3] = 0x80;                       // QR, RD, RA
        m[7] = records.len() as u8;                     // ANCOUNT
        for (addr, ttl) in records {
            m.extend_from_slice(&[0xc0, 0x0c]);         // pointer to the question name
            m.extend_from_slice(&[0, 1, 0, 1]);          // A, IN
            m.extend_from_slice(&ttl.to_be_bytes());
            m.extend_from_slice(&[0, 4]);
            m.extend_from_slice(&addr.octets());
        }
        m
    }

    #[test]
    fn the_question_is_read_lowercase_without_the_dot() {
        let q = question(&query(7, "API.Stripe.COM", A)).unwrap();
        assert_eq!(q.name, "api.stripe.com");
        assert_eq!(q.qtype, A);
        assert!(question(&[0, 1, 2]).is_none());
    }

    #[test]
    fn a_records_follow_compression_pointers_and_keep_ttls() {
        let msg = answer_with_a("api.stripe.com", &[(Ipv4Addr::new(54, 187, 174, 169), 60), (Ipv4Addr::new(54, 187, 174, 170), 3600)]);
        assert_eq!(a_records(&msg), vec![(Ipv4Addr::new(54, 187, 174, 169), 60), (Ipv4Addr::new(54, 187, 174, 170), 3600)]);
        assert!(a_records(&query(1, "x.example", A)).is_empty());
    }

    #[test]
    fn a_refused_reply_keeps_the_id_and_the_question_and_says_refused() {
        let q = query(0x1234, "evil.example", A);
        let r = refused_reply(&q);
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80, "QR");
        assert_eq!(r[3] & 0x0f, 5, "RCODE REFUSED");
        assert_eq!(&r[4..12], &[0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&r[12..], &q[12..]);
        assert_eq!(question(&r).unwrap().name, "evil.example");
    }

    #[test]
    fn tcp_framing_round_trips_and_waits_for_a_whole_frame() {
        let msg = query(1, "a.example", A);
        let framed = tcp_frame(&msg);
        assert_eq!(&framed[..2], &(msg.len() as u16).to_be_bytes());
        let (got, rest) = tcp_unframe(&framed).unwrap();
        assert_eq!(got, &msg[..]);
        assert!(rest.is_empty());
        assert!(tcp_unframe(&framed[..framed.len() - 1]).is_none());
    }

    #[test]
    fn forward_once_talks_to_the_first_upstream_that_answers() {
        // a fake upstream on loopback that echoes the query with QR set
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            let (n, from) = sock.recv_from(&mut buf).unwrap();
            buf[2] |= 0x80;
            sock.send_to(&buf[..n], from).unwrap();
        });
        let q = query(9, "a.example", A);
        let reply = forward_once_to(&q, &[("127.0.0.1".parse().unwrap(), port)], std::time::Duration::from_secs(2)).unwrap();
        assert_eq!(reply[2] & 0x80, 0x80);
        assert_eq!(&reply[..2], &q[..2]);
    }
}
```

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement** — a 12-byte header; `read_name(msg, offset) -> Option<(String, usize)>` that follows `0xC0` pointers (bounded to 64 hops, never past `msg.len()`); `question` reads the first QNAME + QTYPE; `a_records` walks QDCOUNT questions, then ANCOUNT records, skipping non-A types by RDLENGTH; `refused_reply` copies the header, sets `QR` and `RCODE=5`, zeroes AN/NS/AR counts, copies the question section (from offset 12 to the end of the first question); `forward_once_to` binds a UDP socket to `0.0.0.0:0`, sets `read_timeout`, sends to each upstream in turn, returns the first reply whose ID matches, else the last error. No `unwrap` on packet bytes anywhere: every read is bounds-checked and returns `None`/empty.

- [ ] **Step 4: Tests pass; `make check`; `make check-darwin`.** Report.

---

### Task 6: `egress::log` and `ply egress`

**Files:**
- Modify: `ply-core/src/egress/log.rs`
- Create: `ply-cli/src/commands/egress.rs`
- Modify: `ply-cli/src/cli.rs` (`Command::Egress(EgressArgs)` after `Logs`; `EgressArgs`), `ply-cli/src/commands/mod.rs` (`mod egress;` + dispatch)

**Interfaces:**
- Consumes: `crate::paths::data_dir()`, `serde_json`.
- Produces:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Record {
    Resolved { t: String, app: String, n: u32, name: String, declared: bool, addrs: Vec<Ipv4Addr>, ttl: u32 },
    Refused  { t: String, app: String, n: u32, name: String, declared: bool },
    Allowed  { t: String, app: String, n: u32, proto: String, dst: Ipv4Addr, port: u16, name: Option<String>, count: u64 },
    Blocked  { t: String, app: String, n: u32, proto: String, dst: Ipv4Addr, port: u16, name: Option<String>, count: u64 },
}
pub fn dir() -> PathBuf;                       // data_dir()/egress
pub fn path(app: &str, n: u32) -> PathBuf;     // dir()/<app>.<n>.log
pub struct Writer { … }                        // Writer::open(app, n) appends; rotates at CAP_BYTES (512 KiB) to `.1`
impl Writer { pub fn open(app: &str, n: u32) -> Result<Writer>; pub fn open_at(path: PathBuf) -> Result<Writer>; pub fn write(&mut self, r: &Record); }
pub fn read_app(app: &str) -> Vec<Record>;     // every instance's `.1` then live file, in order, bad lines skipped
pub fn now_rfc3339() -> String;                // "2026-09-04T21:03:11Z"
// cli
pub struct Row { dst, name, port, proto, connections, first, last, verdict }
pub fn render_table(records: &[Record], blocked_only: bool) -> String;
```

- [ ] **Step 1: Failing tests**

```rust
// log.rs tests — the writer is tested through `Writer::open_at(path)`; `open(app, n)` is `open_at(path(app, n))`.
#[test]
fn records_serialize_with_the_spec_shape_and_round_trip() {
    let r = Record::Allowed { t: "2026-09-04T21:03:11Z".into(), app: "web".into(), n: 1, proto: "tcp".into(), dst: "54.187.174.169".parse().unwrap(), port: 443, name: Some("api.stripe.com".into()), count: 12 };
    let json = serde_json::to_string(&r).unwrap();
    assert_eq!(json, r#"{"kind":"allowed","t":"2026-09-04T21:03:11Z","app":"web","n":1,"proto":"tcp","dst":"54.187.174.169","port":443,"name":"api.stripe.com","count":12}"#);
    assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), r);
}

#[test]
fn the_writer_appends_lines_and_rotates_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("web.1.log");
    let mut w = Writer::open_at(path.clone()).unwrap();
    let r = Record::Refused { t: "t".into(), app: "web".into(), n: 1, name: "x.example".into(), declared: false };
    for _ in 0..3 { w.write(&r); }
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
    // force rotation
    let big = Record::Refused { t: "t".into(), app: "web".into(), n: 1, name: "y".repeat(CAP_BYTES as usize).into(), declared: false };
    w.write(&big);
    w.write(&r);
    assert!(path.with_extension("log.1").exists());
    assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
}
```

```rust
// commands/egress.rs tests
#[test]
fn the_table_groups_by_destination_and_marks_verdicts() {
    let recs = vec![
        Record::Resolved { t: "2026-09-04T21:00:00Z".into(), app: "web".into(), n: 1, name: "api.stripe.com".into(), declared: true, addrs: vec!["54.187.174.169".parse().unwrap()], ttl: 60 },
        Record::Allowed { t: "2026-09-04T21:00:01Z".into(), app: "web".into(), n: 1, proto: "tcp".into(), dst: "54.187.174.169".parse().unwrap(), port: 443, name: Some("api.stripe.com".into()), count: 3 },
        Record::Allowed { t: "2026-09-04T21:05:00Z".into(), app: "web".into(), n: 1, proto: "tcp".into(), dst: "54.187.174.169".parse().unwrap(), port: 443, name: Some("api.stripe.com".into()), count: 12 },
        Record::Blocked { t: "2026-09-04T21:06:00Z".into(), app: "web".into(), n: 1, proto: "tcp".into(), dst: "203.0.113.9".parse().unwrap(), port: 8443, name: None, count: 1 },
        Record::Refused { t: "2026-09-04T21:07:00Z".into(), app: "web".into(), n: 1, name: "evil.example".into(), declared: false },
    ];
    let out = render_table(&recs, false);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("DESTINATION"), "{out}");
    assert!(out.contains("54.187.174.169  api.stripe.com  443  tcp  12  2026-09-04T21:00:01Z  2026-09-04T21:05:00Z  allowed"), "{out}");
    assert!(out.contains("203.0.113.9") && out.contains("blocked"), "{out}");
    assert!(out.contains("evil.example") && out.contains("refused"), "{out}");
    let only = render_table(&recs, true);
    assert!(!only.contains("api.stripe.com"), "{only}");
    assert!(only.contains("203.0.113.9") && only.contains("evil.example"));
}
```

(Column alignment: pad each column to its widest value with two spaces between; the assertion above uses the exact widths those values produce — compute them and keep the assertion literal.)

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement** `log.rs` (Writer with `open`/`open_at`, `write` serializing one line + `\n`, rotation exactly like `logring::RingWriter::append`; `read_app` lists `dir()` for `<app>.<n>.log(.1)`, parses lines with `serde_json::from_str`, skipping failures; `now_rfc3339` from `SystemTime` with this civil-date conversion — there is no crate for it in the workspace and none is added):

```rust
/// `2026-09-04T21:03:11Z` for a unix timestamp (Howard Hinnant's
/// days-to-civil algorithm; valid for every date the epoch can hold).
pub fn rfc3339(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, (secs % 3600) / 60, secs % 60)
}
pub fn now_rfc3339() -> String {
    let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    rfc3339(unix)
}
```

with the test `assert_eq!(rfc3339(1_788_000_000), "2026-08-29T10:40:00Z"); assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");` (verified: `date -u -d @1788000000` prints that).

`cli.rs`:

```rust
    /// What an app's instances reached: the egress audit log as a table
    Egress(EgressArgs),
…
#[derive(Args)]
pub struct EgressArgs {
    /// App whose instances to show
    #[arg(value_name = "APP")]
    pub app: String,
    /// Keep printing new records as they arrive
    #[arg(short = 'f', long)]
    pub follow: bool,
    /// Only blocked connections and refused names
    #[arg(long)]
    pub blocked: bool,
    /// Raw JSON records instead of the table
    #[arg(long)]
    pub json: bool,
}
```

`commands/egress.rs`: `exec(args)` → `read_app`; `--json` prints records one per line; otherwise `render_table`; `--follow` re-reads every second and prints only records not yet printed (track the count per file) — plain polling, no inotify. `render_table` groups `allowed`/`blocked` by `(dst, port, proto)` taking the max `count` and first/last `t`; `refused` rows show the name in DESTINATION with `-` for port/proto and `1` per record; `resolved` records only contribute names to rows that lack one. Header: `DESTINATION  NAME  PORT  PROTO  CONNECTIONS  FIRST  LAST  VERDICT`.

- [ ] **Step 4: Tests pass; `make check`; `make check-darwin`** (the command is portable: it reads files). Report.

---

### Task 7: The thread — `runtime/ns/egress.rs`, wired into the Linux backend

**Files:**
- Create: `ply-core/src/runtime/ns/egress.rs`
- Modify: `ply-core/src/runtime/ns/mod.rs` (`pub mod egress;`; `launch()` between the clone at `:289` and the release at `:353-355`; `NsInstance` gains `_egress: Option<egress::EgressHandle>`), `ply-core/src/runtime/ns/container.rs` (`:139-148` resolv block; add `pub fn upstream_resolvers(rootless: bool) -> Vec<Ipv4Addr>`; `ContainerSpec` gains `pub egress: bool`)

**Interfaces:**
- Consumes: Tasks 1, 4, 5, 6; `netns::{open_ns, enter}`; `events::emit`; `InstanceSpec.egress`.
- Produces:

```rust
pub struct EgressHandle { stop: std::sync::mpsc::Sender<()>, thread: Option<std::thread::JoinHandle<()>> }   // Drop: send stop, join with a 3 s cap
pub fn spawn(app: &str, n: u32, child_pid: i32, policy: &Policy, upstreams: Vec<Ipv4Addr>) -> Result<EgressHandle>;
// container.rs
pub fn upstream_resolvers(rootless: bool) -> Vec<Ipv4Addr>;   // the nameservers `resolv_conf_for_instance` would write, parsed; empty if it would warn
```

- [ ] **Step 1: Failing unit tests** (only the pure parts are testable here):

```rust
// container.rs tests, next to the resolv tests
#[test]
fn upstream_resolvers_are_the_nameservers_the_instance_would_get() {
    let (text, _) = resolv_conf_for_instance("nameserver 127.0.0.53\n", Some("nameserver 8.8.8.8\nnameserver 1.0.0.1\n"), false);
    assert_eq!(upstreams_in(&text), vec!["8.8.8.8".parse::<std::net::Ipv4Addr>().unwrap(), "1.0.0.1".parse().unwrap()]);
    let (text, _) = resolv_conf_for_instance("nameserver 9.9.9.9\nsearch lan\n", None, false);
    assert_eq!(upstreams_in(&text), vec!["9.9.9.9".parse::<std::net::Ipv4Addr>().unwrap()]);
}
```

(`upstreams_in(text) -> Vec<Ipv4Addr>` is the pure helper `upstream_resolvers` wraps around the two file reads.)

```rust
// ns/egress.rs tests
#[test]
fn the_event_throttle_fires_once_per_destination_per_hour() {
    let mut t = Throttle::new(std::time::Duration::from_secs(3600));
    let key = ("203.0.113.9".parse::<std::net::Ipv4Addr>().unwrap(), 8443u16, "tcp".to_string());
    let now = std::time::Instant::now();
    assert!(t.allow(&key, now));
    assert!(!t.allow(&key, now + std::time::Duration::from_secs(10)));
    assert!(t.allow(&key, now + std::time::Duration::from_secs(3601)));
}

#[test]
fn connection_records_are_emitted_on_first_sight_and_on_growth_at_most_once_a_minute() {
    let mut seen = SeenState::default();
    let e = crate::egress::nft::SetElement { addr: "1.2.3.4".parse().unwrap(), proto: "tcp".into(), port: 443, packets: 1 };
    let t0 = std::time::Instant::now();
    assert!(seen.should_record(&e, t0));                                        // first sight
    assert!(!seen.should_record(&e, t0 + std::time::Duration::from_secs(5)));   // no growth
    let grown = crate::egress::nft::SetElement { packets: 3, ..e.clone() };
    assert!(!seen.should_record(&grown, t0 + std::time::Duration::from_secs(30)));  // growth, but inside the minute
    assert!(seen.should_record(&grown, t0 + std::time::Duration::from_secs(61)));   // growth, minute passed
}

#[test]
fn pin_ttl_has_a_floor_of_five_minutes() {
    assert_eq!(pin_ttl(&[(std::net::Ipv4Addr::new(1, 1, 1, 1), 60)]), 300);
    assert_eq!(pin_ttl(&[(std::net::Ipv4Addr::new(1, 1, 1, 1), 3600), (std::net::Ipv4Addr::new(1, 1, 1, 2), 900)]), 900);
}
```

- [ ] **Step 2: Run to verify they fail.**

- [ ] **Step 3: Implement the thread**

```rust
//! The per-instance egress thread: lives in the instance's network
//! namespace, serves DNS on 127.0.0.53, owns the `egress_<app>` table,
//! pins what the app resolves, polls what it connected to, writes the
//! audit log and emits events. Started before the child is released;
//! ends when its handle drops.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::egress::{dns, log, nft, Mode, Policy};
use crate::error::{Error, Result};
use crate::runtime::ns::netns;

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

pub(crate) struct Throttle { every: Duration, last: HashMap<(Ipv4Addr, u16, String), Instant> }
impl Throttle {
    pub(crate) fn new(every: Duration) -> Self { Throttle { every, last: HashMap::new() } }
    pub(crate) fn allow(&mut self, key: &(Ipv4Addr, u16, String), now: Instant) -> bool {
        match self.last.get(key) {
            Some(t) if now.duration_since(*t) < self.every => false,
            _ => { self.last.insert(key.clone(), now); true }
        }
    }
}

#[derive(Default)]
pub(crate) struct SeenState { last: HashMap<(Ipv4Addr, u16, String), (u64, Instant)> }
impl SeenState {
    /// Record on first sight, then on growth at most once a minute.
    pub(crate) fn should_record(&mut self, e: &nft::SetElement, now: Instant) -> bool {
        let key = (e.addr, e.port, e.proto.clone());
        match self.last.get(&key) {
            None => { self.last.insert(key, (e.packets, now)); true }
            Some((packets, at)) => {
                if e.packets > *packets && now.duration_since(*at) >= Duration::from_secs(60) {
                    self.last.insert(key, (e.packets, now));
                    true
                } else { false }
            }
        }
    }
}

pub(crate) fn pin_ttl(records: &[(Ipv4Addr, u32)]) -> u32 {
    records.iter().map(|(_, ttl)| *ttl).min().unwrap_or(300).max(300)
}

fn run_nft(script: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("nft")
        .args(["-f", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Runtime(format!("egress policy: nft: {e}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| Error::Runtime(format!("egress policy: nft: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Error::Runtime(format!("egress policy: nft: {}", String::from_utf8_lossy(&out.stderr).trim())))
    }
}

fn list_set(app: &str, set: &str) -> Result<Vec<nft::SetElement>> {
    let out = std::process::Command::new("nft")
        .args(["-j", "list", "set", "inet", &nft::table_name(app), set])
        .output()
        .map_err(|e| Error::Runtime(format!("egress policy: nft -j: {e}")))?;
    nft::parse_set_elements(&String::from_utf8_lossy(&out.stdout))
}

/// Start the thread for `app.n` whose child is `child_pid`. Returns once
/// the forwarder listens and the table is installed, or with the reason it
/// could not; the caller decides what a failure means for its mode.
pub fn spawn(app: &str, n: u32, child_pid: i32, policy: &Policy, upstreams: Vec<Ipv4Addr>) -> Result<EgressHandle> {
    if !crate::runtime::ns::network::has_nft() {
        return Err(Error::Runtime("egress policy: nft not found — install nftables or run with --egress audit".into()));
    }
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
    let app = app.to_string();
    let policy = policy.clone();
    let thread = std::thread::Builder::new()
        .name(format!("egress-{app}.{n}"))
        .spawn(move || {
            let setup = (|| -> Result<(std::net::UdpSocket, std::net::TcpListener)> {
                let ns = netns::open_ns(child_pid)?;
                netns::enter(&ns)?;
                let udp = std::net::UdpSocket::bind("127.0.0.53:53")
                    .map_err(|e| Error::Runtime(format!("egress policy: binding 127.0.0.53:53: {e}")))?;
                udp.set_read_timeout(Some(Duration::from_millis(200))).ok();
                let tcp = std::net::TcpListener::bind("127.0.0.53:53")
                    .map_err(|e| Error::Runtime(format!("egress policy: binding 127.0.0.53:53/tcp: {e}")))?;
                tcp.set_nonblocking(true).ok();
                run_nft(&nft::nft_script(&app, &policy, &upstreams))?;
                Ok((udp, tcp))
            })();
            let (udp, tcp) = match setup {
                Ok(s) => { let _ = ready_tx.send(Ok(())); s }
                Err(e) => { let _ = ready_tx.send(Err(e)); return; }
            };
            serve(&app, n, &policy, &upstreams, udp, tcp, stop_rx);
        })
        .map_err(|e| Error::Runtime(format!("egress policy: thread: {e}")))?;
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(EgressHandle { stop: stop_tx, thread: Some(thread) }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(Error::Runtime("egress policy: the forwarder did not start within 5s".into())),
    }
}
```

`serve` (same file): a loop until `stop_rx.try_recv()` says stop:

1. `udp.recv_from` (200 ms timeout): on a datagram, `decide(&policy, question)`; `Refuse` → `dns::refused_reply` back + `Record::Refused`; `Forward` → `dns::forward_once(query, upstreams, 2 s)`; on a reply with A records and a declared name (or any name in `audit`? NO: pin only DECLARED names in both modes — in audit the chain accepts anyway, and pinning undeclared names would mislabel them `allowed` after a flip to enforce) → `run_nft(&nft::pin_script(&app, &addrs, pin_ttl(&records)))`, remember `addr → name` for one hour, `Record::Resolved { declared }`; send the reply back. Upstream failure → `SERVFAIL` (RCODE 2) reply built like `refused_reply` with code 2.
2. `tcp.accept()` non-blocking: on a connection, read one frame (`dns::tcp_unframe`), same decision, `dns::tcp_frame` the reply, close.
3. Every 2 s: `list_set("seen")` and `list_set("blocked")`; for each element passing `SeenState::should_record`, write `Record::Allowed`/`Record::Blocked` with `name` from the addr map; for a `blocked` element passing the hour `Throttle`, `events::emit(app, "egress-blocked", &format!("{proto} {addr}:{port}"))`; in `audit` mode a `blocked`-set element is written as `Blocked` too (it would have been) and emits `egress-undeclared` under the same throttle.
4. `log::Writer::open(app, n)` once at the top; every record gets `t: log::now_rfc3339()`.

`decide`: `Any` → Forward, declared; name matches → Forward, declared; else `enforce` → Refuse, `audit` → Forward, undeclared. AAAA queries follow the same rule (declared names are forwarded; the chain drops v6 in enforce).

`network.rs`: add `pub fn has_nft() -> bool { has("nft") }` (the private `has` exists at `:92`).

- [ ] **Step 4: Wire the backend**

`ns/mod.rs` `launch`, after `prepared` succeeds (cgroup/veth done, `ip` known) and BEFORE `record` — the table must exist before the child runs but the thread needs only the pid:

```rust
        let egress = match &spec.egress {
            None => None,
            Some(policy) => {
                let upstreams = crate::runtime::ns::container::upstream_resolvers(rootless);
                match egress::spawn(&spec.app, spec.n, child.as_raw(), policy, upstreams) {
                    Ok(handle) => Some(handle),
                    Err(e) if policy.mode == crate::egress::Mode::Enforce => {
                        drop(sync_tx);
                        let _ = signal::kill(child, Signal::SIGKILL);
                        let _ = waitpid(child, None);
                        return Err(e);
                    }
                    Err(e) => {
                        eprintln!("ply: warning: {e} — running unobserved");
                        None
                    }
                }
            }
        };
```

(`sync_tx`'s drop must happen before the kill so the parked child does not race; mirror the existing unwind at `:335-345`.) `NsInstance` gets `_egress: egress` so the thread stops when the instance drops — declare the field BEFORE `_cgroup` so it stops first. `ContainerSpec.egress = spec.egress.is_some()`; in `container.rs:141`:

```rust
    let (resolv, warning) = if spec.egress {
        (resolv_conf_via(&host_resolv, "127.0.0.53"), None)
    } else {
        match &spec.dns { … as today … }
    };
```

`upstream_resolvers(rootless)`: read the two files exactly as `:139-140` does, call `resolv_conf_for_instance`, return `upstreams_in(&text)`.

- [ ] **Step 5: Tests pass; `make check`; `make check-darwin`** (the new file is under `ns/`, Linux-only; `container.rs` too). Report — and state plainly that the thread is untested on this box.

---

### Task 8: Docs, the kegs' claims, and the droplet checklist

**Files:**
- Modify: `docs/security.md` (new section `## Egress: the contract` before `## Supply-chain posture`), `docs/manifest.md` (a `[network]` block in the example at the top and a paragraph under `## Key notes`), `docs/stacks.md` (`egress` in `## Members`), `docs/cli.md` (`ply egress` under `## Run & observe`; `--egress`/`--egress-allow` in the `ply run` entry), `docs/ply-vs-docker.md` (a row: `Outbound policy | Falco/Cilium/vendor add-on | declared in the manifest, enforced per instance, audited to a file`)
- Modify: `services/postgres/ply.toml` (after `[volumes]`), `services/redis/runnable.toml` (after `[ports]`): 

```toml
# A database talks to nobody: an empty list is a claim, and a stack that
# runs this keg logs anything it reaches as undeclared.
[network]
egress = []
```

- Modify: `TASKS.md` — append `## Phase 16 — Egress contract` with the droplet checklist below.

- [ ] **Step 1: Write the docs** — each section states the model (claim, policy, enforcement, audit), the entry forms table, the effective-policy table, the always-allowed list, the three modes with their defaults, `ply egress`'s columns, the events, and the limits verbatim from the spec's "Non-goals" and the rootless warning.

- [ ] **Step 2: The droplet checklist** in `TASKS.md`:

```
## Phase 16 — Egress contract ✅-gate: the droplet run below, byte-for-byte
Spec docs/superpowers/specs/2026-09-04-egress-contract-design.md; plan docs/superpowers/plans/2026-09-04-egress-contract.md.
- [ ] 1. Syntax check of the rendered table on a real kernel: `cargo test -p ply-core write_sample_script -- --ignored && sudo nft -c -f target/egress-sample.nft` (here, before pushing).
- [ ] 2. Release, `ply self-update` on the droplet, `nft --version` ≥ 1.0.
- [ ] 3. A test stack: `web` = a debian keg with `entrypoint = ["sleep","infinity"]`, `egress = { mode = "enforce", allow = ["registry.plybox.sh"] }`; `db` = postgres@17 with no override.
      `ply up` → `ply: egress enforce, 1 entry (override)` for web; db prints `ply: egress audit, 0 entries (manifest)` once the re-pushed keg declares `[]`.
- [ ] 4. Inside web (`ply exec web sh`): `curl -sS https://registry.plybox.sh/ >/dev/null && echo OK` → OK;
      `curl -sS https://example.com/` → resolution refused, fast; `curl -m 5 http://93.184.216.34/` → times out.
- [ ] 5. `ply egress web` shows registry.plybox.sh allowed, example.com refused, 93.184.216.34 blocked; `ply egress web --blocked` shows the last two; `ply events` has one `egress-blocked`.
- [ ] 6. `nft list tables` on the HOST is unchanged before, during and after; inside web's namespace (`nsenter -t <pid> -n nft list table inet egress_web`) the table exists while it runs and is gone after `ply stop`.
- [ ] 7. `ply restart web` keeps the log (`.1` after rotation), `ply up --plan` prints the egress lines, `ply inspect postgres@17` prints `egress: none (declared)`.
- [ ] 8. Rootless on the laptop: the same stack prints the rootless warning per member and runs as before.
- [ ] 9. Re-push postgres and redis with `[network] egress = []` (bump versions).
```

- [ ] **Step 3: `make check`** (docs do not affect it, but the keg manifests are parsed by `ply-core` tests? `services/` is not — confirm nothing reads them in tests). Report.

---

## Self-review

- **Spec coverage:** entries and grammar (T1); manifest claim + inspect + check (T2); stack override, `ply run` flags, effective policy in the supervisor, rootless warning, unrestricted warning, start line, `--plan` line, `InstanceSpec.egress` (T3); table, pins, set reading (T4); forwarder wire handling, `REFUSED`, TCP, upstream forwarding (T5); log records, writer, reader, `ply egress` with `--follow/--blocked/--json`, table (T6); the thread (setns, bind, install, ready-before-release, serve, pin-declared-only, poll, throttled events, failure semantics by mode), resolv.conf, upstreams, instance drop (T7); docs, keg claims, droplet gate (T8). Not covered by a task on purpose: the macOS switch (out of scope) and host policy defaults (follow-up).
- **Placeholders:** none; the `{ … as today … }` in T7 Step 4 names the existing `match &spec.dns` at `container.rs:141-144`, kept verbatim.
- **Type consistency:** `EgressEntry`, `Mode`, `Policy`, `PolicySource`, `EgressOverride`, `effective`, `Policy::{unrestricted, allows_name, describe}` (T1) are what T2–T7 call; `nft::{table_name, nft_script, pin_script, parse_set_elements, SetElement}` (T4) are what T7 calls with the same argument order; `dns::{question, a_records, refused_reply, tcp_frame, tcp_unframe, forward_once}` (T5) likewise; `log::{Record, Writer::open, now_rfc3339, read_app}` (T6) are used by T7 and the CLI; `InstanceSpec.egress: Option<Policy>` (T3) is read by T7; `AppContext.egress` (T3) feeds it.
