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
    /// Run the app as this user instead of root: "name:uid:gid"
    /// (e.g. "postgres:70:70"). ply writes the passwd entry, chowns the
    /// app's volumes, and switches uid/gid before rights-stripping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Absolute directory to chdir into before exec. Absent = the app's own
    /// prefix (`/opt/<name>`), which is right for packages ply built. Imported
    /// OCI images carry their own `WorkingDir` here — without it their
    /// entrypoints run from `/`, and the ones that walk `.` (redis does
    /// `find . -exec chown redis {} +`) rampage over the whole rootfs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// What the app keeps after rights stripping. Absent — the default, and
    /// what every ply-native package should stay on — means the app gets
    /// NOTHING: empty bounding set, no_new_privs, seccomp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Capabilities>,
    /// Signal that asks the app to shut down, e.g. "SIGQUIT". Absent =
    /// SIGTERM. Not every daemon agrees TERM means "finish up": nginx wants
    /// SIGQUIT for a graceful drain and httpd wants SIGWINCH, and both would
    /// otherwise be killed mid-request when ply's 10s patience runs out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    /// The package's base story (last field: the table form must serialize
    /// after scalar values). `true` marks the one package per graph that owns
    /// `/` (its files pack at the image root instead of an /opt prefix);
    /// `"alpine@3.20"` / `{ name, version, source }` declares which base
    /// this package runs on.
    #[serde(default, skip_serializing_if = "Base::is_absent")]
    pub base: Base,
}

/// `package.capabilities` — the rights an app keeps, for the rare app that
/// genuinely needs some. Omitting it is always the right answer for a package
/// ply built: a native keg never chowns or setuids, because `[package] user`
/// does that from the parent, before rights stripping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum Capabilities {
    /// `capabilities = "oci"` — Docker's default fourteen. `ply import` sets
    /// this because official images assume it: their entrypoints do
    /// `chown -R x:x /data && exec gosu x …`, which needs CAP_CHOWN and
    /// CAP_SETUID/SETGID. Not a blank cheque — it is Docker's set exactly,
    /// and no more.
    Preset(String),
    /// `capabilities = ["chown", "net_bind_service"]` — exactly these. Names
    /// are case-insensitive and the `CAP_` prefix is optional.
    List(Vec<String>),
}

/// `package.base` — either the base *marker* (`base = true`, in a base
/// package's own manifest) or the base *dependency* (`base = "alpine@3.20"`
/// or `base = { name = "alpine", version = "3.20", source = "corp" }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Base {
    /// `base = true` — this package owns `/`.
    Marker(bool),
    /// `base = "alpine@3.20"` — name@range in one string.
    Spec(String),
    /// `base = { name = "alpine", version = "3.20", source = "corp" }`
    Detailed {
        name: String,
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

impl Default for Base {
    fn default() -> Self {
        Base::Marker(false)
    }
}

impl Base {
    fn is_absent(&self) -> bool {
        matches!(self, Base::Marker(false))
    }
}

impl Package {
    /// True for the one package per graph that owns `/` (FHS, /bin/sh, libc).
    pub fn is_base(&self) -> bool {
        matches!(self.base, Base::Marker(true))
    }
}

/// Parsed `package.user`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
}

pub fn parse_user(s: &str) -> Result<RunUser> {
    let parts: Vec<&str> = s.split(':').collect();
    let bad = || {
        Error::Manifest(format!(
            "package.user `{s}`: expected \"name:uid:gid\", e.g. \"postgres:70:70\""
        ))
    };
    match parts.as_slice() {
        [name, uid, gid] if !name.is_empty() => Ok(RunUser {
            name: name.to_string(),
            uid: uid.parse().map_err(|_| bad())?,
            gid: gid.parse().map_err(|_| bad())?,
        }),
        _ => Err(bad()),
    }
}

/// `package.stop_signal` → the signal to send. Case-insensitive, `SIG`
/// optional: "quit", "SIGQUIT" and "sigquit" are the same request.
pub fn parse_stop_signal(s: &str) -> Result<nix::sys::signal::Signal> {
    let upper = s.trim().to_ascii_uppercase();
    let full = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    full.parse::<nix::sys::signal::Signal>().map_err(|_| {
        Error::Manifest(format!(
            "package.stop_signal `{s}`: not a signal name (expected e.g. \"SIGTERM\", \
             \"SIGQUIT\", \"SIGWINCH\")"
        ))
    })
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
    /// `node = "22"`
    Constraint(String),
    /// `ffmpeg = { source = "github:org/repo", version = "6.1" }`
    Detailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        version: String,
    },
}

