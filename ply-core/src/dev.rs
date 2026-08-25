//! `ply.dev.toml` — the gitignorable dev overlay next to an app's ply.toml.
//!
//! Runtime-only, by design: `ply build` never reads it, so a shipped image
//! cannot contain dev configuration and editing the overlay never dirties
//! the build. `ply run DIR` (and `ply up` path members, which go through
//! it) apply the overlay when starting the container: an argv swap, an env
//! merge, extra bind mounts — none of which touch the artifact.
//!
//! Precedence: manifest [env] < overlay [env] < explicit -e flags.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DevOverlay {
    /// Replaces the image's entrypoint (`npm run dev` instead of `node dist/`).
    pub entrypoint: Option<Vec<String>>,
    /// Merged over the manifest's [env]; explicit -e still wins.
    pub env: Vec<(String, String)>,
    /// Extra bind mounts: (absolute host path, absolute container path).
    pub links: Vec<(PathBuf, String)>,
}

impl DevOverlay {
    /// One line for the "applying" notice: `entrypoint, env(2), links(1)`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.entrypoint.is_some() {
            parts.push("entrypoint".to_string());
        }
        if !self.env.is_empty() {
            parts.push(format!("env({})", self.env.len()));
        }
        if !self.links.is_empty() {
            parts.push(format!("links({})", self.links.len()));
        }
        parts.join(", ")
    }
}

/// Read `<dir>/ply.dev.toml`. `Ok(None)` when absent. `app` names the keg
/// prefix relative container link paths resolve under (`src` → `/opt/<app>/src`);
/// relative host paths resolve against `dir`.
pub fn load(dir: &Path, app: &str) -> Result<Option<DevOverlay>> {
    let path = dir.join("ply.dev.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    parse(&text, &path, dir, app).map(Some)
}

fn parse(text: &str, path: &Path, dir: &Path, app: &str) -> Result<DevOverlay> {
    let doc: toml::Value = text
        .parse()
        .map_err(|e| Error::Manifest(format!("{}: {e}", path.display())))?;
    let table = doc
        .as_table()
        .ok_or_else(|| Error::Manifest(format!("{}: expected a TOML table", path.display())))?;
    for key in table.keys() {
        if !matches!(key.as_str(), "entrypoint" | "env" | "links") {
            return Err(Error::Manifest(format!(
                "{}: unknown key `{key}` (a dev overlay takes entrypoint, [env], links)",
                path.display()
            )));
        }
    }

    let entrypoint = match table.get("entrypoint") {
        None => None,
        Some(toml::Value::Array(items)) => {
            let argv: Option<Vec<String>> = items
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect();
            match argv {
                Some(argv) if !argv.is_empty() => Some(argv),
                _ => {
                    return Err(Error::Manifest(format!(
                        "{}: entrypoint must be a non-empty array of strings",
                        path.display()
                    )))
                }
            }
        }
        Some(_) => {
            return Err(Error::Manifest(format!(
                "{}: entrypoint must be an array, e.g. [\"npm\", \"run\", \"dev\"]",
                path.display()
            )))
        }
    };

    let mut env = Vec::new();
    if let Some(value) = table.get("env") {
        let env_table = value.as_table().ok_or_else(|| {
            Error::Manifest(format!(
                "{}: env must be a table of KEY = \"value\"",
                path.display()
            ))
        })?;
        for (k, v) in env_table {
            let text = match v {
                toml::Value::String(s) => s.clone(),
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(f) => f.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                _ => {
                    return Err(Error::Manifest(format!(
                        "{}: env {k} must be a string or number",
                        path.display()
                    )))
                }
            };
            env.push((k.clone(), text));
        }
    }

    let mut links = Vec::new();
    if let Some(value) = table.get("links") {
        let items = value.as_array().ok_or_else(|| {
            Error::Manifest(format!(
                "{}: links must be an array of \"HOST:CONTAINER\" strings",
                path.display()
            ))
        })?;
        for item in items {
            let Some(spec) = item.as_str() else {
                return Err(Error::Manifest(format!(
                    "{}: links entries are strings, e.g. \"./src:src\"",
                    path.display()
                )));
            };
            let Some((host, container)) = spec.split_once(':') else {
                return Err(Error::Manifest(format!(
                    "{}: link `{spec}`: expected HOST:CONTAINER",
                    path.display()
                )));
            };
            if host.is_empty() || container.is_empty() {
                return Err(Error::Manifest(format!(
                    "{}: link `{spec}`: expected HOST:CONTAINER",
                    path.display()
                )));
            }
            // relative host → against the app dir; relative container →
            // under the app's keg prefix
            let host_path = if Path::new(host).is_absolute() {
                PathBuf::from(host)
            } else {
                dir.join(host)
            };
            let container_path = if container.starts_with('/') {
                container.to_string()
            } else {
                format!("/opt/{app}/{container}")
            };
            links.push((host_path, container_path));
        }
    }

    Ok(DevOverlay {
        entrypoint,
        env,
        links,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(text: &str) -> DevOverlay {
        parse(text, Path::new("ply.dev.toml"), Path::new("/app"), "server").unwrap()
    }

    #[test]
    fn full_overlay_parses_and_resolves() {
        let o = overlay(
            r#"
entrypoint = ["npm", "run", "dev"]
links = ["./src:src", "/data/fixtures:/fixtures"]

[env]
NODE_ENV = "development"
PORT = 3001
"#,
        );
        assert_eq!(
            o.entrypoint,
            Some(vec!["npm".into(), "run".into(), "dev".into()])
        );
        assert!(o.env.contains(&("NODE_ENV".into(), "development".into())));
        assert!(o.env.contains(&("PORT".into(), "3001".into())));
        // relative host → app dir; relative container → keg prefix
        assert_eq!(
            o.links[0],
            (PathBuf::from("/app/src"), "/opt/server/src".into())
        );
        // absolute both sides pass through
        assert_eq!(
            o.links[1],
            (PathBuf::from("/data/fixtures"), "/fixtures".into())
        );
        assert_eq!(o.describe(), "entrypoint, env(2), links(2)");
    }

    #[test]
    fn empty_overlay_is_valid_and_silent() {
        let o = overlay("");
        assert_eq!(o, DevOverlay::default());
        assert_eq!(o.describe(), "");
    }

    #[test]
    fn rejects_typos_and_bad_shapes() {
        for (text, msg) in [
            ("entrypint = [\"x\"]", "unknown key"),
            ("entrypoint = \"npm run dev\"", "must be an array"),
            ("entrypoint = []", "non-empty"),
            ("links = [\"no-colon\"]", "HOST:CONTAINER"),
            ("links = [\":src\"]", "HOST:CONTAINER"),
            ("[links]\nx = 1", "array"),
        ] {
            let err = parse(text, Path::new("p"), Path::new("/a"), "x")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(msg),
                "`{text}` → `{err}` should mention `{msg}`"
            );
        }
    }

    #[test]
    fn absent_file_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(tmp.path(), "x").unwrap().is_none());
    }
}
