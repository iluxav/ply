//! `ply.toml` — the app manifest. Human intent; the lockfile is machine truth.

use std::collections::BTreeMap;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Parsed, validated `ply.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub package: Package,

    /// name -> version constraint or detailed spec
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, Dependency>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    /// Labels of what the app binds internally — not host claims.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ports: BTreeMap<String, u16>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub volumes: BTreeMap<String, Volume>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<Requires>,

    /// Instance restart policy, honored by the `ply run` parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<Restart>,

    /// Health gate used by rolling deploys (`ply deploy`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<Health>,

    /// Env contributions this package exposes to dependents; embedded into
    /// the built image as `/.layer.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<Layer>,

    /// Where to fetch dependencies from. `default` applies to deps without an
    /// explicit source; other keys are aliases usable as `source = "<alias>"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, String>,
}

/// `[restart]` — the run parent respawns instances it started. Nothing more:
/// if the parent itself dies, that's systemd's layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restart {
    /// "never" | "on-failure" (non-zero exit or signal) | "always"
    pub policy: String,
    /// Initial backoff, doubled per consecutive crash: "2s", "500ms", "1m"
    #[serde(default = "default_backoff")]
    pub backoff: String,
    /// Backoff ceiling
    #[serde(default = "default_max_backoff")]
    pub max_backoff: String,
}

fn default_backoff() -> String {
    "2s".into()
}
fn default_max_backoff() -> String {
    "30s".into()
}

/// `[health]` — when is a fresh instance considered good?
/// With `port`: a TCP connect must succeed within `grace`.
/// Without: the process merely has to survive `grace`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default = "default_grace")]
    pub grace: String,
}

fn default_grace() -> String {
    "10s".into()
}

/// "500ms" | "2s" | "1m" → Duration.
pub fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let bad = || {
        Error::Manifest(format!(
            "invalid duration `{s}` — use e.g. \"500ms\", \"2s\", \"1m\""
        ))
    };
    let (digits, unit): (String, String) = (
        s.chars().take_while(|c| c.is_ascii_digit()).collect(),
        s.chars().skip_while(|c| c.is_ascii_digit()).collect(),
    );
    let n: u64 = digits.parse().map_err(|_| bad())?;
    match unit.as_str() {
        "ms" => Ok(std::time::Duration::from_millis(n)),
        "s" => Ok(std::time::Duration::from_secs(n)),
        "m" => Ok(std::time::Duration::from_secs(n * 60)),
        _ => Err(bad()),
    }
}

/// Package env contributions (`/.layer.toml` inside dep images).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ld_library_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub name: String,
    pub version: Version,
    /// argv of the process to exec, e.g. ["node", "server.js"].
    /// Absent = this is a library/runtime package, not an app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    /// True for the one package per graph that owns `/` (FHS, /bin/sh, libc).
    /// Its files pack at the image root instead of an /opt prefix.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub base: bool,
    /// ABI a runtime package provides, e.g. "linux-x64-musl" (matched
    /// against app [requires] abi).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provides_abi: Option<String>,
    /// Paths (relative to the app dir) that go into the image. When set,
    /// ONLY these ship — like npm's "files" field. Absent = pack everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Isolation seam: "ns" (namespaces, default) or "vm" (microVM, future).
    /// The same composed rootfs enters via pivot_root or virtio-fs.
    #[serde(default = "default_isolation", skip_serializing_if = "is_ns")]
    pub isolation: String,
}

fn default_isolation() -> String {
    "ns".into()
}

fn is_ns(s: &String) -> bool {
    s == "ns"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Dependency {
    /// `node = "22"` or `base = "alpine@3.20"`
    Constraint(String),
    /// `ffmpeg = { source = "github:org/repo", version = "6.1" }`
    Detailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        version: String,
    },
}

/// A dependency, normalized: the manifest key is an alias; the value may name
/// a different package (`base = "alpine@3.20"` → package `alpine`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepSpec {
    pub package: String,
    pub constraint: String,
    pub source: Option<String>,
}

