//! `ply build` — dir + ply.toml → deterministic image.
//!
//! Three kinds of builds, one command:
//! - app (has entrypoint): files under `/opt/<name>`, deps resolved, lockfile
//!   written next to ply.toml and embedded as `/.lock.toml`
//! - package (no entrypoint): files under `/opt/<name>-<version>` (kegs),
//!   `[layer]` env contributions embedded as `/.layer.toml`
//! - base package (`base = true`): files pack at `/` — owns FHS, /bin/sh, libc

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{LOCKFILE_PATH, MANIFEST_PATH};
use crate::image::squashfs::{write_image, ExtraFile, TreeSource};
use crate::lockfile::{LockedPackage, Lockfile};
use crate::manifest::Manifest;
use crate::resolve::Resolver;
use crate::store::Store;

pub struct BuildOptions {
    /// Directory containing ply.toml.
    pub dir: PathBuf,
    /// Output image path; defaults to `<dir>/<canonical-filename>`.
    pub output: Option<PathBuf>,
    /// Allow plain-http sources on public hosts.
    pub allow_insecure: bool,
    /// Target architecture (None = the host's). Cross-building resolves the
    /// dependency graph for the target arch and names the image accordingly;
    /// packing is arch-independent, so any host can build for any arch.
    pub arch: Option<Arch>,
    /// Pack credential-shaped files that were swept in implicitly. Off by
    /// default: an image is a distributable artifact, so shipping a `.env`
    /// is a refusal, not a warning.
    pub allow_secrets: bool,
}

/// Never ships, at ANY depth. Deliberately short: only files that cannot be
/// part of a running app. `node_modules` and `target` are NOT here — a plain
/// Node app needs the former at run time and Maven puts its artifact in the
/// latter, so guessing would break real builds. Those are surfaced by the
/// size summary instead, and excluded with `include` when unwanted.
fn is_never_packed(name: &str) -> bool {
    matches!(
        name,
        ".git" | "__pycache__" | ".mypy_cache" | ".pytest_cache" | ".DS_Store"
    )
}

/// Credential-shaped names. An image is distributable — `ply push` puts it on
/// a public registry — so these refuse the build unless the operator either
/// named the file in `include` (an explicit choice) or passed
/// `--allow-secrets`. `.pem` is absent on purpose: a CA bundle is a public
/// certificate and ships legitimately (see `services/postgres`).
fn is_secret_name(name: &str) -> bool {
    matches!(
        name,
        ".npmrc"
            | ".netrc"
            | ".pgpass"
            | ".git-credentials"
            | ".htpasswd"
            | "id_rsa"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
    ) || name == ".env"
        || name.starts_with(".env.")
        || name.starts_with(".ssh")
        || name.starts_with(".aws")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.ends_with(".keystore")
}

#[derive(Debug)]
pub struct BuildOutcome {
    pub image_path: PathBuf,
    pub image_name: ImageName,
    pub digest: String,
    pub size_bytes: u64,
    /// Resolved dependencies (empty for packages and dep-less apps).
    pub locked: Vec<(String, String)>,
}

