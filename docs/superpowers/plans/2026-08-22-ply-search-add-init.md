# `ply search` / `ply add` / `ply init` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a developer discover packages, add them to `ply.toml`, and scaffold a manifest from the terminal — cargo-style — without inventing a registry protocol.

**Architecture:** A static `state.json` catalog next to a source's packages (same schema the website already consumes) is parsed by a new `ply_core::catalog` module; three thin CLI commands (`search`, `add`, `init`) sit on top. `add`/`init` edit TOML with `toml_edit` so user formatting survives. The publisher gains a per-namespace copy of the catalog.

**Tech Stack:** Rust 2021 (workspace: `ply-core` lib, `ply-cli` bin), `clap` derive, `serde`/`serde_json`, `semver`, `ureq` (already used), `toml_edit 0.22` (new direct dep), `tempfile` (tests). Node for `scripts/registry-push.mjs`.

**Spec:** `docs/superpowers/specs/2026-08-22-ply-search-add-design.md`

## Global Constraints

- **Never commit.** The repository owner commits; each task ends with tests green and the working tree left for them. (Project rule.)
- Official source constant, verbatim: `https://registry.plybox.sh/ply/{package}`.
- Catalog location = source prefix: template with `/{package}` and everything after removed; no placeholder → the base itself; forges → error.
- Required catalog fields only: `packages[].name`, `packages[].versions[].version`, `packages[].versions[].img`. Everything else defaults.
- Dependency range shown/written = latest version's `major.minor` (e.g. `6.1`).
- Error strings exactly as spec §6.
- Exit 0 on "no matches"; exit 1 on missing/invalid catalog, network error, bad args.
- Commands run from the working tree: `cargo test -p ply-core`, `cargo test -p ply-cli`, `cargo clippy --workspace`, `cargo fmt --all`.
- Unit tests never touch the network; use `file://` sources with `tempfile`.
- Built-in fallback versions for `init`: alpine `3.20`, python3 `3.12`, node `22`.

---

## File structure

| File | Responsibility |
|---|---|
| `ply-core/src/catalog.rs` (new) | Catalog types, parsing, location-from-source, loading, search ranking, per-package helpers, `OFFICIAL_SOURCE` |
| `ply-core/src/source.rs` (modify) | expose `http_get_string` as `pub(crate)` |
| `ply-core/src/lib.rs` (modify) | `pub mod catalog;` |
| `ply-core/tests/fixtures/state.sample.json` (new) | trimmed real catalog for parse tests |
| `ply-cli/src/commands/search.rs` (new) | `ply search`: source resolution (shared), rendering, JSON |
| `ply-cli/src/commands/add.rs` (new) | `ply add`: manifest edit via `toml_edit` |
| `ply-cli/src/commands/init.rs` (new) | `ply init`: detect, prompt, render, write |
| `ply-cli/src/commands/mod.rs` (modify) | dispatch |
| `ply-cli/src/cli.rs` (modify) | `Init`, `Search`, `Add` subcommands + args |
| `Cargo.toml`, `ply-cli/Cargo.toml` (modify) | `toml_edit`, `tempfile` dev-dep |
| `scripts/registry-push.mjs` (modify) | per-namespace `state.json` |
| `docs/cli.md`, `docs/registries.md`, `docs/image-format.md`, `docs/docker.md`, `docs/quickstart.md` (modify) | user docs |

---

### Task 1: Catalog types, parsing, and location (ply-core)

**Files:**
- Create: `ply-core/src/catalog.rs`
- Create: `ply-core/tests/fixtures/state.sample.json`
- Modify: `ply-core/src/lib.rs` (add `pub mod catalog;` after `pub mod bundle;`)
- Modify: `ply-core/src/source.rs:209` (`fn http_get_string` → `pub(crate) fn http_get_string`)

**Interfaces:**
- Consumes: `crate::source::Source` (enum `Github{org,repo}`, `Gitlab{group,project}`, `Http{base}`, `Dir{path}`), `crate::{Error, Result}`, `crate::source::http_get_string(url) -> Result<String, String>`.
- Produces:
  - `pub const OFFICIAL_SOURCE: &str`
  - `pub struct Catalog { pub packages: Vec<Package> }`
  - `pub struct Package { pub namespace: String, pub name: String, pub description: String, pub license: String, pub homepage: String, pub versions: Vec<ImageVersion> }`
  - `pub struct ImageVersion { pub version: String, pub img: String, pub arch: Option<String>, pub path: String, pub bytes: u64, pub pushed_at: String }` + `pub fn arch(&self) -> &str`
  - `pub enum CatalogLocation { Url(String), Path(PathBuf) }` + `pub fn prefix(&self) -> String`
  - `impl Source { pub fn catalog_location(&self) -> Result<CatalogLocation> }`
  - `impl Catalog { pub fn parse(text: &str, location: &str) -> Result<Catalog>; pub fn load(source: &Source) -> Result<Catalog>; pub fn get(&self, name: &str) -> Option<&Package> }`

- [ ] **Step 1: Create the fixture from the live catalog (trimmed to 6 packages)**

Run:
```bash
mkdir -p ply-core/tests/fixtures
curl -s https://registry.plybox.sh/state.json | python3 -c '
import json,sys
d=json.load(sys.stdin)
keep={"alpine","python3","node","ffmpeg","jq","postgresql16"}
d["packages"]=[p for p in d["packages"] if p["name"] in keep]
d["package_count"]=len(d["packages"]); d["image_count"]=sum(len(p["versions"]) for p in d["packages"])
json.dump(d,open("ply-core/tests/fixtures/state.sample.json","w"),indent=1)
print([p["name"] for p in d["packages"]])'
```
Expected: prints the kept names (at least `alpine`, `python3`, `ffmpeg`, `jq`). If `node` is not in the registry yet, that is fine — tests below only rely on `alpine`, `python3`, `ffmpeg`, `jq`.

- [ ] **Step 2: Write the failing tests**

Create `ply-core/src/catalog.rs` containing only the test module for now:

```rust
//! Package catalog: the optional `state.json` a source publishes next to its
//! packages, read by `ply search` / `ply add` / `ply init`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Source;

    const SAMPLE: &str = include_str!("../tests/fixtures/state.sample.json");

    #[test]
    fn catalog_location_follows_the_source_prefix() {
        let cases = [
            ("https://registry.plybox.sh/ply/{package}", "https://registry.plybox.sh/ply/state.json"),
            ("https://artifacts.corp.net/ply", "https://artifacts.corp.net/ply/state.json"),
            ("https://h.example/{package}/sub", "https://h.example/state.json"),
        ];
        for (spec, want) in cases {
            let loc = Source::parse(spec, false).unwrap().catalog_location().unwrap();
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
        assert_eq!(CatalogLocation::Path("/srv/pkgs/state.json".into()).prefix(), "/srv/pkgs");
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
        assert_eq!(jq.versions[0].arch(), "arm64", "arch derives from the filename");
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
        let err = Catalog::parse("{nope", "https://h/ply/state.json").unwrap_err().to_string();
        assert!(err.starts_with("source error: https://h/ply/state.json: invalid state.json: "), "{err}");
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
        let err = Catalog::load(&Source::parse(&spec, false).unwrap()).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "source error: source {} publishes no catalog (state.json) — browse https://plybox.sh/registry/ or pin a version",
                dir.path().display()
            )
        );
    }
}
```

