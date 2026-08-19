//! Minimal Version Selection (Go-style): pick the *lowest* version satisfying
//! all constraints. No solver. Resolution output changes only when a manifest
//! changes — upgrades are deliberate acts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use semver::{Version, VersionReq};

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::read_manifest;
use crate::manifest::{DepSpec, Manifest};
use crate::source::Source;
use crate::store::Store;

#[derive(Debug)]
pub struct ResolvedPackage {
    pub name: String,
    pub version: Version,
    /// Source spec string as written (recorded in the lockfile).
    pub source_spec: String,
    pub digest: String,
    pub image_path: PathBuf,
    pub manifest: Manifest,
}

#[derive(Debug)]
pub struct Resolution {
    /// Overlay order: dependents before dependencies, base last
    /// (`lowerdir=app:…:base`), ties broken by digest.
    pub packages: Vec<ResolvedPackage>,
}

pub struct Resolver<'a> {
    root: &'a Manifest,
    store: &'a Store,
    os: Os,
    arch: Arch,
    allow_insecure: bool,
    /// (source_spec, package) → available versions, cached for the run.
    listings: BTreeMap<(String, String), Vec<Version>>,
}

impl<'a> Resolver<'a> {
    pub fn new(root: &'a Manifest, store: &'a Store, arch: Arch, allow_insecure: bool) -> Self {
        Resolver {
            root,
            store,
            os: Os::Linux,
            arch,
            allow_insecure,
            listings: BTreeMap::new(),
        }
    }

    pub fn resolve(&mut self) -> Result<Resolution> {
        // name → fetched package (keyed by exact version)
        let mut fetched: BTreeMap<(String, Version), ResolvedPackage> = BTreeMap::new();
        // name → currently selected version
        let mut selected: BTreeMap<String, Version> = BTreeMap::new();

        // Fixpoint: constraints come from the root manifest plus the
        // manifests of currently-selected packages. Graphs are tiny.
        loop {
            let mut constraints: BTreeMap<String, Vec<(VersionReq, String)>> = BTreeMap::new();
            let mut sources: BTreeMap<String, String> = BTreeMap::new();

            let add_deps = |manifest: &Manifest,
                            owner: &str,
                            constraints: &mut BTreeMap<String, Vec<(VersionReq, String)>>,
                            sources: &mut BTreeMap<String, String>|
             -> Result<()> {
                for (alias, dep) in &manifest.dependencies {
                    let spec = dep.spec(alias);
                    let req = VersionReq::parse(&spec.constraint).map_err(|e| {
                        Error::Resolve(format!(
                            "{owner}: invalid version constraint `{}` for `{}`: {e}",
                            spec.constraint, spec.package
                        ))
                    })?;
                    constraints
                        .entry(spec.package.clone())
                        .or_default()
                        .push((req, owner.to_string()));
                    if let Some(spec_source) = self.source_spec_for(manifest, &spec) {
                        sources.entry(spec.package.clone()).or_insert(spec_source);
                    }
                }
                Ok(())
            };

            add_deps(
                self.root,
                &self.root.package.name,
                &mut constraints,
                &mut sources,
            )?;
            for version in selected.values().zip(selected.keys()) {
                let (v, name) = version;
                if let Some(pkg) = fetched.get(&(name.clone(), v.clone())) {
                    let manifest = pkg.manifest.clone();
                    add_deps(&manifest, name, &mut constraints, &mut sources)?;
                }
            }

            // Pick minimal satisfying version per package.
            let mut next: BTreeMap<String, Version> = BTreeMap::new();
            for (package, reqs) in &constraints {
                let source_spec = sources.get(package).cloned().ok_or_else(|| {
                    Error::Resolve(format!(
                        "no source for package `{package}` — set `[sources] default = \"…\"` in ply.toml or give the dependency an explicit `source`"
                    ))
                })?;
                let version = self.pick_min(package, &source_spec, reqs)?;
                next.insert(package.clone(), version);
            }

            // Fetch anything newly selected so its deps join the next round.
            for (name, version) in &next {
                let key = (name.clone(), version.clone());
                if let std::collections::btree_map::Entry::Vacant(entry) = fetched.entry(key) {
                    let source_spec = sources[name].clone();
                    entry.insert(self.fetch_package(name, version, &source_spec)?);
                }
            }

            if next == selected {
                break;
            }
            selected = next;
        }

        let mut packages: Vec<ResolvedPackage> = selected
            .iter()
            .map(|(name, version)| {
                fetched
                    .remove(&(name.clone(), version.clone()))
                    .expect("selected implies fetched")
            })
            .collect();

        self.check_graph(&packages)?;
        order_for_overlay(&mut packages, self.root)?;
        Ok(Resolution { packages })
    }

    /// Explicit dep source, else the requesting manifest's default, else the
    /// root manifest's default.
    fn source_spec_for(&self, requester: &Manifest, spec: &DepSpec) -> Option<String> {
        let alias_or_literal = spec
            .source
            .clone()
            .or_else(|| requester.sources.get("default").cloned())
            .or_else(|| self.root.sources.get("default").cloned())?;
        // A source value may be an alias defined in [sources].
        let resolved = requester
            .sources
            .get(&alias_or_literal)
            .or_else(|| self.root.sources.get(&alias_or_literal))
            .cloned()
            .unwrap_or(alias_or_literal);
        Some(resolved)
    }

