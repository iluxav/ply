//! Sealed values: secrets that live in a manifest, a deployment file or a
//! stack file as ciphertext, and become plaintext only in the run parent's
//! memory, at launch, on their way into the child's environment.
//!
//! # The shape
//!
//! A host has a keypair. Its public half is a short string anyone can be
//! given; its secret half is a 0600 file that never leaves the host. A
//! value sealed for that public key — on a laptop, in CI, anywhere — is
//! `enc:v1:…`, and can sit in a public git repo. The run parent on that
//! host, and only it, opens the value while composing the app's
//! environment. Nothing writes the plaintext anywhere on Linux; the
//! microVM backend hands env to the guest through a 0600 spec disk, which
//! the docs say plainly.
//!
//! This is Kamal's shape (secrets pulled in at deploy time) and SOPS's
//! (encrypted values in the repo), chosen because developers already trust
//! both. It is not Vault: no leases, no rotation, no identity-based
//! access. For those, run Vault and inject via env.
//!
//! # The construction
//!
//! A sealed box: a fresh X25519 keypair per value, Diffie–Hellman against
//! the host's public key, HKDF-SHA256 over the shared secret (salted with
//! both public keys) into a ChaCha20-Poly1305 key, a random nonce, and the
//! env var's NAME as associated data. The name binding matters: a sealed
//! database password cannot be re-labelled as a variable some app prints,
//! because it only opens under the name it was sealed for. Moving a value
//! to a new name means sealing it again, which is the right cost.
//!
//! Values are never logged, here or by callers: `unseal_env` hands back
//! the NAMES it opened, and that is what goes to stderr and the journal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use ply_vm_proto::{b64_decode, b64_encode};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::{Error, Result};

/// How a sealed value begins. The version is part of it: a later
/// construction gets `enc:v2:` and this code refuses it by name.
pub const PREFIX: &str = "enc:v1:";
/// How a host's public key is spelled.
pub const PUBLIC_PREFIX: &str = "ply-host-";
const SECRET_PREFIX: &str = "ply-host-secret-v1:";
const INFO: &[u8] = b"ply sealed env v1";
const KEY_FILE: &str = "host.key";
const NONCE_LEN: usize = 12;

/// Where this user's host key lives: under the data dir, so root's apps and
/// a rootless user's apps have their own. The key belongs to whoever runs
/// the app.
pub fn key_path() -> PathBuf {
    crate::paths::data_dir().join(KEY_FILE)
}

/// A host's sealing keypair.
pub struct HostKey {
    secret: StaticSecret,
}

impl HostKey {
    pub fn generate() -> Self {
        HostKey {
            secret: StaticSecret::random_from_rng(rand_core::OsRng),
        }
    }

    /// The public half, as the string `ply secret seal --for` takes.
    pub fn public(&self) -> String {
        format!(
            "{PUBLIC_PREFIX}{}",
            b64_encode(PublicKey::from(&self.secret).as_bytes())
        )
    }

    fn public_key(&self) -> PublicKey {
        PublicKey::from(&self.secret)
    }

    /// `None` when there is no key yet.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        let body = text
            .trim()
            .strip_prefix(SECRET_PREFIX)
            .ok_or_else(|| Error::Runtime(format!("{}: not a ply host key", path.display())))?;
        let bytes = b64_decode(body)
            .filter(|b| b.len() == 32)
            .ok_or_else(|| Error::Runtime(format!("{}: not a ply host key", path.display())))?;
        let mut raw = [0u8; 32];
        raw.copy_from_slice(&bytes);
        Ok(Some(HostKey {
            secret: StaticSecret::from(raw),
        }))
    }

    /// Written 0600, whole, then renamed into place.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        let tmp = path.with_extension("key.tmp");
        let io = |source| Error::Io {
            path: tmp.clone(),
            source,
        };
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(io)?;
        file.write_all(
            format!("{SECRET_PREFIX}{}\n", b64_encode(&self.secret.to_bytes())).as_bytes(),
        )
        .map_err(io)?;
        std::fs::rename(&tmp, path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The key at `path`, made on first use. `true` when it was just made.
    pub fn load_or_create(path: &Path) -> Result<(Self, bool)> {
        if let Some(key) = Self::load(path)? {
            return Ok((key, false));
        }
        let key = Self::generate();
        key.save(path)?;
        Ok((key, true))
    }
}

