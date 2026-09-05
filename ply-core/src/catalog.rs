//! Package catalog: the optional `state.json` a source publishes next to its
//! packages, read by `ply search` / `ply add` / `ply init`.
//!
//! It also holds every door through which ply turns a REFERENCE into image
//! bytes on this host, and each carries a different guard — which door you
//! take is a decision, not a detail. `fetch_app_image` fetches the newest
//! published match and refuses a keg; `fetch_app_image_unless` is that one
//! with the download skipped when the caller already holds the resolved
//! image; `fetch_app_image_pinned` takes an exact version and digest from a
//! lockfile, with no catalog read and nothing to guess; `fetch_keg_image` is
//! `pub(crate)` and carries NO `is_app` guard, for runtime artifacts the ply
//! binary fetches for itself (today only `ply/microvm-kernel`).
//! `fetch_resolved_image` is their shared, guardless body, private on
//! purpose: it is not a fifth door.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::source::{http_get_string, Source};
use crate::{Error, Result};

/// The official registry, as a `[sources]` template.
pub const OFFICIAL_SOURCE: &str = "https://registry.plybox.sh/ply/{package}";

/// Where `ply run <name>` resolves a BARE name. The same shelf as
/// `OFFICIAL_SOURCE`: a package is `<namespace>/<name>`, and whether it is
/// runnable is a property of the package (it has an entrypoint), not an
/// address. `apps/` used to hold the runnable ones, which made a type into a
/// location — and forced redis to exist twice, once per shelf. Both constants
/// remain because callers mean different things by them; they now name one
/// namespace. `apps/` still serves its old copies for hosts released before
/// this changed.
pub const OFFICIAL_RUN_SOURCE: &str = "https://registry.plybox.sh/ply/{package}";

/// The file a source publishes at its prefix. Same shape the website reads.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Catalog {
    #[serde(default)]
    pub packages: Vec<Package>,
}

