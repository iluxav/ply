//! Declarative host deployments: a deployment IS a file.
//!
//! `/var/lib/ply/deployments/<name>.toml` describes an app this host should
//! run; `ply reconcile` makes systemd agree. The dir is watched by a
//! systemd.path unit (kernel inotify — no resident ply process, not even a
//! poll): file lands → oneshot reconcile → unit written → enable --now.
//! Spec removed → managed unit stopped and deleted. `ls` is the host's app
//! list; `scp *.toml` + reconcile rebuilds a machine.
//!
//! Reconcile writes `<name>.status` (one JSON line) beside each spec — the
//! dashboard's and the operator's feedback channel.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Error, Result};

pub fn dir() -> PathBuf {
    crate::paths::data_dir().join("deployments")
}

/// Marker distinguishing units reconcile owns from hand-written ones — only
/// marked units are ever auto-removed.
pub const UNIT_MARKER: &str =
    "# managed by `ply reconcile` — edit the deployment file, not this unit";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// A runnable app from the registry's apps namespace…
    #[serde(default)]
    pub app: Option<String>,
    /// …or a local image path. Exactly one of the two.
    #[serde(default)]
    pub image: Option<String>,
    /// Version constraint for `app` (newest matching wins, then pinned in
    /// the generated unit by the fetched file's identity).
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub scale: Option<u32>,
    #[serde(default)]
    pub publish: Vec<String>,
    #[serde(default)]
    pub domain: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Secrets that should not live in this file: a root-owned env file.
    #[serde(default)]
    pub env_file: Option<String>,
    /// Mount the [requests] links the image asks for (dashboard-style apps).
    #[serde(default)]
    pub grant_links: bool,
    /// Start after these apps are healthy.
    #[serde(default)]
    pub after: Vec<String>,
    /// Registry source override (any `[sources]` spec).
    #[serde(default)]
    pub source: Option<String>,
}

impl Spec {
    pub fn parse(text: &str) -> Result<Spec> {
        let spec: Spec = toml::from_str(text).map_err(|e| Error::Manifest(e.to_string()))?;
        match (&spec.app, &spec.image) {
            (Some(_), Some(_)) => Err(Error::Manifest(
                "a deployment names `app` (registry) OR `image` (a file) — not both".into(),
            )),
            (None, None) => Err(Error::Manifest(
                "a deployment needs `app = \"name\"` (registry) or `image = \"/path.img\"`".into(),
            )),
            _ => Ok(spec),
        }
    }

    /// The `ply run` flags the generated unit carries.
    pub fn flags(&self) -> Vec<String> {
        let mut flags = Vec::new();
        if let Some(scale) = self.scale {
            flags.extend(["--scale".into(), scale.to_string()]);
        }
        for publish in &self.publish {
            flags.extend(["--publish".into(), publish.clone()]);
        }
        for (key, value) in &self.env {
            flags.extend(["-e".into(), format!("{key}={value}")]);
        }
        if let Some(file) = &self.env_file {
            flags.extend(["--env-file".into(), file.clone()]);
        }
        for domain in &self.domain {
            flags.extend(["--domain".into(), domain.clone()]);
        }
        for app in &self.after {
            flags.extend(["--after".into(), app.clone()]);
        }
        flags
    }
}

/// One reconcile outcome, written as `<name>.status`.
pub fn write_status(name: &str, ok: bool, detail: &str) {
    let dir = dir();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{{\"ok\":{ok},\"detail\":{detail:?},\"ts\":{ts}}}\n");
    let tmp = dir.join(format!(".{name}.status.tmp"));
    if std::fs::write(&tmp, line).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(format!("{name}.status")));
    }
}

/// Deployment specs on this host: (name, parse result).
#[allow(clippy::type_complexity)]
pub fn list() -> Result<Vec<(String, Result<Spec>)>> {
    let dir = dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(source) => return Err(Error::Io { path: dir, source }),
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = file.strip_suffix(".toml") else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let spec = std::fs::read_to_string(entry.path())
            .map_err(|source| Error::Io {
                path: entry.path(),
                source,
            })
            .and_then(|text| Spec::parse(&text));
        out.push((name.to_string(), spec));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_parses_and_builds_flags() {
        let spec = Spec::parse(
            r#"
app = "redis"
version = "8"
publish = ["internal:6379"]
domain = ["cache.example.com"]
scale = 2

[env]
REDIS_PASSWORD = "s3cret"
"#,
        )
        .unwrap();
        assert_eq!(spec.app.as_deref(), Some("redis"));
        let flags = spec.flags();
        assert!(flags
            .windows(2)
            .any(|w| w == ["--publish", "internal:6379"]));
        assert!(flags
            .windows(2)
            .any(|w| w == ["-e", "REDIS_PASSWORD=s3cret"]));
        assert!(flags
            .windows(2)
            .any(|w| w == ["--domain", "cache.example.com"]));
        assert!(flags.windows(2).any(|w| w == ["--scale", "2"]));
    }

    #[test]
    fn spec_rejects_ambiguity() {
        assert!(Spec::parse("app = \"redis\"\nimage = \"/x.img\"\n").is_err());
        assert!(Spec::parse("version = \"8\"\n").is_err());
        assert!(Spec::parse("app = \"redis\"\ntypo = 1\n").is_err());
    }
}
