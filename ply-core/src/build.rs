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
    let include: Vec<PathBuf> = manifest
        .package
        .include
        .iter()
        .map(|e| PathBuf::from(e.trim_end_matches('/')))
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
    let filter = move |rel: &Path| -> bool {
        if rel.components().count() == 1 {
            let name = rel.file_name().unwrap_or_default().to_string_lossy();
            if name == "ply.toml" || name == "ply.lock" || name == ".git" || name.ends_with(".img")
            {
                return false;
            }
            if dir_abs.join(rel) == output_abs {
                return false;
            }
        }
        if include.is_empty() {
            return true;
        }
        // Inside an included subtree, or an ancestor dir the walker must
        // descend through to reach one.
        include
            .iter()
            .any(|entry| rel.starts_with(entry) || entry.starts_with(rel))
    };

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
    // never be mistaken for an image).
    let tmp_path = image_path.with_extension("img.tmp");
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