Add `pub mod catalog;` to `ply-core/src/lib.rs` (alphabetical, after `pub mod bundle;`).

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p ply-core catalog`
Expected: compile errors — `Catalog`, `CatalogLocation`, `ImageVersion`, `catalog_location` not found.

- [ ] **Step 4: Implement**

In `ply-core/src/source.rs` change line 209 `fn http_get_string(` to `pub(crate) fn http_get_string(`.

Prepend to `ply-core/src/catalog.rs` (above the test module):

```rust
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
                Err(e) => return Err(Error::Io { path: p.clone(), source: e }),
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ply-core catalog`
Expected: 9 passed.

- [ ] **Step 6: Lint**

Run: `cargo clippy -p ply-core --all-targets && cargo fmt --all`
Expected: no warnings. Do not commit.

---

### Task 2: Search ranking and per-package helpers (ply-core)

**Files:**
- Modify: `ply-core/src/catalog.rs` (add `impl Package` helpers, `Catalog::search`, tests)

**Interfaces:**
- Consumes: Task 1 types.
- Produces:
  - `impl Package { pub fn latest(&self) -> Option<&ImageVersion>; pub fn range_of_latest(&self) -> Option<String>; pub fn arches_of_latest(&self) -> Vec<&str>; pub fn versions_desc(&self) -> Vec<VersionRow<'_>> }`
  - `pub struct VersionRow<'a> { pub version: &'a str, pub arches: Vec<&'a str>, pub bytes: u64 }`
  - `impl Catalog { pub fn search(&self, query: &str) -> Vec<&Package> }` (ranked; empty query → everything alphabetical)
  - `pub fn version_key(v: &str) -> (Option<semver::Version>, String)` (sort key; semver first, lexical fallback)

- [ ] **Step 1: Write the failing tests** (append inside `mod tests`)

```rust
    fn pkg(name: &str, desc: &str, versions: &[(&str, &str)]) -> Package {
        Package {
            name: name.into(),
            description: desc.into(),
            versions: versions
                .iter()
                .map(|(v, img)| ImageVersion { version: (*v).into(), img: (*img).into(), ..Default::default() })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn latest_is_the_highest_semver_across_arches() {
        let p = pkg("ffmpeg", "", &[
            ("6.0.1", "ffmpeg-6.0.1-linux-x64.img"),
            ("6.1.1", "ffmpeg-6.1.1-linux-arm64.img"),
            ("6.1.1", "ffmpeg-6.1.1-linux-x64.img"),
            ("10.0.0", "ffmpeg-10.0.0-linux-x64.img"),
        ]);
        assert_eq!(p.latest().unwrap().version, "10.0.0", "numeric, not lexical");
        assert_eq!(p.range_of_latest().unwrap(), "10.0");
        let p = pkg("ffmpeg", "", &[("6.0.1", "a-x64.img"), ("6.1.1", "b-arm64.img"), ("6.1.1", "c-x64.img")]);
        assert_eq!(p.arches_of_latest(), vec!["x64", "arm64"], "x64 sorts first");
    }

    #[test]
    fn versions_desc_merges_arches_and_takes_the_largest_image() {
        let mut p = pkg("jq", "", &[("1.7.1", "jq-1.7.1-linux-x64.img"), ("1.7.1", "jq-1.7.1-linux-arm64.img"), ("1.6.0", "jq-1.6.0-linux-x64.img")]);
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
        let names: Vec<&str> = cat.search("FFmpeg").iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["ffmpeg", "ffmpeg-libs", "gst-ffmpeg", "libavcodec"]);
        assert!(cat.search("nothing-here").is_empty());
        assert_eq!(cat.search("").len(), 5, "empty query lists everything");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p ply-core catalog`
Expected: compile errors — `latest`, `range_of_latest`, `arches_of_latest`, `versions_desc`, `search` not found.

- [ ] **Step 3: Implement** (insert above `#[cfg(test)]`)

```rust
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
        let Some(latest) = self.latest() else { return Vec::new() };
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
                None => rows.push(VersionRow { version: &v.version, arches: vec![v.arch()], bytes: v.bytes }),
            }
        }
        for row in &mut rows {
            row.arches.sort_by_key(|a| arch_order(a));
            row.arches.dedup();
        }
        rows.sort_by(|a, b| version_key(b.version).cmp(&version_key(a.version)));
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
        let mut hits: Vec<(u8, &Package)> =
            self.packages.iter().filter_map(|p| rank(p, &q).map(|r| (r, p))).collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        hits.into_iter().map(|(_, p)| p).collect()
    }
}
```

`semver` is already a dependency of `ply-core` (check `ply-core/Cargo.toml`; if absent add `semver.workspace = true`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p ply-core catalog`
Expected: 13 passed.

- [ ] **Step 5: Lint**

Run: `cargo clippy -p ply-core --all-targets && cargo fmt --all`
Expected: clean. Do not commit.

---

### Task 3: `ply search` (ply-cli)

**Files:**
- Create: `ply-cli/src/commands/search.rs`
- Modify: `ply-cli/src/cli.rs` (add `Search(SearchArgs)` to `Command` right after `Build`, add `SearchArgs`)
- Modify: `ply-cli/src/commands/mod.rs` (`mod search;` + `Command::Search(args) => search::exec(args)`)

**Interfaces:**
- Consumes: `ply_core::catalog::{Catalog, Package, OFFICIAL_SOURCE}`, `ply_core::source::Source`, `ply_core::manifest::Manifest` (field `sources: BTreeMap<String,String>`), `crate::commands::build::human_size(u64) -> String`.
- Produces (used by Task 4 and 5):
  - `pub(crate) fn resolve_source(explicit: Option<&str>, dir: &Path) -> anyhow::Result<(Source, String)>` — returns the parsed source and its spec string. Order: `explicit` → `dir/ply.toml` `[sources] default` → `OFFICIAL_SOURCE`.
  - `pub(crate) fn dep_line(pkg: &Package, range: &str) -> String` — `name = "6.1"` or `name = { source = "<ns>", version = "6.1" }` for non-`ply` namespaces; keys containing `.` are quoted.

- [ ] **Step 1: Add the CLI surface**

In `ply-cli/src/cli.rs`, inside `pub enum Command`, directly after the `Build(BuildArgs),` variant add:

```rust
    /// Search a source's package catalog (cargo search for containers)
    ///
    /// Reads `state.json` at the source prefix: --source, else the
    /// `[sources] default` of ./ply.toml, else the official registry.
    Search(SearchArgs),
```

After `pub struct BuildArgs { … }` add:

```rust
#[derive(Args)]
pub struct SearchArgs {
    /// Substring to match against package names and descriptions
    pub query: String,
    /// List every published version and arch instead of one line per package
    #[arg(long)]
    pub versions: bool,
    /// Maximum packages to show (0 = all)
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    /// Search this source instead of the manifest's default (any `[sources]` spec)
    #[arg(long)]
    pub source: Option<String>,
    /// Machine-readable output
    #[arg(long)]
    pub json: bool,
}
```

In `ply-cli/src/commands/mod.rs` add `mod search;` (alphabetical) and the dispatch arm `Command::Search(args) => search::exec(args),` after the `Build` arm.

- [ ] **Step 2: Write the failing tests**

Create `ply-cli/src/commands/search.rs`:

```rust
//! `ply search` — cargo search for containers, over a static catalog file.

use std::path::Path;

use anyhow::{Context, Result};
use ply_core::catalog::{Catalog, Package, OFFICIAL_SOURCE};
use ply_core::manifest::Manifest;
use ply_core::source::Source;
use serde::Serialize;

use crate::cli::SearchArgs;
use crate::commands::build::human_size;

#[cfg(test)]
mod tests {
    use super::*;
    use ply_core::catalog::ImageVersion;

    fn pkg(name: &str, ns: &str, desc: &str, versions: &[(&str, &str, u64)]) -> Package {
        Package {
            namespace: ns.into(),
            name: name.into(),
            description: desc.into(),
            license: "MIT".into(),
            versions: versions
                .iter()
                .map(|(v, img, b)| ImageVersion { version: (*v).into(), img: (*img).into(), bytes: *b, ..Default::default() })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn dep_line_forms() {
        let p = pkg("ffmpeg", "ply", "", &[]);
        assert_eq!(dep_line(&p, "6.1"), r#"ffmpeg = "6.1""#);
        let p = pkg("ffmpeg", "corp", "", &[]);
        assert_eq!(dep_line(&p, "6.1"), r#"ffmpeg = { source = "corp", version = "6.1" }"#);
        let p = pkg("py3.12-tools", "ply", "", &[]);
        assert_eq!(dep_line(&p, "1.0"), r#""py3.12-tools" = "1.0""#);
    }

    #[test]
    fn rows_align_and_truncate() {
        let long = "x".repeat(80);
        let pkgs = vec![
            pkg("ffmpeg", "ply", "Multimedia framework", &[("6.1.1", "ffmpeg-6.1.1-linux-x64.img", 0), ("6.1.1", "ffmpeg-6.1.1-linux-arm64.img", 0)]),
            pkg("ffmpeg-libs", "ply", &long, &[("6.1.1", "ffmpeg-libs-6.1.1-linux-x64.img", 0)]),
            pkg("empty", "ply", "", &[]),
        ];
        let out = render_rows(&pkgs);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with(r#"ffmpeg = "6.1""#), "{}", lines[0]);
        assert!(lines[0].contains("# Multimedia framework"), "{}", lines[0]);
        assert!(lines[0].ends_with("x64 arm64"), "arches of the latest version: {}", lines[0]);
        assert!(lines[1].starts_with(r#"ffmpeg-libs = "6.1"   # "#), "{}", lines[1]);
        assert!(lines[1].contains(&format!("{}…", "x".repeat(59))), "60-char cap with ellipsis: {}", lines[1]);
        assert!(lines[1].ends_with("x64"), "{}", lines[1]);
        assert!(lines[2].starts_with("empty "), "no versions → bare name: {}", lines[2]);
        assert!(lines[2].ends_with("# (no description)"), "trailing arches column trimmed: {}", lines[2]);
        let col = lines[0].find('#').unwrap();
        assert_eq!(lines[1].find('#').unwrap(), col, "comment column aligned");
        assert_eq!(lines[2].find('#').unwrap(), col, "comment column aligned");
    }

    #[test]
    fn versions_block() {
        let p = pkg("jq", "ply", "JSON processor", &[("1.7.1", "jq-1.7.1-linux-x64.img", 503_808), ("1.7.1", "jq-1.7.1-linux-arm64.img", 520_000), ("1.6.0", "jq-1.6.0-linux-x64.img", 0)]);
        let out = render_versions(&p);
        assert_eq!(
            out,
            "jq   JSON processor   (MIT)\n  1.7.1   x64 arm64   507.8 KiB\n  1.6.0   x64         —\n"
        );
    }

    #[test]
    fn json_hits_carry_latest_and_range() {
        let p = pkg("jq", "ply", "", &[("1.7.1", "jq-1.7.1-linux-x64.img", 1)]);
        let hit = SearchHit::new(&p);
        let v = serde_json::to_value(&hit).unwrap();
        assert_eq!(v["name"], "jq");
        assert_eq!(v["latest"], "1.7.1");
        assert_eq!(v["range"], "1.7");
        assert_eq!(v["arches"], serde_json::json!(["x64"]));
    }

    #[test]
    fn resolve_source_order() {
        let dir = tempfile::tempdir().unwrap();
        let (_, spec) = resolve_source(None, dir.path()).unwrap();
        assert_eq!(spec, OFFICIAL_SOURCE, "no manifest → official");
        std::fs::write(
            dir.path().join("ply.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nbase = \"alpine@3.20\"\n[sources]\ndefault = \"https://corp.example/ply/{package}\"\n",
        )
        .unwrap();
        let (_, spec) = resolve_source(None, dir.path()).unwrap();
        assert_eq!(spec, "https://corp.example/ply/{package}", "manifest default wins");
        let (_, spec) = resolve_source(Some("file:///tmp/x/{package}"), dir.path()).unwrap();
        assert_eq!(spec, "file:///tmp/x/{package}", "--source wins over everything");
    }
}
```

Add `tempfile` and `serde` to `ply-cli/Cargo.toml`:

```toml
[dependencies]
ply-core.workspace = true
anyhow.workspace = true
clap.workspace = true
serde.workspace = true
serde_json.workspace = true
semver.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p ply-cli search`
Expected: compile errors — `dep_line`, `render_rows`, `render_versions`, `SearchHit`, `resolve_source`, `exec` not found.

- [ ] **Step 4: Implement** (insert between the `use` block and `#[cfg(test)]`)

```rust
/// Where to search: --source, else ./ply.toml's `[sources] default`, else the
/// official registry. Returns the parsed source and the spec it came from.
pub(crate) fn resolve_source(explicit: Option<&str>, dir: &Path) -> Result<(Source, String)> {
    let spec = match explicit {
        Some(s) => s.to_string(),
        None => {
            let manifest = dir.join("ply.toml");
            if manifest.is_file() {
                let m = Manifest::load(&manifest)?;
                m.sources.get("default").cloned().unwrap_or_else(|| OFFICIAL_SOURCE.to_string())
            } else {
                OFFICIAL_SOURCE.to_string()
            }
        }
    };
    let source = Source::parse(&spec, false)?;
    Ok((source, spec))
}

fn toml_key(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        name.to_string()
    } else {
        format!("\"{name}\"")
    }
}

/// The paste-ready `[dependencies]` line for a package at a range.
pub(crate) fn dep_line(pkg: &Package, range: &str) -> String {
    let key = toml_key(&pkg.name);
    if pkg.namespace.is_empty() || pkg.namespace == "ply" {
        format!("{key} = \"{range}\"")
    } else {
        format!("{key} = {{ source = \"{}\", version = \"{range}\" }}", pkg.namespace)
    }
}

const DESC_MAX: usize = 60;

fn short_desc(desc: &str) -> String {
    let desc = desc.trim();
    if desc.is_empty() {
        return "(no description)".to_string();
    }
    let chars: Vec<char> = desc.chars().collect();
    if chars.len() <= DESC_MAX {
        desc.to_string()
    } else {
        format!("{}…", chars[..DESC_MAX - 1].iter().collect::<String>())
    }
}

/// One line per package: `name = "range"   # description   arches`.
pub(crate) fn render_rows(pkgs: &[Package]) -> String {
    let lefts: Vec<String> = pkgs
        .iter()
        .map(|p| match p.range_of_latest() {
            Some(r) => dep_line(p, &r),
            None => p.name.clone(),
        })
        .collect();
    let descs: Vec<String> = pkgs.iter().map(|p| short_desc(&p.description)).collect();
    let lw = lefts.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let dw = descs.iter().map(|s| s.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    for (i, p) in pkgs.iter().enumerate() {
        let arches = p.arches_of_latest().join(" ");
        let line = format!("{:<lw$}   # {:<dw$}   {arches}", lefts[i], descs[i]);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// `--versions`: header line then one row per version, newest first.
pub(crate) fn render_versions(p: &Package) -> String {
    let mut out = format!("{}   {}", p.name, short_desc(&p.description));
    if !p.license.is_empty() {
        out.push_str(&format!("   ({})", p.license));
    }
    out.push('\n');
    let rows = p.versions_desc();
    let vw = rows.iter().map(|r| r.version.chars().count()).max().unwrap_or(0);
    let aw = rows.iter().map(|r| r.arches.join(" ").len()).max().unwrap_or(0);
    for r in rows {
        let size = if r.bytes == 0 { "—".to_string() } else { human_size(r.bytes) };
        out.push_str(&format!("  {:<vw$}   {:<aw$}   {size}\n", r.version, r.arches.join(" ")));
    }
    out
}

#[derive(Serialize)]
pub(crate) struct SearchHit<'a> {
    #[serde(flatten)]
    package: &'a Package,
    latest: Option<String>,
    range: Option<String>,
    arches: Vec<&'a str>,
}

impl<'a> SearchHit<'a> {
    pub(crate) fn new(package: &'a Package) -> Self {
        SearchHit {
            package,
            latest: package.latest().map(|v| v.version.clone()),
            range: package.range_of_latest(),
            arches: package.arches_of_latest(),
        }
    }
}

pub fn exec(args: SearchArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("current directory")?;
    let (source, _) = resolve_source(args.source.as_deref(), &cwd)?;
    let catalog = Catalog::load(&source)?;
    let hits = catalog.search(&args.query);
    let total = hits.len();
    let shown: Vec<Package> = hits
        .into_iter()
        .take(if args.limit == 0 { usize::MAX } else { args.limit })
        .cloned()
        .collect();

    if args.json {
        let out: Vec<SearchHit<'_>> = shown.iter().map(SearchHit::new).collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if shown.is_empty() {
        eprintln!("no packages match \"{}\"", args.query);
        return Ok(());
    }
    if args.versions {
        for (i, p) in shown.iter().enumerate() {
            if i > 0 {
                println!();
            }
            print!("{}", render_versions(p));
        }
    } else {
        print!("{}", render_rows(&shown));
    }
    if total > shown.len() {
        eprintln!("… and {} more — narrow the query or pass --limit 0", total - shown.len());
    }
    Ok(())
}
```

Check `human_size` is `pub fn human_size(bytes: u64) -> String` in `ply-cli/src/commands/build.rs:24` and that `build.rs` formats 520_000 as `507.8 KiB` (its `UNITS` are `["B","KiB","MiB","GiB"]` with one decimal). If its output differs, adjust the expected string in `versions_block` to whatever `human_size(520_000)` returns — the helper's format is the contract, not the test literal.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ply-cli search`
Expected: 5 passed. Also `cargo test -p ply-cli cli` must still pass (`hints_only_cover_nonexistent_subcommands`).

- [ ] **Step 6: Smoke test against a local copy of the real catalog**

Run:
```bash
mkdir -p /tmp/plycat && curl -s https://registry.plybox.sh/state.json -o /tmp/plycat/state.json
cargo run -q -p ply-cli -- search ffmpeg --source 'file:///tmp/plycat/{package}'
cargo run -q -p ply-cli -- search ffmpeg --versions --limit 2 --source 'file:///tmp/plycat/{package}'
cargo run -q -p ply-cli -- search zzzz --source 'file:///tmp/plycat/{package}'; echo "exit $?"
cargo run -q -p ply-cli -- search jq --json --source 'file:///tmp/plycat/{package}' | head -20
cargo run -q -p ply-cli -- search jq --source 'github:org/repo'; echo "exit $?"
```
Expected: aligned rows; versions blocks; `no packages match "zzzz"` with exit 0; JSON array; forge error with exit 1.

- [ ] **Step 7: Lint**

Run: `cargo clippy -p ply-cli --all-targets && cargo fmt --all`
Expected: clean. Do not commit.

---

### Task 4: `ply add` (ply-cli)

**Files:**
- Modify: `Cargo.toml` (workspace deps: add `toml_edit = "0.22"` after `toml = "0.8"`)
- Modify: `ply-cli/Cargo.toml` (`toml_edit.workspace = true`)
- Create: `ply-cli/src/commands/add.rs`
- Modify: `ply-cli/src/cli.rs` (`Add(AddArgs)` after `Search`, plus `AddArgs`)
- Modify: `ply-cli/src/commands/mod.rs` (`mod add;` + `Command::Add(args) => add::exec(args)`)

**Interfaces:**
- Consumes: Task 3 `resolve_source` is *not* used (add reads the manifest document itself); uses `ply_core::catalog::{Catalog, OFFICIAL_SOURCE}`, `ply_core::source::Source`, `toml_edit::{DocumentMut, Item, Table, InlineTable, value}`.
- Produces:
  - `pub(crate) enum Outcome { Added, Updated { from: String }, Unchanged }`
  - `pub(crate) struct Edit { pub outcome: Outcome, pub line: String, pub sources_added: bool }`
  - `pub(crate) fn apply_add(doc: &mut DocumentMut, name: &str, range: &str, source: Option<&str>) -> Result<Edit>` — pure; inserts `[sources] default = OFFICIAL_SOURCE` if the table is absent; places a new `[dependencies]` right after `[package]`.
  - `pub(crate) fn place_after(doc: &mut DocumentMut, new_key: &str, after_key: &str)`.

- [ ] **Step 1: Add the CLI surface**

`cli.rs`, in `Command` after `Search(SearchArgs),`:

```rust
    /// Add a dependency to ./ply.toml (cargo add for containers)
    ///
    /// `ply add ffmpeg` takes the latest major.minor from the source's
    /// catalog; `ply add ffmpeg@6.0` writes that range without a lookup.
    Add(AddArgs),
```

After `SearchArgs`:

```rust
#[derive(Args)]
pub struct AddArgs {
    /// Package name, optionally with a range: `ffmpeg` or `ffmpeg@6.1`
    pub spec: String,
    /// Take the package from this `[sources]` entry instead of `default`
    #[arg(long)]
    pub source: Option<String>,
}
```

`commands/mod.rs`: `mod add;` and `Command::Add(args) => add::exec(args),`.

- [ ] **Step 2: Write the failing tests**

Create `ply-cli/src/commands/add.rs`:

```rust
//! `ply add` — write a dependency into ./ply.toml, format-preserving.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use ply_core::catalog::{Catalog, OFFICIAL_SOURCE};
use ply_core::source::Source;
use toml_edit::{value, DocumentMut, InlineTable, Item, Table};

use crate::cli::AddArgs;

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "# my app\n[package]\nname = \"a\"\nversion = \"0.1.0\"\nbase = \"alpine@3.20\"\n\n[ports]\nhttp = 8000   # keep\n\n[sources]\ndefault = \"https://registry.plybox.sh/ply/{package}\"\n";

    fn doc(s: &str) -> DocumentMut {
        s.parse().unwrap()
    }

    #[test]
    fn creates_dependencies_right_after_package_and_keeps_comments() {
        let mut d = doc(BASE);
        let e = apply_add(&mut d, "ffmpeg", "6.1", None).unwrap();
        assert!(matches!(e.outcome, Outcome::Added));
        assert_eq!(e.line, r#"ffmpeg = "6.1""#);
        assert!(!e.sources_added);
        let out = d.to_string();
        let pkg = out.find("[package]").unwrap();
        let deps = out.find("[dependencies]").unwrap();
        let ports = out.find("[ports]").unwrap();
        assert!(pkg < deps && deps < ports, "placed after [package], before [ports]:\n{out}");
        assert!(out.contains("# my app"), "leading comment survives");
        assert!(out.contains("http = 8000   # keep"), "inline comment survives");
        assert!(out.contains("ffmpeg = \"6.1\""));
    }

    #[test]
    fn updates_existing_entry_and_reports_old_value() {
        let mut d = doc(&format!("{BASE}[dependencies]\nffmpeg = \"6.0\"\n"));
        let e = apply_add(&mut d, "ffmpeg", "6.1", None).unwrap();
        assert!(matches!(e.outcome, Outcome::Updated { ref from } if from == "6.0"));
        assert!(d.to_string().contains("ffmpeg = \"6.1\""));
        assert!(!d.to_string().contains("6.0"));
    }

    #[test]
    fn unchanged_when_already_present() {
        let mut d = doc(&format!("{BASE}[dependencies]\nffmpeg = \"6.1\"\n"));
        let before = d.to_string();
        let e = apply_add(&mut d, "ffmpeg", "6.1", None).unwrap();
        assert!(matches!(e.outcome, Outcome::Unchanged));
        assert_eq!(d.to_string(), before);
    }

    #[test]
    fn table_form_for_named_source() {
        let mut d = doc(&BASE.replace("[sources]\n", "[sources]\ncorp = \"https://corp/{package}\"\n"));
        let e = apply_add(&mut d, "ffmpeg", "6.1", Some("corp")).unwrap();
        assert_eq!(e.line, r#"ffmpeg = { source = "corp", version = "6.1" }"#);
        assert!(d.to_string().contains("ffmpeg = { source = \"corp\", version = \"6.1\" }"));
    }

    #[test]
    fn unknown_source_lists_available() {
        let mut d = doc(BASE);
        let err = apply_add(&mut d, "ffmpeg", "6.1", Some("nope")).unwrap_err().to_string();
        assert_eq!(err, "no source \"nope\" in [sources] (have: default)");
    }

    #[test]
    fn adds_sources_table_when_missing() {
        let mut d = doc("[package]\nname = \"a\"\nversion = \"0.1.0\"\nbase = \"alpine@3.20\"\n");
        let e = apply_add(&mut d, "ffmpeg", "6.1", None).unwrap();
        assert!(e.sources_added);
        let out = d.to_string();
        assert!(out.contains(&format!("[sources]\ndefault = \"{OFFICIAL_SOURCE}\"")), "{out}");
        let m = ply_core::manifest::Manifest::parse(&out).unwrap();
        assert_eq!(m.sources["default"], OFFICIAL_SOURCE);
    }

    #[test]
    fn dotted_names_are_quoted_keys() {
        let mut d = doc(BASE);
        let e = apply_add(&mut d, "py3.12-tools", "1.0", None).unwrap();
        assert_eq!(e.line, r#""py3.12-tools" = "1.0""#);
        assert!(d.to_string().contains("\"py3.12-tools\" = \"1.0\""));
    }

    #[test]
    fn parse_spec_splits_name_and_range() {
        assert_eq!(parse_spec("ffmpeg").unwrap(), ("ffmpeg".into(), None));
        assert_eq!(parse_spec("ffmpeg@6.1").unwrap(), ("ffmpeg".into(), Some("6.1".into())));
        assert!(parse_spec("@6.1").is_err());
        assert!(parse_spec("ffmpeg@").is_err());
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p ply-cli add`
Expected: compile errors — `apply_add`, `Outcome`, `parse_spec`, `exec` not found (after `toml_edit` is added to `Cargo.toml` and `ply-cli/Cargo.toml`; if the dependency is missing the error is "unresolved import `toml_edit`").

- [ ] **Step 4: Implement** (between the `use` block and `#[cfg(test)]`)

```rust
#[derive(Debug)]
pub(crate) enum Outcome {
    Added,
    Updated { from: String },
    Unchanged,
}

#[derive(Debug)]
pub(crate) struct Edit {
    pub outcome: Outcome,
    /// The line as written, e.g. `ffmpeg = "6.1"`.
    pub line: String,
    /// True when `[sources] default = OFFICIAL_SOURCE` was inserted too.
    pub sources_added: bool,
}

/// `name` or `name@range`.
pub(crate) fn parse_spec(spec: &str) -> Result<(String, Option<String>)> {
    match spec.split_once('@') {
        None if !spec.is_empty() => Ok((spec.to_string(), None)),
        Some((name, range)) if !name.is_empty() && !range.is_empty() => {
            Ok((name.to_string(), Some(range.to_string())))
        }
        _ => bail!("expected <name> or <name>@<range>, got \"{spec}\""),
    }
}

/// Re-number top-level table positions so `new_key` renders right after
/// `after_key`. toml_edit appends new tables at the end by default.
pub(crate) fn place_after(doc: &mut DocumentMut, new_key: &str, after_key: &str) {
    let mut order: Vec<(String, usize)> = doc
        .as_table()
        .iter()
        .filter(|(k, _)| *k != new_key)
        .filter_map(|(k, item)| item.as_table().map(|t| (k.to_string(), t.position().unwrap_or(usize::MAX))))
        .collect();
    order.sort_by_key(|(_, pos)| *pos);
    let mut keys: Vec<String> = order.into_iter().map(|(k, _)| k).collect();
    let at = keys.iter().position(|k| k == after_key).map(|i| i + 1).unwrap_or(keys.len());
    keys.insert(at, new_key.to_string());
    for (i, k) in keys.iter().enumerate() {
        if let Some(t) = doc.get_mut(k).and_then(Item::as_table_mut) {
            t.set_position(i);
        }
    }
}

fn render_value(item: &Item) -> String {
    item.to_string().trim().to_string()
}

/// Pure edit: add/update `name = range` under `[dependencies]`.
pub(crate) fn apply_add(doc: &mut DocumentMut, name: &str, range: &str, source: Option<&str>) -> Result<Edit> {
    let mut sources_added = false;
    if let Some(src) = source {
        let have: Vec<String> = doc
            .get("sources")
            .and_then(Item::as_table)
            .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default();
        if !have.contains(&src.to_string()) {
            bail!("no source \"{src}\" in [sources] (have: {})", have.join(", "));
        }
    } else if doc.get("sources").and_then(Item::as_table).is_none() {
        let mut t = Table::new();
        t["default"] = value(OFFICIAL_SOURCE);
        doc["sources"] = Item::Table(t);
        sources_added = true;
    }

    let new_item: Item = match source {
        None => value(range),
        Some(src) => {
            let mut t = InlineTable::new();
            t.insert("source", src.into());
            t.insert("version", range.into());
            value(t)
        }
    };

    let had_deps = doc.get("dependencies").and_then(Item::as_table).is_some();
    if !had_deps {
        doc["dependencies"] = Item::Table(Table::new());
        place_after(doc, "dependencies", "package");
    }
    let deps = doc["dependencies"].as_table_mut().expect("just ensured");
    let outcome = match deps.get(name) {
        Some(old) if render_value(old) == render_value(&new_item) => Outcome::Unchanged,
        Some(old) => Outcome::Updated { from: render_value(old).trim_matches('"').to_string() },
        None => Outcome::Added,
    };
    if !matches!(outcome, Outcome::Unchanged) {
        deps.insert(name, new_item.clone());
    }
    let key = toml_edit::Key::new(name).to_string();
    let line = format!("{} = {}", key.trim(), render_value(&new_item));
    Ok(Edit { outcome, line, sources_added })
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub fn exec(args: AddArgs) -> Result<()> {
    let cwd = std::env::current_dir().context("current directory")?;
    let path = cwd.join("ply.toml");
    if !path.is_file() {
        bail!("no ply.toml in {}", cwd.display());
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = text.parse().with_context(|| format!("{}: not valid TOML", path.display()))?;
    let (name, range) = parse_spec(&args.spec)?;

    let range = match range {
        Some(r) => r,
        None => {
            let key = args.source.as_deref().unwrap_or("default");
            let spec = doc
                .get("sources")
                .and_then(Item::as_table)
                .and_then(|t| t.get(key))
                .and_then(Item::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| OFFICIAL_SOURCE.to_string());
            if args.source.is_some() && !doc.get("sources").and_then(Item::as_table).map(|t| t.contains_key(key)).unwrap_or(false) {
                let have: Vec<String> = doc.get("sources").and_then(Item::as_table).map(|t| t.iter().map(|(k, _)| k.to_string()).collect()).unwrap_or_default();
                bail!("no source \"{key}\" in [sources] (have: {})", have.join(", "));
            }
            let source = Source::parse(&spec, false)?;
            let catalog = Catalog::load(&source)?;
            let prefix = source.catalog_location()?.prefix();
            catalog
                .get(&name)
                .ok_or_else(|| anyhow!("no package \"{name}\" in {prefix} — try: ply search {name}"))?
                .range_of_latest()
                .ok_or_else(|| anyhow!("package \"{name}\" in {prefix} has no published versions"))?
        }
    };

    let edit = apply_add(&mut doc, &name, &range, args.source.as_deref())?;
    match edit.outcome {
        Outcome::Unchanged => {
            println!("{} already in ply.toml", edit.line);
            return Ok(());
        }
        Outcome::Updated { ref from } => println!("{name} \"{from}\" → \"{range}\" in ply.toml"),
        Outcome::Added => println!("added {} to ply.toml", edit.line),
    }
    if edit.sources_added {
        println!("added [sources] default = \"{OFFICIAL_SOURCE}\"");
    }
    write_atomic(&path, &doc.to_string())?;
    println!("run ply build to resolve and lock");
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ply-cli add`
Expected: 8 passed. If `updates_existing_entry_and_reports_old_value` fails on the `from` value having quotes, `render_value(old).trim_matches('"')` is the place to fix.

- [ ] **Step 6: Smoke test in a scratch dir (uses the local catalog copy from Task 3)**

```bash
d=$(mktemp -d) && cd "$d" && printf '[package]\nname = "t"\nversion = "0.1.0"\nbase = "alpine@3.20"\nentrypoint = ["sh"]\n\n[sources]\ndefault = "file:///tmp/plycat/{package}"\n' > ply.toml
cargo run -q --manifest-path /home/iluxa/projects/ply/Cargo.toml -p ply-cli -- add ffmpeg
cargo run -q --manifest-path /home/iluxa/projects/ply/Cargo.toml -p ply-cli -- add ffmpeg@6.0
cargo run -q --manifest-path /home/iluxa/projects/ply/Cargo.toml -p ply-cli -- add nope; echo "exit $?"
cat ply.toml
```
Expected: `added ffmpeg = "6.1" …`, then `ffmpeg "6.1" → "6.0"`, then `no package "nope" in /tmp/plycat — try: ply search nope` exit 1; `[dependencies]` sits between `[package]` and `[sources]`.

- [ ] **Step 7: Lint**

Run: `cargo clippy -p ply-cli --all-targets && cargo fmt --all`
Expected: clean. Do not commit.

---

### Task 5: `ply init` (ply-cli)

**Files:**
- Create: `ply-cli/src/commands/init.rs`
- Modify: `ply-cli/src/cli.rs` (`Init(InitArgs)` placed *before* `Build`, plus `InitArgs`)
- Modify: `ply-cli/src/commands/mod.rs` (`mod init;` + `Command::Init(args) => init::exec(args)`)

**Interfaces:**
- Consumes: `ply_core::catalog::{Catalog, OFFICIAL_SOURCE}`, `ply_core::source::Source`, `ply_core::manifest::Manifest` (validation in tests).
- Produces:
  - `pub(crate) struct Latest { pub alpine: String, pub python3: String, pub node: String }` + `Latest::builtin()`, `Latest::from_catalog(&Catalog)`
  - `pub(crate) struct Defaults { pub name: String, pub entrypoint: Vec<String>, pub runtime: Option<(String, String)>, pub port: Option<u16> }`
  - `pub(crate) fn sanitize_name(raw: &str) -> String`
  - `pub(crate) fn detect(dir: &Path, latest: &Latest) -> Defaults`
  - `pub(crate) struct Answers { pub name, pub version, pub entrypoint: Vec<String>, pub base: String, pub runtime: Option<(String,String)>, pub port: Option<u16> }`
  - `pub(crate) fn prompt(input: &mut impl BufRead, out: &mut impl Write, d: &Defaults, latest: &Latest, yes: bool) -> Result<Answers>`
  - `pub(crate) fn render_manifest(a: &Answers) -> String`

- [ ] **Step 1: Add the CLI surface**

`cli.rs`, in `Command` *before* `Build(BuildArgs),`:

```rust
    /// Write a starter ply.toml (npm init for containers)
    ///
    /// Detects Node/Python projects for defaults, asks a few questions
    /// (Enter accepts the default), and writes the manifest the quickstart shows.
    Init(InitArgs),
```

After `BuildArgs`:

```rust
#[derive(Args)]
pub struct InitArgs {
    /// Directory to initialise (defaults to the current one)
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    /// Accept every default without asking
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Overwrite an existing ply.toml
    #[arg(long)]
    pub force: bool,
}
```

`commands/mod.rs`: `mod init;` and `Command::Init(args) => init::exec(args),` as the first arm.

- [ ] **Step 2: Write the failing tests**

Create `ply-cli/src/commands/init.rs`:

```rust
//! `ply init` — write a starter ply.toml, npm-init style.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use ply_core::catalog::{Catalog, OFFICIAL_SOURCE};
use ply_core::source::Source;

use crate::cli::InitArgs;

#[cfg(test)]
mod tests {
    use super::*;
    use ply_core::manifest::Manifest;

    fn latest() -> Latest {
        Latest::builtin()
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(sanitize_name("My App"), "my-app");
        assert_eq!(sanitize_name("  --Weird__Name!!  "), "weird-name");
        assert_eq!(sanitize_name("ok-name"), "ok-name");
        assert_eq!(sanitize_name("!!!"), "app");
    }

    #[test]
    fn detects_node() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), r#"{"name":"x","main":"dist/index.js"}"#).unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime, Some(("node".into(), "22".into())));
        assert_eq!(d.entrypoint, vec!["node", "dist/index.js"]);
        assert_eq!(d.port, Some(3000));
    }

    #[test]
    fn node_without_main_falls_back_to_server_js() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect(dir.path(), &latest()).entrypoint, vec!["node", "server.js"]);
    }

    #[test]
    fn detects_python() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("requirements.txt"), "flask\n").unwrap();
        std::fs::write(dir.path().join("main.py"), "").unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime, Some(("python3".into(), "3.12".into())));
        assert_eq!(d.entrypoint, vec!["python3", "main.py"], "main.py when app.py is absent");
        assert_eq!(d.port, Some(8000));
        std::fs::write(dir.path().join("app.py"), "").unwrap();
        assert_eq!(detect(dir.path(), &latest()).entrypoint, vec!["python3", "app.py"]);
    }

    #[test]
    fn detects_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let d = detect(dir.path(), &latest());
        assert_eq!(d.runtime, None);
        assert_eq!(d.entrypoint, vec!["/bin/sh", "-c", "echo hello from ply"]);
        assert_eq!(d.port, None);
        assert_eq!(d.name, sanitize_name(&dir.path().file_name().unwrap().to_string_lossy()));
    }

    fn defaults() -> Defaults {
        Defaults {
            name: "myapp".into(),
            entrypoint: vec!["python3".into(), "app.py".into()],
            runtime: Some(("python3".into(), "3.12".into())),
            port: Some(8000),
        }
    }

    #[test]
    fn yes_takes_every_default_without_reading_stdin() {
        let mut input = std::io::Cursor::new(b"SHOULD NOT BE READ\n".to_vec());
        let mut out = Vec::new();
        let a = prompt(&mut input, &mut out, &defaults(), &latest(), true).unwrap();
        assert_eq!(a.name, "myapp");
        assert_eq!(a.version, "0.1.0");
        assert_eq!(a.base, "alpine@3.20");
        assert_eq!(a.runtime, Some(("python3".into(), "3.12".into())));
        assert_eq!(a.port, Some(8000));
        assert_eq!(input.position(), 0);
    }

    #[test]
    fn enter_accepts_defaults_and_answers_override() {
        let mut input = std::io::Cursor::new(b"\n1.2.3\nnode server.js\n\n3.11\n\n".to_vec());
        let mut out = Vec::new();
        let a = prompt(&mut input, &mut out, &defaults(), &latest(), false).unwrap();
        assert_eq!(a.name, "myapp");
        assert_eq!(a.version, "1.2.3");
        assert_eq!(a.entrypoint, vec!["node", "server.js"]);
        assert_eq!(a.base, "alpine@3.20");
        assert_eq!(a.runtime, Some(("python3".into(), "3.11".into())));
        assert_eq!(a.port, Some(8000));
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("package name [myapp]:"), "{shown}");
        assert!(shown.contains("runtime [python3 = \"3.12\"]"), "{shown}");
    }

    #[test]
    fn bad_version_is_asked_again_and_empty_runtime_means_none() {
        let mut input = std::io::Cursor::new(b"\nnot-a-version\n0.2.0\n\n\n-\n\n".to_vec());
        let mut out = Vec::new();
        let a = prompt(&mut input, &mut out, &defaults(), &latest(), false).unwrap();
        assert_eq!(a.version, "0.2.0");
        assert_eq!(a.runtime, None, "'-' answers none");
        assert!(String::from_utf8(out).unwrap().contains("not a version"));
    }

    #[test]
    fn rendered_manifest_is_valid_and_round_trips() {
        let a = Answers {
            name: "myapp".into(),
            version: "0.1.0".into(),
            entrypoint: vec!["python3".into(), "app.py".into()],
            base: "alpine@3.20".into(),
            runtime: Some(("python3".into(), "3.12".into())),
            port: Some(8000),
        };
        let text = render_manifest(&a);
        let m = Manifest::parse(&text).expect("ply build must accept what init wrote");
        assert_eq!(m.package.name, "myapp");
        assert_eq!(m.package.entrypoint.as_deref(), Some(&["python3".to_string(), "app.py".to_string()][..]));
        assert_eq!(m.ports["http"], 8000);
        assert_eq!(m.sources["default"], OFFICIAL_SOURCE);
        assert!(text.contains("# include = [\"dist/\"]"));
        assert!(text.contains("[dependencies]\npython3 = \"3.12\""));
    }

    #[test]
    fn empty_sections_are_omitted() {
        let a = Answers {
            name: "bare".into(),
            version: "0.1.0".into(),
            entrypoint: vec!["/bin/sh".into(), "-c".into(), "echo hi".into()],
            base: "alpine@3.20".into(),
            runtime: None,
            port: None,
        };
        let text = render_manifest(&a);
        assert!(!text.contains("[dependencies]"));
        assert!(!text.contains("[ports]"));
        Manifest::parse(&text).unwrap();
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p ply-cli init`
Expected: compile errors — `Latest`, `Defaults`, `Answers`, `sanitize_name`, `detect`, `prompt`, `render_manifest`, `exec` not found.

- [ ] **Step 4: Implement** (between the `use` block and `#[cfg(test)]`)

```rust
/// Latest `major.minor` ranges for the packages `init` suggests.
#[derive(Debug, Clone)]
pub(crate) struct Latest {
    pub alpine: String,
    pub python3: String,
    pub node: String,
}

impl Latest {
    pub(crate) fn builtin() -> Self {
        Latest { alpine: "3.20".into(), python3: "3.12".into(), node: "22".into() }
    }

    pub(crate) fn from_catalog(cat: &Catalog) -> Self {
        let b = Self::builtin();
        let pick = |name: &str, fallback: String| {
            cat.get(name).and_then(|p| p.range_of_latest()).unwrap_or(fallback)
        };
        Latest {
            alpine: pick("alpine", b.alpine),
            python3: pick("python3", b.python3),
            node: pick("node", b.node),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Defaults {
    pub name: String,
    pub entrypoint: Vec<String>,
    pub runtime: Option<(String, String)>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answers {
    pub name: String,
    pub version: String,
    pub entrypoint: Vec<String>,
    pub base: String,
    pub runtime: Option<(String, String)>,
    pub port: Option<u16>,
}

/// Lowercase, `[a-z0-9-]` only, runs collapsed, trimmed; `app` if nothing is left.
pub(crate) fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in raw.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "app".to_string() } else { out }
}

fn node_main(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("package.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("main").and_then(|m| m.as_str()).map(str::to_string))
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "server.js".to_string())
}

fn has_py_files(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.path().extension().is_some_and(|x| x == "py")))
        .unwrap_or(false)
}

/// Filesystem-only project detection.
pub(crate) fn detect(dir: &Path, latest: &Latest) -> Defaults {
    let name = sanitize_name(
        &std::fs::canonicalize(dir)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default(),
    );
    if dir.join("package.json").is_file() {
        return Defaults {
            name,
            entrypoint: vec!["node".into(), node_main(dir)],
            runtime: Some(("node".into(), latest.node.clone())),
            port: Some(3000),
        };
    }
    if dir.join("requirements.txt").is_file() || dir.join("pyproject.toml").is_file() || has_py_files(dir) {
        let script = if !dir.join("app.py").is_file() && dir.join("main.py").is_file() { "main.py" } else { "app.py" };
        return Defaults {
            name,
            entrypoint: vec!["python3".into(), script.into()],
            runtime: Some(("python3".into(), latest.python3.clone())),
            port: Some(8000),
        };
    }
    Defaults {
        name,
        entrypoint: vec!["/bin/sh".into(), "-c".into(), "echo hello from ply".into()],
        runtime: None,
        port: None,
    }
}

fn ask(input: &mut impl BufRead, out: &mut impl Write, label: &str, default: &str) -> Result<String> {
    write!(out, "{label} [{default}]: ")?;
    out.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() { default.to_string() } else { line.to_string() })
}

/// npm-init style questions. `yes` returns the defaults without reading input.
pub(crate) fn prompt(
    input: &mut impl BufRead,
    out: &mut impl Write,
    d: &Defaults,
    latest: &Latest,
    yes: bool,
) -> Result<Answers> {
    let base_default = format!("alpine@{}", latest.alpine);
    if yes {
        return Ok(Answers {
            name: d.name.clone(),
            version: "0.1.0".into(),
            entrypoint: d.entrypoint.clone(),
            base: base_default,
            runtime: d.runtime.clone(),
            port: d.port,
        });
    }
    writeln!(out, "This writes a ply.toml. Enter accepts the default; `-` answers none.")?;
    let name = sanitize_name(&ask(input, out, "package name", &d.name)?);
    let version = loop {
        let v = ask(input, out, "version", "0.1.0")?;
        if semver::Version::parse(&v).is_ok() {
            break v;
        }
        writeln!(out, "  not a version (want MAJOR.MINOR.PATCH, e.g. 0.1.0)")?;
    };
    let entrypoint: Vec<String> = ask(input, out, "entrypoint", &d.entrypoint.join(" "))?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let base = ask(input, out, "base", &base_default)?;
    let runtime_default = match &d.runtime {
        Some((n, r)) => format!("{n} = \"{r}\""),
        None => "-".to_string(),
    };
    let runtime = match ask(input, out, "runtime", &runtime_default)?.as_str() {
        "-" => None,
        answer if answer == runtime_default => d.runtime.clone(),
        answer => match (&d.runtime, answer.split_once('=')) {
            (_, Some((n, r))) => Some((n.trim().to_string(), r.trim().trim_matches('"').to_string())),
            (Some((n, _)), None) => Some((n.clone(), answer.trim().trim_matches('"').to_string())),
            (None, None) => Some((answer.trim().to_string(), String::new())),
        },
    };
    let runtime = runtime.filter(|(n, r)| !n.is_empty() && !r.is_empty());
    let port_default = d.port.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string());
    let port = match ask(input, out, "port", &port_default)?.as_str() {
        "-" => None,
        p => Some(p.parse::<u16>().with_context(|| format!("port must be a number, got {p}"))?),
    };
    Ok(Answers { name, version, entrypoint, base, runtime, port })
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The quickstart's manifest, commented. Must always pass `Manifest::parse`.
pub(crate) fn render_manifest(a: &Answers) -> String {
    let mut t = String::new();
    t.push_str("[package]\n");
    t.push_str(&format!("name = {}\n", toml_str(&a.name)));
    t.push_str(&format!("version = {}\n", toml_str(&a.version)));
    let args: Vec<String> = a.entrypoint.iter().map(|s| toml_str(s)).collect();
    t.push_str(&format!("entrypoint = [{}]\n", args.join(", ")));
    t.push_str(&format!("base = {}\n", toml_str(&a.base)));
    t.push_str("# include = [\"dist/\"]   # ship only these paths (default: everything in this directory)\n");
    if let Some((name, range)) = &a.runtime {
        t.push_str("\n[dependencies]\n");
        t.push_str(&format!("{name} = {}\n", toml_str(range)));
    }
    if let Some(port) = a.port {
        t.push_str("\n[ports]\n");
        t.push_str(&format!("http = {port}\n"));
    }
    t.push_str("\n[sources]\n");
    t.push_str(&format!("default = {}\n", toml_str(OFFICIAL_SOURCE)));
    t
}

fn latest_versions() -> Latest {
    match Source::parse(OFFICIAL_SOURCE, false).and_then(|s| Catalog::load(&s)) {
        Ok(cat) => Latest::from_catalog(&cat),
        Err(_) => {
            eprintln!("note: could not reach the registry — using built-in defaults");
            Latest::builtin()
        }
    }
}

pub fn exec(args: InitArgs) -> Result<()> {
    let dir = &args.dir;
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let path = dir.join("ply.toml");
    if path.exists() && !args.force {
        bail!("{} already exists (use --force to overwrite)", path.display());
    }
    let latest = latest_versions();
    let defaults = detect(dir, &latest);
    let yes = args.yes || !std::io::stdin().is_terminal();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    let answers = prompt(&mut input, &mut out, &defaults, &latest, yes)?;
    let text = render_manifest(&answers);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    println!("\n{text}");
    println!("wrote {}", path.display());
    println!("next: ply build {}          # resolve, lock, build the image", dir.display());
    println!("      ply add <package>    # add a dependency from the registry");
    println!("      commit ply.lock; ignore *.img");
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p ply-cli init`
Expected: 11 passed. The key one is `rendered_manifest_is_valid_and_round_trips` — if `Manifest::parse` rejects the text, fix `render_manifest`, never the test.

- [ ] **Step 6: Smoke test**

```bash
d=$(mktemp -d) && cd "$d" && printf 'print("hi")\n' > app.py && echo flask > requirements.txt
printf '\n\n\n\n\n\n' | cargo run -q --manifest-path /home/iluxa/projects/ply/Cargo.toml -p ply-cli -- init
cat ply.toml
cargo run -q --manifest-path /home/iluxa/projects/ply/Cargo.toml -p ply-cli -- init; echo "exit $?"
cargo run -q --manifest-path /home/iluxa/projects/ply/Cargo.toml -p ply-cli -- init --yes --force && cargo run -q --manifest-path /home/iluxa/projects/ply/Cargo.toml -p ply-cli -- build . 2>&1 | tail -3
```
Expected: python defaults detected; second run refuses with `already exists`; the `--yes --force` manifest then **builds** (`locked …` / `built …`) against the real registry — this is the end-to-end proof that `init` writes what `build` accepts. (Note: since `ply/state.json` is not yet published, the first run prints `note: could not reach the registry — using built-in defaults`; that is expected until Task 6 is deployed.)

- [ ] **Step 7: Lint + whole-workspace tests**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: clean, all green. Do not commit.

---

### Task 6: Publisher — per-namespace catalog

**Files:**
- Modify: `scripts/registry-push.mjs:258-325` (`publishState`)

**Interfaces:**
- Produces: `ply/state.json` (and any other namespace) on the bucket with the same schema as the root file, filtered to that namespace.

- [ ] **Step 1: Refactor `publishState` to build one snapshot per namespace**

Replace the tail of `publishState()` — from `const out = {` through the `console.log(\`state.json published …\`)` line — with:

```js
  const snapshot = (pkgs) => ({
    updated: new Date().toISOString().replace(/\.\d+Z$/, "Z"),
    package_count: pkgs.length,
    image_count: pkgs.reduce((n, p) => n + p.versions.length, 0),
    total_bytes: pkgs.reduce((n, p) => n + p.versions.reduce((m, v) => m + v.bytes, 0), 0),
    packages: pkgs,
  });

  const dir = join(ROOT, "scripts/.push-work");
  mkdirSync(dir, { recursive: true });
  const publish = (key, obj) => {
    const file = join(dir, key.replace(/\//g, "__"));
    writeFileSync(file, JSON.stringify(obj, null, 1));
    execFileSync("npx", ["wrangler", "r2", "object", "put",
      `${args.bucket}/${key}`, "--file", file, "--remote",
      "--cache-control", "public, max-age=300", "--content-type", "application/json"],
      { stdio: ["ignore", "ignore", "pipe"] });
    rmSync(file, { force: true });
    console.log(`${key} published (${obj.package_count} packages, ${obj.image_count} images)`);
  };

  // Root: the website's view of everything.
  publish("state.json", snapshot(packages));
  // Per namespace: the catalog `ply search` / `ply add` read at the source
  // prefix (https://registry.plybox.sh/<ns>/state.json).
  for (const ns of new Set(packages.map((p) => p.namespace)))
    publish(`${ns}/state.json`, snapshot(packages.filter((p) => p.namespace === ns)));
```

Keep everything above it (`entries`, size backfill, `byKey`, `packages` sorting) unchanged. Note `image_count` for the root previously used `seenImg.size`; `pkgs.reduce(versions.length)` is the same number because `seenImg` de-duplicates by `upload_path` before `versions.push`.

- [ ] **Step 2: Dry-run the script**

Run: `node --check scripts/registry-push.mjs && ./scripts/registry-push.mjs --help 2>&1 | head -5` (or `--dry-run` if the script has one; read its header comment).
Expected: syntax OK. Actual publishing needs R2 credentials — **ask the repository owner to run** `./scripts/registry-push.mjs --state-only`, then verify: `curl -sI https://registry.plybox.sh/ply/state.json | head -1` → `HTTP/2 200`, and `ply search ffmpeg` (no `--source`) returns rows.

Do not commit.

---

### Task 7: Docs

**Files:**
- Modify: `docs/cli.md` ("Build & validate" section)
- Modify: `docs/registries.md` (after "Version listing (index.json)")
- Modify: `docs/image-format.md:75-78` (registry protocol block)
- Modify: `docs/docker.md:61-76` (translation table)
- Modify: `docs/quickstart.md` ("Write a manifest")

- [ ] **Step 1: `docs/cli.md`** — replace the "Build & validate" section with:

````markdown
## Build & validate

```sh
ply init [DIR] [-y] [--force]
```
Write a starter `ply.toml`. Detects Node/Python projects for defaults and
asks a few questions (Enter accepts the default; `-y` accepts all). Never
touches anything but `ply.toml`.

```sh
ply search QUERY [--versions] [--limit N] [--source SPEC] [--json]
```
Search a source's catalog. One line per package, paste-ready:
`ffmpeg = "6.1"   # Multimedia framework   x64 arm64`. `--versions` lists
every published version and arch. The source is `--source`, else the
`[sources] default` of `./ply.toml`, else the official registry.

```sh
ply add NAME[@RANGE] [--source NAME]
```
Add a dependency to `./ply.toml`. Without a range, takes the latest
`major.minor` from the catalog. Comments and formatting are preserved.
Then `ply build` to resolve and lock.

```sh
ply build [DIR] [-o FILE] [--insecure-source]
```
Resolve dependencies (writing `ply.lock`), produce a deterministic image
named `<name>-<version>-<os>-<arch>.img`.

```sh
ply check IMAGE [--against policy.toml]
```
Validate an image; with `--against`, check it against a host runtime policy.
Pure function — wire it into CI.
````

- [ ] **Step 2: `docs/registries.md`** — insert after the "Version listing (index.json)" section (before "## Private packages"):

````markdown
## Catalog (state.json)

`ply search` and `ply add` read an optional catalog at the source's
**prefix** — the template with `/{package}` removed:

| source | catalog |
|---|---|
| `https://registry.plybox.sh/ply/{package}` | `https://registry.plybox.sh/ply/state.json` |
| `https://artifacts.corp.net/ply` | `https://artifacts.corp.net/ply/state.json` |
| `file:///srv/ply-packages/{package}` | `/srv/ply-packages/state.json` |

The official registry publishes it; for your own host, a minimal one is:

```json
{ "packages": [
  { "name": "ffmpeg", "description": "Multimedia framework", "license": "LGPL-2.1",
    "versions": [
      { "version": "6.1.1", "img": "ffmpeg-6.1.1-linux-x64.img" },
      { "version": "6.1.1", "img": "ffmpeg-6.1.1-linux-arm64.img" } ] } ] }
```

Only `name`, `version` and `img` are required; `arch` is derived from the
filename when absent. Forge sources have no catalog. Neither `index.json`
nor `state.json` is needed to fetch a pinned version.
````

- [ ] **Step 3: `docs/image-format.md`** — change the protocol block to:

```
<base-url>/<name>-<version>-<os>-<arch>.img    # GET an image
<base-url>/index.json                          # optional: list for range resolution
<prefix>/state.json                            # optional: catalog for ply search / ply add
```

and in the paragraph below it, replace "The official registry additionally publishes `state.json` at the bucket root — a machine-readable snapshot (packages, versions, sizes, timestamps) that powers [registry.plybox.sh](https://registry.plybox.sh)." with "The official registry publishes `state.json` at the bucket root (for [plybox.sh/registry](https://plybox.sh/registry/)) and at each namespace prefix (for `ply search`); see [Registries](/docs/registries/)."

- [ ] **Step 4: `docs/docker.md`** — add two rows after the `docker build .` row:

```markdown
| `docker search` | `ply search` | same idea; the catalog is a static file next to the images, no API |
| `docker init` | `ply init` | writes a manifest, not a Dockerfile |
```

Then run `cargo test -p ply-cli cli` — `hints_only_cover_nonexistent_subcommands` must still pass (neither `search` nor `init` is in the hint table).

- [ ] **Step 5: `docs/quickstart.md`** — in "Write a manifest", before the TOML block, add:

```markdown
`ply init` writes this for you (it detects Python/Node projects and asks a
few questions); `ply add python3` adds a dependency at its latest version.
By hand, it is:
```

- [ ] **Step 6: Rebuild the site's docs check**

Run: `cd app && npm run sync-content && npx tsc --noEmit -p .`
Expected: clean (the docs are synced into the site at build time; no code change needed).

Do not commit.

---

### Task 8: Final verification

- [ ] **Step 1: Whole workspace**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets && cargo test --workspace`
Expected: all green.

- [ ] **Step 2: Help text sanity**

Run: `cargo run -q -p ply-cli -- --help | sed -n 1,25p` and `cargo run -q -p ply-cli -- search --help`, `-- add --help`, `-- init --help`.
Expected: `init`, `build`, `search`, `add` appear in that order near the top; flags as specified.

- [ ] **Step 3: Hand-off list for the owner**

Report: files changed, tests added (count), the one action only they can do (`./scripts/registry-push.mjs --state-only`), and the follow-ups parked in the spec §9.