/// A dependency, normalized. The `[dependencies]` key IS the package name;
/// the base dependency comes from `[package] base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepSpec {
    pub package: String,
    pub constraint: String,
    pub source: Option<String>,
}

impl Dependency {
    pub fn spec(&self, name: &str) -> DepSpec {
        let (version, source) = match self {
            Dependency::Constraint(v) => (v.as_str(), None),
            Dependency::Detailed { source, version } => (version.as_str(), source.clone()),
        };
        DepSpec {
            package: name.to_string(),
            constraint: version.to_string(),
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

    /// The base declared in `[package] base`, as a normal dependency spec.
    /// None for base packages themselves (`base = true`) and for manifests
    /// with no base.
    pub fn base_dep(&self) -> Option<DepSpec> {
        match &self.package.base {
            Base::Marker(_) => None,
            // validate() guarantees the `name@range` shape.
            Base::Spec(s) => s.split_once('@').map(|(name, constraint)| DepSpec {
                package: name.to_string(),
                constraint: constraint.to_string(),
                source: None,
            }),
            Base::Detailed {
                name,
                version,
                source,
            } => Some(DepSpec {
                package: name.clone(),
                constraint: version.clone(),
                source: source.clone(),
            }),
        }
    }

    /// Every dependency spec: the base (if declared) plus `[dependencies]`.
    pub fn dep_specs(&self) -> Vec<DepSpec> {
        self.base_dep()
            .into_iter()
            .chain(self.dependencies.iter().map(|(name, dep)| dep.spec(name)))
            .collect()
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
        if let Some(workdir) = &self.package.workdir {
            if !workdir.starts_with('/') {
                return Err(Error::Manifest(format!(
                    "package.workdir must be an absolute path inside the image, got `{workdir}`"
                )));
            }
        }
        // Resolve now so a typo fails at `ply build`, not at 3am on the box.
        crate::runtime::security::keep_set(self.package.capabilities.as_ref(), false)?;
        if let Some(sig) = &self.package.stop_signal {
            parse_stop_signal(sig)?;
        }
        if let Some(user) = &self.package.user {
            parse_user(user)?;
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
        match &self.package.base {
            Base::Marker(_) => {}
            Base::Spec(s) => match s.split_once('@') {
                Some((name, constraint)) if !name.is_empty() && !constraint.is_empty() => {
                    validate_package_name(name)?;
                }
                _ => {
                    return Err(Error::Manifest(format!(
                        "package.base `{s}`: expected \"name@version\" (e.g. \"alpine@3.20\") or {{ name = \"alpine\", version = \"3.20\", source = \"…\" }}"
                    )));
                }
            },
            Base::Detailed { name, version, .. } => {
                validate_package_name(name)?;
                if version.is_empty() {
                    return Err(Error::Manifest(
                        "package.base: version must not be empty".into(),
                    ));
                }
            }
        }
        for (name, dep) in &self.dependencies {
            if name == "base" {
                return Err(Error::Manifest(
                    "the base is declared under [package] now — move it there: `base = \"alpine@3.20\"` (or { name, version, source })"
                        .into(),
                ));
            }
            validate_package_name(name)?;
            let spec = dep.spec(name);
            if spec.constraint.contains('@') {
                return Err(Error::Manifest(format!(
                    "dependency `{name}`: the key is the package name — write the version alone (`{name} = \"6.1\"`); `name@version` is only valid for `[package] base`"
                )));
            }
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
        if let Some(user) = &self.package.user {
            parse_user(user)?;
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
        base = "alpine@3.20"

        [dependencies]
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
        assert_eq!(m.dependencies.len(), 2);
        assert_eq!(
            m.base_dep(),
            Some(DepSpec {
                package: "alpine".into(),
                constraint: "3.20".into(),
                source: None,
            })
        );
        assert_eq!(m.volumes["data"].scope, "instance");
        assert_eq!(m.volumes["shared"].scope, "shared");
        assert!(m.volumes["cache"].ephemeral);
        // canonical roundtrip
        let round = Manifest::parse(&m.to_toml().unwrap()).unwrap();
        assert_eq!(round.to_toml().unwrap(), m.to_toml().unwrap());
    }

    #[test]
    fn parses_base_table_with_source() {
        let m = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
            base = { name = "alpine", version = "3.20", source = "corp" }

            [sources]
            corp = "https://artifacts.corp.net/ply"
        "#,
        )
        .unwrap();
        assert_eq!(
            m.base_dep(),
            Some(DepSpec {
                package: "alpine".into(),
                constraint: "3.20".into(),
                source: Some("corp".into()),
            })
        );
        assert!(!m.package.is_base());
        // canonical roundtrip
        let round = Manifest::parse(&m.to_toml().unwrap()).unwrap();
        assert_eq!(round.to_toml().unwrap(), m.to_toml().unwrap());
    }

    #[test]
    fn base_marker_bool_still_parses() {
        let m = Manifest::parse(
            r#"
            [package]
            name = "alpine"
            version = "3.20.0"
            base = true
        "#,
        )
        .unwrap();
        assert!(m.package.is_base());
        assert_eq!(m.base_dep(), None);
        // canonical roundtrip keeps the marker
        let round = Manifest::parse(&m.to_toml().unwrap()).unwrap();
        assert!(round.package.is_base());
    }

    #[test]
    fn no_base_at_all() {
        let m = Manifest::parse(
            r#"
            [package]
            name = "mylib"
            version = "1.0.0"
        "#,
        )
        .unwrap();
        assert!(!m.package.is_base());
        assert_eq!(m.base_dep(), None);
    }

    #[test]
    fn rejects_base_in_dependencies() {
        let err = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]

            [dependencies]
            base = "alpine@3.20"
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("[package]"), "got: {err}");
    }

    #[test]
    fn rejects_base_string_without_name() {
        let err = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
            base = "3.20"
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("alpine@3.20"), "got: {err}");
    }

    #[test]
    fn rejects_at_in_dependency_version() {
        let err = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]

            [dependencies]
            tools = "ffmpeg@6.1"
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("package name"), "got: {err}");
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

