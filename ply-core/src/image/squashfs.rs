//! Deterministic squashfs writer.
//!
//! Same input tree → byte-identical image (content-addressing depends on
//! this): entries sorted, timestamps zeroed, uid/gid fixed to 0, zstd.

use std::fs::File;
use std::path::{Path, PathBuf};

use backhand::compression::Compressor;
use backhand::{FilesystemCompressor, FilesystemWriter, NodeHeader, DEFAULT_BLOCK_SIZE};
use walkdir::WalkDir;

use crate::error::{Error, Result};

/// An in-memory file to place into the image (e.g. `/.manifest.toml`).
pub struct ExtraFile {
    /// Absolute path inside the image.
    pub path: String,
    pub bytes: Vec<u8>,
    pub mode: u16,
}

/// Filter over paths relative to the tree root (return false to skip; a
/// skipped directory prunes its whole subtree).
pub type TreeFilter<'a> = &'a dyn Fn(&Path) -> bool;

pub struct TreeSource<'a> {
    /// Host directory to pack.
    pub dir: &'a Path,
    /// Destination prefix inside the image, e.g. `/opt/myapp` ("" = root).
    pub prefix: &'a str,
    /// Skip entries (relative paths) for which this returns false.
    pub filter: Option<TreeFilter<'a>>,
}

fn header(mode: u16) -> NodeHeader {
    NodeHeader {
        permissions: mode,
        uid: 0,
        gid: 0,
        mtime: 0,
    }
}

/// Write a deterministic squashfs image to `out`.
pub fn write_image(trees: &[TreeSource], extra: &[ExtraFile], out: &Path) -> Result<()> {
    let err = |path: &Path, source: std::io::Error| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let berr = |what: &str, e: backhand::BackhandError| {
        Error::Build(format!("squashfs write failed at {what}: {e}"))
    };

    let mut fs = FilesystemWriter::default();
    fs.set_time(0);
    fs.set_block_size(DEFAULT_BLOCK_SIZE);
    fs.set_only_root_id();
    fs.set_root_mode(0o755);
    let compressor = FilesystemCompressor::new(Compressor::Zstd, None)
        .map_err(|e| berr("compressor init", e))?;
    fs.set_compressor(compressor);

    // Keep file handles alive until write(); FilesystemWriter borrows readers.
    let mut open_files: Vec<(PathBuf, String, u16)> = Vec::new();

    for tree in trees {
        if !tree.prefix.is_empty() {
            fs.push_dir_all(tree.prefix, header(0o755))
                .map_err(|e| berr(tree.prefix, e))?;
        }
        let walker = WalkDir::new(tree.dir)
            .min_depth(1)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| {
                if let Some(filter) = tree.filter {
                    if let Ok(rel) = e.path().strip_prefix(tree.dir) {
                        return filter(rel);
                    }
                }
                true
            });
        for entry in walker {
            let entry =
                entry.map_err(|e| Error::Build(format!("walking {}: {e}", tree.dir.display())))?;
            let rel = entry
                .path()
                .strip_prefix(tree.dir)
                .expect("walkdir yields children of root");
            let dest = format!(
                "{}/{}",
                tree.prefix.trim_end_matches('/'),
                rel.to_string_lossy()
            );
            let meta = entry
                .path()
                .symlink_metadata()
                .map_err(|e| err(entry.path(), e))?;
            let mode = (std::os::unix::fs::MetadataExt::mode(&meta) & 0o7777) as u16;

            let ftype = meta.file_type();
            if ftype.is_dir() {
                fs.push_dir_all(&dest, header(mode))
                    .map_err(|e| berr(&dest, e))?;
            } else if ftype.is_file() {
                open_files.push((entry.path().to_path_buf(), dest, mode));
            } else if ftype.is_symlink() {
                let target = std::fs::read_link(entry.path()).map_err(|e| err(entry.path(), e))?;
                fs.push_symlink(target.to_string_lossy().as_ref(), &dest, header(0o777))
                    .map_err(|e| berr(&dest, e))?;
            } else {
                return Err(Error::Build(format!(
                    "{}: unsupported file type (only regular files, dirs, symlinks can go into an image)",
                    entry.path().display()
                )));
            }
        }
    }

    for (host_path, dest, mode) in &open_files {
        let file = File::open(host_path).map_err(|e| err(host_path, e))?;
        fs.push_file(file, dest, header(*mode))
            .map_err(|e| berr(dest, e))?;
    }

    for ef in extra {
        fs.push_file(
            std::io::Cursor::new(ef.bytes.clone()),
            &ef.path,
            header(ef.mode),
        )
        .map_err(|e| berr(&ef.path, e))?;
    }

    let mut out_file = std::io::BufWriter::new(File::create(out).map_err(|e| err(out, e))?);
    fs.write(&mut out_file).map_err(|e| berr("finalize", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::sha256_file;
    use std::os::unix::fs::PermissionsExt;

    fn make_tree(root: &Path) {
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        std::fs::write(root.join("hello.txt"), b"hello").unwrap();
        std::fs::write(root.join("sub/deep/x.bin"), vec![7u8; 200_000]).unwrap();
        let exe = root.join("run.sh");
        std::fs::write(&exe, b"#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("hello.txt", root.join("link")).unwrap();
    }

    #[test]
    fn byte_identical_rebuilds() {
        let src = tempfile::tempdir().unwrap();
        make_tree(src.path());
        let out = tempfile::tempdir().unwrap();

        let build = |name: &str| {
            let path = out.path().join(name);
            let trees = [TreeSource {
                dir: src.path(),
                prefix: "/opt/app",
                filter: None,
            }];
            let extra = [ExtraFile {
                path: "/.manifest.toml".into(),
                bytes: b"x = 1\n".to_vec(),
                mode: 0o444,
            }];
            write_image(&trees, &extra, &path).unwrap();
            sha256_file(&path).unwrap()
        };

        let a = build("a.img");
        // Touch mtimes to prove timestamps don't leak into the image.
        std::process::Command::new("touch")
            .arg("-d")
            .arg("2001-02-03")
            .arg(src.path().join("hello.txt"))
            .status()
            .unwrap();
        let b = build("b.img");
        assert_eq!(a, b, "rebuild must be byte-identical");
    }

    #[test]
    fn content_change_changes_hash() {
        let src = tempfile::tempdir().unwrap();
        make_tree(src.path());
        let out = tempfile::tempdir().unwrap();
        let tree = TreeSource {
            dir: src.path(),
            prefix: "/opt/app",
            filter: None,
        };
        let p1 = out.path().join("1.img");
        write_image(std::slice::from_ref(&tree), &[], &p1).unwrap();
        std::fs::write(src.path().join("hello.txt"), b"changed").unwrap();
        let p2 = out.path().join("2.img");
        write_image(std::slice::from_ref(&tree), &[], &p2).unwrap();
        assert_ne!(sha256_file(&p1).unwrap(), sha256_file(&p2).unwrap());
    }
}
