//! `ply.toml` — the app manifest. Human intent; the lockfile is machine truth.

use std::collections::BTreeMap;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::params;

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

    /// Named param declarations. `None` = table absent — `{}` holes in
    /// `[env]` and elsewhere stay literal text and none of the param
    /// validation below runs. `Some(_)` (even empty) turns holes on: every
    /// `{name}` must resolve to a declared param or a non-live built-in.
    /// Stored as raw `toml::Value` (not `ParamDecl`) so `Serialize` round-trips
    /// byte-for-byte when this manifest is embedded into an image; use
    /// [`Manifest::param_decls`] for the validated, typed form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<BTreeMap<String, toml::Value>>,

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

    /// `[network]` — the egress contract: destinations this package needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,

    /// Env contributions this package exposes to dependents; embedded into
    /// the built image as `/.layer.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<Layer>,

    /// Where to fetch dependencies from. `default` applies to deps without an
    /// explicit source; other keys are aliases usable as `source = "<alias>"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, String>,

    /// Host access the image asks for. NEVER applied on its own — a manifest
    /// ships inside the image, and an image must not grant itself host
    /// access. `ply run --grant-links` is the operator's explicit yes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<Requests>,
}

/// `[requests]` — declared needs, granted (or not) at run time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requests {
    /// Bind mounts as "HOST:CONTAINER", both absolute. The dashboard's
    /// read surfaces are the canonical example. Also accepts the spelled-out
    /// form `{ host = "/run/ply", at = "/ply/host/run" }` — a colon-packed
    /// pair is a shell-ism, and this is a config file with structure
    /// available. Both normalise to "HOST:CONTAINER" here, so nothing
    /// downstream sees the difference.
    #[serde(default, deserialize_with = "de_links")]
    pub links: Vec<String>,
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

/// `[network]` — the egress contract. `egress`: the destinations this
/// package needs, as written (see `egress::entry::EgressEntry` for the
/// grammar and `Manifest::egress_entries` for the parsed form).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Network {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<Vec<String>>,
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
    ///
    /// A STRING is shell form and becomes `["/bin/sh", "-c", <string>]` —
    /// which is what a long entrypoint was already doing by hand, on one
    /// unreadable line. A TOML multi-line string then makes it diffable:
    ///
    /// ```toml
    /// entrypoint = """
    ///   [ -f /etc/caddy/Caddyfile ] || cp /opt/edge/Caddyfile /etc/caddy/
    ///   exec caddy run --config /etc/caddy/Caddyfile --watch
    /// """
    /// ```
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_entrypoint"
    )]
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
    /// The same composed rootfs enters as the new root (namespaces) or a
    /// mounted disk (virtio-fs).
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
    /// Registry namespace this package publishes under (`<owner>/<name>`).
    /// Absent for a local/unpublished manifest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Human-readable summary, shown by `ply search` / the registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// SPDX identifier or free-text license name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Upstream project URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
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

/// Every name the `caps` crate's `Capability` enum recognizes (41 variants)
/// — a fixed copy, checked against the crate itself by
/// `runtime::ns::security::cap_tests::capability_table_matches_the_caps_crate`
/// (there, not here: `caps` is a Linux-only dependency of this crate — see
/// Cargo.toml — and the only files that may name it are `runtime/ns/*` and
/// `craft.rs`, so the name table lives here and the crate-parity test lives
/// in `runtime/ns/security.rs`; the `pub(crate)` below is what lets that
/// test reach this table).
/// `LINUX_CAPABILITY_NAMES` itself is plain strings, no `caps` crate needed,
/// so a capability name can be validated without it — kept here rather than
/// only in `runtime::ns::security` so a typo is caught by `ply build` on
/// every platform, not only on Linux.
pub(crate) const LINUX_CAPABILITY_NAMES: &[&str] = &[
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_LINUX_IMMUTABLE",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_BROADCAST",
    "CAP_NET_ADMIN",
    "CAP_NET_RAW",
    "CAP_IPC_LOCK",
    "CAP_IPC_OWNER",
    "CAP_SYS_MODULE",
    "CAP_SYS_RAWIO",
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",
    "CAP_SYS_PACCT",
    "CAP_SYS_ADMIN",
    "CAP_SYS_BOOT",
    "CAP_SYS_NICE",
    "CAP_SYS_RESOURCE",
    "CAP_SYS_TIME",
    "CAP_SYS_TTY_CONFIG",
    "CAP_MKNOD",
    "CAP_LEASE",
    "CAP_AUDIT_WRITE",
    "CAP_AUDIT_CONTROL",
    "CAP_SETFCAP",
    "CAP_MAC_OVERRIDE",
    "CAP_MAC_ADMIN",
    "CAP_SYSLOG",
    "CAP_WAKE_ALARM",
    "CAP_BLOCK_SUSPEND",
    "CAP_AUDIT_READ",
    "CAP_PERFMON",
    "CAP_BPF",
    "CAP_CHECKPOINT_RESTORE",
];