    fn pick_min(
        &mut self,
        package: &str,
        source_spec: &str,
        reqs: &[(VersionReq, String)],
    ) -> Result<Version> {
        // Exact `x.y.z` constraints need no listing (zero-API hot path).
        let exact: Option<Version> = reqs
            .iter()
            .filter_map(|(req, _)| {
                let [comparator] = req.comparators.as_slice() else {
                    return None;
                };
                match (comparator.minor, comparator.patch) {
                    (Some(minor), Some(patch)) => Some(Version {
                        major: comparator.major,
                        minor,
                        patch,
                        pre: comparator.pre.clone(),
                        build: Default::default(),
                    }),
                    _ => None,
                }
            })
            .max();
        let available = match &exact {
            Some(v) if reqs.iter().all(|(req, _)| req.matches(v)) => vec![v.clone()],
            _ => self.available(package, source_spec)?,
        };

        available
            .iter()
            .find(|v| reqs.iter().all(|(req, _)| req.matches(v)))
            .cloned()
            .ok_or_else(|| {
                let wanted: Vec<String> = reqs
                    .iter()
                    .map(|(req, owner)| format!("{req} (required by {owner})"))
                    .collect();
                Error::Resolve(format!(
                    "no version of `{package}` satisfies {} — available: [{}]",
                    wanted.join(" and "),
                    available
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
    }

    fn available(&mut self, package: &str, source_spec: &str) -> Result<Vec<Version>> {
        let key = (source_spec.to_string(), package.to_string());
        if let Some(cached) = self.listings.get(&key) {
            return Ok(cached.clone());
        }
        let source = Source::parse(source_spec, self.allow_insecure)?;
        let versions = source.list_versions(package, self.os, self.arch)?;
        self.listings.insert(key, versions.clone());
        Ok(versions)
    }

    fn fetch_package(
        &mut self,
        name: &str,
        version: &Version,
        source_spec: &str,
    ) -> Result<ResolvedPackage> {
        let source = Source::parse(source_spec, self.allow_insecure)?;
        let image = ImageName::new(name, version.clone(), self.os, self.arch)?;
        let (digest, image_path) = source.fetch(&image, None, self.store)?;
        let manifest = read_manifest(&image_path)?;
        if manifest.package.name != *name || manifest.package.version != *version {
            return Err(Error::Resolve(format!(
                "{image}: embedded manifest says {}-{} — filename is lying, refusing",
                manifest.package.name, manifest.package.version
            )));
        }
        Ok(ResolvedPackage {
            name: name.to_string(),
            version: version.clone(),
            source_spec: source_spec.to_string(),
            digest,
            image_path,
            manifest,
        })
    }

    fn check_graph(&self, packages: &[ResolvedPackage]) -> Result<()> {
        let bases: Vec<&str> = packages
            .iter()
            .filter(|p| p.manifest.package.base)
            .map(|p| p.name.as_str())
            .collect();
        if !packages.is_empty() && bases.is_empty() {
            return Err(Error::Resolve(
                "no base package in the graph — exactly one dependency must own `/` (e.g. `base = \"alpine@3.20\"`)"
                    .into(),
            ));
        }
        if bases.len() > 1 {
            return Err(Error::Resolve(format!(
                "multiple base packages in the graph: {} — exactly one may own `/`",
                bases.join(", ")
            )));
        }
        if let Some(requires) = &self.root.requires {
            for pkg in packages {
                if let Some(provides) = &pkg.manifest.package.provides_abi {
                    if provides != &requires.abi {
                        return Err(Error::Resolve(format!(
                            "ABI mismatch: app requires `{}` but {}-{} provides `{provides}`",
                            requires.abi, pkg.name, pkg.version
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

/// Topological sort: dependents before dependencies, base last, ties by
/// digest. This is the physical lowerdir stacking derived from the logical
/// dependency set.
fn order_for_overlay(packages: &mut Vec<ResolvedPackage>, root: &Manifest) -> Result<()> {
    let names: BTreeSet<String> = packages.iter().map(|p| p.name.clone()).collect();
    let dep_names = |manifest: &Manifest| -> BTreeSet<String> {
        manifest
            .dependencies
            .iter()
            .map(|(alias, dep)| dep.spec(alias).package)
            .filter(|n| names.contains(n))
            .collect()
    };

    let _ = root;
    let mut remaining: Vec<ResolvedPackage> = std::mem::take(packages);
    let mut placed: Vec<ResolvedPackage> = Vec::new();

    // Kahn's algorithm from the top: place packages no *unplaced* package
    // depends on (dependents first, base naturally last).
    while !remaining.is_empty() {
        let mut ready: Vec<usize> = (0..remaining.len())
            .filter(|&i| {
                let name = &remaining[i].name;
                !remaining
                    .iter()
                    .enumerate()
                    .any(|(j, other)| j != i && dep_names(&other.manifest).contains(name))
            })
            .collect();
        if ready.is_empty() {
            return Err(Error::Resolve(
                "dependency cycle detected between packages".into(),
            ));
        }
        // Deterministic ties: by digest.
        ready.sort_by(|&a, &b| remaining[a].digest.cmp(&remaining[b].digest));
        let idx = ready[0];
        let pkg = remaining.remove(idx);
        placed.push(pkg);
    }
    *packages = placed;
    Ok(())
}
