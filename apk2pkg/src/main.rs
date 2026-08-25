//! apk2pkg — repackage Alpine's prebuilt artifacts as ply packages.
//!
//! An .apk is a tar.gz. This tool resolves the requested package plus its
//! runtime dependency closure from APKINDEX, unpacks everything into one keg
//! (`/opt/<name>-<version>/…`), writes a ply.toml with `[layer]` env
//! contributions, and produces a deterministic image via ply-core's builder.
//! Packages the alpine base already provides (musl, busybox, …) are skipped.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

/// Convert an Alpine package (+ runtime deps) into a ply package image.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Alpine package name, e.g. ffmpeg, jq, imagemagick
    package: String,

    /// Alpine release branch
    #[arg(long, default_value = "3.20")]
    alpine: String,

    /// Output directory for the image
    #[arg(short, long, default_value = ".")]
    outdir: PathBuf,

    /// Override the ply package name (default: sanitized apk name)
    #[arg(long)]
    name: Option<String>,

    /// Target architecture: x64 or arm64 (default: the host's).
    /// Conversion is arch-independent — it only unpacks files — so any
    /// host can convert for any arch.
    #[arg(long)]
    arch: Option<String>,

    /// Alpine base version constraint the package declares (default: the
    /// --alpine branch). Required with `--alpine edge`, which is rolling
    /// and has no version to declare.
    #[arg(long)]
    base: Option<String>,
}

/// Provided by the alpine base package — never vendored into kegs.
const BASE_PROVIDES: &[&str] = &[
    "musl",
    "musl-utils",
    "busybox",
    "busybox-binsh",
    "alpine-baselayout",
    "alpine-baselayout-data",
    "alpine-keys",
    "alpine-release",
    "libc-utils",
];

#[derive(Debug, Clone)]
struct ApkEntry {
    name: String,
    version: String,
    depends: Vec<String>,
    provides: Vec<String>,
    repo_url: String,
}

