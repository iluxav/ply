//! deb2pkg — repackage Debian's prebuilt artifacts as ply packages (glibc lane).
//!
//! A .deb is an `ar` archive whose `data.tar.{xz,zst,gz}` holds the files.
//! Unlike Alpine, Debian's index has no surgical `so:` depends — walking
//! `Depends` drags in perl-sized subtrees. So this tool resolves the runtime
//! closure from the binaries themselves: unpack the requested package (plus
//! same-source siblings its Depends names — Debian splits binaries across
//! them), read every ELF's DT_NEEDED, map each soname to its owning package
//! via the Contents index, and recurse. Packages the debian base already
//! provides (read from the base rootfs manifest) are skipped. Maintainer
//! scripts live in control.tar, which is never read: no hooks, ever, by
//! construction. Output is one keg (`/opt/<name>-<version>/…`) with a
//! ply.toml declaring `provides_abi = "linux-<arch>-gnu"`, built into a
//! deterministic image via ply-core's builder.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

const MIRROR: &str = "https://deb.debian.org/debian";
const ARTIFACTS: &str = "https://raw.githubusercontent.com/debuerreotype/docker-debian-artifacts";
/// Sonames the Contents index may miss but glibc always provides.
const GLIBC_FALLBACK: &[&str] = &["libc.so.6", "ld-linux-x86-64.so.2", "ld-linux-aarch64.so.1"];

/// Convert a Debian package (+ runtime ELF closure) into a ply package image.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Debian binary package name, e.g. redis-server, postgresql-17, nodejs
    package: String,

    /// Debian suite
    #[arg(long, default_value = "trixie")]
    suite: String,

    /// Target architecture: x64 or arm64 (default: the host's).
    /// Conversion only unpacks files, so any host converts for any arch.
    #[arg(long)]
    arch: Option<String>,

    /// Override the ply package name (default: sanitized deb name)
    #[arg(long)]
    name: Option<String>,

    /// Output directory for the image
    #[arg(short, long, default_value = ".")]
    outdir: PathBuf,

    /// Debian base version constraint the package declares
    #[arg(long, default_value = "13")]
    base: String,

    /// ELF files (by basename) to drop from the keg and exclude from the
    /// dependency walk — for dlopen'd plugins, e.g. postgres' llvmjit.so
    /// which alone drags in 150 MiB of LLVM.
    #[arg(long = "skip-so")]
    skip_so: Vec<String>,

    /// Extra packages to vendor that the ELF walk can't see — non-ELF
    /// runtime data like Debian node's externalized builtins
    /// (node-cjs-module-lexer, node-undici, …). Named packages only,
    /// no dependency recursion: list each one explicitly.
    #[arg(long = "with")]
    with: Vec<String>,

    /// Create usr/bin symlinks the .debs don't ship because a maintainer
    /// script would have made them (ply runs no hooks, ever). Format
    /// LINK=TARGET, e.g. --symlink python3=python3.13.
    #[arg(long = "symlink")]
    symlinks: Vec<String>,

    /// Append a TOML fragment to the generated manifest — how to RUN what
    /// the deb ships (`[package] entrypoint`, `[ports]`, `[volumes]`,
    /// `[health]`). A keg with an entrypoint is simply a package that is
    /// also runnable: one `ply/redis` instead of a keg plus a wrapper of the
    /// same name, which two namespaces were papering over.
    #[arg(long = "manifest-extra", value_name = "FILE")]
    manifest_extra: Option<PathBuf>,

    /// Re-download cached indexes regardless of age
    #[arg(long)]
    refresh: bool,
}

#[derive(Debug, Clone, Default)]
struct DebEntry {
    name: String,
    version: String,
    depends: String, // raw Depends + Pre-Depends, comma-joined
    provides: String,
    source: String,
    filename: String,
    installed_size_kib: u64,
}

