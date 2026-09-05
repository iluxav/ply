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
    Policy {
        mode,
        allow,
        source,
    }
}

impl Policy {
    pub fn unrestricted(&self) -> bool {
        self.allow.contains(&EgressEntry::Any)
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
        format!(
            "{}, {n} {}{source}",
            self.mode,
            if n == 1 { "entry" } else { "entries" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::entry::EgressEntry;

    fn entries(raw: &[&str]) -> Vec<EgressEntry> {
        raw.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn the_effective_policy_table_from_the_spec() {
        let m = entries(&["api.stripe.com"]);
        let over_mode = EgressOverride {
            mode: Some(Mode::Enforce),
            allow: None,
        };
        let over_list = EgressOverride {
            mode: Some(Mode::Audit),
            allow: Some(entries(&["*.stripe.com"])),
        };
        // row 1: nothing anywhere
        assert_eq!(
            effective(None, None),
            Policy {
                mode: Mode::Off,
                allow: vec![],
                source: PolicySource::None
            }
        );
        // row 2: manifest only → audit with the manifest's list
        assert_eq!(
            effective(Some(&m), None),
            Policy {
                mode: Mode::Audit,
                allow: m.clone(),
                source: PolicySource::Manifest
            }
        );
        // row 3: override mode, no manifest → that mode, no list
        assert_eq!(
            effective(None, Some(&over_mode)),
            Policy {
                mode: Mode::Enforce,
                allow: vec![],
                source: PolicySource::None
            }
        );
        // row 4: override mode + manifest → that mode, manifest's list
        assert_eq!(
            effective(Some(&m), Some(&over_mode)),
            Policy {
                mode: Mode::Enforce,
                allow: m.clone(),
                source: PolicySource::Manifest
            }
        );
        // row 5: override list replaces the manifest's
        assert_eq!(
            effective(Some(&m), Some(&over_list)),
            Policy {
                mode: Mode::Audit,
                allow: entries(&["*.stripe.com"]),
                source: PolicySource::Override
            }
        );
        // an override list with no mode keeps the default (audit)
        let list_only = EgressOverride {
            mode: None,
            allow: Some(vec![]),
        };
        assert_eq!(effective(Some(&m), Some(&list_only)).mode, Mode::Audit);
        assert_eq!(
            effective(Some(&m), Some(&list_only)).source,
            PolicySource::Override
        );
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
        let p = effective(
            Some(&entries(&["a.example", "b.example", "1.1.1.1"])),
            Some(&EgressOverride {
                mode: Some(Mode::Enforce),
                allow: None,
            }),
        );
        assert_eq!(p.describe(), "enforce, 3 entries (manifest)");
        assert!(!p.unrestricted());
        let any = effective(Some(&entries(&["*"])), None);
        assert!(any.unrestricted());
        assert!(any.allows_name("whatever.example"));
        assert_eq!(effective(None, None).describe(), "off");
        let over = effective(
            None,
            Some(&EgressOverride {
                mode: Some(Mode::Audit),
                allow: Some(vec![]),
            }),
        );
        assert_eq!(over.describe(), "audit, 0 entries (override)");
    }
}