/// Resolve `[package] capabilities` so a typo fails at `ply build`, not at
/// 3am on the box.
///
/// Two layers, not one: on Linux, `runtime::ns::security::keep_set` does the
/// real resolution (presets included, and name parsing straight off the
/// `caps` crate) and runs FIRST, so a typo's error text there is unchanged
/// byte-for-byte from before this platform split existed. Every name that
/// passes it also passes the portable table check below (the two are kept
/// in lockstep by
/// `runtime::ns::security::cap_tests::capability_table_matches_the_caps_crate`),
/// so on Linux that second check never actually fires — it is there for every OTHER
/// platform, where `caps` isn't even a dependency (see
/// `ply-core/Cargo.toml`) and there is no runtime backend yet to resolve
/// against, but the manifest field is still just a list of strings, and a
/// typo in it is a typo regardless of what machine ran `ply build`.
fn validate_capabilities(capabilities: Option<&Capabilities>) -> Result<()> {
    #[cfg(target_os = "linux")]
    crate::runtime::ns::security::keep_set(capabilities, false)?;
    validate_capability_names(capabilities)
}

/// The portable half of `validate_capabilities`: string matching against
/// `LINUX_CAPABILITY_NAMES`, mirroring
/// `runtime::ns::security::parse_capability`'s normalization (trim,
/// uppercase, prepend `CAP_` if missing) and error text exactly, so a typo
/// reads the same wherever it's caught.
fn validate_capability_names(capabilities: Option<&Capabilities>) -> Result<()> {
    match capabilities {
        None => Ok(()),
        Some(Capabilities::Preset(name)) if name.eq_ignore_ascii_case("oci") => Ok(()),
        Some(Capabilities::Preset(name)) => Err(Error::Manifest(format!(
            "unknown capabilities preset `{name}` — the only preset is \"oci\" \
             (Docker's default set); otherwise list the capabilities explicitly"
        ))),
        Some(Capabilities::List(names)) => {
            for name in names {
                let upper = name.trim().to_ascii_uppercase();
                let full = if upper.starts_with("CAP_") {
                    upper
                } else {
                    format!("CAP_{upper}")
                };
                if !LINUX_CAPABILITY_NAMES.contains(&full.as_str()) {
                    return Err(Error::Manifest(format!(
                        "unknown capability `{name}` in package.capabilities (expected e.g. \"chown\", \
                         \"setuid\", \"net_bind_service\", or the preset \"oci\")"
                    )));
                }
            }
            Ok(())
        }
    }
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

#[derive(Debug, Clone, Serialize)]
pub struct Volume {
    /// Mount path inside the instance.
    pub path: String,
    /// "instance" (default) or "shared" — shared is an explicit opt-in.
    pub scope: String,
    /// GC-able cache contract.
    pub ephemeral: bool,
}

/// `data = "/var/lib/x"` is the same as `data = { path = "/var/lib/x" }`.
/// Every volume in this repo used the table form to set one key; the extra
/// braces bought nothing. The table form stays for `scope` and `ephemeral`,
/// which is when it says something.
impl<'de> Deserialize<'de> for Volume {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            path: String,
            #[serde(default = "default_scope")]
            scope: String,
            #[serde(default)]
            ephemeral: bool,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Path(String),
            Table(Table),
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Path(path) => Volume {
                path,
                scope: default_scope(),
                ephemeral: false,
            },
            Repr::Table(t) => Volume {
                path: t.path,
                scope: t.scope,
                ephemeral: t.ephemeral,
            },
        })
    }
}