    #[test]
    fn workdir_round_trips_and_must_be_absolute() {
        let manifest = Manifest::parse(
            r#"
            [package]
            name = "redis"
            version = "7.2.9"
            entrypoint = ["docker-entrypoint.sh", "redis-server"]
            workdir = "/data"
        "#,
        )
        .expect("absolute workdir parses");
        assert_eq!(manifest.package.workdir.as_deref(), Some("/data"));
        // survives the embed → read cycle every image goes through
        let reparsed = Manifest::parse(&manifest.to_toml().unwrap()).unwrap();
        assert_eq!(reparsed.package.workdir.as_deref(), Some("/data"));

        let err = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
            workdir = "data"
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn capabilities_round_trip_and_typos_fail_at_build_time() {
        let m = Manifest::parse(
            r#"
            [package]
            name = "imported"
            version = "1.0.0"
            entrypoint = ["docker-entrypoint.sh"]
            capabilities = "oci"
        "#,
        )
        .expect("preset parses");
        assert_eq!(
            m.package.capabilities,
            Some(Capabilities::Preset("oci".into()))
        );
        let reparsed = Manifest::parse(&m.to_toml().unwrap()).unwrap();
        assert_eq!(reparsed.package.capabilities, m.package.capabilities);

        let m = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
            capabilities = ["chown", "setuid"]
        "#,
        )
        .expect("explicit list parses");
        assert_eq!(
            m.package.capabilities,
            Some(Capabilities::List(vec!["chown".into(), "setuid".into()]))
        );

        // a typo must not survive to run time
        let err = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
            capabilities = ["definitely_not_a_cap"]
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("definitely_not_a_cap"), "{err}");
    }

    #[test]
    fn native_manifests_omit_capabilities_entirely() {
        let m = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
        "#,
        )
        .unwrap();
        assert_eq!(m.package.capabilities, None);
        assert!(!m.to_toml().unwrap().contains("capabilities"));
    }

    #[test]
    fn workdir_is_optional_and_omitted_when_unset() {
        let manifest = Manifest::parse(
            r#"
            [package]
            name = "a"
            version = "1.0.0"
            entrypoint = ["a"]
        "#,
        )
        .unwrap();
        assert_eq!(manifest.package.workdir, None);
        assert!(!manifest.to_toml().unwrap().contains("workdir"));
    }
}
