//! Package catalog: the optional `state.json` a source publishes next to its
//! packages, read by `ply search` / `ply add` / `ply init`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::source::{http_get_string, Source};
use crate::{Error, Result};

/// The official registry, as a `[sources]` template.
pub const OFFICIAL_SOURCE: &str = "https://registry.plybox.sh/ply/{package}";

/// The file a source publishes at its prefix. Same shape the website reads.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Catalog {
    #[serde(default)]
    pub packages: Vec<Package>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Package {
    #[serde(default)]
    pub namespace: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub versions: Vec<ImageVersion>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ImageVersion {
    pub version: String,
    pub img: String,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub pushed_at: String,
}

impl ImageVersion {
    /// Explicit `arch` if present, else derived from the canonical filename
    /// (`…-linux-arm64.img`), exactly as the website does.
    pub fn arch(&self) -> &str {
        match self.arch.as_deref() {
            Some(a) if !a.is_empty() => a,
            _ if self.img.ends_with("-arm64.img") => "arm64",
            _ => "x64",
        }
    }
}

/// Where a source's catalog lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogLocation {
    Url(String),
    Path(PathBuf),
}

impl CatalogLocation {
    /// The prefix (location minus `/state.json`) — what error messages name.
    pub fn prefix(&self) -> String {
        match self {
            CatalogLocation::Url(u) => u.trim_end_matches("/state.json").to_string(),
            CatalogLocation::Path(p) => p
                .parent()
                .map(|d| d.display().to_string())
                .unwrap_or_default(),
        }
    }

    fn display(&self) -> String {
        match self {
            CatalogLocation::Url(u) => u.clone(),
            CatalogLocation::Path(p) => p.display().to_string(),
        }
    }
}

/// Template → prefix: drop `/{package}` and anything after it.
fn prefix_of(template: &str) -> String {
    let cut = match template.find("/{package}") {
        Some(i) => &template[..i],
        None => template,
    };
    cut.trim_end_matches('/').to_string()
}

impl Source {
    pub fn catalog_location(&self) -> Result<CatalogLocation> {
        match self {
            Source::Http { base } => Ok(CatalogLocation::Url(format!("{}/state.json", prefix_of(base)))),
            Source::Dir { path } => {
                let prefix = prefix_of(&path.to_string_lossy());
                Ok(CatalogLocation::Path(PathBuf::from(prefix).join("state.json")))
            }
            Source::Github { org, repo } => Err(Error::Source(format!(
                "searching a forge source (github:{org}/{repo}) is not supported — pin a version"
            ))),
            Source::Gitlab { group, project } => Err(Error::Source(format!(
                "searching a forge source (gitlab:{group}/{project}) is not supported — pin a version"
            ))),
        }
    }
}

impl Catalog {
    /// Parse catalog JSON; `location` is only used in the error message.
    pub fn parse(text: &str, location: &str) -> Result<Catalog> {
        serde_json::from_str(text)
            .map_err(|e| Error::Source(format!("{location}: invalid state.json: {e}")))
    }

    /// Fetch and parse the catalog a source publishes at its prefix.
    pub fn load(source: &Source) -> Result<Catalog> {
        let loc = source.catalog_location()?;
        let missing = || {
            Error::Source(format!(
                "source {} publishes no catalog (state.json) — browse https://plybox.sh/registry/ or pin a version",
                loc.prefix()
            ))
        };
        let text = match &loc {
            CatalogLocation::Path(p) => match std::fs::read_to_string(p) {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
                Err(e) => {
                    return Err(Error::Io {
                        path: p.clone(),
                        source: e,
                    })
                }
            },
            CatalogLocation::Url(u) => match http_get_string(u) {
                Ok(t) => t,
                Err(e) if e.contains("404") => return Err(missing()),
                Err(e) => return Err(Error::Source(format!("fetching {u} failed ({e})"))),
            },
        };
        Catalog::parse(&text, &loc.display())
    }

    /// Exact-name lookup.
    pub fn get(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }
}

/// Sort key: real semver first (numeric), anything else lexical after it.
pub fn version_key(v: &str) -> (Option<semver::Version>, String) {
    (semver::Version::parse(v).ok(), v.to_string())
}

/// One row of `ply search --versions`: a version with every arch it ships on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow<'a> {
    pub version: &'a str,
    pub arches: Vec<&'a str>,
    pub bytes: u64,
}

fn arch_order(a: &str) -> u8 {
    match a {
        "x64" => 0,
        "arm64" => 1,
        _ => 2,
    }
}

impl Package {
    /// The highest version published (any arch).
    pub fn latest(&self) -> Option<&ImageVersion> {
        self.versions.iter().max_by_key(|v| version_key(&v.version))
    }