/// `entrypoint`: an argv array, or a string meaning `sh -c`.
fn de_entrypoint<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Shell(String),
        Argv(Vec<String>),
    }
    Ok(match Option::<Repr>::deserialize(d)? {
        None => None,
        Some(Repr::Argv(v)) => Some(v),
        Some(Repr::Shell(script)) => Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            script.trim().to_string(),
        ]),
    })
}

/// `links` entries: `"HOST:CONTAINER"` or `{ host = "…", at = "…" }`.
fn de_links<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Pair {
        host: String,
        at: String,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Packed(String),
        Pair(Pair),
    }
    Ok(Vec::<Entry>::deserialize(d)?
        .into_iter()
        .map(|e| match e {
            Entry::Packed(s) => s,
            // Validated like any other entry by `validate()` below, so a
            // relative path is rejected in both spellings alike.
            Entry::Pair(p) => format!("{}:{}", p.host, p.at),
        })
        .collect())
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
    /// memory.swap.max — cap the swap OVERFLOW separately from RAM
    /// (default: unlimited, which is what lets a fenced batch job like a
    /// build spill to disk instead of dying or evicting neighbors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<String>,
    /// cpu.weight 1–10000 (default 100) — background jobs take ~25 so
    /// interactive apps win the CPU under contention.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_weight: Option<u32>,
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

    /// The declared list, parsed; `None` when `[network] egress` is absent.
    pub fn egress_entries(&self) -> Result<Option<Vec<crate::egress::EgressEntry>>> {
        match self.network.as_ref().and_then(|n| n.egress.as_ref()) {
            Some(raw) => crate::egress::entry::parse_list(raw).map(Some),
            None => Ok(None),
        }
    }

    /// The manifest's `[params]` table, converted to typed [`ParamDecl`]s and
    /// validated per-entry (secret/default shape — see [`params::ParamDecl::from_toml`]).
    /// Empty when `[params]` is absent. Kept separate from the raw
    /// `toml::Value` storage in the `params` field so `Serialize` round-trips
    /// the manifest byte-for-byte when it's embedded into an image.
    pub fn param_decls(&self) -> Result<BTreeMap<String, params::ParamDecl>> {
        let Some(raw) = &self.params else {
            return Ok(BTreeMap::new());
        };
        raw.iter()
            .map(|(name, v)| {
                Ok((
                    name.clone(),
                    params::ParamDecl::from_toml(name, v, "params")?,
                ))
            })
            .collect()
    }

    /// Validates `[params]` and every `{...}` hole reachable from it: `[env]`
    /// values and computed param defaults. A manifest with no `[params]`
    /// table skips all of this — braces stay literal text, per the doc
    /// comment on the `params` field.
    fn validate_params(&self) -> Result<()> {
        let Some(raw) = &self.params else {
            return Ok(());
        };

        // Rule 1: reserved names can't be redeclared.
        for name in raw.keys() {
            if params::RESERVED.contains(&name.as_str()) {
                return Err(Error::Manifest(format!(
                    "`{name}` is a built-in param — pick another name"
                )));
            }
        }

        // Rule 2 (secret+default shape) is enforced by ParamDecl::from_toml.
        let decls = self.param_decls()?;

        let check_ref = |pref: &params::PRef| -> Result<()> {
            if pref.app.is_some() {
                return Err(Error::Manifest(
                    "a manifest can only reference its own params".into(),
                ));
            }
            let name = pref.param.as_str();
            if decls.contains_key(name) {
                return Ok(());
            }
            if params::LIVE.contains(&name) {
                return Err(Error::Manifest(format!(
                    "`{{{name}}}` is live — apps read /run/ply/self/state, dependents wait with `after`"
                )));
            }
            if params::RESERVED.contains(&name) {
                return Ok(());
            }
            Err(Error::Manifest(format!(
                "`{{{name}}}` is not a declared param — add it to [params], or fix the typo"
            )))
        };

        let check_holes = |s: &str, who: &str| -> Result<()> {
            for piece in params::parse_template(s, who)? {
                if let params::Piece::Hole(pref) = piece {
                    check_ref(&pref)?;
                }
            }
            Ok(())
        };

        // Rule 4/5: every hole in [env] must resolve (params table present).
        for (key, val) in &self.env {
            check_holes(val, &format!("env.{key}"))?;
        }

        // Rule 4: every hole in a computed param's value must resolve too;
        // Rule 3: and the param→param edges among those must be acyclic.
        let mut edges: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (name, decl) in &decls {
            if let params::ParamDecl::Value(v) = decl {
                check_holes(v, &format!("params.{name}"))?;
                let refs: Vec<String> = params::refs(v)
                    .into_iter()
                    .filter(|r| r.app.is_none() && decls.contains_key(r.param.as_str()))
                    .map(|r| r.param)
                    .collect();
                edges.insert(name.clone(), refs);
            }
        }
        detect_param_cycle(&edges)?;

        Ok(())
    }

    fn validate(&self) -> Result<()> {
        validate_package_name(&self.package.name)?;
        if let Some(requests) = &self.requests {
            for link in &requests.links {
                let ok = matches!(link.split_once(':'),
                    Some((host, container)) if host.starts_with('/') && container.starts_with('/'));
                if !ok {
                    return Err(Error::Manifest(format!(
                        "requests.links `{link}`: expected \"/abs/host:/abs/container\""
                    )));
                }
            }
        }
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
        validate_capabilities(self.package.capabilities.as_ref())?;
        if let Some(network) = &self.network {
            if let Some(raw) = &network.egress {
                crate::egress::entry::parse_list(raw)?;
            }
        }
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
        self.validate_params()?;
        Ok(())
    }
}

