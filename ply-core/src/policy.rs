//! Host runtime policy: `/etc/ply/runtimes.toml`.
//!
//! No policy file (laptop) → permissive. With one (fleet) → enforced.
//! `ply check img --against policy.toml` is a pure function usable in CI.
//!
//! ```toml
//! [[runtime]]
//! name = "node"
//! version = "24.6.0"
//! sha256 = "sha256:…"          # optional pin
//! source = "http://…"          # optional, enables `ply sync`
//! status = "default"           # default | supported | deprecated | refused
//! ```

use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::lockfile::Lockfile;

pub const DEFAULT_POLICY_PATH: &str = "/etc/ply/runtimes.toml";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default, rename = "runtime")]
    pub runtimes: Vec<RuntimeEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeEntry {
    pub name: String,
    pub version: Version,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_status() -> String {
    "supported".into()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
}

impl Policy {
    pub fn load(path: &Path) -> Result<Policy> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let policy: Policy = toml::from_str(&text)
            .map_err(|e| Error::Manifest(format!("{}: {e}", path.display())))?;
        for entry in &policy.runtimes {
            if !matches!(
                entry.status.as_str(),
                "default" | "supported" | "deprecated" | "refused"
            ) {
                return Err(Error::Manifest(format!(
                    "{}: runtime {}: status must be default|supported|deprecated|refused, got `{}`",
                    path.display(),
                    entry.name,
                    entry.status
                )));
            }
        }
        Ok(policy)
    }

    /// The host's policy, if it has one.
    pub fn load_default() -> Result<Option<Policy>> {
        let path = Path::new(DEFAULT_POLICY_PATH);
        if path.exists() {
            Ok(Some(Self::load(path)?))
        } else {
            Ok(None)
        }
    }

    /// Pure check of a lockfile against this policy.
    pub fn check_lockfile(&self, lockfile: &Lockfile) -> Vec<Finding> {
        let mut findings = Vec::new();
        for pkg in &lockfile.packages {
            let entry = self
                .runtimes
                .iter()
                .find(|r| r.name == pkg.name && r.version == pkg.version);
            match entry {
                None => {
                    // only enforce packages the policy talks about by name
                    if self.runtimes.iter().any(|r| r.name == pkg.name) {
                        findings.push(Finding {
                            severity: Severity::Error,
                            message: format!(
                                "{} {} is not a version this host supports",
                                pkg.name, pkg.version
                            ),
                        });
                    }
                }
                Some(entry) => {
                    if let Some(expected) = &entry.sha256 {
                        if expected != &pkg.sha256 {
                            findings.push(Finding {
                                severity: Severity::Error,
                                message: format!(
                                    "{} {}: digest mismatch vs policy ({} != {})",
                                    pkg.name, pkg.version, pkg.sha256, expected
                                ),
                            });
                            continue;
                        }
                    }
                    match entry.status.as_str() {
                        "refused" => findings.push(Finding {
                            severity: Severity::Error,
                            message: format!(
                                "{} {} is refused by host policy",
                                pkg.name, pkg.version
                            ),
                        }),
                        "deprecated" => findings.push(Finding {
                            severity: Severity::Warning,
                            message: format!(
                                "{} {} is deprecated on this host — plan a rebase",
                                pkg.name, pkg.version
                            ),
                        }),
                        _ => {}
                    }
                }
            }
        }
        findings
    }
}
