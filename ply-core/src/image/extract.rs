//! Extract a squashfs image's tree to a host directory (no mounting, no
//! root) — the read path for bundle/rebase.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use backhand::{FilesystemReader, InnerNode};

use crate::error::{Error, Result};

/// Extract every file/dir/symlink of `image` into `dest` (created if
/// missing). Later extractions over the same dest overwrite — overlay
/// semantics for flattening: extract base first, app last.
pub fn extract_rootfs(image: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(image).map_err(|source| Error::Io {
        path: image.to_path_buf(),
        source,
    })?;
    let fs = FilesystemReader::from_reader(std::io::BufReader::new(file)).map_err(|e| {
        Error::Build(format!(
            "{}: not a valid squashfs image: {e}",
            image.display()
        ))
    })?;

    // Directory modes are applied only after every node is written: a
    // read-only directory (0555 is common in RHEL-family trees) would
    // otherwise refuse the files that belong inside it. Root would not
    // notice — it has CAP_DAC_OVERRIDE — which is exactly how this stays
    // hidden until someone runs rootless.
    let mut deferred: std::collections::BTreeMap<std::path::PathBuf, u32> =
        std::collections::BTreeMap::new();

    for node in fs.files() {
        let rel = node
            .fullpath
            .strip_prefix("/")
            .unwrap_or(&node.fullpath)
            .to_path_buf();
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(&rel);
        let ioerr = |source: std::io::Error| Error::Io {
            path: target.clone(),
            source,
        };
        match &node.inner {
            InnerNode::Dir(_) => {
                std::fs::create_dir_all(&target).map_err(ioerr)?;
                let mode = node.header.permissions as u32;
                let _ = std::fs::set_permissions(
                    &target,
                    std::fs::Permissions::from_mode(mode | 0o700),
                );
                deferred.insert(target.clone(), mode);
            }
            InnerNode::File(f) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(ioerr)?;
                }
                // overwrite semantics: a symlink/file from a lower layer loses
                let _ = std::fs::remove_file(&target);
                let mut reader = fs.file(f).reader();
                let mut out = std::fs::File::create(&target).map_err(ioerr)?;
                std::io::copy(&mut reader, &mut out).map_err(ioerr)?;
                out.set_permissions(std::fs::Permissions::from_mode(
                    node.header.permissions as u32,
                ))
                .map_err(ioerr)?;
            }
            InnerNode::Symlink(link) => {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).map_err(ioerr)?;
                }
                let _ = std::fs::remove_file(&target);
                std::os::unix::fs::symlink(&link.link, &target).map_err(ioerr)?;
            }
            // devices/fifos never make it into ply images (writer refuses)
            _ => {}
        }
    }

    // Deepest first: BTreeMap orders a parent before its children, so
    // reversing seals a directory only once nothing else goes inside it.
    for (dir, mode) in deferred.iter().rev() {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(*mode));
    }
    Ok(())
}
