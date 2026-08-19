//! Sources are URL templates — the whole "registry client".
//!
//! ```text
//! github:org/repo   → https://github.com/org/repo/releases/download/v{version}/{filename}
//! gitlab:group/proj → https://gitlab.com/group/proj/-/releases/v{version}/downloads/{filename}
//! https://any.url/  → https://any.url/{filename}
//! file:///path/     → local directory
//! ```
//!
//! Fetch is zero-API: construct URL → GET → verify sha256 → store. Transport
//! is untrusted by design; wrong bytes fail the hash.

use std::io::Read;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::store::Store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Github { org: String, repo: String },
    Gitlab { group: String, project: String },
    Http { base: String },
    Dir { path: PathBuf },
}

impl Source {
    pub fn parse(spec: &str, allow_insecure: bool) -> Result<Source> {
        let bad = |why: &str| Error::Source(format!("invalid source `{spec}`: {why}"));
        if let Some(rest) = spec.strip_prefix("github:") {
            let (org, repo) = rest
                .split_once('/')
                .ok_or_else(|| bad("expected github:org/repo"))?;
            return Ok(Source::Github {
                org: org.into(),
                repo: repo.into(),
            });
        }
        if let Some(rest) = spec.strip_prefix("gitlab:") {
            let (group, project) = rest
                .split_once('/')
                .ok_or_else(|| bad("expected gitlab:group/proj"))?;
            return Ok(Source::Gitlab {
                group: group.into(),
                project: project.into(),
            });
        }
        if let Some(rest) = spec.strip_prefix("file://") {
            return Ok(Source::Dir {
                path: PathBuf::from(rest),
            });
        }
        if spec.starts_with("https://") {
            return Ok(Source::Http {
                base: spec.trim_end_matches('/').to_string(),
            });
        }
        if let Some(host_part) = spec.strip_prefix("http://") {
            let host = host_part.split(['/', ':']).next().unwrap_or("");
            if !allow_insecure && !is_private_host(host) {
                return Err(Error::Source(format!(
                    "plain http source `{spec}` on a public host — use https:// or pass --insecure-source"
                )));
            }
            return Ok(Source::Http {
                base: spec.trim_end_matches('/').to_string(),
            });
        }
        Err(bad(
            "expected github:org/repo, gitlab:group/proj, https://…, http://… (private hosts), or file:///path",
        ))
    }

    /// Download URL for one artifact (not meaningful for Dir sources).
    fn url_for(&self, filename: &str, version: &Version) -> String {
        match self {
            Source::Github { org, repo } => {
                format!("https://github.com/{org}/{repo}/releases/download/v{version}/{filename}")
            }
            Source::Gitlab { group, project } => format!(
                "https://gitlab.com/{group}/{project}/-/releases/v{version}/downloads/{filename}"
            ),
            Source::Http { base } => format!("{base}/{filename}"),
            Source::Dir { .. } => unreachable!("Dir sources are read directly"),
        }
    }

    /// All published versions of `name` for os/arch. Dir sources read the
    /// directory; http sources read `index.json` (a JSON array of filenames).
    /// Forges have no cheap listing yet — pin exact versions for them.
    pub fn list_versions(&self, name: &str, os: Os, arch: Arch) -> Result<Vec<Version>> {
        let filenames: Vec<String> = match self {
            Source::Dir { path } => {
                let entries = std::fs::read_dir(path).map_err(|source| Error::Io {
                    path: path.clone(),
                    source,
                })?;
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            }
            Source::Http { base } => {
                let url = format!("{base}/index.json");
                let body = http_get_string(&url).map_err(|e| {
                    Error::Source(format!(
                        "cannot list versions of `{name}`: fetching {url} failed ({e}) — publish an index.json (array of image filenames) or pin an exact version"
                    ))
                })?;
                serde_json::from_str(&body)
                    .map_err(|e| Error::Source(format!("{url}: invalid index.json: {e}")))?
            }
            Source::Github { .. } | Source::Gitlab { .. } => {
                return Err(Error::Source(format!(
                    "listing versions of `{name}` from a forge source is not supported yet — pin an exact version (e.g. `{name} = \"1.2.3\"`)"
                )));
            }
        };
        let mut versions: Vec<Version> = filenames
            .iter()
            .filter_map(|f| ImageName::parse(f).ok())
            .filter(|img| img.name == name && img.os == os && img.arch == arch)
            .map(|img| img.version)
            .collect();
        versions.sort();
        versions.dedup();
        Ok(versions)
    }

