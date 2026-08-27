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
    /// …or a local image path…
    #[serde(default)]
    pub image: Option<String>,
    /// …or a GitHub repo whose releases carry the CI-built .img
    /// (`github = "org/repo"`). Exactly one of the three.
    #[serde(default)]
    pub github: Option<String>,
    /// Asset app name for `github` (the `<name>` in
    /// `<name>-<ver>-linux-<arch>.img`); defaults to the deployment name.
    #[serde(default)]
    pub asset: Option<String>,
    /// Release-stream filter for monorepos: `tag_prefix = "web-v"` follows
    /// the newest `web-v<x.y.z>` release and leaves the repo's plain
    /// `v<x.y.z>` stream (and its `latest` marker) alone.
    #[serde(default)]
    pub tag_prefix: Option<String>,
    /// Fine-grained PAT (Contents: read) for private repos — a root-owned
    /// file, never the token itself. One credential for both git lanes:
    /// release-asset downloads (`github`) and https clones (`repo`).
    /// Relative paths resolve against the deployments dir.
    #[serde(default)]
    pub token_file: Option<String>,
    /// …or a git repo to clone and BUILD ON THIS HOST (lane 2):
    /// `repo = "https://github.com/org/app"` or `git@github.com:org/app.git`.
    #[serde(default)]
    pub repo: Option<String>,
    /// Branch (or any committish) to build; default: the remote HEAD branch.
    #[serde(default)]
    pub r#ref: Option<String>,
    /// Build command, run inside a memory-fenced ply container whose only
    /// toolchain is `runtime` from the registry. The checkout persists
    /// between builds — node_modules and framework caches ARE the cache.
    #[serde(default)]
    pub build: Option<String>,
    /// Builder toolchain, e.g. "node@24" (registry package + version range).
    #[serde(default)]
    pub runtime: Option<String>,
    /// Read-only per-repo SSH key for private repos (git lanes only).
    #[serde(default)]
    pub deploy_key: Option<String>,
    /// When the repo has no ply.toml: the app manifest, inline.
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub include: Vec<String>,
    /// Declared app port (labels + health gate for the generated manifest).
    #[serde(default)]
    pub port: Option<u16>,
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
    /// Grouping label: deployments sharing a stack render together in the
    /// dashboard. Cosmetic — the wiring is `after` + discovery env.
    #[serde(default)]
    pub stack: Option<String>,
    /// Registry source override (any `[sources]` spec).
    #[serde(default)]
    pub source: Option<String>,
    /// Follow the source automatically on background reconcile runs
    /// (the timer). `auto = false` = manual only: the spec converges only
    /// when the FILE is touched — an edit, or the dashboard's deploy-now.
    #[serde(default = "default_true")]
    pub auto: bool,
}

fn default_true() -> bool {
    true
}

impl Spec {
    pub fn parse(text: &str) -> Result<Spec> {
        let spec: Spec = toml::from_str(text).map_err(|e| Error::Manifest(e.to_string()))?;
        let sources = spec.app.is_some() as u8
            + spec.image.is_some() as u8
            + spec.github.is_some() as u8
            + spec.repo.is_some() as u8;
        match sources {
            1 => Ok(spec),
            0 => Err(Error::Manifest(
                "a deployment needs one of: `app` (registry), `image` (a file), `github` (release assets), `repo` (build here)".into(),
            )),
            _ => Err(Error::Manifest(
                "a deployment names exactly one of `app`, `image`, `github`, `repo`".into(),
            )),
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

/// Status lives in a SUBDIRECTORY of the watched dir: systemd's
/// PathModified fires on the dir's own entries, so reconcile writing
/// results next to the specs would retrigger itself forever (it did).
/// Writes inside `.status/` change nothing the watcher sees.
pub fn status_dir() -> PathBuf {
    dir().join(".status")
}

pub fn status_path(name: &str) -> PathBuf {
    status_dir().join(format!("{name}.status"))
}

/// One reconcile outcome, written as `.status/<name>.status`.
pub fn write_status(name: &str, ok: bool, detail: &str) {
    let dir = status_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{{\"ok\":{ok},\"detail\":{detail:?},\"ts\":{ts}}}\n");
    let tmp = dir.join(format!(".{name}.tmp"));
    if std::fs::write(&tmp, line).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(format!("{name}.status")));
    }
}

/// The spec file itself — mtime on this is how reconcile reads intent.
pub fn spec_path(name: &str) -> PathBuf {
    dir().join(format!("{name}.toml"))
}

/// Last reconcile outcome: (ok, unix ts). None = never reconciled.
pub fn read_status(name: &str) -> Option<(bool, u64)> {
    let raw = std::fs::read_to_string(status_path(name)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some((v.get("ok")?.as_bool()?, v.get("ts")?.as_u64()?))
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
