//! Read metadata embedded in a ply image (squashfs) without mounting it.

use std::io::{BufReader, Read};
use std::path::Path;

use backhand::{FilesystemReader, InnerNode};

use crate::error::{Error, Result};
use crate::lockfile::Lockfile;
use crate::manifest::Manifest;

pub const MANIFEST_PATH: &str = "/.manifest.toml";
pub const LOCKFILE_PATH: &str = "/.lock.toml";

/// Extract one embedded file's contents from an image.
pub fn read_embedded(image: &Path, inner_path: &str) -> Result<Option<Vec<u8>>> {
    let file = std::fs::File::open(image).map_err(|source| Error::Io {
        path: image.to_path_buf(),
        source,
    })?;
    let fs = FilesystemReader::from_reader(BufReader::new(file)).map_err(|e| {
        Error::Build(format!(
            "{}: not a valid ply image (squashfs): {e}",
            image.display()
        ))
    })?;
    for node in fs.files() {
        if node.fullpath.to_string_lossy() == inner_path {
            if let InnerNode::File(f) = &node.inner {
                let mut reader = fs.file(f).reader();
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).map_err(|source| Error::Io {
                    path: image.to_path_buf(),
                    source,
                })?;
                return Ok(Some(bytes));
            }
        }
    }
    Ok(None)
}

/// Every ply image embeds its manifest at `/.manifest.toml`.
pub fn read_manifest(image: &Path) -> Result<Manifest> {
    let bytes = read_embedded(image, MANIFEST_PATH)?.ok_or_else(|| {
        Error::Build(format!(
            "{}: no {MANIFEST_PATH} inside — not a ply image?",
            image.display()
        ))
    })?;
    Manifest::parse(&String::from_utf8_lossy(&bytes))
}

/// App images embed their resolved lockfile at `/.lock.toml`.
pub fn read_lockfile(image: &Path) -> Result<Option<Lockfile>> {
    match read_embedded(image, LOCKFILE_PATH)? {
        Some(bytes) => Ok(Some(Lockfile::parse(&String::from_utf8_lossy(&bytes))?)),
        None => Ok(None),
    }
}
