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
    extract_subtree(image, "/", dest, false)
}

/// Extract the part of `image` under `subdir` into `dest`, and — when
/// `chown` — give every node the uid and gid the image records. That is
/// what a volume restore needs: the app has to find its files owned by
/// itself. It only works with CAP_CHOWN over those ids, which the
/// container's init has inside its user namespace and an unprivileged
/// host process does not; the caller knows which it is.
pub fn extract_subtree(image: &Path, subdir: &str, dest: &Path, chown: bool) -> Result<()> {
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

    let subdir = subdir.trim_end_matches('/');
    for node in fs.files() {
        let full = node.fullpath.to_string_lossy();
        // Inside `subdir` only, and relative to it.
        let rel: std::path::PathBuf = if subdir.is_empty() || subdir == "/" {
            node.fullpath
                .strip_prefix("/")
                .unwrap_or(&node.fullpath)
                .to_path_buf()
        } else {
            match full.strip_prefix(subdir) {
                // The subdir root itself: not a child to write under `dest`,
                // but its mode and ownership ARE `dest`'s. A volume snapshot
                // keeps postgres's data dir at 0700, and postgres refuses to
                // start from anything looser — so the mount point must become
                // exactly what the snapshot's top directory was, not the
                // 0755 the launch created it with.
                Some("") => {
                    let _ = std::fs::set_permissions(
                        dest,
                        std::fs::Permissions::from_mode(node.header.permissions as u32),
                    );
                    if chown {
                        let _ = std::os::unix::fs::lchown(
                            dest,
                            Some(node.header.uid),
                            Some(node.header.gid),
                        );
                    }
                    continue;
                }
                Some(rest) if rest.starts_with('/') => std::path::PathBuf::from(&rest[1..]),
                _ => continue,
            }
        };
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
        if chown {
            // lchown: a symlink's own ownership, never its target's.
            std::os::unix::fs::lchown(&target, Some(node.header.uid), Some(node.header.gid))
                .map_err(ioerr)?;
        }
    }

    // Deepest first: BTreeMap orders a parent before its children, so
    // reversing seals a directory only once nothing else goes inside it.
    for (dir, mode) in deferred.iter().rev() {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(*mode));
    }
    Ok(())
}