fn main() -> Result<()> {
    ply_core::restore_default_sigpipe();
    let args = Args::parse();
    let (ply_arch, deb_arch, git_arch) = resolve_arch(args.arch.as_deref())?;

    // --- indexes: fetched once, cached for a day -----------------------------
    let cache = cache_dir(&args.suite, deb_arch)?;
    let packages_xz = fetch_cached(
        &cache.join("Packages.xz"),
        &format!(
            "{MIRROR}/dists/{}/main/binary-{deb_arch}/Packages.xz",
            args.suite
        ),
        args.refresh,
    )?;
    let contents_gz = fetch_cached(
        &cache.join("Contents.gz"),
        &format!("{MIRROR}/dists/{}/main/Contents-{deb_arch}.gz", args.suite),
        args.refresh,
    )?;
    let base_manifest = fetch_cached(
        &cache.join("base.manifest"),
        &format!(
            "{ARTIFACTS}/dist-{git_arch}/{}/slim/rootfs.manifest",
            args.suite
        ),
        args.refresh,
    )?;

    eprintln!("deb2pkg: parsing indexes");
    let index = parse_packages(&decompress_xz_file(&packages_xz)?)?;
    let provides = build_provides(&index);
    let soname_owners = parse_contents(&contents_gz)?;
    let base_set = parse_base_manifest(&base_manifest)?;

    let root = lookup(&index, &provides, &args.package)
        .ok_or_else(|| {
            let close: Vec<&str> = index
                .keys()
                .filter(|k| k.starts_with(&args.package))
                .take(5)
                .map(String::as_str)
                .collect();
            anyhow::anyhow!(
                "`{}` not found in Debian {} main{}",
                args.package,
                args.suite,
                if close.is_empty() {
                    String::new()
                } else {
                    format!(" — close matches: {}", close.join(", "))
                }
            )
        })?
        .clone();
    if base_set.contains(&root.name) {
        bail!(
            "`{}` is already provided by the debian base — nothing to convert",
            root.name
        );
    }

    // --- seed set: the package + same-source siblings its Depends names ------
    // (Debian splits one source's binaries across packages: redis-server is a
    // 0.2 MiB shell whose binaries live in redis-tools.)
    let mut seeds = vec![root.name.clone()];
    for group in split_depends(&root.depends) {
        if let Some(entry) = first_alternative(&group, &index, &provides) {
            if entry.source == root.source && entry.name != root.name {
                seeds.push(entry.name.clone());
            }
        }
    }
    seeds.extend(args.with.iter().cloned());

    // --- closure: unpack, walk DT_NEEDED, map sonames to packages, recurse ---
    let staging = tempfile_dir()?;
    let mut vendored: BTreeMap<String, u64> = BTreeMap::new(); // name -> KiB
    let mut queue: VecDeque<String> = seeds.into();
    let mut unresolved: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    while let Some(name) = queue.pop_front() {
        let Some(entry) = lookup(&index, &provides, &name) else {
            eprintln!("deb2pkg: warning: `{name}` not in index — skipped");
            continue;
        };
        if vendored.contains_key(&entry.name) || base_set.contains(&entry.name) {
            continue;
        }
        vendored.insert(entry.name.clone(), entry.installed_size_kib);
        eprintln!("deb2pkg: unpacking {}-{}", entry.name, entry.version);
        let deb = download(&format!("{MIRROR}/{}", entry.filename))?;
        let files = unpack_deb(&deb, &staging, &args.skip_so)
            .with_context(|| format!("unpack {}", entry.name))?;
        for file in &files {
            let Ok(data) = std::fs::read(file) else {
                continue;
            };
            let Some(needed) = dt_needed(&data) else {
                continue;
            };
            let shown = file.strip_prefix(&staging).unwrap_or(file);
            for soname in needed {
                match resolve_soname(&soname, &soname_owners, &base_set, &vendored) {
                    SonameOwner::Vendor(pkg) => queue.push_back(pkg),
                    SonameOwner::Covered => {}
                    SonameOwner::Unknown => {
                        unresolved
                            .entry(soname)
                            .or_default()
                            .insert(shown.display().to_string());
                    }
                }
            }
        }
    }
    let total_kib: u64 = vendored.values().sum();
    eprintln!(
        "deb2pkg: vendored {} deb(s), {:.1} MiB installed: {}",
        vendored.len(),
        total_kib as f64 / 1024.0,
        vendored.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    if !unresolved.is_empty() {
        eprintln!("deb2pkg: warning: unresolved sonames (dlopen'd optionals, or Contents gaps):");
        for (soname, needers) in &unresolved {
            let mut who: Vec<&str> = needers.iter().map(String::as_str).collect();
            who.truncate(3);
            eprintln!("  {soname}  (needed by {})", who.join(", "));
        }
    }

    // Symlinks maintainer scripts would have created (update-alternatives
    // and friends) — ply runs no hooks, so they're declared instead.
    for spec in &args.symlinks {
        let (link, target) = parse_symlink_spec(spec)?;
        let bin = staging.join("usr/bin");
        std::fs::create_dir_all(&bin)?;
        let path = bin.join(link);
        let _ = std::fs::remove_file(&path);
        std::os::unix::fs::symlink(target, &path)
            .with_context(|| format!("symlink usr/bin/{link} -> {target}"))?;
        eprintln!("deb2pkg: symlinked usr/bin/{link} -> {target}");
    }

    // --- emit the keg (same shape as apk2pkg) --------------------------------
    let ply_name = args.name.unwrap_or_else(|| sanitize_name(&root.name));
    let ply_version = semverize(&root.version)?;
    let prefix = format!("/opt/{ply_name}-{ply_version}");
    let mut paths = Vec::new();
    for rel in ["usr/bin", "bin", "usr/sbin", "sbin"] {
        if staging.join(rel).is_dir() {
            paths.push(format!("{prefix}/{rel}"));
        }
    }
    // Every dir containing a shared object goes on ld_library_path — Debian's
    // multiarch dirs (usr/lib/x86_64-linux-gnu) land here naturally, as do
    // private subdirs normally found via absolute RPATHs that keg relocation
    // breaks.
    let mut so_dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in walkdir(&staging) {
        let name = entry
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if name.contains(".so") {
            if let Some(parent) = entry.parent() {
                if let Ok(rel) = parent.strip_prefix(&staging) {
                    so_dirs.insert(rel.to_path_buf());
                }
            }
        }
    }
    let mut lib_paths = Vec::new();
    for rel in ["usr/lib", "lib"] {
        if so_dirs.remove(Path::new(rel)) {
            lib_paths.push(format!("{prefix}/{rel}"));
        }
    }
    for rel in so_dirs {
        lib_paths.push(format!("{prefix}/{}", rel.display()));
    }

    let mut manifest = format!(
        "[package]\nname = \"{ply_name}\"\nversion = \"{ply_version}\"\nprovides_abi = \"linux-{ply_arch}-gnu\"\n\n[layer]\n"
    );
    manifest.push_str(&format!("path = {}\n", toml_array(&paths)));
    if !lib_paths.is_empty() {
        manifest.push_str(&format!("ld_library_path = {}\n", toml_array(&lib_paths)));
    }
    manifest.push_str(&format!("\n[dependencies]\ndebian = \"{}\"\n", args.base));
    // Hand-written runtime metadata. The generated part says what the
    // package CONTAINS; this says how to RUN it. Its `[package]` keys are
    // merged into the generated one — two `[package]` tables would be a
    // duplicate key, and the fragment must read like the manifest it becomes.
    if let Some(extra) = &args.manifest_extra {
        manifest = merge_manifest_extra(&manifest, extra)?;
    }
    std::fs::write(staging.join("ply.toml"), &manifest)?;

    std::fs::create_dir_all(&args.outdir)
        .with_context(|| format!("create outdir {}", args.outdir.display()))?;
    let outcome = ply_core::build::build(&ply_core::build::BuildOptions {
        dir: staging.clone(),
        output: Some(
            args.outdir
                .join(format!("{ply_name}-{ply_version}-linux-{ply_arch}.img")),
        ),
        allow_insecure: false,
        arch: match ply_arch {
            "arm64" => Some(ply_core::image::name::Arch::Arm64),
            _ => Some(ply_core::image::name::Arch::X64),
        },
    })?;
    println!(
        "built {} ({:.1} MiB)",
        outcome.image_path.display(),
        outcome.size_bytes as f64 / (1024.0 * 1024.0)
    );
    println!("{}", outcome.digest);
    println!("\nuse it:\n  [dependencies]\n  {ply_name} = \"{ply_version}\"");
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

// --- index fetch + cache -----------------------------------------------------

fn cache_dir(suite: &str, deb_arch: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home)
        .join(".cache/deb2pkg")
        .join(format!("{suite}-{deb_arch}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Download `url` to `path` unless a fresh (<24h) copy exists. Returns `path`.
fn fetch_cached(path: &Path, url: &str, refresh: bool) -> Result<PathBuf> {
    let fresh = !refresh
        && path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() < 24 * 3600);
    if !fresh {
        eprintln!("deb2pkg: fetching {url}");
        let bytes = download(url)?;
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
    }
    Ok(path.to_path_buf())
}

fn download(url: &str) -> Result<Vec<u8>> {
    let response = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {url}"))?;
    Ok(bytes)
}

fn decompress_xz_file(path: &Path) -> Result<String> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut out = Vec::new();
    lzma_rs::xz_decompress(&mut reader, &mut out)
        .with_context(|| format!("xz-decompress {}", path.display()))?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// --- Packages index ----------------------------------------------------------

/// RFC822 stanzas → entries; multiple stanzas per name keep the highest
/// version (dpkg ordering). Continuation lines (leading whitespace) fold
/// into the previous field.
fn parse_packages(text: &str) -> Result<BTreeMap<String, DebEntry>> {
    let mut index: BTreeMap<String, DebEntry> = BTreeMap::new();
    let mut cur = DebEntry::default();
    let mut depends = String::new();
    let mut pre_depends = String::new();
    let mut last: Option<&'static str> = None;
    let flush = |cur: &mut DebEntry,
                 dep: &mut String,
                 pre: &mut String,
                 index: &mut BTreeMap<String, DebEntry>| {
        if cur.name.is_empty() {
            return;
        }
        let mut entry = std::mem::take(cur);
        entry.depends = match (dep.is_empty(), pre.is_empty()) {
            (true, _) => std::mem::take(pre),
            (_, true) => std::mem::take(dep),
            _ => format!("{}, {}", std::mem::take(dep), std::mem::take(pre)),
        };
        if entry.source.is_empty() {
            entry.source = entry.name.clone();
        }
        match index.get(&entry.name) {
            Some(existing) if deb_version_cmp(&existing.version, &entry.version).is_ge() => {}
            _ => {
                index.insert(entry.name.clone(), entry);
            }
        }
    };
    for line in text.lines() {
        if line.is_empty() {
            flush(&mut cur, &mut depends, &mut pre_depends, &mut index);
            last = None;
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            match last {
                Some("Depends") => depends.push_str(line),
                Some("Pre-Depends") => pre_depends.push_str(line),
                Some("Provides") => cur.provides.push_str(line),
                _ => {}
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        last = None;
        match key {
            "Package" => cur.name = value.to_string(),
            "Version" => cur.version = value.to_string(),
            "Depends" => {
                depends = value.to_string();
                last = Some("Depends");
            }
            "Pre-Depends" => {
                pre_depends = value.to_string();
                last = Some("Pre-Depends");
            }
            "Provides" => {
                cur.provides = value.to_string();
                last = Some("Provides");
            }
            "Source" => {
                // "Source: redis (5:8.0.2-1)" → "redis"
                cur.source = value.split_whitespace().next().unwrap_or(value).to_string();
            }
            "Filename" => cur.filename = value.to_string(),
            "Installed-Size" => cur.installed_size_kib = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    flush(&mut cur, &mut depends, &mut pre_depends, &mut index);
    Ok(index)
}

/// virtual name → providers (for Depends targets that aren't real packages)
fn build_provides(index: &BTreeMap<String, DebEntry>) -> BTreeMap<String, Vec<String>> {
    let mut provides: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for entry in index.values() {
        for item in entry.provides.split(',') {
            let name = item.split_whitespace().next().unwrap_or("");
            if !name.is_empty() {
                provides
                    .entry(name.to_string())
                    .or_default()
                    .push(entry.name.clone());
            }
        }
    }
    for list in provides.values_mut() {
        list.sort();
    }
    provides
}

fn lookup<'a>(
    index: &'a BTreeMap<String, DebEntry>,
    provides: &BTreeMap<String, Vec<String>>,
    name: &str,
) -> Option<&'a DebEntry> {
    index.get(name).or_else(|| {
        let providers = provides.get(name)?;
        let best = providers
            .iter()
            .min_by_key(|p| (package_score(p), p.len(), (*p).clone()))?;
        index.get(best)
    })
}

/// "a (>= 1), b | c, d:any" → [["a"], ["b", "c"], ["d"]]
fn split_depends(depends: &str) -> Vec<Vec<String>> {
    depends
        .split(',')
        .filter_map(|group| {
            let alts: Vec<String> = group
                .split('|')
                .filter_map(|alt| {
                    let name = alt.split_whitespace().next()?;
                    let name = name.split(':').next().unwrap_or(name); // strip :any
                    (!name.is_empty()).then(|| name.to_string())
                })
                .collect();
            (!alts.is_empty()).then_some(alts)
        })
        .collect()
}

fn first_alternative<'a>(
    group: &[String],
    index: &'a BTreeMap<String, DebEntry>,
    provides: &BTreeMap<String, Vec<String>>,
) -> Option<&'a DebEntry> {
    group.iter().find_map(|alt| lookup(index, provides, alt))
}

// --- Contents index (soname → owning package) --------------------------------

/// Stream the Contents index, keeping only shared-object basenames.
/// Lines are "path<spaces>section/pkg[,section/pkg…]"; paths have no leading /.
fn parse_contents(path: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let file = std::fs::File::open(path)?;
    let gz = flate2::read::MultiGzDecoder::new(file);
    let mut reader = std::io::BufReader::new(gz);
    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        let Some((file_path, pkgs)) = trimmed.rsplit_once(char::is_whitespace) else {
            continue;
        };
        let file_path = file_path.trim_end();
        // multilib trees (usr/lib32, usr/libx32) hold foreign-width libraries
        // that can't be loaded by this arch's binaries — never map from them
        if file_path.split('/').any(|d| d == "lib32" || d == "libx32") {
            continue;
        }
        let basename = file_path.rsplit('/').next().unwrap_or("");
        if !is_soname(basename) {
            continue;
        }
        for cell in pkgs.split(',') {
            let pkg = cell.rsplit('/').next().unwrap_or("").trim();
            if !pkg.is_empty() {
                owners
                    .entry(basename.to_string())
                    .or_default()
                    .insert(pkg.to_string());
            }
        }
    }
    Ok(owners
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().collect()))
        .collect())
}