fn main() -> Result<()> {
    ply_core::restore_default_sigpipe();
    let args = Args::parse();

    let mirror = branch_url(&args.alpine);
    let (ply_arch, alpine_arch) = resolve_arch(args.arch.as_deref())?;
    let base = base_constraint(&args.alpine, args.base.as_deref())?;

    // Load both standard repos' indexes.
    let mut index: BTreeMap<String, ApkEntry> = BTreeMap::new();
    for repo in ["main", "community"] {
        let url = format!("{mirror}/{repo}/{alpine_arch}");
        eprintln!("apk2pkg: reading {url}/APKINDEX.tar.gz");
        match load_index(&url) {
            Ok(entries) => {
                for entry in entries {
                    index.entry(entry.name.clone()).or_insert(entry);
                }
            }
            Err(e) => eprintln!("apk2pkg: warning: {repo} index unavailable: {e}"),
        }
    }
    let root = index
        .get(&args.package)
        .with_context(|| {
            format!(
                "`{}` not found in Alpine {} main/community",
                args.package, args.alpine
            )
        })?
        .clone();

    // Deps may name PROVIDERS rather than packages: `so:libfoo.so.5`
    // (shared libs) or plain virtuals like `icu-data`. Build the provider
    // map from the index's `p:` fields.
    let mut so_providers: BTreeMap<String, String> = BTreeMap::new();
    let mut virtual_providers: BTreeMap<String, String> = BTreeMap::new();
    for entry in index.values() {
        for provide in &entry.provides {
            if let Some(so) = provide.strip_prefix("so:") {
                let so_name = so.split('=').next().unwrap_or(so);
                so_providers
                    .entry(format!("so:{so_name}"))
                    .or_insert(entry.name.clone());
            } else if !provide.contains(':') {
                let name = provide.split('=').next().unwrap_or(provide);
                virtual_providers
                    .entry(name.to_string())
                    .or_insert(entry.name.clone());
            }
        }
    }

    // Runtime dependency closure.
    let mut wanted: Vec<ApkEntry> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::from([root.name.clone()]);
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) || BASE_PROVIDES.contains(&name.as_str()) {
            continue;
        }
        let Some(entry) = index.get(&name).or_else(|| {
            // not a real package — maybe a virtual satisfied by a provider
            virtual_providers.get(&name).and_then(|p| index.get(p))
        }) else {
            eprintln!("apk2pkg: warning: dependency `{name}` not in index — skipped");
            continue;
        };
        if seen.contains(&entry.name) && entry.name != name {
            continue; // provider already vendored under its real name
        }
        seen.insert(entry.name.clone());
        wanted.push(entry.clone());
        for dep in &entry.depends {
            // strip version constraints: "zlib>=1.2" → "zlib"
            let dep_name: String = dep
                .chars()
                .take_while(|c| !matches!(c, '<' | '>' | '=' | '~'))
                .collect();
            let resolved = if let Some(so) = dep_name.strip_prefix("so:") {
                let key = format!("so:{}", so.split('=').next().unwrap_or(so));
                match so_providers.get(&key) {
                    Some(provider) => provider.clone(),
                    None => continue, // provided by musl/base
                }
            } else if dep_name.contains(':') || dep_name.starts_with('/') {
                continue; // pc:, cmd:, /bin/sh — not packages
            } else {
                dep_name
            };
            if !BASE_PROVIDES.contains(&resolved.as_str()) {
                queue.push_back(resolved);
            }
        }
    }
    eprintln!(
        "apk2pkg: vendoring {} apk(s): {}",
        wanted.len(),
        wanted
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Stage: apk contents merge into one dir; ply's builder kegs it at
    // /opt/<name>-<version> automatically (package = no entrypoint).
    let ply_name = args.name.unwrap_or_else(|| sanitize_name(&root.name));
    let ply_version = semverize(&root.version)?;
    let staging = tempfile_dir()?;
    for entry in &wanted {
        let apk_url = format!("{}/{}-{}.apk", entry.repo_url, entry.name, entry.version);
        eprintln!("apk2pkg: unpacking {}-{}", entry.name, entry.version);
        let response = ureq::get(&apk_url)
            .call()
            .with_context(|| format!("download {apk_url}"))?;
        unpack_apk(response.into_body().into_reader(), &staging)?;
    }

    // Env contributions from whatever the keg actually contains.
    let prefix = format!("/opt/{ply_name}-{ply_version}");
    let mut paths = Vec::new();
    let mut lib_paths = Vec::new();
    for rel in ["usr/bin", "bin", "usr/sbin", "sbin"] {
        if staging.join(rel).is_dir() {
            paths.push(format!("{prefix}/{rel}"));
        }
    }
    // Every dir containing a shared object goes on ld_library_path: distro
    // packages keep private libs in subdirs (usr/lib/pulseaudio/…) normally
    // found via absolute RPATHs that keg relocation breaks.
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
    // usr/lib and lib first (the common case), then the private subdirs
    for rel in ["usr/lib", "lib"] {
        if so_dirs.remove(Path::new(rel)) {
            lib_paths.push(format!("{prefix}/{rel}"));
        }
    }
    for rel in so_dirs {
        lib_paths.push(format!("{prefix}/{}", rel.display()));
    }

    let mut manifest = format!(
        "[package]\nname = \"{ply_name}\"\nversion = \"{ply_version}\"\nprovides_abi = \"linux-{ply_arch}-musl\"\n\n[layer]\n"
    );
    manifest.push_str(&format!("path = {}\n", toml_array(&paths)));
    if !lib_paths.is_empty() {
        manifest.push_str(&format!("ld_library_path = {}\n", toml_array(&lib_paths)));
    }
    manifest.push_str(&format!("\n[dependencies]\nalpine = \"{base}\"\n"));
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

fn load_index(repo_url: &str) -> Result<Vec<ApkEntry>> {
    let response = ureq::get(format!("{repo_url}/APKINDEX.tar.gz").as_str()).call()?;
    let gz = flate2::read::MultiGzDecoder::new(response.into_body().into_reader());
    let mut archive = tar::Archive::new(gz);
    for file in archive.entries()? {
        let mut file = file?;
        if file.path()?.to_string_lossy() == "APKINDEX" {
            let mut text = String::new();
            file.read_to_string(&mut text)?;
            return Ok(parse_index(&text, repo_url));
        }
    }
    bail!("no APKINDEX inside {repo_url}/APKINDEX.tar.gz")
}

fn parse_index(text: &str, repo_url: &str) -> Vec<ApkEntry> {
    let mut entries = Vec::new();
    for block in text.split("\n\n") {
        let field = |tag: &str| -> Option<&str> {
            block
                .lines()
                .find(|l| l.starts_with(tag))
                .map(|l| l[tag.len()..].trim())
        };
        if let (Some(name), Some(version)) = (field("P:"), field("V:")) {
            entries.push(ApkEntry {
                name: name.to_string(),
                version: version.to_string(),
                depends: field("D:")
                    .map(|d| d.split_whitespace().map(String::from).collect())
                    .unwrap_or_default(),
                provides: field("p:")
                    .map(|p| p.split_whitespace().map(String::from).collect())
                    .unwrap_or_default(),
                repo_url: repo_url.to_string(),
            });
        }
    }
    entries
}

/// Unpack an .apk (tar.gz), skipping its metadata files (.PKGINFO, .SIGN.*).
fn unpack_apk(reader: impl Read, dest: &Path) -> Result<()> {
    let gz = flate2::read::MultiGzDecoder::new(reader);
    let mut archive = tar::Archive::new(gz);
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(false);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let top = path
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default();
        if top.starts_with('.') {
            continue; // .PKGINFO, .SIGN.*, .pre-install (ignored: no hooks, ever)
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
        // Some packages ship entries their own unpacker can't work with as
        // non-root: read-only dirs (bluez: /etc/bluetooth is 0555) break
        // unpacking the files inside, and unreadable files (haserl-lua5.4 is
        // ---s--x--x) break the keg build that must read them. Keep owner
        // rwx on dirs and owner read on files — root in the container
        // bypasses modes anyway.
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
    }
    Ok(())
}

/// ply names: lowercase, no `+`, no `-<digit>`.
fn sanitize_name(apk_name: &str) -> String {
    apk_name
        .to_lowercase()
        .replace("++", "pp")
        .replace('+', "p")
        .replace('_', "-")
}

/// "6.1.1-r5" → 6.1.1; "1.7" → 1.7.0; "9.1_p1-r0" → 9.1.0
fn semverize(apk_version: &str) -> Result<semver::Version> {
    let core: String = apk_version
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let parts: Vec<&str> = core.trim_matches('.').split('.').collect();
    let get = |i: usize| -> u64 { parts.get(i).and_then(|p| p.parse().ok()).unwrap_or(0) };
    if parts.is_empty() || parts[0].is_empty() {
        bail!("cannot derive a semver from Alpine version `{apk_version}`");
    }
    Ok(semver::Version::new(get(0), get(1), get(2)))
}

fn toml_array(items: &[String]) -> String {
    let quoted: Vec<String> = items.iter().map(|i| format!("\"{i}\"")).collect();
    format!("[{}]", quoted.join(", "))
}

fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        _ => "x64",
    }
}

