//! `ply build` — dir + ply.toml → deterministic image.
//!
//! Phase 1 scope: thin mode only (app files + embedded manifest, no
//! dependency resolution yet). App files live under `/opt/<name>` — the app
//! owns its prefix, like every other package.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::squashfs::{write_image, ExtraFile, TreeSource};
use crate::manifest::Manifest;

pub struct BuildOptions {
    /// Directory containing ply.toml.
    pub dir: PathBuf,
    /// Output image path; defaults to `<dir>/<canonical-filename>`.
    pub output: Option<PathBuf>,
}

#[derive(Debug)]
pub struct BuildOutcome {
    pub image_path: PathBuf,
    pub image_name: ImageName,
    pub digest: String,
    pub size_bytes: u64,
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

    if !manifest.dependencies.is_empty() {
        return Err(Error::Unimplemented(
            "[dependencies] resolution (Phase 3) — for now build thin images: vendor static binaries in the app dir",
        ));
    }

    let image_name = ImageName::new(
        &manifest.package.name,
        manifest.package.version.clone(),
        Os::Linux,
        Arch::host(),
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
    let filter = move |top: &Path| -> bool {
        let name = top.file_name().unwrap_or_default().to_string_lossy();
        if name == "ply.toml" || name == "ply.lock" || name == ".git" || name.ends_with(".img") {
            return false;
        }
        std::path::absolute(top)
            .map(|p| p != output_abs)
            .unwrap_or(true)
    };

    let prefix = format!("/opt/{}", manifest.package.name);
    let trees = [TreeSource {
        dir: &opts.dir,
        prefix: &prefix,
        filter: Some(&filter),
    }];
    let extra = [ExtraFile {
        path: "/.manifest.toml".into(),
        bytes: manifest.to_toml()?.into_bytes(),
        mode: 0o444,
    }];

    // Build to a temp name, fsync-rename into place (a half-written .img must
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
        };
        let one = build(&opts).unwrap();
        assert!(one
            .image_path
            .to_string_lossy()
            .ends_with(&format!("hello-0.1.0-linux-{}.img", Arch::host().as_str())));
        let two = build(&opts).unwrap();
        assert_eq!(one.digest, two.digest, "rebuild must be byte-identical");
        // the previously built image must not have been packed into the rebuild
        assert_eq!(one.size_bytes, two.size_bytes);
    }

    #[test]
    fn build_refuses_dependencies_for_now() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ply.toml"),
            r#"
            [package]
            name = "app"
            version = "0.1.0"
            entrypoint = ["./app"]
            [dependencies]
            node = "22"
            "#,
        )
        .unwrap();
        let err = build(&BuildOptions {
            dir: dir.path().to_path_buf(),
            output: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("Phase 3"));
    }
}
