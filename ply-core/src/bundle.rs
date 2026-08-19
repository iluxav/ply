//! `ply bundle` — flatten an app + its locked closure into one
//! self-sufficient (fat) image. Offline/airgapped story: zero fetches at run.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::image::extract::extract_rootfs;
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{read_embedded, read_lockfile, read_manifest, MANIFEST_PATH};
use crate::image::squashfs::{write_image, ExtraFile, TreeSource};
use crate::manifest::Layer;
use crate::source::Source;
use crate::store::Store;

pub struct BundleOutcome {
    pub image_path: PathBuf,
    pub digest: String,
    pub size_bytes: u64,
}

pub fn bundle(image: &Path, output: &Path, allow_insecure: bool) -> Result<BundleOutcome> {
    let mut manifest = read_manifest(image)?;
    if !manifest.is_app() {
        return Err(Error::Build(format!(
            "{}: only app images can be bundled",
            image.display()
        )));
    }
    let lockfile = read_lockfile(image)?.unwrap_or_default();

    // Collect all layer images: deps from the store (fetch if missing).
    let store = Store::open_default()?;
    let mut dep_images: Vec<PathBuf> = Vec::new();
    let mut dep_layers: Vec<Layer> = Vec::new();
    for pkg in &lockfile.packages {
        let path = match store.image_path(&pkg.sha256) {
            Some(path) => path,
            None => {
                let source = Source::parse(&pkg.source, allow_insecure)?;
                let img = ImageName::new(&pkg.name, pkg.version.clone(), Os::Linux, Arch::host())?;
                eprintln!("ply: fetching {img} ({})", pkg.sha256);
                source.fetch(&img, Some(&pkg.sha256), &store)?.1
            }
        };
        if let Some(bytes) = read_embedded(&path, "/.layer.toml")? {
            dep_layers.push(
                toml::from_str(&String::from_utf8_lossy(&bytes))
                    .map_err(|e| Error::Build(format!("{}: bad /.layer.toml: {e}", pkg.name)))?,
            );
        }
        dep_images.push(path);
    }

    // Flatten: base first, app last — later extraction wins, exactly like
    // the overlay stacking at run time.
    let tmp = tempfile::tempdir().map_err(|source| Error::Io {
        path: PathBuf::from("tempdir"),
        source,
    })?;
    let rootfs = tmp.path().join("rootfs");
    for img in dep_images.iter().rev() {
        extract_rootfs(img, &rootfs)?;
    }
    extract_rootfs(image, &rootfs)?;
    // Metadata files are re-embedded fresh below.
    let _ = std::fs::remove_file(rootfs.join(".manifest.toml"));
    let _ = std::fs::remove_file(rootfs.join(".lock.toml"));
    let _ = std::fs::remove_file(rootfs.join(".layer.toml"));

    // Bake the layers' env contributions into the manifest: a fat image has
    // no dep layers left to contribute PATH/LD_LIBRARY_PATH at run time.
    let layer_refs: Vec<&Layer> = dep_layers.iter().collect();
    let composed = crate::env::compose_env(&layer_refs, &manifest.env, &[]);
    manifest.env = composed;
    manifest.dependencies.clear();
    manifest.sources.clear();

    let trees = [TreeSource {
        dir: &rootfs,
        prefix: "",
        filter: None,
    }];
    let extra = [ExtraFile {
        path: MANIFEST_PATH.into(),
        bytes: manifest.to_toml()?.into_bytes(),
        mode: 0o444,
    }];
    let tmp_out = output.with_extension("img.tmp");
    write_image(&trees, &extra, &tmp_out)?;
    std::fs::rename(&tmp_out, output).map_err(|source| Error::Io {
        path: output.to_path_buf(),
        source,
    })?;

    let digest = crate::digest::sha256_file(output)?;
    let size_bytes = std::fs::metadata(output)
        .map_err(|source| Error::Io {
            path: output.to_path_buf(),
            source,
        })?
        .len();
    Ok(BundleOutcome {
        image_path: output.to_path_buf(),
        digest,
        size_bytes,
    })
}