    /// Fetch one package image, verify its sha256, insert into the store.
    /// Returns (digest, stored path). If `expected_digest` is given the
    /// download must match it.
    pub fn fetch(
        &self,
        image: &ImageName,
        expected_digest: Option<&str>,
        store: &Store,
    ) -> Result<(String, PathBuf)> {
        if let Some(digest) = expected_digest {
            if let Some(path) = store.image_path(digest) {
                return Ok((digest.to_string(), path));
            }
        }
        let filename = image.to_string();
        let tmp = store.root().join(format!(".fetch-{filename}"));

        match self {
            Source::Dir { path } => {
                let src = path.join(&filename);
                std::fs::copy(&src, &tmp).map_err(|source| Error::Io { path: src, source })?;
            }
            _ => {
                let url = self.url_for(&filename, &image.version);
                http_get_file(&url, &tmp)
                    .map_err(|e| Error::Source(format!("download {url} failed: {e}")))?;
            }
        }

        let digest = crate::digest::sha256_file(&tmp)?;
        if let Some(expected) = expected_digest {
            if digest != expected {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::Source(format!(
                    "digest mismatch for {filename}: expected {expected}, got {digest} — the source served different bytes than the lockfile pinned"
                )));
            }
        }
        let path = store.insert(&tmp, &digest)?;
        Ok((digest, path))
    }
}

/// RFC-1918 / localhost hosts may use plain http (hash catches tampering
/// anyway; this is hygiene).
fn is_private_host(host: &str) -> bool {
    if host == "localhost" || host == "127.0.0.1" || host == "::1" {
        return true;
    }
    let octets: Vec<u8> = host.split('.').filter_map(|p| p.parse().ok()).collect();
    match octets.as_slice() {
        [10, ..] => true,
        [172, b, ..] => (16..=31).contains(b),
        [192, 168, ..] => true,
        [127, ..] => true,
        _ => false,
    }
}

fn http_get_string(url: &str) -> std::result::Result<String, String> {
    let mut response = ureq::get(url).call().map_err(|e| e.to_string())?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

fn http_get_file(url: &str, dest: &Path) -> std::result::Result<(), String> {
    let mut response = ureq::get(url).call().map_err(|e| e.to_string())?;
    let mut reader = response.body_mut().as_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_kinds() {
        assert!(matches!(
            Source::parse("github:someorg/ffmpeg-pkg", false).unwrap(),
            Source::Github { .. }
        ));
        assert!(matches!(
            Source::parse("file:///srv/pkgs", false).unwrap(),
            Source::Dir { .. }
        ));
        assert!(matches!(
            Source::parse("https://artifacts.corp.net/ply", false).unwrap(),
            Source::Http { .. }
        ));
    }

    #[test]
    fn http_policy() {
        assert!(Source::parse("http://localhost:8000", false).is_ok());
        assert!(Source::parse("http://192.168.1.10/pkgs", false).is_ok());
        assert!(Source::parse("http://example.com/pkgs", false).is_err());
        assert!(Source::parse("http://example.com/pkgs", true).is_ok());
    }

    #[test]
    fn github_url_template() {
        let source = Source::parse("github:org/repo", false).unwrap();
        let image = ImageName::parse("ffmpeg-6.1.0-linux-x64.img").unwrap();
        assert_eq!(
            source.url_for(&image.to_string(), &image.version),
            "https://github.com/org/repo/releases/download/v6.1.0/ffmpeg-6.1.0-linux-x64.img"
        );
    }
}