/// DFS cycle detection over the param→param reference graph (own-namespace
/// refs only — app-scoped refs are rejected before an edge is ever built).
fn detect_param_cycle(edges: &BTreeMap<String, Vec<String>>) -> Result<()> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Visiting,
        Done,
    }

    fn visit<'a>(
        node: &'a str,
        edges: &'a BTreeMap<String, Vec<String>>,
        state: &mut BTreeMap<&'a str, State>,
        path: &mut Vec<&'a str>,
    ) -> Result<()> {
        match state.get(node) {
            Some(State::Done) => return Ok(()),
            Some(State::Visiting) => {
                let start = path.iter().position(|&n| n == node).unwrap_or(0);
                let mut cycle: Vec<&str> = path[start..].to_vec();
                cycle.push(node);
                return Err(Error::Manifest(format!(
                    "[params] cycle: {}",
                    cycle.join(" → ")
                )));
            }
            None => {}
        }
        state.insert(node, State::Visiting);
        path.push(node);
        if let Some(neighbors) = edges.get(node) {
            for n in neighbors {
                visit(n.as_str(), edges, state, path)?;
            }
        }
        path.pop();
        state.insert(node, State::Done);
        Ok(())
    }

    let mut state = BTreeMap::new();
    let mut path = Vec::new();
    for node in edges.keys() {
        visit(node.as_str(), edges, &mut state, &mut path)?;
    }
    Ok(())
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
mod shorthand_tests {
    use super::*;

    #[test]
    fn entrypoint_string_is_shell_form_and_links_take_a_table() {
        let m = Manifest::parse(
            r#"
            [package]
            name = "edge"
            version = "1.0.0"
            base = "debian@13"
            entrypoint = """
              [ -f /etc/caddy/Caddyfile ] || cp /opt/edge/Caddyfile /etc/caddy/Caddyfile
              exec caddy run --config /etc/caddy/Caddyfile --watch
            """

            [requests]
            links = [
              "/run/ply:/ply/host/run",
              { host = "/var/lib/ply/apps", at = "/ply/host/apps" },
            ]
            "#,
        )
        .unwrap();

        let ep = m.package.entrypoint.as_ref().unwrap();
        assert_eq!(ep[0], "/bin/sh");
        assert_eq!(ep[1], "-c");
        assert!(ep[2].contains("exec caddy run"), "{ep:?}");
        assert!(
            ep[2].starts_with('['),
            "leading blank line must be trimmed: {ep:?}"
        );

        // both link spellings normalise to HOST:CONTAINER
        assert_eq!(
            m.requests.unwrap().links,
            vec!["/run/ply:/ply/host/run", "/var/lib/ply/apps:/ply/host/apps"]
        );
    }

    #[test]
    fn a_relative_link_is_rejected_in_the_table_form_too() {
        let err = Manifest::parse(
            r#"
            [package]
            name = "x"
            version = "1.0.0"
            entrypoint = ["./x"]

            [requests]
            links = [{ host = "run/ply", at = "/ply/host/run" }]
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("abs"), "{err}");
    }
}