/// "libfoo.so", "libfoo.so.1", "libfoo.so.1.2.3" — but not "libfoo.software"
fn is_soname(name: &str) -> bool {
    name.match_indices(".so").any(|(idx, _)| {
        let rest = &name[idx + 3..];
        rest.is_empty()
            || (rest.starts_with('.')
                && rest[1..]
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
    })
}

enum SonameOwner {
    /// vendor this package into the keg
    Vendor(String),
    /// the base or an already-vendored package provides it
    Covered,
    /// nobody in Contents claims it
    Unknown,
}

fn resolve_soname(
    soname: &str,
    owners: &BTreeMap<String, Vec<String>>,
    base_set: &BTreeSet<String>,
    vendored: &BTreeMap<String, u64>,
) -> SonameOwner {
    let Some(candidates) = owners.get(soname) else {
        return if GLIBC_FALLBACK.contains(&soname) {
            SonameOwner::Covered
        } else {
            SonameOwner::Unknown
        };
    };
    if candidates.iter().any(|c| base_set.contains(c)) {
        return SonameOwner::Covered;
    }
    if candidates.iter().any(|c| vendored.contains_key(c)) {
        return SonameOwner::Covered;
    }
    let best = candidates
        .iter()
        .min_by_key(|c| (package_score(c), c.len(), (*c).clone()));
    match best {
        Some(pkg) => SonameOwner::Vendor(pkg.clone()),
        None => SonameOwner::Unknown,
    }
}

/// Lower is better: runtime lib packages beat -dev/-dbg and non-lib names.
/// (The naive pick hands libLLVM.so to llvm-19-dev, 339 MiB, instead of
/// libllvm19, 124 MiB.)
fn package_score(name: &str) -> u32 {
    let mut score = 0;
    if name.ends_with("-dev") || name.ends_with("-dbg") || name.contains("-dbgsym") {
        score += 100;
    }
    // multilib variants (lib32readline8) ship foreign-width libraries
    if name.starts_with("lib32") || name.starts_with("lib64") || name.starts_with("libx32") {
        score += 100;
    }
    if !name.starts_with("lib") {
        score += 10;
    }
    score
}

// --- base manifest -----------------------------------------------------------

/// debuerreotype rootfs.manifest: "pkg<TAB>version" per line.
fn parse_base_manifest(path: &Path) -> Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(path)?;
    let set: BTreeSet<String> = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(|s| s.split(':').next().unwrap_or(s).to_string()) // strip :arch
        .collect();
    if set.is_empty() {
        bail!("base manifest {} is empty", path.display());
    }
    Ok(set)
}