/// A public key as `ply secret hostkey` prints it.
pub fn parse_public(text: &str) -> Result<PublicKey> {
    let body = text.trim().strip_prefix(PUBLIC_PREFIX).ok_or_else(|| {
        Error::Runtime(format!(
            "`{text}` is not a host key — `ply secret hostkey` on the host prints one, starting \
             with `{PUBLIC_PREFIX}`"
        ))
    })?;
    let bytes = b64_decode(body)
        .filter(|b| b.len() == 32)
        .ok_or_else(|| Error::Runtime(format!("`{text}` is not a host key (bad encoding)")))?;
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&bytes);
    Ok(PublicKey::from(raw))
}

pub fn is_sealed(value: &str) -> bool {
    value.starts_with("enc:")
}

fn derive(shared: &[u8], eph_pub: &PublicKey, recipient: &PublicKey) -> Key {
    let mut salt = Vec::with_capacity(64);
    salt.extend_from_slice(eph_pub.as_bytes());
    salt.extend_from_slice(recipient.as_bytes());
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
    let mut okm = [0u8; 32];
    hk.expand(INFO, &mut okm)
        .expect("32 bytes is within HKDF-SHA256's output length");
    *Key::from_slice(&okm)
}

/// Seal `value` under `name` for `recipient`.
pub fn seal(name: &str, value: &str, recipient: &PublicKey) -> String {
    use rand_core::RngCore;
    let eph = StaticSecret::random_from_rng(rand_core::OsRng);
    let eph_pub = PublicKey::from(&eph);
    let shared = eph.diffie_hellman(recipient);
    let key = derive(shared.as_bytes(), &eph_pub, recipient);
    let mut nonce = [0u8; NONCE_LEN];
    rand_core::OsRng.fill_bytes(&mut nonce);
    let ct = ChaCha20Poly1305::new(&key)
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: value.as_bytes(),
                aad: name.as_bytes(),
            },
        )
        .expect("ChaCha20-Poly1305 encryption cannot fail on in-memory input");
    let mut out = Vec::with_capacity(32 + NONCE_LEN + ct.len());
    out.extend_from_slice(eph_pub.as_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    format!("{PREFIX}{}", b64_encode(&out))
}

/// Open a sealed value with this host's key. The error names the variable
/// and never the value.
pub fn unseal(name: &str, sealed: &str, key: &HostKey) -> Result<String> {
    let body = match sealed.strip_prefix(PREFIX) {
        Some(body) => body,
        None if sealed.starts_with("enc:") => {
            return Err(Error::Runtime(format!(
                "{name}: sealed with a format this ply does not know (expected `{PREFIX}…`) — \
                 update ply on this host"
            )))
        }
        None => return Ok(sealed.to_string()),
    };
    let bytes = b64_decode(body)
        .filter(|b| b.len() > 32 + NONCE_LEN)
        .ok_or_else(|| Error::Runtime(format!("{name}: sealed value is malformed")))?;
    let mut eph_raw = [0u8; 32];
    eph_raw.copy_from_slice(&bytes[..32]);
    let eph_pub = PublicKey::from(eph_raw);
    let nonce = &bytes[32..32 + NONCE_LEN];
    let ct = &bytes[32 + NONCE_LEN..];
    let shared = key.secret.diffie_hellman(&eph_pub);
    let derived = derive(shared.as_bytes(), &eph_pub, &key.public_key());
    let plain = ChaCha20Poly1305::new(&derived)
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ct,
                aad: name.as_bytes(),
            },
        )
        .map_err(|_| {
            Error::Runtime(format!(
                "{name}: sealed value does not open with this host's key — it was sealed for \
                 another host, or under a different variable name (a value opens only under the \
                 name it was sealed for)"
            ))
        })?;
    String::from_utf8(plain)
        .map_err(|_| Error::Runtime(format!("{name}: sealed value is not text")))
}

