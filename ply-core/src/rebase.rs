//! `ply rebase` — swap a runtime under an app without rebuilding the app.
//! Fleet security patching = a metadata operation + one new store file.

use std::path::{Path, PathBuf};

use semver::Version;

use crate::error::{Error, Result};
use crate::image::name::{Arch, ImageName, Os};
use crate::image::read::{read_lockfile, read_manifest, LOCKFILE_PATH};
use crate::source::Source;
use crate::store::Store;

pub struct RebaseOutcome {
    pub image_path: PathBuf,
    pub digest: String,
    pub replaced: (String, Version, Version),
}

/// `runtime` is `name@exact.version`, e.g. `node@24.6.1`.
pub fn rebase(
    image: &Path,
    runtime: &str,
    output: &Path,
    allow_insecure: bool,
) -> Result<RebaseOutcome> {
    let (name, version) = runtime.split_once('@').ok_or_else(|| {
        Error::Build(format!(
            "--runtime `{runtime}`: expected name@exact.version, e.g. node@24.6.1"
        ))
    })?;
    let version = Version::parse(version)
        .map_err(|e| Error::Build(format!("--runtime version `{version}`: {e}")))?;

    let manifest = read_manifest(image)?;
    let mut lockfile = read_lockfile(image)?.ok_or_else(|| {
        Error::Build(format!(
            "{}: no embedded lockfile — only split images can be rebased",
            image.display()
        ))
    })?;

    let entry = lockfile
        .packages
        .iter_mut()
        .find(|p| p.name == name)
        .ok_or_else(|| {
            Error::Build(format!(
                "{}: lockfile has no package `{name}`",
                image.display()
            ))
        })?;
    let old_version = entry.version.clone();

    // The manifest's constraint still applies — rebase is not a bypass.
    for (alias, dep) in &manifest.dependencies {
        let spec = dep.spec(alias);
        if spec.package == name {
            if let Ok(req) = semver::VersionReq::parse(&spec.constraint) {
                if !req.matches(&version) {
                    return Err(Error::Build(format!(
                        "{name} {version} does not satisfy the manifest constraint `{}` — change ply.toml and rebuild instead",
                        spec.constraint
                    )));
                }
            }
        }
    }

    // Fetch the new runtime to learn (and pin) its digest.
    let store = Store::open_default()?;
    let source = Source::parse(&entry.source, allow_insecure)?;
    let img = ImageName::new(name, version.clone(), Os::Linux, Arch::host())?;
    let (digest, _) = source.fetch(&img, None, &store)?;
    entry.version = version.clone();
    entry.sha256 = digest;

    // Rewrite only the embedded lockfile; every other byte of the app image
    // is reused as-is.
    let file = std::fs::File::open(image).map_err(|source| Error::Io {
        path: image.to_path_buf(),
        source,
    })?;
    let reader = backhand::FilesystemReader::from_reader(std::io::BufReader::new(file))
        .map_err(|e| Error::Build(format!("{}: {e}", image.display())))?;
    let mut writer = backhand::FilesystemWriter::from_fs_reader(&reader)
        .map_err(|e| Error::Build(format!("rebase writer: {e}")))?;
    writer.set_time(0);
    let lock_bytes = lockfile.to_toml().into_bytes();
    writer
        .replace_file(LOCKFILE_PATH, std::io::Cursor::new(lock_bytes))
        .map_err(|e| Error::Build(format!("replace {LOCKFILE_PATH}: {e}")))?;

    let tmp_out = output.with_extension("img.tmp");
    let mut out =
        std::io::BufWriter::new(std::fs::File::create(&tmp_out).map_err(|source| Error::Io {
            path: tmp_out.clone(),
            source,
        })?);
    writer
        .write(&mut out)
        .map_err(|e| Error::Build(format!("rebase write: {e}")))?;
    drop(out);
    std::fs::rename(&tmp_out, output).map_err(|source| Error::Io {
        path: output.to_path_buf(),
        source,
    })?;

    Ok(RebaseOutcome {
        image_path: output.to_path_buf(),
        digest: crate::digest::sha256_file(output)?,
        replaced: (name.to_string(), old_version, version),
    })
}