    /// The dependency range for the latest version: `major.minor`.
    pub fn range_of_latest(&self) -> Option<String> {
        let latest = self.latest()?;
        Some(match semver::Version::parse(&latest.version) {
            Ok(v) => format!("{}.{}", v.major, v.minor),
            Err(_) => latest.version.clone(),
        })
    }

    /// Arches the latest version ships on, `x64` before `arm64`.
    pub fn arches_of_latest(&self) -> Vec<&str> {
        let Some(latest) = self.latest() else {
            return Vec::new();
        };
        let mut arches: Vec<&str> = self
            .versions
            .iter()
            .filter(|v| v.version == latest.version)
            .map(|v| v.arch())
            .collect();
        arches.sort_by_key(|a| arch_order(a));
        arches.dedup();
        arches
    }

    /// Every version, newest first, arches merged, size = largest image.
    pub fn versions_desc(&self) -> Vec<VersionRow<'_>> {
        let mut rows: Vec<VersionRow<'_>> = Vec::new();
        for v in &self.versions {
            match rows.iter_mut().find(|r| r.version == v.version) {
                Some(row) => {
                    row.arches.push(v.arch());
                    row.bytes = row.bytes.max(v.bytes);
                }
                None => rows.push(VersionRow {
                    version: &v.version,
                    arches: vec![v.arch()],
                    bytes: v.bytes,
                }),
            }
        }
        for row in &mut rows {
            row.arches.sort_by_key(|a| arch_order(a));
            row.arches.dedup();
        }
        rows.sort_by_key(|r| std::cmp::Reverse(version_key(r.version)));
        rows
    }
}

fn rank(p: &Package, q: &str) -> Option<u8> {
    let name = p.name.to_lowercase();
    if q.is_empty() || name == q {
        Some(0)
    } else if name.starts_with(q) {
        Some(1)
    } else if name.contains(q) {
        Some(2)
    } else if p.description.to_lowercase().contains(q) {
        Some(3)
    } else {
        None
    }
}

impl Catalog {
    /// Case-insensitive substring search over name and description, ranked:
    /// exact name, name prefix, name substring, description match; then by name.
    pub fn search(&self, query: &str) -> Vec<&Package> {
        let q = query.trim().to_lowercase();
        let mut hits: Vec<(u8, &Package)> = self
            .packages
            .iter()
            .filter_map(|p| rank(p, &q).map(|r| (r, p)))
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        hits.into_iter().map(|(_, p)| p).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Source;

    const SAMPLE: &str = include_str!("../tests/fixtures/state.sample.json");

    #[test]
    fn catalog_location_follows_the_source_prefix() {
        let cases = [
            (
                "https://registry.plybox.sh/ply/{package}",
                "https://registry.plybox.sh/ply/state.json",
            ),
            (
                "https://artifacts.corp.net/ply",
                "https://artifacts.corp.net/ply/state.json",
            ),
            (
                "https://h.example/{package}/sub",
                "https://h.example/state.json",
            ),
        ];
        for (spec, want) in cases {
            let loc = Source::parse(spec, false)
                .unwrap()
                .catalog_location()
                .unwrap();
            assert_eq!(loc, CatalogLocation::Url(want.into()), "{spec}");
        }
        let dir = Source::parse("file:///srv/pkgs/{package}", false).unwrap();
        assert_eq!(
            dir.catalog_location().unwrap(),
            CatalogLocation::Path("/srv/pkgs/state.json".into())
        );
        let dir = Source::parse("file:///srv/pkgs", false).unwrap();
        assert_eq!(
            dir.catalog_location().unwrap(),
            CatalogLocation::Path("/srv/pkgs/state.json".into())
        );
    }

    #[test]
    fn forge_sources_have_no_catalog() {
        let err = Source::parse("github:org/repo", false)
            .unwrap()
            .catalog_location()
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "source error: searching a forge source (github:org/repo) is not supported — pin a version"
        );
    }

    #[test]
    fn prefix_strips_state_json() {
        assert_eq!(
            CatalogLocation::Url("https://h/ply/state.json".into()).prefix(),
            "https://h/ply"
        );
        assert_eq!(
            CatalogLocation::Path("/srv/pkgs/state.json".into()).prefix(),
            "/srv/pkgs"
        );
    }

    #[test]
    fn parses_the_real_catalog_shape() {
        let cat = Catalog::parse(SAMPLE, "fixture").unwrap();
        let ffmpeg = cat.get("ffmpeg").expect("ffmpeg in fixture");
        assert_eq!(ffmpeg.namespace, "ply");
        assert!(!ffmpeg.versions.is_empty());
        assert!(ffmpeg.versions[0].img.ends_with(".img"));
        assert!(cat.get("nope").is_none());
    }