// --- .deb unpack -------------------------------------------------------------

/// Unpack a .deb's data.tar into `dest`, returning staged regular files.
/// control.tar (maintainer scripts) is deliberately never read.
fn unpack_deb(deb: &[u8], dest: &Path, skip_so: &[String]) -> Result<Vec<PathBuf>> {
    for (name, data) in ar_members(deb)? {
        let Some(compression) = name.strip_prefix("data.tar") else {
            continue;
        };
        let tar_bytes: Vec<u8> = match compression {
            ".xz" => {
                let mut reader = std::io::BufReader::new(data);
                let mut out = Vec::new();
                lzma_rs::xz_decompress(&mut reader, &mut out).context("xz data.tar")?;
                out
            }
            ".zst" => zstd::stream::decode_all(data).context("zstd data.tar")?,
            ".gz" => {
                let mut out = Vec::new();
                flate2::read::MultiGzDecoder::new(data)
                    .read_to_end(&mut out)
                    .context("gz data.tar")?;
                out
            }
            "" => data.to_vec(),
            other => bail!("unsupported data.tar compression `{other}`"),
        };
        return unpack_tar(&tar_bytes, dest, skip_so);
    }
    bail!("no data.tar member in .deb")
}

fn unpack_tar(tar_bytes: &[u8], dest: &Path, skip_so: &[String]) -> Result<Vec<PathBuf>> {
    let mut archive = tar::Archive::new(tar_bytes);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);
    let mut files = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let basename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if skip_so.iter().any(|s| s == &basename) {
            continue; // dlopen'd plugin: not staged, not walked
        }
        match entry.header().entry_type() {
            tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo => continue,
            _ => {}
        }
        let target = dest.join(&path);
        if target.exists() && !target.is_dir() {
            let _ = std::fs::remove_file(&target);
        }
        entry
            .unpack_in(dest)
            .with_context(|| format!("unpack {}", path.display()))?;
        // Same widening as apk2pkg: keep owner rwx on dirs / read on files so
        // the keg build can read everything — root in the container bypasses
        // modes anyway.
        let widen = match entry.header().entry_type() {
            tar::EntryType::Directory => 0o700,
            tar::EntryType::Regular => 0o400,
            _ => 0,
        };
        if widen != 0 {
            if let Ok(mode) = entry.header().mode() {
                if mode & widen != widen {
                    use std::os::unix::fs::PermissionsExt;
                    let widened = std::fs::Permissions::from_mode(mode | widen);
                    let _ = std::fs::set_permissions(dest.join(&path), widened);
                }
            }
        }
        if entry.header().entry_type() == tar::EntryType::Regular {
            files.push(dest.join(&path));
        }
    }
    Ok(files)
}

