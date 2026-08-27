//! GitHub release assets as a deployment source (CD lane 3).
//!
//! The user's CI builds the .img and attaches it to a release (the repo's
//! own release workflow); the droplet pulls it. Latest-version discovery is
//! pull-based and tokenless for public repos: `releases/latest/download/…`
//! answers with a redirect whose Location names the tag — one HEAD request,
//! no API, no rate limit. Private repos use the REST API with a
//! fine-grained PAT read from a root-owned file.

use std::io::Read;
use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::store::Store;

/// The newest release version (without the leading `v`).
pub fn latest_version(repo: &str, token: Option<&str>) -> Result<String> {
    if let Some(token) = token {
        // private (or just authenticated): the API names the tag
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        let body = ureq::get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("User-Agent", "ply")
            .call()
            .map_err(|e| Error::Source(format!("GET {url}: {e}")))?
            .body_mut()
            .read_to_string()
            .map_err(|e| Error::Source(format!("{url}: {e}")))?;
        return tag_from_json(&body)
            .ok_or_else(|| Error::Source(format!("{url}: no tag_name in response")));
    }
    // public: the redirect names the tag — no API, no rate limit
    let url = format!("https://github.com/{repo}/releases/latest/download/probe");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .build()
        .into();
    let response = agent
        .head(&url)
        .call()
        .map_err(|e| Error::Source(format!("HEAD {url}: {e}")))?;
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            Error::Source(format!(
                "{repo}: releases/latest did not redirect — no releases yet, or the repo is private (set token_file)"
            ))
        })?;
    version_from_location(location).ok_or_else(|| {
        Error::Source(format!(
            "{repo}: cannot read a version out of redirect `{location}`"
        ))
    })
}

/// `…/releases/download/v0.1.3/probe` → `0.1.3`
fn version_from_location(location: &str) -> Option<String> {
    let idx = location.find("/releases/download/")?;
    let rest = &location[idx + "/releases/download/".len()..];
    let tag = rest.split('/').next()?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty() {
        return None;
    }
    Some(version.to_string())
}

fn tag_from_json(body: &str) -> Option<String> {
    // one field from a known shape; a JSON dependency would be overkill
    let idx = body.find("\"tag_name\"")?;
    let rest = &body[idx..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let quoted = after.strip_prefix('"')?;
    let end = quoted.find('"')?;
    let tag = &quoted[..end];
    Some(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

/// The newest release whose tag starts with `prefix` — monorepos carry
/// several release streams in one repo (`web-v0.3.4` beside `v0.1.37`).
/// Conditional requests keep anonymous polling free: a 304 answer does not
/// count against GitHub's rate limit, and the steady state is all 304s.
/// `cache_file` holds two lines: the ETag and the last answer.
pub fn latest_version_matching(
    repo: &str,
    prefix: &str,
    token: Option<&str>,
    cache_file: &std::path::Path,
) -> Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    let cached = std::fs::read_to_string(cache_file).ok();
    let (cached_etag, cached_version) = match cached.as_deref().and_then(|s| s.split_once('\n')) {
        Some((e, v)) if !v.trim().is_empty() => {
            (Some(e.trim().to_string()), Some(v.trim().to_string()))
        }
        _ => (None, None),
    };

    let mut request = ureq::get(&url).header("User-Agent", "ply");
    if let Some(token) = token {
        request = request.header("Authorization", &format!("Bearer {token}"));
    }
    if let Some(etag) = &cached_etag {
        request = request.header("If-None-Match", etag);
    }
    let mut response = match request.call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(304)) => {
            return cached_version
                .ok_or_else(|| Error::Source(format!("{url}: 304 with an empty cache")));
        }
        Err(e) => return Err(Error::Source(format!("GET {url}: {e}"))),
    };
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Source(format!("{url}: {e}")))?;
    let releases: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| Error::Source(format!("{url}: invalid JSON: {e}")))?;
    let version = releases
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("tag_name")?.as_str()?.strip_prefix(prefix))
        .filter_map(|v| semver::Version::parse(v).ok())
        .max()
        .ok_or_else(|| {
            Error::Source(format!(
                "{repo}: no release tagged `{prefix}<x.y.z>` among the newest 30"
            ))
        })?
        .to_string();
    let _ = std::fs::write(cache_file, format!("{etag}\n{version}\n"));
    Ok(version)
}

