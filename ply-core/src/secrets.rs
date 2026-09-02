//! Secret file store — persisted minted or external secrets.
//!
//! Secrets are stored in plaintext files (mode 0600) in a secrets directory,
//! one file per member.param. The store can mint new secrets or load existing
//! ones, and can be scoped to a stack (visible in `.ply/secrets`) or a
//! deployment (in a systemd-watch-invisible `.secrets` subdirectory).

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// A secrets file store.
pub struct SecretStore {
    dir: PathBuf,
}

impl SecretStore {
    /// Create a store in `stack_dir/.ply/secrets`.
    pub fn for_stack(stack_dir: &Path) -> SecretStore {
        SecretStore {
            dir: stack_dir.join(".ply").join("secrets"),
        }
    }

    /// Create a store in `crate::deployments::dir()/.secrets/<stack_name>`.
    ///
    /// The `.secrets` subdirectory is invisible to systemd's `PathModified`
    /// watch, preventing recursive reconcile loops — same reasoning as
    /// `.status/`.
    pub fn for_deployments(stack_name: &str) -> SecretStore {
        SecretStore {
            dir: crate::deployments::dir().join(".secrets").join(stack_name),
        }
    }

    /// Compute the path for a secret file: `<dir>/<member>.<param>`.
    pub fn path(&self, member: &str, param: &str) -> PathBuf {
        self.dir.join(format!("{member}.{param}"))
    }

    /// A secret's location as a person should read it: the store's own
    /// directory name plus the file — `secrets/db.password` for a stack
    /// store (`<stack>/.ply/secrets/`), `todos/db.password` for a
    /// deployments one (`<deployments>/.secrets/todos/`). Short on purpose:
    /// the absolute path is mostly the layout's own plumbing, and the two
    /// components that vary are the ones that identify the file.
    pub fn label(&self, member: &str, param: &str) -> String {
        match self.dir.file_name() {
            Some(dir) => format!("{}/{member}.{param}", dir.to_string_lossy()),
            None => self.path(member, param).display().to_string(),
        }
    }

    /// Read and return an existing secret, or None if the file doesn't exist.
    ///
    /// Distinguishes between NotFound (returns None) and other IO errors
    /// (propagates as Error::Io/Runtime), so transient read errors never
    /// cause silent re-minting.
    pub fn get(&self, member: &str, param: &str) -> Result<Option<String>> {
        let path = self.path(member, param);
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s.trim_end_matches('\n').to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::Io { path, source: e }),
        }
    }

    /// Write a secret to a file (mode 0600, created atomically).
    ///
    /// Creates parent directories (mode 0700) if needed, writes to a temporary
    /// file, and renames it into place.
    pub fn set(&self, member: &str, param: &str, value: &str) -> Result<()> {
        let path = self.path(member, param);
        let dir = path.parent().unwrap_or_else(|| Path::new("."));

        // Create directory with mode 0700 (owner-only access) if needed
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .recursive(true)
                .create(dir)
                .map_err(|e| Error::Io {
                    path: dir.to_path_buf(),
                    source: e,
                })?;
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir_all(dir).map_err(|e| Error::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
        }

        // Write to temporary file with mode 0600, using appended name to avoid
        // races: db.password and db.username both get .db.password.tmp and
        // .db.username.tmp respectively, not both .db.tmp
        let tmp = dir.join(format!(".{member}.{param}.tmp"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| Error::Io {
                    path: tmp.clone(),
                    source: e,
                })?;
            use std::io::Write;
            f.write_all(value.as_bytes()).map_err(|e| Error::Io {
                path: tmp.clone(),
                source: e,
            })?;
            f.write_all(b"\n").map_err(|e| Error::Io {
                path: tmp.clone(),
                source: e,
            })?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, format!("{}\n", value)).map_err(|e| Error::Io {
                path: tmp.clone(),
                source: e,
            })?;
        }

        // Atomic rename
        std::fs::rename(&tmp, &path).map_err(|e| Error::Io {
            path: path.clone(),
            source: e,
        })?;

        Ok(())
    }

    /// Load an existing secret, or if missing, mint a new one (unless external).
    ///
    /// If `external` is true and the secret doesn't exist, returns an error
    /// with instructions on how to provide it. Otherwise, mints a new secret,
    /// persists it, and returns it.
    pub fn load_or_mint(&self, member: &str, param: &str, external: bool) -> Result<String> {
        // Try to load existing secret
        if let Some(value) = self.get(member, param)? {
            return Ok(value);
        }

        // External secrets must be provided, not minted
        if external {
            return Err(Error::Runtime(format!(
                "secret {member}.{param} is external — provide it: ply secret set {member}.{param} (or write {})",
                self.path(member, param).display()
            )));
        }

        // Mint and persist a new secret
        let secret = mint()?;
        self.set(member, param, &secret)?;
        Ok(secret)
    }

    /// List all secret names in sorted order (never values).
    ///
    /// Returns a Vec of "member.param" strings, excluding temporary `.*.tmp`
    /// files (partial writes, leftover from crashes).
    pub fn list(&self) -> Result<Vec<String>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }

        let mut names = Vec::new();

        // List files in the secrets directory
        for entry in std::fs::read_dir(&self.dir).map_err(|e| Error::Io {
            path: self.dir.clone(),
            source: e,
        })? {
            let entry = entry.map_err(|e| Error::Io {
                path: self.dir.clone(),
                source: e,
            })?;
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Filter out temporary files
                    if !name.ends_with(".tmp") {
                        names.push(name.to_string());
                    }
                }
            }
        }

        names.sort();
        Ok(names)
    }
}