pub fn build(opts: &BuildOptions) -> Result<BuildOutcome> {
    let manifest_path = opts.dir.join("ply.toml");
    if !manifest_path.exists() {
        return Err(Error::Build(format!(
            "no ply.toml in {} — create one with a [package] section (name, version, entrypoint)",
            opts.dir.display()
        )));
    }
    let manifest = Manifest::load(&manifest_path)?;

    let arch = opts.arch.unwrap_or_else(Arch::host);

    // Only apps resolve at build time. A dep package's [dependencies] are
    // metadata consumed when an app's graph is resolved.
    let lockfile = if !manifest.is_app() || manifest.dep_specs().is_empty() {
        None
    } else {
        let store = Store::open_default()?;
        let mut resolver = Resolver::new(&manifest, &store, arch, opts.allow_insecure);
        let resolution = resolver.resolve()?;
        let lockfile = Lockfile {
            packages: resolution
                .packages
                .iter()
                .map(|p| LockedPackage {
                    name: p.name.clone(),
                    version: p.version.clone(),
                    source: p.source_spec.clone(),
                    sha256: p.digest.clone(),
                })
                .collect(),
        };
        lockfile.save(&opts.dir.join("ply.lock"))?;
        Some(lockfile)
    };

    let image_name = ImageName::new(
        &manifest.package.name,
        manifest.package.version.clone(),
        Os::Linux,
        arch,
    )?;
    let image_path = match &opts.output {
        Some(path) => path.clone(),
        None => opts.dir.join(image_name.to_string()),
    };

    // Never pack build inputs/outputs into the image.
    let output_abs = std::path::absolute(&image_path).map_err(|source| Error::Io {
        path: image_path.clone(),
        source,
    })?;

    // `[package] include` whitelist: only listed paths ship. Typos fail loudly.
    // Normalised as PATHS: `./dist`, `dist/` and `dist` are one entry. The
    // filter below compares components (`Path::starts_with`), and a leading
    // `./` component used to pass the existence check yet match nothing —
    // an empty image, no error, no summary line.
    let include: Vec<PathBuf> = manifest
        .package
        .include
        .iter()
        .map(|e| {
            Path::new(e)
                .components()
                .filter(|c| !matches!(c, std::path::Component::CurDir))
                .collect::<PathBuf>()
        })
        .collect();
    for entry in &include {
        if opts.dir.join(entry).symlink_metadata().is_err() {
            return Err(Error::Build(format!(
                "package.include entry `{}` not found in {} — remove it or create it (did the app's own build step run?)",
                entry.display(),
                opts.dir.display()
            )));
        }
    }

    let dir_abs = std::path::absolute(&opts.dir).map_err(|source| Error::Io {
        path: opts.dir.clone(),
        source,
    })?;
    let inc = include.clone();
    let filter = move |rel: &Path| -> bool {
        let name = rel
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if rel.components().count() == 1 {
            if name == "ply.toml" || name == "ply.lock" || name.ends_with(".img") {
                return false;
            }
            if dir_abs.join(rel) == output_abs {
                return false;
            }
        }
        // Build detritus never ships, at any depth — a vendored `.git` used to
        // sail through because this test was depth-1 only. An include entry
        // that names it, or points INTO it (`include = [".git/config"]`), is
        // the operator's decision and wins; an include of an enclosing dir
        // (`include = ["vendor"]`) does not — that is the vendored-.git case.
        if is_never_packed(&name) && !inc.iter().any(|e| e.starts_with(rel)) {
            return false;
        }
        if inc.is_empty() {
            return true;
        }
        // Inside an included subtree, or an ancestor dir the walker must
        // descend through to reach one.
        inc.iter()
            .any(|entry| rel.starts_with(entry) || entry.starts_with(rel))
    };

    // What would ship, before it ships: credential-shaped files refuse the
    // build, and an implicit pack says how much it swept up (a squashfs of
    // 200 MB of node_modules can report a few KiB, so size is no signal).
    let (packed_files, packed_bytes, secrets) = audit_tree(&opts.dir, &filter, &include);
    if !secrets.is_empty() && !opts.allow_secrets {
        let list: Vec<String> = secrets
            .iter()
            .map(|p| format!("  {}", p.display()))
            .collect();
        return Err(Error::Build(format!(
            "refusing to pack credential-shaped files into {}:\n{}\n\n\
             An image is distributable — `ply push` puts it on a public registry.\n\
             Move them out of the app dir, or name what SHOULD ship:\n\
             \n    [package]\n    include = [\"...\"]\n\n\
             Pass --allow-secrets to override.",
            opts.dir.display(),
            list.join("\n")
        )));
    }
    if include.is_empty() {
        eprintln!(
            "ply: packing {packed_files} files ({}) — no `include` in ply.toml, so everything in {} ships",
            human_bytes(packed_bytes),
            opts.dir.display()
        );
    }

    let prefix = if manifest.package.is_base() {
        String::new() // base owns the image root
    } else if manifest.is_app() {
        format!("/opt/{}", manifest.package.name)
    } else {
        format!(
            "/opt/{}-{}",
            manifest.package.name, manifest.package.version
        )
    };
    let trees = [TreeSource {
        dir: &opts.dir,
        prefix: &prefix,
        filter: Some(&filter),
    }];

    let mut extra = vec![ExtraFile {
        path: MANIFEST_PATH.into(),
        bytes: manifest.to_toml()?.into_bytes(),
        mode: 0o444,
    }];
    if let Some(lockfile) = &lockfile {
        extra.push(ExtraFile {
            path: LOCKFILE_PATH.into(),
            bytes: lockfile.to_toml().into_bytes(),
            mode: 0o444,
        });
    }
    if let Some(layer) = &manifest.layer {
        let layer_toml = toml::to_string_pretty(layer).map_err(|e| Error::Build(e.to_string()))?;
        extra.push(ExtraFile {
            path: "/.layer.toml".into(),
            bytes: layer_toml.into_bytes(),
            mode: 0o444,
        });
    }

    // Build to a temp name, rename into place (a half-written .img must
    // never be mistaken for an image). The pid keeps that name unique:
    // two builds of the same app at once — a leftover `ply up` still
    // building while you start another — would otherwise share one temp
    // path, and the loser finalizes into a file the winner already
    // renamed away (ENOENT at finalize).
    let tmp_path = image_path.with_extension(format!("img.tmp.{}", std::process::id()));
    write_image(&trees, &extra, &tmp_path)?;
    std::fs::rename(&tmp_path, &image_path).map_err(|source| Error::Io {
        path: image_path.clone(),
        source,
    })?;

    let digest = crate::digest::sha256_file(&image_path)?;
    let size_bytes = std::fs::metadata(&image_path)
        .map_err(|source| Error::Io {
            path: image_path.clone(),
            source,
        })?
        .len();

    Ok(BuildOutcome {
        image_path,
        image_name,
        digest,
        size_bytes,
        locked: lockfile
            .map(|l| {
                l.packages
                    .iter()
                    .map(|p| (p.name.clone(), p.version.to_string()))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The dir's canonical image when it is newer than every file under the dir
/// (make's contract: timestamps; ctime too, so chmod counts). None = build
/// needed. Conservative: any unreadable entry means rebuild. Top-level build
/// outputs (images, ply.lock) and .git are not inputs.
pub fn up_to_date_image(dir: &Path, arch: Option<Arch>) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::MetadataExt;
    let stamp =
        |md: &std::fs::Metadata| (md.mtime(), md.mtime_nsec()).max((md.ctime(), md.ctime_nsec()));

    let manifest_path = dir.join("ply.toml");
    if !manifest_path.exists() {
        return Ok(None);
    }
    let manifest = Manifest::load(&manifest_path)?;
    let image_name = ImageName::new(
        &manifest.package.name,
        manifest.package.version.clone(),
        Os::Linux,
        arch.unwrap_or_else(Arch::host),
    )?;
    let image = dir.join(image_name.to_string());
    let Ok(image_meta) = image.metadata() else {
        return Ok(None);
    };
    let image_stamp = stamp(&image_meta);

    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            return Ok(None);
        };
        for entry in entries {
            let Ok(entry) = entry else { return Ok(None) };
            let name = entry.file_name().to_string_lossy().into_owned();
            if d == dir && (name.ends_with(".img") || name == "ply.lock" || name == ".git") {
                continue;
            }
            let Ok(md) = entry.metadata() else {
                return Ok(None);
            };
            if stamp(&md) > image_stamp {
                return Ok(None);
            }
            if md.is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    Ok(Some(image))
}

/// Walk exactly what `filter` would pack: (files, uncompressed bytes,
/// credential-shaped paths). Symlinks count as files and are never followed,
/// so a link out of the tree cannot smuggle a directory in.
fn audit_tree(
    dir: &Path,
    filter: &dyn Fn(&Path) -> bool,
    includes: &[PathBuf],
) -> (u64, u64, Vec<PathBuf>) {
    let (mut files, mut bytes, mut secrets) = (0u64, 0u64, Vec::new());
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir.join(&rel)) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let child = if rel.as_os_str().is_empty() {
                PathBuf::from(entry.file_name())
            } else {
                rel.join(entry.file_name())
            };
            if !filter(&child) {
                continue;
            }
            let Ok(md) = entry.path().symlink_metadata() else {
                continue;
            };
            // Credential-shaped names are checked BEFORE the dir/file split:
            // `.ssh/` and `.aws/` are directories, and the old file-only check
            // let `.aws/credentials` ship. A secret directory is reported once
            // and never descended. The exemption is "the operator named it OR
            // named a subtree containing it" — `include = ["config"]` is a
            // decision about config/.env too; making the user repeat the file
            // defeated the point of the whitelist.
            let name = child
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let named = includes.iter().any(|e| child.starts_with(e));
            if is_secret_name(&name) && !named {
                secrets.push(child);
                continue;
            }
            if md.is_dir() {
                stack.push(child);
                continue;
            }
            files += 1;
            bytes += md.len();
        }
    }
    (files, bytes, secrets)
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_dir(dir: &Path) {
        std::fs::write(
            dir.join("ply.toml"),
            r#"
            [package]
            name = "hello"
            version = "0.1.0"
            entrypoint = ["./hello"]
            "#,
        )
        .unwrap();
        std::fs::write(dir.join("hello"), b"\x7fELF fake binary").unwrap();
    }

    #[test]
    fn build_emits_canonical_name_and_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        app_dir(dir.path());
        let opts = BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
            allow_insecure: false,
            allow_secrets: false,
            arch: None,
        };
        let one = build(&opts).unwrap();
        assert!(one
            .image_path
            .to_string_lossy()
            .ends_with(&format!("hello-0.1.0-linux-{}.img", Arch::host().as_str())));
        let two = build(&opts).unwrap();
        assert_eq!(one.digest, two.digest, "rebuild must be byte-identical");
        assert_eq!(one.size_bytes, two.size_bytes);
    }

    #[test]
    fn secrets_refuse_the_build_unless_named_or_overridden() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server"), b"#!/bin/sh\n").unwrap();
        std::fs::write(dir.path().join(".env"), b"AWS_SECRET_ACCESS_KEY=hunter2\n").unwrap();
        std::fs::write(
            dir.path().join("ply.toml"),
            r#"
            [package]
            name = "app"
            version = "1.0.0"
            entrypoint = ["./server"]
            "#,
        )
        .unwrap();
        let mut opts = BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
            allow_insecure: false,
            allow_secrets: false,
            arch: None,
        };

        // swept in implicitly → refused, and the message names the file
        let err = build(&opts).unwrap_err().to_string();
        assert!(err.contains(".env"), "{err}");
        assert!(err.contains("--allow-secrets"), "{err}");

        // explicit override ships it
        opts.allow_secrets = true;
        build(&opts).unwrap();

        // naming it in `include` is also an explicit choice
        opts.allow_secrets = false;
        std::fs::write(
            dir.path().join("ply.toml"),
            r#"
            [package]
            name = "app"
            version = "1.0.0"
            entrypoint = ["./server"]
            include = ["server", ".env"]
            "#,
        )
        .unwrap();
        build(&opts).unwrap();
    }

    #[test]
    fn credential_directories_are_refused_not_just_files() {
        // .ssh/ and .aws/ are DIRECTORIES; the first version of this check
        // only looked at files and let .aws/credentials ship.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".aws")).unwrap();
        std::fs::create_dir_all(dir.path().join(".ssh")).unwrap();
        std::fs::write(
            dir.path().join(".aws/credentials"),
            b"aws_secret_access_key=x",
        )
        .unwrap();
        std::fs::write(dir.path().join(".ssh/config"), b"Host x").unwrap();
        std::fs::write(dir.path().join("server"), b"#!/bin/sh\n").unwrap();
        std::fs::write(
            dir.path().join("ply.toml"),
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\nentrypoint = [\"./server\"]\n",
        )
        .unwrap();
        let err = build(&BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
            allow_insecure: false,
            allow_secrets: false,
            arch: None,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains(".aws"), "{err}");
        assert!(err.contains(".ssh"), "{err}");
        // reported once each, as the directory — not once per file inside
        assert!(!err.contains("credentials"), "{err}");
    }

    #[test]
    fn dot_slash_include_is_the_same_include() {
        // `./dist` passed the existence check but never matched the
        // component-based filter: an empty image, no error, no summary.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/a.js"), b"js").unwrap();
        std::fs::write(
            dir.path().join("ply.toml"),
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\nentrypoint = [\"node\", \"dist/a.js\"]\ninclude = [\"./dist/\"]\n",
        )
        .unwrap();
        let out = build(&BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
            allow_insecure: false,
            allow_secrets: false,
            arch: None,
        })
        .unwrap();
        let file = std::fs::File::open(&out.image_path).unwrap();
        let fs = backhand::FilesystemReader::from_reader(std::io::BufReader::new(file)).unwrap();
        assert!(
            fs.files().any(|n| n.fullpath.ends_with("dist/a.js")),
            "./dist must ship dist/a.js"
        );
    }

    #[test]
    fn including_a_subtree_is_a_decision_about_secrets_inside_it() {
        // `include = ["config"]` with config/.env: the operator named the
        // subtree. Refusing — and advising them to add `include` — was
        // circular. Naming the dir is the explicit choice.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/.env"), b"K=v").unwrap();
        std::fs::write(dir.path().join("config/app.toml"), b"x").unwrap();
        std::fs::write(dir.path().join(".env"), b"TOP=secret").unwrap(); // NOT included
        std::fs::write(
            dir.path().join("ply.toml"),
            "[package]\nname = \"app\"\nversion = \"1.0.0\"\nentrypoint = [\"./x\"]\ninclude = [\"config\"]\n",
        )
        .unwrap();
        let out = build(&BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
            allow_insecure: false,
            allow_secrets: false,
            arch: None,
        })
        .unwrap();
        let file = std::fs::File::open(&out.image_path).unwrap();
        let fs = backhand::FilesystemReader::from_reader(std::io::BufReader::new(file)).unwrap();
        let listing: Vec<String> = fs
            .files()
            .map(|n| n.fullpath.to_string_lossy().into_owned())
            .collect();
        assert!(
            listing.iter().any(|p| p.ends_with("config/.env")),
            "{listing:?}"
        );
        assert!(
            !listing.iter().any(|p| p.ends_with("/app/.env")),
            "top-level .env was not included: {listing:?}"
        );
    }

    #[test]
    fn nested_git_and_caches_never_pack() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/foo/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("app/__pycache__")).unwrap();
        std::fs::write(dir.path().join("vendor/foo/.git/config"), b"x").unwrap();
        std::fs::write(dir.path().join("app/__pycache__/m.pyc"), b"x").unwrap();
        std::fs::write(dir.path().join("app/main.py"), b"print(1)").unwrap();
        std::fs::write(
            dir.path().join("ply.toml"),
            r#"
            [package]
            name = "app"
            version = "1.0.0"
            entrypoint = ["python", "app/main.py"]
            "#,
        )
        .unwrap();
        let outcome = build(&BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
            allow_insecure: false,
            allow_secrets: false,
            arch: None,
        })
        .unwrap();
        let file = std::fs::File::open(&outcome.image_path).unwrap();
        let fs = backhand::FilesystemReader::from_reader(std::io::BufReader::new(file)).unwrap();
        let listing: Vec<String> = fs
            .files()
            .map(|n| n.fullpath.to_string_lossy().into_owned())
            .collect();
        assert!(
            listing.iter().any(|p| p.ends_with("app/main.py")),
            "{listing:?}"
        );
        // the depth-1 test used to let both of these through
        assert!(!listing.iter().any(|p| p.contains(".git")), "{listing:?}");
        assert!(
            !listing.iter().any(|p| p.contains("__pycache__")),
            "{listing:?}"
        );
    }

    #[test]
    fn include_whitelist_limits_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("dist/deep")).unwrap();
        std::fs::write(dir.path().join("dist/deep/app.js"), b"js").unwrap();
        std::fs::write(dir.path().join("junk.log"), b"junk").unwrap();
        std::fs::write(
            dir.path().join("ply.toml"),
            r#"
            [package]
            name = "app"
            version = "1.0.0"
            entrypoint = ["node", "dist/deep/app.js"]
            include = ["dist/deep/"]
            "#,
        )
        .unwrap();
        let opts = BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
            allow_insecure: false,
            allow_secrets: false,
            arch: None,
        };
        let outcome = build(&opts).unwrap();
        let listing: Vec<String> = {
            let file = std::fs::File::open(&outcome.image_path).unwrap();
            let fs =
                backhand::FilesystemReader::from_reader(std::io::BufReader::new(file)).unwrap();
            fs.files()
                .map(|n| n.fullpath.to_string_lossy().into_owned())
                .collect()
        };
        assert!(listing.contains(&"/opt/app/dist/deep/app.js".to_string()));
        assert!(!listing.iter().any(|p| p.contains("junk")));

        // typo in include → hard error
        std::fs::write(
            dir.path().join("ply.toml"),
            r#"
            [package]
            name = "app"
            version = "1.0.0"
            entrypoint = ["node", "dist/deep/app.js"]
            include = ["dost/"]
            "#,
        )
        .unwrap();
        assert!(build(&opts).unwrap_err().to_string().contains("dost"));
    }

    #[test]
    fn package_build_uses_keg_prefix_and_layer_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        std::fs::write(dir.path().join("bin/tool"), b"bin").unwrap();
        std::fs::write(
            dir.path().join("ply.toml"),
            r#"
            [package]
            name = "tool"
            version = "2.0.0"

            [layer]
            path = ["/opt/tool-2.0.0/bin"]
            "#,
        )
        .unwrap();
        let outcome = build(&BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
            allow_insecure: false,
            allow_secrets: false,
            arch: None,
        })
        .unwrap();
        let layer = crate::image::read::read_embedded(&outcome.image_path, "/.layer.toml")
            .unwrap()
            .expect("layer.toml embedded");
        assert!(String::from_utf8_lossy(&layer).contains("/opt/tool-2.0.0/bin"));
        let manifest = crate::image::read::read_manifest(&outcome.image_path).unwrap();
        assert!(!manifest.is_app());
    }
}