/// Download `<app>-<version>-linux-<arch>.img` from the release named by
/// `tag` into the store (sha256-addressed). Public via the direct asset
/// URL; with a token via the API (asset id + octet-stream), which private
/// repos require.
pub fn fetch_asset(
    repo: &str,
    app: &str,
    version: &str,
    tag: &str,
    token: Option<&str>,
    store: &Store,
) -> Result<(PathBuf, ImageName)> {
    let semver: semver::Version = version
        .parse()
        .map_err(|e| Error::Source(format!("version `{version}`: {e}")))?;
    let image = ImageName::new(app, semver, Os::Linux, Arch::host())?;
    let filename = image.to_string();

    let tmp = store
        .root()
        .join(format!(".github-{filename}.{}", std::process::id()));
    let result = (|| -> Result<()> {
        match token {
            None => {
                let url = format!("https://github.com/{repo}/releases/download/{tag}/{filename}");
                stream_to_file(ureq::get(&url).header("User-Agent", "ply"), &url, &tmp)
            }
            Some(token) => {
                // resolve the asset id, then download as octet-stream
                let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
                let body = ureq::get(&url)
                    .header("Authorization", &format!("Bearer {token}"))
                    .header("User-Agent", "ply")
                    .call()
                    .map_err(|e| Error::Source(format!("GET {url}: {e}")))?
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| Error::Source(format!("{url}: {e}")))?;
                let id = asset_id(&body, &filename).ok_or_else(|| {
                    Error::Source(format!("release {tag} has no asset `{filename}`"))
                })?;
                let url = format!("https://api.github.com/repos/{repo}/releases/assets/{id}");
                stream_to_file(
                    ureq::get(&url)
                        .header("Authorization", &format!("Bearer {token}"))
                        .header("Accept", "application/octet-stream")
                        .header("User-Agent", "ply"),
                    &url,
                    &tmp,
                )
            }
        }
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    let digest = crate::digest::sha256_file(&tmp)?;
    let path = store.insert(&tmp, &digest)?;
    Ok((path, image))
}

/// `"name":"<filename>"` preceded (in the same asset object) by `"id":N —
/// walk assets in order, remembering the last id seen before the name hits.
fn asset_id(body: &str, filename: &str) -> Option<u64> {
    let needle = format!("\"name\":\"{filename}\"");
    let normalized = body.replace(": ", ":");
    let name_at = normalized.find(&needle)?;
    let before = &normalized[..name_at];
    let id_at = before.rfind("\"id\":")?;
    let digits: String = before[id_at + 5..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn stream_to_file(
    request: ureq::RequestBuilder<ureq::typestate::WithoutBody>,
    url: &str,
    dest: &std::path::Path,
) -> Result<()> {
    let mut response = request
        .call()
        .map_err(|e| Error::Source(format!("download {url}: {e}")))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = std::fs::File::create(dest).map_err(|source| Error::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| Error::Source(format!("read {url}: {e}")))?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n]).map_err(|source| Error::Io {
            path: dest.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Network test — run explicitly: cargo test -p ply-core github -- --ignored
    #[test]
    #[ignore]
    fn live_latest_version_via_redirect() {
        let v = latest_version("iluxav/ply-dashboard", None).unwrap();
        assert!(v.starts_with("0."), "got {v}");
        eprintln!("latest ply-dashboard release: {v}");
    }

    #[test]
    fn version_out_of_redirect() {
        assert_eq!(
            version_from_location(
                "https://github.com/iluxav/ply-dashboard/releases/download/v0.1.3/probe"
            ),
            Some("0.1.3".to_string())
        );
        assert_eq!(
            version_from_location("/releases/download/2.0/x"),
            Some("2.0".into())
        );
        assert_eq!(version_from_location("https://github.com/x/y"), None);
    }

    #[test]
    fn tag_out_of_api_json() {
        assert_eq!(
            tag_from_json(r#"{"url":"…","tag_name": "v1.2.3","name":"x"}"#),
            Some("1.2.3".to_string())
        );
        assert_eq!(tag_from_json("{}"), None);
    }

    #[test]
    fn asset_id_out_of_api_json() {
        let body = r#"{"assets":[{"id":111,"name":"other.img"},{"id":222,"name":"app-1.0.0-linux-x64.img"}]}"#;
        assert_eq!(asset_id(body, "app-1.0.0-linux-x64.img"), Some(222));
        assert_eq!(asset_id(body, "missing.img"), None);
    }
}