/// Minimal `ar` reader: 8-byte magic, then 60-byte headers + data, 2-aligned.
fn ar_members(data: &[u8]) -> Result<Vec<(String, &[u8])>> {
    let Some(rest) = data.strip_prefix(b"!<arch>\n") else {
        bail!("not an ar archive (.deb expected)");
    };
    let mut members = Vec::new();
    let mut pos = 0usize;
    while pos + 60 <= rest.len() {
        let header = &rest[pos..pos + 60];
        let name = String::from_utf8_lossy(&header[0..16])
            .trim_end()
            .trim_end_matches('/')
            .to_string();
        let size: usize = String::from_utf8_lossy(&header[48..58])
            .trim()
            .parse()
            .with_context(|| format!("bad ar member size for `{name}`"))?;
        pos += 60;
        if pos + size > rest.len() {
            bail!("truncated ar member `{name}`");
        }
        members.push((name, &rest[pos..pos + size]));
        pos += size + (size & 1);
    }
    Ok(members)
}

// --- ELF DT_NEEDED -----------------------------------------------------------

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;

/// DT_NEEDED sonames of a little-endian ELF64 file; None for anything else.
fn dt_needed(data: &[u8]) -> Option<Vec<String>> {
    if data.len() < 64 || &data[0..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return None;
    }
    let phoff = u64le(data, 0x20)? as usize;
    let phentsize = u16le(data, 0x36)? as usize;
    let phnum = u16le(data, 0x38)? as usize;
    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); // (vaddr, offset, filesz)
    let mut dynamic: Option<(usize, usize)> = None;
    for k in 0..phnum {
        let ph = phoff.checked_add(k.checked_mul(phentsize)?)?;
        let p_type = u32le(data, ph)?;
        let p_offset = u64le(data, ph + 0x08)?;
        let p_vaddr = u64le(data, ph + 0x10)?;
        let p_filesz = u64le(data, ph + 0x20)?;
        match p_type {
            PT_LOAD => loads.push((p_vaddr, p_offset, p_filesz)),
            PT_DYNAMIC => dynamic = Some((p_offset as usize, p_filesz as usize)),
            _ => {}
        }
    }
    let (dyn_off, dyn_size) = dynamic?;
    let mut needed_offsets = Vec::new();
    let mut strtab_vaddr = None;
    let mut p = dyn_off;
    while p + 16 <= dyn_off.checked_add(dyn_size)? && p + 16 <= data.len() {
        let tag = u64le(data, p)? as i64;
        let value = u64le(data, p + 8)?;
        match tag {
            DT_NULL => break,
            DT_NEEDED => needed_offsets.push(value),
            DT_STRTAB => strtab_vaddr = Some(value),
            _ => {}
        }
        p += 16;
    }
    let strtab_vaddr = strtab_vaddr?;
    let strtab_off = loads
        .iter()
        .find(|(vaddr, _, filesz)| strtab_vaddr >= *vaddr && strtab_vaddr < vaddr + filesz)
        .map(|(vaddr, offset, _)| (strtab_vaddr - vaddr + offset) as usize)?;
    Some(
        needed_offsets
            .iter()
            .filter_map(|off| cstr_at(data, strtab_off.checked_add(*off as usize)?))
            .collect(),
    )
}

fn u16le(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}
fn u32le(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}
fn u64le(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}
fn cstr_at(data: &[u8], at: usize) -> Option<String> {
    let slice = data.get(at..)?;
    let end = slice.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&slice[..end]).into_owned())
}

// --- dpkg version comparison -------------------------------------------------

/// dpkg's algorithm: epoch, then upstream, then revision; within each,
/// alternating non-digit/digit spans where `~` sorts before everything.
fn deb_version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (epoch_a, rest_a) = split_epoch(a);
    let (epoch_b, rest_b) = split_epoch(b);
    epoch_a.cmp(&epoch_b).then_with(|| {
        let (up_a, rev_a) = split_revision(rest_a);
        let (up_b, rev_b) = split_revision(rest_b);
        verrevcmp(up_a, up_b).then_with(|| verrevcmp(rev_a, rev_b))
    })
}

fn split_epoch(v: &str) -> (u64, &str) {
    match v.split_once(':') {
        Some((epoch, rest)) => (epoch.parse().unwrap_or(0), rest),
        None => (0, v),
    }
}

fn split_revision(v: &str) -> (&str, &str) {
    match v.rsplit_once('-') {
        Some((upstream, revision)) => (upstream, revision),
        None => (v, ""),
    }
}

/// `~` < end-of-string < letters < everything else; digit runs numeric.
fn char_order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_alphabetic() => c as i32,
        Some(c) => c as i32 + 256,
    }
}

