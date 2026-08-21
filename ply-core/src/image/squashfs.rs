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

/// A file reader that opens on first use and releases its fd at EOF.
/// FilesystemWriter holds every pushed reader until `write()` finishes, so
/// eager `File::open` needs one fd per packed file — more than `ulimit -n`
/// allows for big trees (a Next.js standalone tree easily beats 1024).
struct LazyFile {
    path: PathBuf,
    file: Option<File>,
    pos: u64,
}

impl LazyFile {
    fn new(path: PathBuf) -> Self {
        LazyFile {
            path,
            file: None,
            pos: 0,
        }
    }

    fn open(&mut self) -> std::io::Result<&mut File> {
        use std::io::Seek;
        if self.file.is_none() {
            let mut file = File::open(&self.path).map_err(|e| {
                std::io::Error::new(e.kind(), format!("{}: {e}", self.path.display()))
            })?;
            if self.pos != 0 {
                file.seek(std::io::SeekFrom::Start(self.pos))?;
            }
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("just opened"))
    }
}

impl std::io::Read for LazyFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.open()?.read(buf)?;
        self.pos += n as u64;
        if n == 0 && !buf.is_empty() {
            self.file = None; // EOF — release the fd (reopens if read again)
        }
        Ok(n)
    }
}

impl std::io::Seek for LazyFile {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        match to {
            // Answerable without an fd — don't reopen a released file.
            SeekFrom::Start(p) if self.file.is_none() => {
                self.pos = p;
                Ok(p)
            }
            SeekFrom::Current(0) if self.file.is_none() => Ok(self.pos),
            _ => {
                self.pos = self.open()?.seek(to)?;
                Ok(self.pos)
            }
        }
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

    // Files are pushed after the walk (stable data order) as lazy readers:
    // FilesystemWriter holds every reader until write(), so eager opens
    // would need one fd per file.
    let mut lazy_files: Vec<(PathBuf, String, u16)> = Vec::new();

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
                lazy_files.push((entry.path().to_path_buf(), dest, mode));
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

    for (host_path, dest, mode) in lazy_files {
        fs.push_file(LazyFile::new(host_path), &dest, header(mode))
            .map_err(|e| berr(&dest, e))?;
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
    fn empty_files_have_no_fragment_ref() {
        // backhand 0.25.1 gave zero-length files a dangling fragment ref;
        // the kernel EINVALs such inodes (vendor/backhand carries the fix).
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("empty"), b"").unwrap();
        std::fs::write(src.path().join("full"), b"data").unwrap();
        let out = tempfile::tempdir().unwrap();
        let img = out.path().join("t.img");
        let tree = TreeSource {
            dir: src.path(),
            prefix: "",
            filter: None,
        };
        write_image(std::slice::from_ref(&tree), &[], &img).unwrap();

        let file = std::fs::File::open(&img).unwrap();
        let fs = backhand::FilesystemReader::from_reader(std::io::BufReader::new(file)).unwrap();
        let node = fs
            .files()
            .find(|n| n.fullpath.to_string_lossy() == "/empty")
            .expect("empty file present");
        let backhand::InnerNode::File(f) = &node.inner else {
            panic!("not a file");
        };
        let backhand::SquashfsFileReader::Basic(basic) = f else {
            panic!("expected basic file inode");
        };
        assert_eq!(basic.file_size, 0);
        assert_eq!(
            basic.frag_index, 0xffffffff,
            "empty file must not reference a fragment"
        );
    }

    #[test]
    fn packing_many_files_holds_few_fds() {
        // Packing must need O(1) fds, not one per file: a Next.js standalone
        // tree has thousands of files and default `ulimit -n` is often 1024.
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        fn count_fds() -> usize {
            std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
        }

        let src = tempfile::tempdir().unwrap();
        for i in 0..512 {
            std::fs::write(src.path().join(format!("f{i:04}")), format!("data {i:04}\n").repeat(64)).unwrap();
        }
        let out = tempfile::tempdir().unwrap();
        let img = out.path().join("t.img");

        let baseline = count_fds();
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicUsize::new(0));
        let sampler = {
            let (stop, peak) = (stop.clone(), peak.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    peak.fetch_max(count_fds(), Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
            })
        };

        let tree = TreeSource {
            dir: src.path(),
            prefix: "",
            filter: None,
        };
        write_image(std::slice::from_ref(&tree), &[], &img).unwrap();
        stop.store(true, Ordering::Relaxed);
        sampler.join().unwrap();

        let peak = peak.load(Ordering::Relaxed);
        assert!(
            peak.saturating_sub(baseline) < 64,
            "fd peak {peak} vs baseline {baseline} — writer is holding one fd per packed file"
        );
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
