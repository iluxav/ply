//! Content addressing: a package IS its sha256. Names are lookup aliases;
//! trust lives in the hash.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// `sha256:<hex>` of a file's contents.
pub fn sha256_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

/// Bare hex sha256 of a string — a filesystem-safe cache key.
pub fn sha256_str(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}