impl Dependency {
    pub fn spec(&self, alias: &str) -> DepSpec {
        let (version, source) = match self {
            Dependency::Constraint(v) => (v.as_str(), None),
            Dependency::Detailed { source, version } => (version.as_str(), source.clone()),
        };
        let (package, constraint) = match version.split_once('@') {
            Some((name, constraint)) => (name.to_string(), constraint.to_string()),
            None => (alias.to_string(), version.to_string()),
        };
        DepSpec {
            package,
            constraint,
            source,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    /// Mount path inside the instance.
    pub path: String,
    /// "instance" (default) or "shared" — shared is an explicit opt-in.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// GC-able cache contract.
    #[serde(default)]
    pub ephemeral: bool,
}

fn default_scope() -> String {
    "instance".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// ABI the app layer's native deps were built against, e.g. "linux-x64-musl".
    pub abi: String,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        let manifest: Manifest =
            toml::from_str(text).map_err(|e| Error::Manifest(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text).map_err(|e| match e {
            Error::Manifest(msg) => Error::Manifest(format!("{}: {msg}", path.display())),
            other => other,
        })
    }

    /// Canonical serialization, embedded into images as `/.manifest.toml`.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|e| Error::Manifest(e.to_string()))
    }

    /// An app has an entrypoint; a package (runtime/library/base) does not.
    pub fn is_app(&self) -> bool {
        self.package.entrypoint.is_some()
    }

    fn validate(&self) -> Result<()> {
        validate_package_name(&self.package.name)?;
        if let Some(entrypoint) = &self.package.entrypoint {
            if entrypoint.is_empty() {
                return Err(Error::Manifest(
                    "package.entrypoint must have at least one element, e.g. [\"node\", \"server.js\"] (or omit it entirely for a library/runtime package)"
                        .into(),
                ));
            }
        }
        for (name, volume) in &self.volumes {
            if !volume.path.starts_with('/') {
                return Err(Error::Manifest(format!(
                    "volume `{name}`: path must be absolute, got `{}`",
                    volume.path
                )));
            }
            if volume.scope != "instance" && volume.scope != "shared" {
                return Err(Error::Manifest(format!(
                    "volume `{name}`: scope must be \"instance\" or \"shared\", got `{}`",
                    volume.scope
                )));
            }
        }
        for (alias, dep) in &self.dependencies {
            validate_package_name(alias)?;
            validate_package_name(&dep.spec(alias).package)?;
        }
        if let Some(restart) = &self.restart {
            if !matches!(restart.policy.as_str(), "never" | "on-failure" | "always") {
                return Err(Error::Manifest(format!(
                    "restart.policy must be \"never\", \"on-failure\" or \"always\", got `{}`",
                    restart.policy
                )));
            }
            parse_duration(&restart.backoff)?;
            parse_duration(&restart.max_backoff)?;
        }
        if let Some(health) = &self.health {
            parse_duration(&health.grace)?;
        }
        if self.package.isolation != "ns" && self.package.isolation != "vm" {
            return Err(Error::Manifest(format!(
                "package.isolation must be \"ns\" or \"vm\", got `{}`",
                self.package.isolation
            )));
        }
        for entry in &self.package.include {
            let trimmed = entry.trim_end_matches('/');
            if trimmed.is_empty()
                || trimmed.starts_with('/')
                || trimmed
                    .split('/')
                    .any(|part| part == ".." || part.is_empty())
            {
                return Err(Error::Manifest(format!(
                    "package.include entry `{entry}`: must be a relative path inside the app dir (no leading `/`, no `..`)"
                )));
            }
        }
        Ok(())
    }
}

/// Package/app names: lowercase alphanumeric with `-` / `_`/ `.`, must not
/// contain `-` followed by a digit (would be ambiguous in the image filename
/// grammar `<name>-<semver>-<os>-<arch>.img`).
pub fn validate_package_name(name: &str) -> Result<()> {
    let valid_chars = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && name.starts_with(|c: char| c.is_ascii_lowercase());
    if !valid_chars {
        return Err(Error::Manifest(format!(
            "invalid package name `{name}`: use lowercase letters, digits, `-`, `_`, `.`, starting with a letter"
        )));
    }
    let bytes = name.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        if bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
            return Err(Error::Manifest(format!(
                "invalid package name `{name}`: `-` followed by a digit is not allowed (ambiguous with the version in image filenames)"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
        [package]
        name = "myapp"
        version = "1.2.3"
        entrypoint = ["node", "server.js"]

        [dependencies]
        base = "alpine@3.20"
        node = "22"
        ffmpeg = { source = "github:someorg/ffmpeg-pkg", version = "6.1" }

        [env]
        NODE_ENV = "production"

        [ports]
        web = 3000

        [volumes]
        data = { path = "/var/lib/myapp" }
        shared = { path = "/srv/uploads", scope = "shared" }
        cache = { path = "/var/cache/myapp", ephemeral = true }

        [resources]
        mem = "512M"
        cpu = "1.5"
        pids = 256

        [requires]
        abi = "linux-x64-musl"
    "#;

    #[test]
    fn parses_full_manifest() {
        let m = Manifest::parse(FULL).unwrap();
        assert_eq!(m.package.name, "myapp");
        assert_eq!(m.package.version, Version::new(1, 2, 3));
        assert_eq!(m.dependencies.len(), 3);
        assert_eq!(m.volumes["data"].scope, "instance");
        assert_eq!(m.volumes["shared"].scope, "shared");
        assert!(m.volumes["cache"].ephemeral);
        // canonical roundtrip
        let round = Manifest::parse(&m.to_toml().unwrap()).unwrap();
        assert_eq!(round.to_toml().unwrap(), m.to_toml().unwrap());
    }

    #[test]
    fn rejects_empty_entrypoint() {
        let err = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = []
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("entrypoint"));
    }

    #[test]
    fn rejects_dash_digit_name() {
        assert!(validate_package_name("lib-2d").is_err());
        assert!(validate_package_name("ffmpeg").is_ok());
        assert!(validate_package_name("chrome-headless").is_ok());
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
            typo_field = 1
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("typo_field"));
    }

    #[test]
    fn rejects_relative_volume_path() {
        let err = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
            [volumes]
            data = { path = "data" }
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("absolute"));
    }
}