/// Open every sealed value in `env` in place, with the key at `key_path`,
/// and give back the names that were opened. A sealed value with no key on
/// this host is an error before anything launches: an app started with
/// `enc:v1:…` as its database URL is not a useful app.
pub fn unseal_env(env: &mut BTreeMap<String, String>, key_path: &Path) -> Result<Vec<String>> {
    let sealed: Vec<String> = env
        .iter()
        .filter(|(_, v)| is_sealed(v))
        .map(|(k, _)| k.clone())
        .collect();
    if sealed.is_empty() {
        return Ok(sealed);
    }
    let key = HostKey::load(key_path)?.ok_or_else(|| {
        Error::Runtime(format!(
            "{} {} sealed, and this host has no key to open sealed values ({} does not exist). \
             `ply secret hostkey` makes one and prints its public half; seal the values for it",
            sealed.join(", "),
            if sealed.len() == 1 { "is" } else { "are" },
            key_path.display()
        ))
    })?;
    for name in &sealed {
        let opened = unseal(name, &env[name], &key)?;
        env.insert(name.clone(), opened);
    }
    Ok(sealed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_round_trips_and_opens_only_under_its_own_name_and_key() {
        let host = HostKey::generate();
        let recipient = parse_public(&host.public()).unwrap();
        let sealed = seal("DATABASE_URL", "postgres://u:p@h/db", &recipient);
        assert!(sealed.starts_with(PREFIX));
        assert!(is_sealed(&sealed));
        assert_eq!(
            unseal("DATABASE_URL", &sealed, &host).unwrap(),
            "postgres://u:p@h/db"
        );

        // The name is associated data: re-labelling a secret does not open it.
        let err = unseal("GREETING", &sealed, &host).unwrap_err().to_string();
        assert!(err.contains("different variable name"), "{err}");
        assert!(!err.contains("postgres://"), "never the value: {err}");

        // Another host's key does not open it either.
        let other = HostKey::generate();
        assert!(unseal("DATABASE_URL", &sealed, &other).is_err());

        // Two seals of one value differ: fresh ephemeral key and nonce each time.
        assert_ne!(
            sealed,
            seal("DATABASE_URL", "postgres://u:p@h/db", &recipient)
        );
    }

    #[test]
    fn a_key_survives_the_file_and_the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("host.key");
        assert!(HostKey::load(&path).unwrap().is_none(), "nothing yet");
        let (made, created) = HostKey::load_or_create(&path).unwrap();
        assert!(created);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (again, created) = HostKey::load_or_create(&path).unwrap();
        assert!(!created);
        assert_eq!(made.public(), again.public());
        // …and the reloaded key opens what the original sealed.
        let sealed = seal("K", "v", &parse_public(&made.public()).unwrap());
        assert_eq!(unseal("K", &sealed, &again).unwrap(), "v");
    }

    #[test]
    fn unseal_env_opens_in_place_names_what_it_opened_and_refuses_without_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.key");
        let (host, _) = HostKey::load_or_create(&path).unwrap();
        let recipient = parse_public(&host.public()).unwrap();
        let mut env: BTreeMap<String, String> = [
            ("PLAIN".to_string(), "x".to_string()),
            ("SECRET".to_string(), seal("SECRET", "s3", &recipient)),
        ]
        .into();
        let opened = unseal_env(&mut env, &path).unwrap();
        assert_eq!(opened, vec!["SECRET".to_string()]);
        assert_eq!(env["SECRET"], "s3");
        assert_eq!(env["PLAIN"], "x");

        // Nothing sealed: nothing to say, no key needed.
        let mut plain: BTreeMap<String, String> = [("A".to_string(), "b".to_string())].into();
        assert!(unseal_env(&mut plain, Path::new("/nonexistent"))
            .unwrap()
            .is_empty());

        // Sealed but keyless: an error that names the variable and the fix.
        let mut env: BTreeMap<String, String> =
            [("SECRET".to_string(), seal("SECRET", "s3", &recipient))].into();
        let err = unseal_env(&mut env, Path::new("/nonexistent/host.key"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("SECRET is sealed") && err.contains("ply secret hostkey"),
            "{err}"
        );
    }

    #[test]
    fn a_public_key_string_is_checked_before_it_is_used() {
        assert!(parse_public("age1abc").is_err());
        assert!(parse_public("ply-host-notbase64!!").is_err());
        assert!(parse_public(&HostKey::generate().public()).is_ok());
        // A future format is refused by name, not misread.
        let host = HostKey::generate();
        let err = unseal("K", "enc:v2:xyz", &host).unwrap_err().to_string();
        assert!(err.contains("update ply"), "{err}");
    }
}
