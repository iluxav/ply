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
    /// Http bases may contain `{package}` — the registry-per-repo layout
    /// (`https://host/user/{package}/<file>`), the local preview of
    /// GitHub-style user/repo coordinates.
    fn url_for(&self, package: &str, filename: &str, version: &Version) -> String {
        match self {
            Source::Github { org, repo } => {
                format!("https://github.com/{org}/{repo}/releases/download/v{version}/{filename}")
            }
            Source::Gitlab { group, project } => format!(
                "https://gitlab.com/{group}/{project}/-/releases/v{version}/downloads/{filename}"
            ),
            Source::Http { base } => {
                format!("{}/{filename}", base.replace("{package}", package))
            }
            Source::Dir { .. } => unreachable!("Dir sources are read directly"),
        }
    }

    /// All published versions of `name` for os/arch. Dir sources read the
    /// directory; http sources read `index.json` (a JSON array of filenames).
    /// Forges have no cheap listing yet — pin exact versions for them.
    pub fn list_versions(&self, name: &str, os: Os, arch: Arch) -> Result<Vec<Version>> {
        let filenames: Vec<String> = match self {
            Source::Dir { path } => {
                let dir = PathBuf::from(path.to_string_lossy().replace("{package}", name));
                let entries = std::fs::read_dir(&dir).map_err(|source| Error::Io {
                    path: dir.clone(),
                    source,
                })?;
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            }
            Source::Http { base } => {
                let url = format!("{}/index.json", base.replace("{package}", name));
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
        // Unique per fetch: concurrent fetchers of the same image (parallel
        // builds sharing a store) must never write through one temp path —
        // they'd hash each other's half-written bytes.
        static FETCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = store.root().join(format!(
            ".fetch-{filename}.{}.{}",
            std::process::id(),
            FETCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));

        match self {
            Source::Dir { path } => {
                let src = PathBuf::from(path.to_string_lossy().replace("{package}", &image.name))
                    .join(&filename);
                std::fs::copy(&src, &tmp).map_err(|source| Error::Io { path: src, source })?;
            }
            _ => {
                let url = self.url_for(&image.name, &filename, &image.version);
                if let Err(e) = http_get_file(&url, &tmp) {
                    let _ = std::fs::remove_file(&tmp); // no partial downloads left behind
                    return Err(Error::Source(format!("download {url} failed: {e}")));
                }
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

/// Fetch a plain image URL — `https://…/<name>-<version>-<os>-<arch>.img` —
/// into the store and return (path, parsed name). The basename is the
/// contract: it must parse as an image filename and match this host's
/// platform. A marker under `<data>/urls/` maps sha256(URL) → the digest it
/// served, so a versioned artifact URL (immutable by convention) is fetched
/// once and reconcile's minute loop stays off the network; deploying a new
/// version means naming a new URL. Plain http is private-hosts-only, the
/// same hygiene as sources.
pub fn fetch_url_image(url: &str) -> Result<(PathBuf, ImageName)> {
    let filename = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("");
    let image = ImageName::parse(filename).map_err(|_| {
        Error::Source(format!(
            "`{url}`: an image URL must end in `<name>-<version>-<os>-<arch>.img` (got `{filename}`)"
        ))
    })?;
    if image.os != Os::Linux || image.arch != Arch::host() {
        return Err(Error::Source(format!(
            "{filename}: built for {}-{}, this host is {}-{}",
            image.os.as_str(),
            image.arch.as_str(),
            Os::Linux.as_str(),
            Arch::host().as_str(),
        )));
    }
    if let Some(host_part) = url.strip_prefix("http://") {
        let host = host_part.split(['/', ':']).next().unwrap_or("");
        if !is_private_host(host) {
            return Err(Error::Source(format!(
                "plain http image URL `{url}` on a public host — use https://"
            )));
        }
    } else if !url.starts_with("https://") {
        return Err(Error::Source(format!("`{url}`: expected an http(s) URL")));
    }

    let store = Store::open_default()?;
    let markers = crate::paths::data_dir().join("urls");
    let marker = markers.join(crate::digest::sha256_str(url));
    if let Ok(digest) = std::fs::read_to_string(&marker) {
        if let Some(path) = store.image_path(digest.trim()) {
            return Ok((path, image));
        }
    }

    let tmp = store
        .root()
        .join(format!(".url-{filename}.{}", std::process::id()));
    if let Err(e) = http_get_file(url, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(Error::Source(format!("download {url} failed: {e}")));
    }
    let digest = crate::digest::sha256_file(&tmp)?;
    let path = store.insert(&tmp, &digest)?;
    if std::fs::create_dir_all(&markers).is_ok() {
        let _ = std::fs::write(&marker, &digest);
    }
    Ok((path, image))
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

pub(crate) fn http_get_string(url: &str) -> std::result::Result<String, String> {
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

    /// N threads fetch the SAME image from a dir source into one store —
    /// the fleet case. Fixed `.fetch-<name>` temp names made concurrent
    /// fetchers hash each other's half-written bytes (poisoned store
    /// entries) or lose the file mid-operation.
    #[test]
    fn concurrent_fetches_of_same_image_are_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let pkgs = tmp.path().join("pkgs");
        std::fs::create_dir_all(&pkgs).unwrap();
        let img = crate::image::name::ImageName::new(
            "thing",
            semver::Version::new(1, 0, 0),
            crate::image::name::Os::Linux,
            crate::image::name::Arch::X64,
        )
        .unwrap();
        let bytes = vec![42u8; 400_000];
        std::fs::write(pkgs.join(img.to_string()), &bytes).unwrap();
        let expected = crate::digest::sha256_file(&pkgs.join(img.to_string())).unwrap();

        let store_root = tmp.path().join("store");
        std::fs::create_dir_all(&store_root).unwrap();
        let src = format!("file://{}", pkgs.display());

        let mut handles = Vec::new();
        for _ in 0..8 {
            let (src, img, store_root) = (src.clone(), img.clone(), store_root.clone());
            handles.push(std::thread::spawn(move || {
                let source = Source::parse(&src, false).unwrap();
                source.fetch(&img, None, &crate::store::Store::at(store_root))
            }));
        }
        for h in handles {
            let (digest, path) = h.join().unwrap().expect("fetch must not race to failure");
            assert_eq!(digest, expected, "hashed someone else's partial bytes");
            assert!(path.exists());
        }
        let stored = store_root.join(&expected).join("pkg.img");
        assert_eq!(std::fs::read(stored).unwrap().len(), bytes.len());
    }

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

    /// Validation happens before any network or store access, so bad URLs
    /// fail fast with a message naming the contract.
    #[test]
    fn url_image_validation() {
        let host_arch = Arch::host().as_str();
        let other = if host_arch == "x64" { "arm64" } else { "x64" };
        // basename must parse as an image filename
        let err = fetch_url_image("https://x.example/readme.txt").unwrap_err();
        assert!(err.to_string().contains("must end in"), "{err}");
        // wrong arch is refused
        let err =
            fetch_url_image(&format!("https://x.example/a-1.0.0-linux-{other}.img")).unwrap_err();
        assert!(err.to_string().contains("this host"), "{err}");
        // plain http on a public host is refused
        let err = fetch_url_image(&format!("http://x.example/a-1.0.0-linux-{host_arch}.img"))
            .unwrap_err();
        assert!(err.to_string().contains("use https"), "{err}");
        // query strings don't confuse the basename
        let err = fetch_url_image("https://x.example/dir/?download=1").unwrap_err();
        assert!(err.to_string().contains("must end in"), "{err}");
    }

    #[test]
    fn github_url_template() {
        let source = Source::parse("github:org/repo", false).unwrap();
        let image = ImageName::parse("ffmpeg-6.1.0-linux-x64.img").unwrap();
        assert_eq!(
            source.url_for("ffmpeg", &image.to_string(), &image.version),
            "https://github.com/org/repo/releases/download/v6.1.0/ffmpeg-6.1.0-linux-x64.img"
        );
    }
}