fn verrevcmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    let digit = |s: &[u8], k: usize| k < s.len() && s[k].is_ascii_digit();
    while i < a.len() || j < b.len() {
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let oa = char_order(if digit(a, i) { None } else { a.get(i).copied() });
            let ob = char_order(if digit(b, j) { None } else { b.get(j).copied() });
            match oa.cmp(&ob) {
                Ordering::Equal => {}
                other => return other,
            }
            i += 1;
            j += 1;
        }
        while digit(a, i) && a[i] == b'0' {
            i += 1;
        }
        while digit(b, j) && b[j] == b'0' {
            j += 1;
        }
        let mut first_diff = Ordering::Equal;
        while digit(a, i) && digit(b, j) {
            if first_diff == Ordering::Equal {
                first_diff = a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
        if digit(a, i) {
            return Ordering::Greater;
        }
        if digit(b, j) {
            return Ordering::Less;
        }
        if first_diff != Ordering::Equal {
            return first_diff;
        }
    }
    std::cmp::Ordering::Equal
}

// --- names + versions --------------------------------------------------------

/// "python3=python3.13" → ("python3", "python3.13"); the link is a basename.
fn parse_symlink_spec(spec: &str) -> Result<(&str, &str)> {
    match spec.split_once('=') {
        Some((link, target)) if !link.is_empty() && !target.is_empty() && !link.contains('/') => {
            Ok((link, target))
        }
        _ => bail!("--symlink `{spec}`: expected LINK=TARGET (link is a basename in usr/bin)"),
    }
}

/// ply names: lowercase, no `+`, no `_`, no `-<digit>`.
/// "postgresql-17" → postgresql17, "libstdc++6" → libstdcpp6
fn sanitize_name(deb_name: &str) -> String {
    let flat = deb_name
        .to_lowercase()
        .replace("++", "pp")
        .replace('+', "p")
        .replace('_', "-");
    let mut out = String::with_capacity(flat.len());
    let mut chars = flat.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek().is_some_and(|n| n.is_ascii_digit()) {
            continue;
        }
        out.push(c);
    }
    out
}

/// "5:8.0.2-2" → 8.0.2; "1:2.41.5-0+deb13u1" → 2.41.5; "20230311" → 20230311.0.0
fn semverize(deb_version: &str) -> Result<semver::Version> {
    let no_epoch = deb_version
        .split_once(':')
        .map(|(_, rest)| rest)
        .unwrap_or(deb_version);
    let core: String = no_epoch
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let parts: Vec<&str> = core.trim_matches('.').split('.').collect();
    let get = |i: usize| -> u64 { parts.get(i).and_then(|p| p.parse().ok()).unwrap_or(0) };
    if parts.is_empty() || parts[0].is_empty() {
        bail!("cannot derive a semver from Debian version `{deb_version}`");
    }
    Ok(semver::Version::new(get(0), get(1), get(2)))
}

fn toml_array(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

/// `--arch` flag → (ply arch, debian arch, artifacts branch arch).
fn resolve_arch(flag: Option<&str>) -> Result<(&'static str, &'static str, &'static str)> {
    let host = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x64",
    };
    match flag.unwrap_or(host) {
        "x64" => Ok(("x64", "amd64", "amd64")),
        "arm64" => Ok(("arm64", "arm64", "arm64v8")),
        other => bail!("--arch `{other}`: supported values are x64, arm64"),
    }
}

/// All file paths under `root`, recursively.
fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() && !path.is_symlink() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
    }
    out
}