/// What a package IS. `app` = one runnable image; `layer` = a keg consumed
/// via `[dependencies]`; `stack` = no image of its own, a list of runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageKind {
    #[default]
    App,
    Layer,
    Stack,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Package {
    #[serde(default)]
    pub namespace: String,
    pub name: String,
    /// Always emitted, so consumers can filter without guessing.
    #[serde(rename = "type", default)]
    pub kind: PackageKind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub homepage: String,
    /// v3: the namespace this package actually publishes under — a v2
    /// catalog carried this only implicitly (as `namespace`). Empty and
    /// omitted for a v2 entry.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    #[serde(default)]
    pub versions: Vec<ImageVersion>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ImageVersion {
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    /// Canonical location: a full, http-fetchable URL to the artifact
    /// (image, or the stack toml for a `stack`). Always present in v2.
    #[serde(default)]
    pub src: String,
    /// Image filename; serialized as `null` for a stack (no image of its
    /// own) — the key is always present so consumers never guess.
    #[serde(default)]
    pub img: Option<String>,
    /// Legacy pre-v2 field: a bare path. Read-tolerated, never written.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default)]
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pushed_at: String,
    /// Derived from the image manifest at push (never hand-authored).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// Shape is the publisher's: v2 wrote `[{name, version}]`, v3 writes an
    /// object (`name -> range`) — this stays untyped and tolerant so both
    /// parse; nothing in the CLI reads it structurally.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub dependencies: serde_json::Value,
    /// v3: URL of the `.toml` manifest embedded in the artifact — what
    /// `record_for_toml`/`record_for_image` would also produce from the
    /// bytes themselves. Empty (and omitted) for a v2 entry, which predates
    /// it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manifest: String,
    /// v3: whether the registry has re-hashed the artifact against the
    /// record's sha256 (an artifact published with `--src` starts
    /// unverified). `None` for a v2 entry that predates the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
}

impl ImageVersion {
    /// Explicit `arch` if present, else derived from the canonical filename
    /// (`…-linux-arm64.img`), exactly as the website does.
    pub fn arch(&self) -> &str {
        match self.arch.as_deref() {
            Some(a) if !a.is_empty() => a,
            _ if self
                .img
                .as_deref()
                .is_some_and(|i| i.ends_with("-arm64.img")) =>
            {
                "arm64"
            }
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

/// `postgres` / `iluxav/myapp@1.2` — the reference grammar for registry
/// lookups, with an optional namespace. Returns (reference, version), the
/// reference keeping its namespace so `fetch_app_image` knows where to
/// look. A namespaced ref is how a deployment or a stack member follows a
/// published app: the newest matching version wins on every resolve, which
/// is what makes "push a version, the host converges" work without a URL
/// to bump by hand.
pub fn parse_namespaced_ref(spec: &str) -> Option<(String, Option<String>)> {
    match spec.split_once('/') {
        Some((ns, rest)) => {
            // both halves obey the package-name grammar; no nesting
            let (name, want) = parse_run_ref(rest)?;
            let (ns_ok, _) = parse_run_ref(ns)?;
            Some((format!("{ns_ok}/{name}"), want))
        }
        None => parse_run_ref(spec),
    }
}

/// `postgres` / `myapp@1.2` — the reference grammar `ply run` accepts for
/// registry lookups. Returns (name, version prefix). Anything with a path
/// separator or an `.img` suffix is not a reference (it's a file path).
pub fn parse_run_ref(spec: &str) -> Option<(String, Option<String>)> {
    if spec.contains('/') || spec.ends_with(".img") {
        return None;
    }
    let (name, version) = match spec.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    };
    let name_ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !name_ok {
        return None;
    }
    if let Some(v) = version {
        let parts: Vec<&str> = v.split('.').collect();
        let v_ok = !parts.is_empty()
            && parts.len() <= 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !v_ok {
            return None;
        }
        return Some((name.to_string(), Some(v.to_string())));
    }
    Some((name.to_string(), None))
}

/// `17` matches 17.x.y, `17.10` matches 17.10.x, `17.10.0` exactly.
fn version_matches(version: &semver::Version, want: &str) -> bool {
    let parts: Vec<u64> = want.split('.').filter_map(|p| p.parse().ok()).collect();
    // `@latest`, `@stable`, `@v17`: nothing numeric parsed, so the zip below
    // would be empty and `all` vacuously true — matching EVERY version and
    // silently handing back the newest. A pin that says nothing is a typo,
    // not a wildcard; matching nothing turns it into "no published X@latest".
    if parts.is_empty() && !want.is_empty() {
        return false;
    }
    let actual = [version.major, version.minor, version.patch];
    parts.iter().zip(actual.iter()).all(|(w, a)| w == a)
}

fn split_ref_version(s: &str) -> (&str, Option<String>) {
    match s.split_once('@') {
        Some((n, v)) => (n, Some(v.to_string())),
        None => (s, None),
    }
}

fn pick_stack_version<'a>(pkg: &'a Package, want: Option<&str>) -> Option<&'a ImageVersion> {
    pkg.versions
        .iter()
        .filter(|v| match want {
            Some(w) => semver::Version::parse(&v.version)
                .map(|sv| version_matches(&sv, w))
                .unwrap_or(false),
            None => true,
        })
        .max_by_key(|v| version_key(&v.version))
}

/// Resolve a registry stack reference to its parsed Stack: load the namespace
/// catalog, confirm the package is a stack, fetch the version's `src` (the
/// stack toml, holes intact), and parse it. `reference` is `namespace/name`
/// (optionally `@version`) or a bare `name` resolved via `default_source`.
pub fn fetch_stack(reference: &str, default_source: &str) -> Result<crate::stack::Stack> {
    let (source_spec, name, want) = match reference.split_once('/') {
        Some((ns, rest)) => {
            let (name, want) = split_ref_version(rest);
            (
                format!("https://registry.plybox.sh/{ns}/{{package}}"),
                name.to_string(),
                want,
            )
        }
        None => {
            let (name, want) = split_ref_version(reference);
            (default_source.to_string(), name.to_string(), want)
        }
    };
    let source = Source::parse(&source_spec, false)?;
    let catalog = Catalog::load(&source)?;
    let pkg = catalog.get(&name).ok_or_else(|| {
        Error::Source(format!(
            "no `{reference}` in the registry — `ply search {name}` lists what exists"
        ))
    })?;
    if pkg.kind != PackageKind::Stack {
        let what = match pkg.kind {
            PackageKind::App => "an app",
            PackageKind::Layer => "a layer",
            PackageKind::Stack => "a stack",
        };
        return Err(Error::Source(format!(
            "`{reference}` is {what}, not a stack — `ply run {reference}` runs an app"
        )));
    }
    let version = pick_stack_version(pkg, want.as_deref()).ok_or_else(|| {
        Error::Source(format!(
            "no published `{reference}`{}",
            want.map(|w| format!("@{w}")).unwrap_or_default()
        ))
    })?;
    if version.src.is_empty() {
        return Err(Error::Source(format!(
            "`{reference}` {}: the catalog entry has no src URL to fetch",
            version.version
        )));
    }
    let text = http_get_string(&version.src)
        .map_err(|e| Error::Source(format!("fetching {} failed ({e})", version.src)))?;
    crate::stack::parse(&text, std::path::Path::new(reference))?.ok_or_else(|| {
        Error::Source(format!(
            "`{reference}` src is not a stack file (no [[app]]): {}",
            version.src
        ))
    })
}

/// Whether the bytes came down the wire in this call, or were already on
/// this host under the resolved name. The app guard applies only to a fresh
/// download — `have`'s answer was fetched, verified and named by an earlier
/// call that already ran the guard, and re-reading its manifest on every
/// reconcile beat is exactly the work `have` exists to avoid.
enum Fetched {
    Downloaded,
    AlreadyHere,
}

/// Resolve `name`(`@want`) against a source and put the newest matching
/// image in the store.
///
/// The shared body of `fetch_app_image_unless` and `fetch_keg_image`:
/// namespaced ref → source → `list_versions` → newest match → `ImageName` →
/// `have` or `source.fetch`. It deliberately says NOTHING about what kind of
/// package came back; that question belongs to the caller, because the two
/// callers answer it differently.
fn fetch_resolved_image(
    name: &str,
    want: Option<&str>,
    source_spec: &str,
    have: impl Fn(&crate::image::name::ImageName) -> Option<PathBuf>,
) -> Result<(PathBuf, crate::image::name::ImageName, String, Fetched)> {
    use crate::image::name::{Arch, ImageName, Os};

    // `<namespace>/<name>` looks in that namespace's catalog; a bare name
    // uses the caller's source. One resolution point, so every caller —
    // `ply run`, a deployment's `app =`, a stack member's `run =` — gets
    // namespaced refs for free.
    let (source_spec, name) = match name.split_once('/') {
        Some((ns, rest)) => (
            format!("https://registry.plybox.sh/{ns}/{{package}}"),
            rest.to_string(),
        ),
        None => (source_spec.to_string(), name.to_string()),
    };
    let (source_spec, name) = (source_spec.as_str(), name.as_str());
    let source = Source::parse(source_spec, false)?;
    let store = crate::store::Store::open_default()?;
    let (os, arch) = (Os::Linux, Arch::host());
    let mut versions = source.list_versions(name, os, arch)?;
    if let Some(want) = want {
        versions.retain(|v| version_matches(v, want));
    }
    let Some(version) = versions.into_iter().max() else {
        return Err(Error::Source(match want {
            Some(w) => format!(
                "no published `{name}@{w}` for {}-{} — `ply search {name}` lists what exists",
                os.as_str(),
                arch.as_str()
            ),
            None => format!(
                "no published `{name}` for {}-{} — `ply search {name}` lists what exists",
                os.as_str(),
                arch.as_str()
            ),
        }));
    };
    let image = ImageName::new(name, version, os, arch)?;
    if let Some(path) = have(&image) {
        // Already on this host under its own name: it was fetched, verified
        // and hardlinked by a previous beat. Its digest is the store's.
        let digest = crate::digest::sha256_file(&path)?;
        return Ok((path, image, digest, Fetched::AlreadyHere));
    }
    let (digest, path) = source.fetch(&image, None, &store)?;
    Ok((path, image, digest, Fetched::Downloaded))
}

/// Resolve a name reference against a source, fetch the newest matching app
/// image into the store, and return its path.
///
/// Interactive semantics, deliberately not MVS: `ply run postgres` means
/// "the latest published postgres" — locked, repeatable resolution is what
/// `ply build` + a lockfile are for.
pub fn fetch_app_image(
    name: &str,
    want: Option<&str>,
    source_spec: &str,
) -> Result<(PathBuf, crate::image::name::ImageName, String)> {
    fetch_app_image_unless(name, want, source_spec, |_| None)
}

/// `fetch_app_image`, but the caller may already HAVE the resolved image
/// somewhere ply trusts — reconcile keeps the last deployed one hardlinked
/// under `/var/lib/ply/deploys/<name>/<filename>`. `have` is asked once the
/// version is known and before any bytes move; `Some(path)` short-circuits
/// the download entirely.
///
/// Without this, every `auto = true` deployment re-downloaded and re-hashed
/// its full image on every 1-minute beat, only for `store.insert` to find
/// the digest already present and discard the bytes. Resolution (one catalog
/// read) is the cheap part and still runs, so a new version is still seen.
pub fn fetch_app_image_unless(
    name: &str,
    want: Option<&str>,
    source_spec: &str,
    have: impl Fn(&crate::image::name::ImageName) -> Option<PathBuf>,
) -> Result<(PathBuf, crate::image::name::ImageName, String)> {
    let (path, image, digest, fetched) = fetch_resolved_image(name, want, source_spec, have)?;
    // The guard that makes `ply run <a keg>` a refusal rather than a
    // confusing failure much later. It reads the image's manifest, so it
    // runs only on the download path, exactly as it did before
    // `fetch_keg_image` gave this function a sibling.
    if matches!(fetched, Fetched::Downloaded) {
        let manifest = crate::image::read::read_manifest(&path)?;
        if !manifest.is_app() {
            return Err(Error::Source(format!(
                "`{image}` is a library package (keg), not a runnable app — add it to a ply.toml [dependencies] instead"
            )));
        }
    }
    Ok((path, image, digest))
}

/// Fetch a published image by ref, without requiring it to be runnable.
///
/// The second door, and a deliberately narrow one: it exists for RUNTIME
/// artifacts that are neither an app nor any app's dependency — things the
/// ply binary fetches for ITSELF, that no `ply.toml` names and no `ply.lock`
/// ever mentions. Today `ply/microvm-kernel` is the only such artifact: the
/// macOS VM backend pins a kernel version in `runtime::vm::kernel` and
/// fetches it the first time a microVM boots.
///
/// Everything a user asks for by name goes through `fetch_app_image`, whose
/// `is_app` guard is what stops a keg being `ply run`. Do not reach for this
/// to silence that guard — the guard is the answer, not the obstacle. A keg
/// that an app needs belongs in `[dependencies]`, where the resolver already
/// handles it.
///
/// `source_spec` is the fallback for a BARE name; a `<ns>/<name>` ref names
/// its own namespace, as everywhere else. In production there is exactly one
/// caller and it passes `OFFICIAL_RUN_SOURCE` — a runtime artifact has no
/// manifest to carry a `[sources]` entry, so there is no user-supplied source
/// to honour. It is a parameter so that this door can be TESTED over a
/// `file://` source: without it, repointing this function at
/// `fetch_app_image_unless` — the exact regression the split exists to
/// prevent — passed every test in the crate.
///
/// `pub(crate)`, not `pub`: its one caller is `runtime::vm::kernel`, in this
/// crate. Prose is not what should stop the next person reaching for it to
/// silence an inconvenient `is_app` error.
pub(crate) fn fetch_keg_image(
    name: &str,
    want: Option<&str>,
    source_spec: &str,
) -> Result<(PathBuf, crate::image::name::ImageName, String)> {
    let (path, image, digest, _) = fetch_resolved_image(name, want, source_spec, |_| None)?;
    Ok((path, image, digest))
}

/// A lock-pinned app image: straight from the store when the digest is
/// already there (no index fetch, no download — `ply up` offline), else one
/// digest-verified download.
pub fn fetch_app_image_pinned(
    name: &str,
    version: &str,
    digest: &str,
    source_spec: &str,
) -> Result<(PathBuf, crate::image::name::ImageName)> {
    use crate::image::name::{Arch, ImageName, Os};

    let store = crate::store::Store::open_default()?;
    let version = semver::Version::parse(version)
        .map_err(|e| Error::Source(format!("locked version `{version}` for `{name}`: {e}")))?;
    let image = ImageName::new(name, version, Os::Linux, Arch::host())?;
    if let Some(path) = store.image_path(digest) {
        return Ok((path, image));
    }
    let source = Source::parse(source_spec, false)?;
    let (_digest, path) = source.fetch(&image, Some(digest), &store)?;
    Ok((path, image))
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_non_numeric_pin_matches_nothing_not_everything() {
        use super::version_matches;
        let v = semver::Version::parse("17.10.6").unwrap();
        assert!(version_matches(&v, "17"));
        assert!(version_matches(&v, "17.10"));
        assert!(version_matches(&v, "17.10.6"));
        assert!(!version_matches(&v, "16"));
        // `@latest` parsed to zero numeric parts, so `all()` over an empty
        // zip was vacuously true and it matched every version — silently
        // handing back the newest. A pin that says nothing is a typo.
        assert!(!version_matches(&v, "latest"));
        assert!(!version_matches(&v, "stable"));
    }
    use super::*;
    use crate::source::Source;

    /// The two doors, side by side, over a real image each.
    ///
    /// A `file://` source is a Dir source: `list_versions` reads the
    /// directory and `fetch` copies the file, so this exercises the whole
    /// shared resolution body — namespaced-or-bare ref, version pick,
    /// `ImageName`, store insert — without a network or a registry.
    ///
    /// It calls `fetch_keg_image` ITSELF, not the shared body underneath it:
    /// that function is the thing the VM backend depends on, and pointing it
    /// at `fetch_app_image_unless` must fail a test rather than a Mac. What
    /// this cannot cover is the source constant its caller passes — that is
    /// `runtime::vm::kernel`'s to get right.
    #[test]
    fn a_keg_is_refused_as_an_app_and_accepted_as_a_runtime_artifact() {
        use crate::image::name::{Arch, ImageName, Os};

        let td = tempfile::tempdir().unwrap();
        let src_dir = td.path().join("source");
        let build = td.path().join("build");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&build).unwrap();

        let put = |name: &str, manifest: &str| {
            let img = crate::image::squashfs::test_image_with_manifest(&build, manifest);
            let version = semver::Version::parse("1.0.0").unwrap();
            let canonical = ImageName::new(name, version, Os::Linux, Arch::host())
                .unwrap()
                .to_string();
            std::fs::rename(&img, src_dir.join(canonical)).unwrap();
        };
        // No entrypoint: `is_app()` is false, which is what makes it a keg.
        put(
            "plytestkeg",
            "[package]\nname = \"plytestkeg\"\nversion = \"1.0.0\"\n",
        );
        put(
            "plytestapp",
            "[package]\nname = \"plytestapp\"\nversion = \"1.0.0\"\nentrypoint = [\"/bin/true\"]\n",
        );

        let spec = format!("file://{}", src_dir.display());
        // The store is process-global state; this is the only test in the
        // crate that touches it, and it puts it back.
        let previous = std::env::var_os("PLY_STORE");
        std::env::set_var("PLY_STORE", td.path().join("store"));

        let keg_as_app = fetch_app_image("plytestkeg", None, &spec);
        let keg_as_keg = fetch_keg_image("plytestkeg", None, &spec);
        let app_as_app = fetch_app_image("plytestapp", None, &spec);

        match previous {
            Some(v) => std::env::set_var("PLY_STORE", v),
            None => std::env::remove_var("PLY_STORE"),
        }

        // The guard the VM backend must NOT launder: a keg is still not a
        // runnable app, and the message still names the way out.
        let err = keg_as_app.expect_err("a keg is not runnable").to_string();
        assert!(
            err.contains("library package (keg), not a runnable app"),
            "{err}"
        );
        // The second door: the same image, fetched as a runtime artifact.
        let (path, image, digest) = keg_as_keg.expect("a keg fetches as a runtime artifact");
        assert!(path.exists(), "the bytes are in the store");
        assert_eq!(image.name, "plytestkeg");
        assert!(digest.starts_with("sha256:"), "{digest}");
        // And the app path is untouched by the split.
        let (_, app, _) = app_as_app.expect("an app still fetches as an app");
        assert_eq!(app.name, "plytestapp");
    }

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
        assert!(ffmpeg.versions[0].img.as_deref().unwrap().ends_with(".img"));
        assert!(cat.get("nope").is_none());
        // The v2 fixture predates `manifest`/`verified` (task 9) — both must
        // come back at their defaults, not fail to parse.
        assert!(
            ffmpeg.versions[0].manifest.is_empty(),
            "v2 entries have no manifest url"
        );
        assert!(
            ffmpeg.versions[0].verified.is_none(),
            "v2 entries predate verified"
        );
    }

    #[test]
    fn a_v3_state_literal_parses_and_round_trips_the_new_fields() {
        // The registry's v3 wire shape: a `manifest` URL, a `verified` bool,
        // object-shaped `dependencies` (v2 wrote an array of {name,version}),
        // a `params` object, and a package-level `owner`. `params` has no
        // dedicated field on `ImageVersion` — it must still parse (ignored
        // as an unknown key), same as the extra `publish`/`sha256` keys the
        // registry also writes.
        let text = r#"{
  "packages": [
    {
      "namespace": "iluxav",
      "name": "myapp",
      "type": "app",
      "description": "demo app",
      "license": "MIT",
      "homepage": "",
      "owner": "iluxav",
      "versions": [
        {
          "version": "1.2.3",
          "arch": "x64",
          "img": "myapp-1.2.3-linux-x64.img",
          "src": "https://registry.plybox.sh/iluxav/myapp/myapp-1.2.3-linux-x64.img",
          "sha256": "deadbeef",
          "bytes": 12345,
          "pushed_at": "2026-09-01T00:00:00Z",
          "manifest": "https://registry.plybox.sh/iluxav/myapp/myapp-1.2.3.toml",
          "verified": true,
          "volumes": ["/data"],
          "links": [],
          "publish": "80",
          "dependencies": {"node": "24"},
          "params": {"password": {"secret": true}}
        }
      ]
    }
  ]
}"#;
        let cat = Catalog::parse(text, "v3").unwrap();
        let pkg = cat.get("myapp").expect("myapp in the v3 catalog");
        assert_eq!(pkg.owner, "iluxav");
        let v = &pkg.versions[0];
        assert_eq!(
            v.manifest,
            "https://registry.plybox.sh/iluxav/myapp/myapp-1.2.3.toml"
        );
        assert_eq!(v.verified, Some(true));
        assert_eq!(v.dependencies, serde_json::json!({"node": "24"}));

        // Round-trips: the fields this task added come back out on
        // serialize, at the same JSON keys the registry reads.
        let j = serde_json::to_value(&cat).unwrap();
        assert_eq!(j["packages"][0]["owner"], "iluxav");
        let jv = &j["packages"][0]["versions"][0];
        assert_eq!(
            jv["manifest"],
            "https://registry.plybox.sh/iluxav/myapp/myapp-1.2.3.toml"
        );
        assert_eq!(jv["verified"], true);
        assert_eq!(jv["dependencies"], serde_json::json!({"node": "24"}));
    }

    /// ply 0.1.68's catalog types, copied VERBATIM from the released tag
    /// (`git show v0.1.68:ply-core/src/catalog.rs`). Never widened, never
    /// kept in sync with the types above: this is the reader already
    /// installed on every machine, and the golden literal below is what the
    /// registry now writes. If a change to the wire breaks this test, it
    /// breaks `ply search`/`ply add`/`ply up <ns>/<stack>` for everyone who
    /// has not upgraded — `Catalog::parse` is ONE `serde_json::from_str`
    /// over the whole document, so one badly-typed field fails all of it.
    // Every field is declared because 0.1.68 declares it — the TYPES are the
    // assertion, whether or not this test reads each one back.
    #[allow(dead_code)]
    mod released_0_1_68 {
        use serde::Deserialize;

        #[derive(Debug, Clone, Default, Deserialize)]
        pub struct Catalog {
            #[serde(default)]
            pub packages: Vec<Package>,
        }

        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum PackageKind {
            #[default]
            App,
            Layer,
            Stack,
        }

        #[derive(Debug, Clone, Default, Deserialize)]
        pub struct Package {
            #[serde(default)]
            pub namespace: String,
            pub name: String,
            #[serde(rename = "type", default)]
            pub kind: PackageKind,
            #[serde(default)]
            pub description: String,
            #[serde(default)]
            pub license: String,
            #[serde(default)]
            pub homepage: String,
            #[serde(default)]
            pub versions: Vec<ImageVersion>,
        }

        #[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
        pub struct Dep {
            pub name: String,
            #[serde(default)]
            pub version: String,
        }

        #[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
        pub struct StackApp {
            pub run: String,
            #[serde(default)]
            pub name: Option<String>,
            #[serde(default)]
            pub e: Vec<String>,
            #[serde(default)]
            pub after: Vec<String>,
            #[serde(default)]
            pub publish: Vec<String>,
            #[serde(default)]
            pub volume: Vec<String>,
            #[serde(default)]
            pub domain: Vec<String>,
            #[serde(default)]
            pub scale: Option<u32>,
        }

        #[derive(Debug, Clone, Default, Deserialize)]
        pub struct ImageVersion {
            pub version: String,
            #[serde(default)]
            pub arch: Option<String>,
            #[serde(default)]
            pub src: String,
            #[serde(default)]
            pub img: Option<String>,
            #[serde(default)]
            pub path: String,
            #[serde(default)]
            pub bytes: u64,
            #[serde(default)]
            pub pushed_at: String,
            #[serde(default)]
            pub volumes: Vec<String>,
            #[serde(default)]
            pub links: Vec<String>,
            #[serde(default)]
            pub dependencies: Vec<Dep>,
            #[serde(default)]
            pub apps: Vec<StackApp>,
        }
    }

    /// Exactly what `app/lib/catalog-files.ts` writes today for one package
    /// with one version: `manifest`, `verified` and `params` are new KEYS
    /// (0.1.68 ignores what it does not declare), and `dependencies` keeps
    /// the v2 ARRAY type it has always had. Additive means new keys — never
    /// a changed type.
    const V3_WIRE: &str = r#"{
 "updated": "2026-09-02T16:00:00.000Z",
 "packages": [
  {
   "namespace": "ply",
   "owner": "ply",
   "name": "postgres",
   "type": "app",
   "description": "PostgreSQL relational database",
   "license": "PostgreSQL",
   "homepage": "https://www.postgresql.org",
   "versions": [
    {
     "version": "17.10.7",
     "img": "postgres-17.10.7-linux-x64.img",
     "arch": "x64",
     "src": "https://registry.plybox.sh/ply/postgres/postgres-17.10.7-linux-x64.img",
     "sha256": "aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44",
     "bytes": 12345,
     "pushed_at": "2026-09-02T15:51:37.000Z",
     "manifest": "https://registry.plybox.sh/ply/postgres/postgres-17.10.7.toml",
     "verified": true,
     "volumes": ["/var/lib/postgresql/data"],
     "publish": "internal:5432",
     "dependencies": [{"name": "postgresql17", "version": "17"}, {"name": "rclone", "version": "1.60"}],
     "params": {"user": "postgres", "password": {"secret": true}}
    }
   ]
  }
 ]
}"#;

    #[test]
    fn the_released_cli_still_parses_what_the_registry_writes() {
        let cat: released_0_1_68::Catalog =
            serde_json::from_str(V3_WIRE).expect("ply 0.1.68 must parse the v3 wire shape");
        let pkg = &cat.packages[0];
        assert_eq!(pkg.name, "postgres");
        assert_eq!(pkg.kind, released_0_1_68::PackageKind::App);
        assert_eq!(pkg.description, "PostgreSQL relational database");
        let v = &pkg.versions[0];
        assert_eq!(v.img.as_deref(), Some("postgres-17.10.7-linux-x64.img"));
        assert_eq!(v.bytes, 12345);
        assert_eq!(v.volumes, vec!["/var/lib/postgresql/data".to_string()]);
        // the field that matters: an ARRAY of {name, version}, as 0.1.68 types it
        assert_eq!(
            v.dependencies,
            vec![
                released_0_1_68::Dep {
                    name: "postgresql17".into(),
                    version: "17".into()
                },
                released_0_1_68::Dep {
                    name: "rclone".into(),
                    version: "1.60".into()
                },
            ]
        );
    }

    #[test]
    fn this_cli_parses_the_same_wire_bytes() {
        // The same literal through today's reader: one file, two generations.
        let cat = Catalog::parse(V3_WIRE, "v3").unwrap();
        let pkg = cat.get("postgres").expect("postgres in the v3 catalog");
        assert_eq!(pkg.owner, "ply");
        let v = &pkg.versions[0];
        assert_eq!(v.verified, Some(true));
        assert_eq!(
            v.manifest,
            "https://registry.plybox.sh/ply/postgres/postgres-17.10.7.toml"
        );
        assert_eq!(v.dependencies[0]["name"], "postgresql17");
        assert_eq!(v.arch(), "x64");
    }

    #[test]
    fn v2_array_shaped_dependencies_still_parse() {
        // v2 wrote `dependencies` as `[{name, version}]`; v3 writes an
        // object. `ImageVersion::dependencies` stays untyped `Value`
        // specifically so both shapes parse without error.
        let text = r#"{"packages":[{"name":"x","versions":[{"version":"1.0.0","img":"x-1.0.0-linux-x64.img","dependencies":[{"name":"rclone","version":"1.68"}]}]}]}"#;
        let cat = Catalog::parse(text, "v2").unwrap();
        let v = &cat.get("x").unwrap().versions[0];
        assert_eq!(v.dependencies[0]["name"], "rclone");
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
            img: Some("x-1.0.0-linux-arm64.img".into()),
            arch: Some("x64".into()),
            ..Default::default()
        };
        assert_eq!(v.arch(), "x64");
    }

    #[test]
    fn serializes_v2_shape() {
        // an app: type=app, src is a URL, derived volumes/deps, no legacy path
        let app = Package {
            namespace: "ply".into(),
            name: "postgres".into(),
            kind: PackageKind::App,
            description: "db".into(),
            versions: vec![ImageVersion {
                version: "17.10.3".into(),
                arch: Some("x64".into()),
                src: "https://registry.plybox.sh/ply/postgres/postgres-17.10.3-linux-x64.img"
                    .into(),
                img: Some("postgres-17.10.3-linux-x64.img".into()),
                bytes: 42,
                volumes: vec!["/var/lib/postgresql/data".into()],
                dependencies: serde_json::json!([{"name": "rclone", "version": "1.68"}]),
                ..Default::default()
            }],
            ..Default::default()
        };
        let j = serde_json::to_value(Catalog {
            packages: vec![app],
        })
        .unwrap();
        assert_eq!(j["packages"][0]["type"], "app");
        let v = &j["packages"][0]["versions"][0];
        assert!(v["src"].as_str().unwrap().starts_with("https://"));
        assert_eq!(v["img"], "postgres-17.10.3-linux-x64.img");
        assert_eq!(v["volumes"][0], "/var/lib/postgresql/data");
        assert_eq!(v["dependencies"][0]["name"], "rclone");
        assert!(v.get("path").is_none(), "legacy path is never written");

        // a stack: img null, no arch — its members live in the stack toml the
        // `src` points at, never in a second copy here
        let pkg = Package {
            namespace: "ply".into(),
            name: "umami".into(),
            kind: PackageKind::Stack,
            versions: vec![ImageVersion {
                version: "3.0.0".into(),
                src: "https://registry.plybox.sh/ply/umami/umami-3.0.0.stack.toml".into(),
                img: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let j = serde_json::to_value(Catalog {
            packages: vec![pkg],
        })
        .unwrap();
        assert_eq!(j["packages"][0]["type"], "stack");
        let v = &j["packages"][0]["versions"][0];
        assert!(v["img"].is_null(), "a stack's img is explicit null");
        assert!(v.get("dependencies").is_none(), "null deps are not written");

        // and it round-trips back
        let back: Catalog = serde_json::from_value(j).unwrap();
        assert_eq!(back.packages[0].kind, PackageKind::Stack);
        assert_eq!(back.packages[0].versions[0].version, "3.0.0");
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
                    img: Some((*img).into()),
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
    fn run_ref_grammar() {
        assert_eq!(parse_run_ref("postgres"), Some(("postgres".into(), None)));
        assert_eq!(
            parse_run_ref("postgres@17"),
            Some(("postgres".into(), Some("17".into())))
        );
        assert_eq!(
            parse_run_ref("myapp@1.2.3"),
            Some(("myapp".into(), Some("1.2.3".into())))
        );
        // paths and image files are not references
        assert_eq!(parse_run_ref("db/labs-db-0.1.0-linux-x64.img"), None);
        assert_eq!(parse_run_ref("labs-db-0.1.0-linux-x64.img"), None);
        assert_eq!(parse_run_ref("./postgres"), None);
        // bad shapes
        assert_eq!(parse_run_ref("Postgres"), None);
        assert_eq!(parse_run_ref("postgres@"), None);
        assert_eq!(parse_run_ref("postgres@17.x"), None);
        assert_eq!(parse_run_ref("postgres@1.2.3.4"), None);
        assert_eq!(parse_run_ref(""), None);
    }

    #[test]
    fn namespaced_ref_grammar() {
        // a bare name still resolves, unchanged
        assert_eq!(
            parse_namespaced_ref("postgres@17"),
            Some(("postgres".into(), Some("17".into())))
        );
        // the namespace rides along in the returned reference
        assert_eq!(
            parse_namespaced_ref("ply/ply-web"),
            Some(("ply/ply-web".into(), None))
        );
        assert_eq!(
            parse_namespaced_ref("iluxav/myapp@1.2"),
            Some(("iluxav/myapp".into(), Some("1.2".into())))
        );
        // an image filename is still a path, not a reference
        assert_eq!(parse_namespaced_ref("db/labs-db-0.1.0-linux-x64.img"), None);
        assert_eq!(parse_namespaced_ref("./web"), None);
        // no nesting, no empty halves, same grammar on both sides
        assert_eq!(parse_namespaced_ref("a/b/c"), None);
        assert_eq!(parse_namespaced_ref("/web"), None);
        assert_eq!(parse_namespaced_ref("ply/"), None);
        assert_eq!(parse_namespaced_ref("Ply/web"), None);
    }

    #[test]
    fn version_prefix_matching() {
        let v = semver::Version::new(17, 10, 0);
        assert!(version_matches(&v, "17"));
        assert!(version_matches(&v, "17.10"));
        assert!(version_matches(&v, "17.10.0"));
        assert!(!version_matches(&v, "17.9"));
        assert!(!version_matches(&v, "16"));
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