    #[test]
    fn minimal_catalog_needs_only_name_version_img() {
        let cat = Catalog::parse(
            r#"{"packages":[{"name":"jq","versions":[{"version":"1.7.1","img":"jq-1.7.1-linux-arm64.img"}]}]}"#,
            "mini",
        )
        .unwrap();
        let jq = cat.get("jq").unwrap();
        assert_eq!(jq.description, "");
        assert_eq!(jq.versions[0].bytes, 0);
        assert_eq!(
            jq.versions[0].arch(),
            "arm64",
            "arch derives from the filename"
        );
    }

    #[test]
    fn explicit_arch_wins_over_filename() {
        let v = ImageVersion {
            version: "1.0.0".into(),
            img: "x-1.0.0-linux-arm64.img".into(),
            arch: Some("x64".into()),
            path: String::new(),
            bytes: 0,
            pushed_at: String::new(),
        };
        assert_eq!(v.arch(), "x64");
    }

    #[test]
    fn invalid_json_names_the_location() {
        let err = Catalog::parse("{nope", "https://h/ply/state.json")
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("source error: https://h/ply/state.json: invalid state.json: "),
            "{err}"
        );
    }

    #[test]
    fn loads_from_a_directory_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("state.json"), SAMPLE).unwrap();
        let spec = format!("file://{}/{{package}}", dir.path().display());
        let cat = Catalog::load(&Source::parse(&spec, false).unwrap()).unwrap();
        assert!(cat.get("alpine").is_some());
    }

    #[test]
    fn missing_catalog_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let spec = format!("file://{}/{{package}}", dir.path().display());
        let err = Catalog::load(&Source::parse(&spec, false).unwrap())
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "source error: source {} publishes no catalog (state.json) — browse https://plybox.sh/registry/ or pin a version",
                dir.path().display()
            )
        );
    }

    fn pkg(name: &str, desc: &str, versions: &[(&str, &str)]) -> Package {
        Package {
            name: name.into(),
            description: desc.into(),
            versions: versions
                .iter()
                .map(|(v, img)| ImageVersion {
                    version: (*v).into(),
                    img: (*img).into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn latest_is_the_highest_semver_across_arches() {
        let p = pkg(
            "ffmpeg",
            "",
            &[
                ("6.0.1", "ffmpeg-6.0.1-linux-x64.img"),
                ("6.1.1", "ffmpeg-6.1.1-linux-arm64.img"),
                ("6.1.1", "ffmpeg-6.1.1-linux-x64.img"),
                ("10.0.0", "ffmpeg-10.0.0-linux-x64.img"),
            ],
        );
        assert_eq!(
            p.latest().unwrap().version,
            "10.0.0",
            "numeric, not lexical"
        );
        assert_eq!(p.range_of_latest().unwrap(), "10.0");
        let p = pkg(
            "ffmpeg",
            "",
            &[
                ("6.0.1", "a-x64.img"),
                ("6.1.1", "b-arm64.img"),
                ("6.1.1", "c-x64.img"),
            ],
        );
        assert_eq!(
            p.arches_of_latest(),
            vec!["x64", "arm64"],
            "x64 sorts first"
        );
    }

    #[test]
    fn versions_desc_merges_arches_and_takes_the_largest_image() {
        let mut p = pkg(
            "jq",
            "",
            &[
                ("1.7.1", "jq-1.7.1-linux-x64.img"),
                ("1.7.1", "jq-1.7.1-linux-arm64.img"),
                ("1.6.0", "jq-1.6.0-linux-x64.img"),
            ],
        );
        p.versions[0].bytes = 500;
        p.versions[1].bytes = 700;
        let rows = p.versions_desc();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].version, "1.7.1");
        assert_eq!(rows[0].arches, vec!["x64", "arm64"]);
        assert_eq!(rows[0].bytes, 700);
        assert_eq!(rows[1].version, "1.6.0");
    }

    #[test]
    fn empty_package_has_no_latest() {
        assert!(pkg("x", "", &[]).latest().is_none());
        assert!(pkg("x", "", &[]).range_of_latest().is_none());
    }

    #[test]
    fn search_ranks_exact_prefix_substring_then_description() {
        let cat = Catalog {
            packages: vec![
                pkg("libavcodec", "Codec library used by ffmpeg", &[]),
                pkg("ffmpeg-libs", "FFmpeg shared libraries", &[]),
                pkg("ffmpeg", "Multimedia framework", &[]),
                pkg("gst-ffmpeg", "GStreamer ffmpeg plugin", &[]),
                pkg("zlib", "Compression", &[]),
            ],
        };
        let names: Vec<&str> = cat
            .search("FFmpeg")
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["ffmpeg", "ffmpeg-libs", "gst-ffmpeg", "libavcodec"]
        );
        assert!(cat.search("nothing-here").is_empty());
        assert_eq!(cat.search("").len(), 5, "empty query lists everything");
    }
}