#[cfg(test)]
mod volume_shorthand_tests {
    use super::*;

    #[test]
    fn volume_accepts_a_bare_path_or_a_table() {
        let m = Manifest::parse(
            r#"
            [package]
            name = "app"
            version = "1.0.0"
            entrypoint = ["./x"]

            [volumes]
            data = "/var/lib/app"
            cache = { path = "/cache", ephemeral = true }
            shared = { path = "/shared", scope = "shared" }
            "#,
        )
        .unwrap();

        // shorthand gets the same defaults the long form would
        assert_eq!(m.volumes["data"].path, "/var/lib/app");
        assert_eq!(m.volumes["data"].scope, "instance");
        assert!(!m.volumes["data"].ephemeral);

        assert_eq!(m.volumes["cache"].path, "/cache");
        assert!(m.volumes["cache"].ephemeral);
        assert_eq!(m.volumes["shared"].scope, "shared");

        // a typo in the table form still fails loudly rather than defaulting
        let err = Manifest::parse(
            r#"
            [package]
            name = "app"
            version = "1.0.0"
            entrypoint = ["./x"]

            [volumes]
            data = { path = "/d", ephemerall = true }
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("data"), "{err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_links_parse_and_validate() {
        let m = Manifest::parse(
            "[package]\nname = \"dash\"\nversion = \"1.0.0\"\nentrypoint = [\"./x\"]\nbase = \"debian@13\"\n\n[requests]\nlinks = [\"/run/ply:/ply/host/run\"]\n",
        )
        .unwrap();
        assert_eq!(m.requests.unwrap().links, vec!["/run/ply:/ply/host/run"]);

        for bad in ["relative:/abs", "/abs:relative", "no-colon"] {
            let text = format!(
                "[package]\nname = \"dash\"\nversion = \"1.0.0\"\nentrypoint = [\"./x\"]\nbase = \"debian@13\"\n\n[requests]\nlinks = [\"{bad}\"]\n"
            );
            assert!(Manifest::parse(&text).is_err(), "{bad} must be rejected");
        }
    }

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
    fn package_carries_registry_facing_fields() {
        let m = Manifest::parse(
            r#"
[package]
name = "postgres"
owner = "ply"
version = "17.10.7"
description = "PostgreSQL relational database"
license = "PostgreSQL"
homepage = "https://www.postgresql.org"
"#,
        )
        .unwrap();
        assert_eq!(m.package.owner.as_deref(), Some("ply"));
        assert_eq!(m.package.license.as_deref(), Some("PostgreSQL"));
        let back = toml::to_string(&m).unwrap();
        assert!(back.contains("owner = \"ply\""), "round-trips: {back}");
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

    fn manifest_with(params: &str, env: &str) -> String {
        format!("[package]\nname=\"pg\"\nversion=\"1.0.0\"\nentrypoint=[\"pg\"]\n{params}\n[env]\n{env}")
    }

    #[test]
    fn params_reserved_name_rejected() {
        let m = Manifest::parse(&manifest_with("[params]\nhost = \"x\"", ""))
            .unwrap_err()
            .to_string();
        assert!(m.contains("built-in param"), "{m}");
    }

    #[test]
    fn env_hole_must_be_declared_when_params_present() {
        let ok = manifest_with("[params]\nuser = \"postgres\"", "PGUSER = \"{user}\"");
        Manifest::parse(&ok).unwrap();
        let bad = manifest_with("[params]\nuser = \"postgres\"", "PGX = \"{typo}\"");
        assert!(Manifest::parse(&bad).is_err());
    }

    #[test]
    fn braces_are_literal_without_params_table() {
        let m = manifest_with("", "JSONISH = \"{not-a-param!}\"");
        Manifest::parse(&m).unwrap();
    }

    #[test]
    fn computed_cycle_rejected() {
        let m = manifest_with("[params]\na = \"{b}\"\nb = \"{a}\"", "");
        let e = Manifest::parse(&m).unwrap_err().to_string();
        assert!(e.contains("cycle"), "{e}");
    }

    #[test]
    fn live_param_in_env_is_rejected() {
        let m = manifest_with("[params]\nuser = \"x\"", "S = \"{state}\"");
        let e = Manifest::parse(&m).unwrap_err().to_string();
        assert!(e.contains("live"), "{e}");
    }

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
        )
        .unwrap();
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
}