/// Mint a new secret: 32 URL-safe characters from [A-Za-z0-9].
///
/// Reads 64 bytes from `/dev/urandom`, maps each byte to [A-Za-z0-9] via
/// `byte % 62`, and returns the first 32 characters.
///
/// A `/dev/urandom` that cannot be opened or read is an error, never a
/// panic: this runs inside `ply up`'s resolver, where an abort would take a
/// whole stack down without saying why.
pub fn mint() -> Result<String> {
    use std::io::Read;

    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

    let path = Path::new("/dev/urandom");
    let mut bytes = [0u8; 64];
    let mut file = std::fs::File::open(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.read_exact(&mut bytes).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;

    Ok(bytes[..32]
        .iter()
        .map(|b| ALPHABET[(b % 62) as usize] as char)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn mint_is_32_urlsafe_chars() {
        let s = mint().unwrap();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(mint().unwrap(), s);
    }

    #[test]
    fn load_or_mint_persists_0600_and_is_stable() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());
        let a = store.load_or_mint("db", "password", false).unwrap();
        let b = store.load_or_mint("db", "password", false).unwrap();
        assert_eq!(a, b, "the file is the truth until deleted");
        let mode = std::fs::metadata(store.path("db", "password"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The label a `--plan` line shows: short, and different per store, so
    /// "which secrets dir is this?" is answered by the label itself.
    #[test]
    fn label_names_the_store_dir_and_the_file() {
        let td = tempfile::tempdir().unwrap();
        assert_eq!(
            SecretStore::for_stack(td.path()).label("db", "password"),
            "secrets/db.password"
        );
        assert_eq!(
            SecretStore::for_deployments("todos").label("db", "password"),
            "todos/db.password"
        );
    }

    #[test]
    fn external_refuses_until_provided() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());
        let e = store
            .load_or_mint("api", "stripe_key", true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("ply secret set api.stripe_key"), "{e}");
        store.set("api", "stripe_key", "sk_live_x").unwrap();
        assert_eq!(
            store.load_or_mint("api", "stripe_key", true).unwrap(),
            "sk_live_x"
        );
    }

    #[test]
    fn multiple_params_of_same_member_dont_collide() {
        let td = tempfile::tempdir().unwrap();
        let store = SecretStore::for_stack(td.path());

        // Set two different params for the same member
        store.set("db", "password", "secret_pwd").unwrap();
        store.set("db", "username", "secret_user").unwrap();

        // Both files should exist with their own values
        assert_eq!(
            store.get("db", "password").unwrap(),
            Some("secret_pwd".to_string())
        );
        assert_eq!(
            store.get("db", "username").unwrap(),
            Some("secret_user".to_string())
        );

        // Both files should exist in the directory
        let list = store.list().unwrap();
        assert!(
            list.contains(&"db.password".to_string()),
            "db.password missing: {list:?}"
        );
        assert!(
            list.contains(&"db.username".to_string()),
            "db.username missing: {list:?}"
        );

        // Verify no .tmp files are in the list
        for name in &list {
            assert!(!name.ends_with(".tmp"), "tmp file leaked: {name}");
        }
    }
}
