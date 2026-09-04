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
        } else if crate::paths::is_root() {
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
        eprintln!("ply: extracting {digest} (unpacked once, then reused)");
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
        // Staging is unique per inserter: a shared `.tmp-<digest>` path let
        // concurrent inserters of the same digest delete each other's
        // half-staged files.
        static STAGE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let staging = self.root.join(format!(
            ".tmp-{digest}.{}.{}",
            std::process::id(),
            STAGE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&staging).map_err(|source| Error::Io {
            path: staging.clone(),
            source,
        })?;
        let staged = staging.join("pkg.img");
        if let Err(e) = move_file(file, &staged) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        // Atomic claim of the digest dir; a concurrent winner is fine — the
        // loser discards its staging.
        match std::fs::rename(&staging, &dir) {
            Ok(()) => {}
            Err(_) if dest.exists() => {
                let _ = std::fs::remove_dir_all(&staging);
            }
            Err(_) if dir.is_dir() => {
                // The entry exists but holds no pkg.img — extraction-only,
                // left by extracted_rootfs() when the same bytes first
                // arrived as a plain file path. Claim just the image file.
                let moved = std::fs::rename(&staged, &dest);
                let _ = std::fs::remove_dir_all(&staging);
                match moved {
                    Ok(()) => {}
                    Err(_) if dest.exists() => {} // concurrent winner
                    Err(source) => return Err(Error::Io { path: dest, source }),
                }
            }
            Err(source) => {
                let _ = std::fs::remove_dir_all(&staging);
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

extern "C" {}

#[cfg(test)]
mod tests {
    use super::*;

    /// N threads insert the same digest concurrently (a fleet fetching one
    /// dep into a shared store). Every insert must succeed and the stored
    /// bytes must be intact — fixed staging names corrupted this.
    #[test]
    fn concurrent_inserts_of_same_digest_are_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        let content = vec![7u8; 300_000];
        let digest = "sha256:test-digest";

        let mut handles = Vec::new();
        for i in 0..8 {
            let root = root.clone();
            let content = content.clone();
            let src = tmp.path().join(format!("src-{i}"));
            handles.push(std::thread::spawn(move || {
                std::fs::write(&src, &content).unwrap();
                Store::at(root).insert(&src, digest)
            }));
        }
        for h in handles {
            let path = h.join().unwrap().expect("insert must not race to failure");
            assert!(path.ends_with(format!("{digest}/pkg.img")));
        }
        let store = Store::at(root);
        let stored = std::fs::read(store.root().join(digest).join("pkg.img")).unwrap();
        assert_eq!(stored.len(), content.len(), "stored bytes must be intact");
        // no staging litter left behind
        let litter: Vec<_> = std::fs::read_dir(store.root())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(litter.is_empty(), "staging litter: {litter:?}");
    }

    /// A digest dir can pre-exist WITHOUT pkg.img: extracted_rootfs() creates
    /// `<digest>/rootfs` when the same bytes first arrived via a plain file
    /// path (never inserted). insert() must claim the image file into that
    /// entry instead of failing ENOTEMPTY on the dir rename.
    #[test]
    fn insert_into_extraction_only_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("store");
        let digest = "sha256:test-digest";
        let entry = root.join(digest);
        std::fs::create_dir_all(entry.join("rootfs/usr/bin")).unwrap();
        std::fs::write(entry.join(".rootfs-ok"), b"").unwrap();

        let src = tmp.path().join("src");
        std::fs::write(&src, b"image bytes").unwrap();
        let store = Store::at(root);
        let path = store.insert(&src, digest).expect("claim the file slot");
        assert_eq!(std::fs::read(&path).unwrap(), b"image bytes");
        // the extraction survives alongside the image
        assert!(entry.join(".rootfs-ok").exists());
        assert!(entry.join("rootfs/usr/bin").is_dir());
        assert_eq!(store.image_path(digest), Some(path));
    }
}