fn tempfile_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("deb2pkg-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Fold a hand-written fragment into a generated keg manifest: `[package]`
/// keys join the generated `[package]`, every other table is appended whole.
/// Returns the manifest text, re-rendered.
fn merge_manifest_extra(manifest: &str, extra: &Path) -> Result<String> {
    let text =
        std::fs::read_to_string(extra).with_context(|| format!("reading {}", extra.display()))?;
    let fragment: toml::Table = text
        .parse()
        .with_context(|| format!("{}: not valid TOML", extra.display()))?;
    let mut doc: toml::Table = manifest
        .parse()
        .context("generated manifest is not valid TOML (deb2pkg bug)")?;

    for (key, value) in fragment {
        match (doc.get_mut(&key), value) {
            // merge table-into-table, so `[package] entrypoint` lands beside
            // the generated name/version instead of redeclaring the table
            (Some(toml::Value::Table(into)), toml::Value::Table(from)) => {
                for (k, v) in from {
                    into.insert(k, v);
                }
            }
            (_, value) => {
                doc.insert(key, value);
            }
        }
    }
    toml::to_string_pretty(&doc).context("re-rendering the merged manifest")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    // -- versions --

    #[test]
    fn semverize_strips_epoch_and_revision() {
        assert_eq!(
            semverize("5:8.0.2-2").unwrap(),
            semver::Version::new(8, 0, 2)
        );
        assert_eq!(
            semverize("1:2.41.5-0+deb13u1").unwrap(),
            semver::Version::new(2, 41, 5)
        );
        assert_eq!(
            semverize("17.10-1").unwrap(),
            semver::Version::new(17, 10, 0)
        );
        assert_eq!(
            semverize("20230311").unwrap(),
            semver::Version::new(20230311, 0, 0)
        );
    }

    #[test]
    fn semverize_rejects_versionless() {
        assert!(semverize("beta").is_err());
    }

    #[test]
    fn deb_version_ordering() {
        assert_eq!(deb_version_cmp("8.0.2-2", "8.0.2-1"), Ordering::Greater);
        assert_eq!(deb_version_cmp("1:1.0", "3.0"), Ordering::Greater); // epoch wins
        assert_eq!(deb_version_cmp("1.0~rc1", "1.0"), Ordering::Less); // ~ sorts first
        assert_eq!(deb_version_cmp("9.20", "9.9"), Ordering::Greater); // numeric, not lexical
        assert_eq!(
            deb_version_cmp("2.41.5-0+deb13u1", "2.41.5-0+deb13u1"),
            Ordering::Equal
        );
        assert_eq!(deb_version_cmp("1.0-1", "1.0"), Ordering::Greater);
    }

    // -- names --

    #[test]
    fn sanitize_folds_digit_suffix() {
        assert_eq!(sanitize_name("postgresql-17"), "postgresql17");
        assert_eq!(sanitize_name("redis-server"), "redis-server");
        assert_eq!(sanitize_name("libstdc++6"), "libstdcpp6");
        assert_eq!(sanitize_name("gcc_tools"), "gcc-tools");
    }

    #[test]
    fn symlink_spec_parsing() {
        assert_eq!(
            parse_symlink_spec("python3=python3.13").unwrap(),
            ("python3", "python3.13")
        );
        assert!(parse_symlink_spec("no-equals").is_err());
        assert!(parse_symlink_spec("=target").is_err());
        assert!(parse_symlink_spec("a/b=c").is_err());
    }

    // -- Packages parsing --

    const SAMPLE_INDEX: &str = "\
Package: redis-server
Version: 5:8.0.2-2
Installed-Size: 243
Source: redis
Depends: redis-tools (= 5:8.0.2-2),
 adduser
Filename: pool/main/r/redis/redis-server_8.0.2-2_amd64.deb

Package: redis-tools
Version: 5:8.0.2-2
Installed-Size: 6963
Source: redis
Depends: libc6 (>= 2.38), libssl3t64
Filename: pool/main/r/redis/redis-tools_8.0.2-2_amd64.deb

Package: old-thing
Version: 1.0-1
Filename: pool/main/o/old-thing/old-thing_1.0-1_amd64.deb

Package: old-thing
Version: 1.2-1
Filename: pool/main/o/old-thing/old-thing_1.2-1_amd64.deb

Package: mta-provider
Version: 2.0-1
Provides: mail-transport-agent, other-virtual (= 4)
Filename: pool/main/m/mta/mta_2.0-1_amd64.deb
";

    #[test]
    fn packages_parser_reads_stanzas_and_folds_continuations() {
        let index = parse_packages(SAMPLE_INDEX).unwrap();
        let redis = &index["redis-server"];
        assert_eq!(redis.version, "5:8.0.2-2");
        assert_eq!(redis.source, "redis");
        assert_eq!(redis.installed_size_kib, 243);
        // continuation line folded into Depends
        let groups = split_depends(&redis.depends);
        assert_eq!(
            groups,
            vec![vec!["redis-tools".to_string()], vec!["adduser".to_string()]]
        );
        // Source defaults to the package name when absent
        assert_eq!(index["old-thing"].source, "old-thing");
    }

    #[test]
    fn packages_parser_keeps_highest_version() {
        let index = parse_packages(SAMPLE_INDEX).unwrap();
        assert_eq!(index["old-thing"].version, "1.2-1");
    }

    #[test]
    fn provides_resolve_virtuals() {
        let index = parse_packages(SAMPLE_INDEX).unwrap();
        let provides = build_provides(&index);
        let entry = lookup(&index, &provides, "mail-transport-agent").unwrap();
        assert_eq!(entry.name, "mta-provider");
    }

    #[test]
    fn depends_alternatives_and_arch_qualifiers() {
        let groups = split_depends("a (>= 1) | b, python3:any, c");
        assert_eq!(
            groups,
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["python3".to_string()],
                vec!["c".to_string()]
            ]
        );
    }

    // -- soname matching + scoring --

    #[test]
    fn soname_shapes() {
        assert!(is_soname("libssl.so.3"));
        assert!(is_soname("libfoo.so"));
        assert!(is_soname("libfoo.so.1.2.3"));
        assert!(!is_soname("libfoo.software"));
        assert!(!is_soname("README"));
        assert!(!is_soname("libfoo.so.conf"));
        assert!(is_soname("weird.software.so.1"));
    }

    #[test]
    fn soname_scoring_prefers_runtime_libs() {
        let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
        owners.insert(
            "libLLVM.so.19.1".into(),
            vec!["llvm-19-dev".into(), "libllvm19".into()],
        );
        let base = BTreeSet::new();
        let vendored = BTreeMap::new();
        match resolve_soname("libLLVM.so.19.1", &owners, &base, &vendored) {
            SonameOwner::Vendor(pkg) => assert_eq!(pkg, "libllvm19"),
            _ => panic!("expected a vendor decision"),
        }
        // multilib variant loses even when its name is shorter (t64 names)
        owners.insert(
            "libreadline.so.8".into(),
            vec!["lib32readline8".into(), "libreadline8t64".into()],
        );
        match resolve_soname("libreadline.so.8", &owners, &base, &vendored) {
            SonameOwner::Vendor(pkg) => assert_eq!(pkg, "libreadline8t64"),
            _ => panic!("expected a vendor decision"),
        }
    }

    #[test]
    fn soname_in_base_is_covered_and_glibc_fallback_holds() {
        let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
        owners.insert("libz.so.1".into(), vec!["zlib1g".into()]);
        let base: BTreeSet<String> = ["zlib1g".to_string()].into();
        let vendored = BTreeMap::new();
        assert!(matches!(
            resolve_soname("libz.so.1", &owners, &base, &vendored),
            SonameOwner::Covered
        ));
        // libc.so.6 missing from Contents still resolves as covered
        assert!(matches!(
            resolve_soname("libc.so.6", &owners, &base, &vendored),
            SonameOwner::Covered
        ));
        assert!(matches!(
            resolve_soname("libmystery.so.9", &owners, &base, &vendored),
            SonameOwner::Unknown
        ));
    }

    // -- ar --

    fn ar_fixture() -> Vec<u8> {
        let mut ar = b"!<arch>\n".to_vec();
        let member = |name: &str, data: &[u8], ar: &mut Vec<u8>| {
            ar.extend(format!("{name:<16}").bytes());
            ar.extend(format!("{:<12}", 0).bytes()); // mtime
            ar.extend(format!("{:<6}", 0).bytes()); // uid
            ar.extend(format!("{:<6}", 0).bytes()); // gid
            ar.extend(format!("{:<8}", 100644).bytes()); // mode
            ar.extend(format!("{:<10}", data.len()).bytes());
            ar.extend(b"`\n");
            ar.extend(data);
            if data.len() % 2 == 1 {
                ar.push(b'\n');
            }
        };
        member("debian-binary", b"2.0\n", &mut ar);
        member("control.tar.xz", b"CONTROL", &mut ar);
        member("data.tar.gz", b"DATA!", &mut ar);
        ar
    }

    #[test]
    fn ar_parser_reads_members() {
        let ar = ar_fixture();
        let members = ar_members(&ar).unwrap();
        let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["debian-binary", "control.tar.xz", "data.tar.gz"]
        );
        assert_eq!(members[2].1, b"DATA!");
    }

    #[test]
    fn ar_parser_rejects_non_ar() {
        assert!(ar_members(b"\x7fELF....").is_err());
    }

    // -- ELF --

    /// Hand-assembled minimal ELF64: one PT_LOAD mapping the file at vaddr 0,
    /// one PT_DYNAMIC with DT_NEEDED entries + DT_STRTAB, then the strtab.
    fn mini_elf(needed: &[&str]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut offsets = Vec::new();
        for name in needed {
            offsets.push(strtab.len() as u64);
            strtab.extend(name.bytes());
            strtab.push(0);
        }
        let ehsize = 64usize;
        let phentsize = 56usize;
        let phnum = 2usize;
        let dyn_off = ehsize + phnum * phentsize;
        let dyn_len = (needed.len() + 2) * 16;
        let str_off = dyn_off + dyn_len;
        let total = str_off + strtab.len();

        let mut elf = vec![0u8; total];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2; // ELF64
        elf[5] = 1; // little-endian
        elf[0x20..0x28].copy_from_slice(&(ehsize as u64).to_le_bytes()); // e_phoff
        elf[0x36..0x38].copy_from_slice(&(phentsize as u16).to_le_bytes());
        elf[0x38..0x3a].copy_from_slice(&(phnum as u16).to_le_bytes());

        let phdr =
            |idx: usize, p_type: u32, offset: u64, vaddr: u64, filesz: u64, elf: &mut Vec<u8>| {
                let at = ehsize + idx * phentsize;
                elf[at..at + 4].copy_from_slice(&p_type.to_le_bytes());
                elf[at + 0x08..at + 0x10].copy_from_slice(&offset.to_le_bytes());
                elf[at + 0x10..at + 0x18].copy_from_slice(&vaddr.to_le_bytes());
                elf[at + 0x20..at + 0x28].copy_from_slice(&filesz.to_le_bytes());
            };
        phdr(0, PT_LOAD, 0, 0, total as u64, &mut elf);
        phdr(
            1,
            PT_DYNAMIC,
            dyn_off as u64,
            dyn_off as u64,
            dyn_len as u64,
            &mut elf,
        );

        let mut dyn_at = dyn_off;
        let dyn_entry = |tag: i64, value: u64, elf: &mut Vec<u8>, at: &mut usize| {
            elf[*at..*at + 8].copy_from_slice(&tag.to_le_bytes());
            elf[*at + 8..*at + 16].copy_from_slice(&value.to_le_bytes());
            *at += 16;
        };
        for off in &offsets {
            dyn_entry(DT_NEEDED, *off, &mut elf, &mut dyn_at);
        }
        dyn_entry(DT_STRTAB, str_off as u64, &mut elf, &mut dyn_at);
        dyn_entry(DT_NULL, 0, &mut elf, &mut dyn_at);
        elf[str_off..].copy_from_slice(&strtab);
        elf
    }

    #[test]
    fn dt_needed_reads_sonames() {
        let elf = mini_elf(&["libssl.so.3", "libjemalloc.so.2"]);
        assert_eq!(
            dt_needed(&elf).unwrap(),
            vec!["libssl.so.3".to_string(), "libjemalloc.so.2".to_string()]
        );
    }

    #[test]
    fn dt_needed_ignores_non_elf() {
        assert!(dt_needed(b"#!/bin/sh\necho hi\n").is_none());
        assert!(dt_needed(&[]).is_none());
        // ELF32 (class byte 1) is skipped, not parsed
        let mut elf32 = mini_elf(&["libx.so.1"]);
        elf32[4] = 1;
        assert!(dt_needed(&elf32).is_none());
    }

    // -- Contents --

    #[test]
    fn base_manifest_strips_arch_qualifiers() {
        let dir = tempfile_dir().unwrap();
        let path = dir.join("manifest");
        std::fs::write(&path, "apt\t3.0.3\nlibgcc-s1:amd64\t14.2\nzlib1g\t1:1.3\n").unwrap();
        let set = parse_base_manifest(&path).unwrap();
        assert!(set.contains("apt"));
        assert!(set.contains("libgcc-s1"));
        assert!(set.contains("zlib1g"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- arch --

    #[test]
    fn arch_mapping() {
        assert_eq!(
            resolve_arch(Some("x64")).unwrap(),
            ("x64", "amd64", "amd64")
        );
        assert_eq!(
            resolve_arch(Some("arm64")).unwrap(),
            ("arm64", "arm64", "arm64v8")
        );
        assert!(resolve_arch(Some("riscv64")).is_err());
    }
}
