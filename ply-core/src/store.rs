//! Content-addressed store: `<root>/sha256:<hash>/pkg.img`.
//!
//! The dir name proves the content; the filesystem IS the database.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

pub const SYSTEM_STORE: &str = "/var/lib/ply/store";

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn at(root: PathBuf) -> Store {
        Store { root }
    }

    /// `$PLY_STORE` override → system store (root) → per-user store.
    pub fn open_default() -> Result<Store> {
        let root = if let Ok(dir) = std::env::var("PLY_STORE") {
            PathBuf::from(dir)
        } else if rustix_is_root() {
            PathBuf::from(SYSTEM_STORE)
        } else {
            let home = std::env::var("HOME").map_err(|_| {
                Error::Store("cannot locate store: neither $PLY_STORE nor $HOME is set".into())
            })?;
            Path::new(&home).join(".local/share/ply/store")
        };
        std::fs::create_dir_all(&root).map_err(|source| Error::Io {
            path: root.clone(),
            source,
        })?;
        Ok(Store { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of a stored image, if present.
    pub fn image_path(&self, digest: &str) -> Option<PathBuf> {
        let path = self.root.join(digest).join("pkg.img");
        path.exists().then_some(path)
    }

    /// Rootless fallback: a store entry extracted to a plain dir (same hash
    /// identity — extraction is a cache of the image's contents). Extracts
    /// on first use; a marker file makes partial extractions retry.
    pub fn extracted_rootfs(&self, image: &Path, digest: &str) -> Result<PathBuf> {
        let entry = self.root.join(digest);
        let rootfs = entry.join("rootfs");
        let marker = entry.join(".rootfs-ok");
        if marker.exists() {
            return Ok(rootfs);
        }
        let _ = std::fs::remove_dir_all(&rootfs);
        std::fs::create_dir_all(&entry).map_err(|source| Error::Io {
            path: entry.clone(),
            source,
        })?;
        eprintln!("ply: extracting {digest} (rootless runs use extracted layers)");
        crate::image::extract::extract_rootfs(image, &rootfs)?;
        std::fs::write(&marker, b"").map_err(|source| Error::Io {
            path: marker,
            source,
        })?;
        Ok(rootfs)
    }

    /// Move a verified file into the store. `digest` must already be the
    /// file's sha256 (callers verify before insertion). Idempotent.
    pub fn insert(&self, file: &Path, digest: &str) -> Result<PathBuf> {
        let dir = self.root.join(digest);
        let dest = dir.join("pkg.img");
        if dest.exists() {
            let _ = std::fs::remove_file(file);
            return Ok(dest);
        }
        let staging = self.root.join(format!(".tmp-{digest}"));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(|source| Error::Io {
            path: staging.clone(),
            source,
        })?;
        let staged = staging.join("pkg.img");
        move_file(file, &staged)?;
        // Atomic claim of the digest dir; a concurrent winner is fine.
        match std::fs::rename(&staging, &dir) {
            Ok(()) => {}
            Err(_) if dest.exists() => {
                let _ = std::fs::remove_dir_all(&staging);
            }
            Err(source) => {
                return Err(Error::Io { path: dir, source });
            }
        }
        Ok(dest)
    }
}

/// rename() when possible, copy+remove across filesystems.
fn move_file(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to).map_err(|source| Error::Io {
        path: to.to_path_buf(),
        source,
    })?;
    let _ = std::fs::remove_file(from);
    Ok(())
}

fn rustix_is_root() -> bool {
    // effective uid 0; avoids pulling a uid crate for one call
    unsafe { libc_geteuid() == 0 }
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}
