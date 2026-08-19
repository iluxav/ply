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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub name: String,
    pub version: Version,
    /// argv of the process to exec, e.g. ["node", "server.js"]
    pub entrypoint: Vec<String>,
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

    fn validate(&self) -> Result<()> {
        validate_package_name(&self.package.name)?;
        if self.package.entrypoint.is_empty() {
            return Err(Error::Manifest(
                "package.entrypoint must have at least one element, e.g. [\"node\", \"server.js\"]"
                    .into(),
            ));
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
        for name in self.dependencies.keys() {
            validate_package_name(name)?;
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