/// `--arch` flag → (ply arch, alpine arch). None = the host's.
fn resolve_arch(flag: Option<&str>) -> Result<(&'static str, &'static str)> {
    match flag.unwrap_or(host_arch()) {
        "x64" => Ok(("x64", "x86_64")),
        "arm64" => Ok(("arm64", "aarch64")),
        other => bail!("--arch `{other}`: supported values are x64, arm64"),
    }
}

/// Mirror URL for a branch: releases get a `v` prefix, edge doesn't.
fn branch_url(branch: &str) -> String {
    if branch == "edge" {
        "https://dl-cdn.alpinelinux.org/alpine/edge".to_string()
    } else {
        format!("https://dl-cdn.alpinelinux.org/alpine/v{branch}")
    }
}

/// The `alpine = "…"` constraint the converted package declares.
fn base_constraint(branch: &str, flag: Option<&str>) -> Result<String> {
    match (branch, flag) {
        (_, Some(base)) => Ok(base.to_string()),
        ("edge", None) => bail!(
            "--alpine edge is rolling and has no version to declare as the base constraint — pass --base <version>, e.g. --base 3.20"
        ),
        (branch, None) => Ok(branch.to_string()),
    }
}

/// All file paths under `root`, recursively (tiny local walker — the ply
/// workspace's walkdir crate isn't a dependency of this bin's hot path).
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
    let dir = std::env::temp_dir().join(format!("apk2pkg-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arch_flag_selects_target() {
        assert_eq!(resolve_arch(Some("arm64")).unwrap(), ("arm64", "aarch64"));
        assert_eq!(resolve_arch(Some("x64")).unwrap(), ("x64", "x86_64"));
    }

    #[test]
    fn arch_defaults_to_host() {
        let (ply, alpine) = resolve_arch(None).unwrap();
        assert_eq!(ply, host_arch());
        assert_eq!(alpine, if ply == "arm64" { "aarch64" } else { "x86_64" });
    }

    #[test]
    fn arch_rejects_unknown() {
        assert!(resolve_arch(Some("riscv64")).is_err());
    }

    #[test]
    fn mirror_url_versioned_branch_gets_v_prefix() {
        assert_eq!(
            branch_url("3.20"),
            "https://dl-cdn.alpinelinux.org/alpine/v3.20"
        );
    }

    #[test]
    fn mirror_url_edge_has_no_v_prefix() {
        assert_eq!(
            branch_url("edge"),
            "https://dl-cdn.alpinelinux.org/alpine/edge"
        );
    }

    #[test]
    fn base_constraint_defaults_to_branch() {
        assert_eq!(base_constraint("3.20", None).unwrap(), "3.20");
    }

    #[test]
    fn base_constraint_flag_overrides() {
        assert_eq!(base_constraint("3.20", Some("3.21")).unwrap(), "3.21");
    }

    #[test]
    fn base_constraint_required_for_edge() {
        assert!(base_constraint("edge", None).is_err());
        assert_eq!(base_constraint("edge", Some("3.20")).unwrap(), "3.20");
    }
}
