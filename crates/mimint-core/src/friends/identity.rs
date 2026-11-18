//! Device identity: one long-lived ECDSA P-256 keypair per body
//! (Phase 7, §3.2).
//!
//! The private key (PKCS#8 DER) lives in exactly one file in the
//! data dir, mode `0600` from the first write, read through a
//! descriptor that was checked the same way [`crate::vault`]
//! checks key files (`O_NOFOLLOW`, regular file, owner-only
//! permissions on the open descriptor — no check-then-open
//! window). The key is never logged, never synced, never backed
//! up, and never leaves the machine. Losing the file means
//! losing the identity: re-pairing from scratch is the only
//! recovery (see [`crate::friends::texts::KEY_LOSS_WARNING`]).
//!
//! The key is used ONLY for signing (handshake transcripts,
//! ceremony transcripts, handoff packages). ECDH always uses a
//! fresh ephemeral keypair per session. One key, one purpose.
//!
//! OS keyrings are deliberately NOT used in v1: `KeychainVault`
//! is read-only and no write backend exists in the tree
//! (D-DESIGN7-3). File 0600 + descriptor checks match the
//! threat model (malware running as the same user is out of
//! scope, T10).

use crate::error::MimintError;
use crate::friends::crypto::{
    device_id_from_public_key, fingerprint_from_public_key, generate_identity_pkcs8,
    identity_public_key, identity_sign, PUBKEY_LEN,
};
use crate::friends::util::hex_encode;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// File name of the identity key inside the data dir.
pub const IDENTITY_KEY_FILE: &str = "device-identity.key";

/// A body's long-lived identity: the private key (zeroized on
/// drop), the public key, and the identifiers derived from it.
/// `Debug` never prints key material.
pub struct Identity {
    pkcs8_der: Zeroizing<Vec<u8>>,
    public_key: [u8; PUBKEY_LEN],
    device_id: String,
    fingerprint: String,
}

impl Identity {
    /// Build from PKCS#8 DER bytes (already read through a
    /// checked descriptor).
    pub fn from_pkcs8(der: Zeroizing<Vec<u8>>) -> Result<Self, MimintError> {
        let public_key = identity_public_key(&der)?;
        let device_id = device_id_from_public_key(&public_key);
        let fingerprint = fingerprint_from_public_key(&public_key);
        Ok(Self {
            pkcs8_der: der,
            public_key,
            device_id,
            fingerprint,
        })
    }

    /// Generate a fresh identity (not yet persisted).
    pub fn generate() -> Self {
        let der = generate_identity_pkcs8();
        Self::from_pkcs8(der).expect("freshly generated key must parse")
    }

    /// The device id: 32 hex characters.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// The full fingerprint: 64 hex characters. Not a secret.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The uncompressed public key (65 bytes). Not a secret.
    pub fn public_key(&self) -> &[u8; PUBKEY_LEN] {
        &self.public_key
    }

    /// The public key as 130 hex characters.
    pub fn public_key_hex(&self) -> String {
        hex_encode(&self.public_key)
    }

    /// Sign `message` with the identity key (ECDSA P-256,
    /// SHA-256, ASN.1 DER signature).
    pub fn sign(&self, message: &[u8]) -> Result<Vec<u8>, MimintError> {
        identity_sign(&self.pkcs8_der, message)
    }
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// The on-disk store for the identity key: one file, `0600`,
/// descriptor-checked reads, no overwrite through the normal
/// API. Rotation goes through [`Self::rotate`], which replaces
/// the file atomically and invalidates every pairing at once.
#[derive(Debug)]
pub struct DeviceKeyStore {
    path: PathBuf,
}

impl DeviceKeyStore {
    /// The store for `data_dir/device-identity.key`. Nothing is
    /// read or created by this constructor (lazy: a body that
    /// never pairs has no identity).
    pub fn in_data_dir(data_dir: impl AsRef<Path>) -> Self {
        Self {
            path: data_dir.as_ref().join(IDENTITY_KEY_FILE),
        }
    }

    /// The store at an explicit path (tests).
    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// Path of the key file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the key file exists.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Load the identity, failing closed when the file is
    /// missing, a symlink, not a regular file, or too open.
    pub fn load(&self) -> Result<Identity, MimintError> {
        let der = read_checked_key_file(&self.path)?;
        Identity::from_pkcs8(der)
    }

    /// Load the identity, creating it (0600, create-new only)
    /// when no key file exists yet.
    pub fn ensure(&self) -> Result<Identity, MimintError> {
        if self.exists() {
            return self.load();
        }
        let identity = Identity::generate();
        create_key_file_bytes(&self.path, identity.pkcs8_bytes_for_store())?;
        // Re-read through the checked path so the returned
        // identity is exactly what is on disk.
        self.load()
    }

    /// Replace the identity with a fresh keypair ("Đặt lại danh
    /// tính bảo mật của thiết bị này", §3.6). The old key is
    /// rejected immediately: there is no grace period and no
    /// parallel-compatibility phase. The new key is written to
    /// a temporary file (0600) and renamed over the old one, so
    /// a crash never leaves a half-written key file. Callers
    /// must confirm with the user first (see
    /// [`crate::friends::texts::ROTATE_WARNING`]) and audit the
    /// rotation; this function only replaces the file.
    pub fn rotate(&self) -> Result<Identity, MimintError> {
        let identity = Identity::generate();
        let tmp_path = self.path.with_extension("key.new");
        // Remove a stale temp file from an interrupted rotation.
        let _ = std::fs::remove_file(&tmp_path);
        create_key_file_bytes(&tmp_path, identity.pkcs8_bytes_for_store())?;
        std::fs::rename(&tmp_path, &self.path).map_err(|err| {
            MimintError::Config(format!(
                "cannot replace identity key file {}: {err}",
                self.path.display()
            ))
        })?;
        self.load()
    }
}

impl Identity {
    /// The raw DER bytes, for the store's first write only. The
    /// store writes them once and never returns them.
    fn pkcs8_bytes_for_store(&self) -> &[u8] {
        &self.pkcs8_der
    }
}

/// Read a key file through a checked descriptor: open with
/// `O_NOFOLLOW`, verify the descriptor is a regular file with
/// owner-only permissions, then read from that same descriptor.
/// Mirrors `open_checked_file` in [`crate::vault`], generalized
/// to bytes.
#[cfg(unix)]
fn read_checked_key_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, MimintError> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| {
            if err.raw_os_error() == Some(libc::ELOOP) {
                MimintError::Config(format!(
                    "identity key file {} is a symbolic link; refusing to follow symlinks",
                    path.display()
                ))
            } else {
                MimintError::Config(format!(
                    "cannot access identity key file {}: {err}",
                    path.display()
                ))
            }
        })?;
    let metadata = file.metadata().map_err(|err| {
        MimintError::Config(format!(
            "cannot access identity key file {}: {err}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_file() {
        return Err(MimintError::Config(format!(
            "identity key file {} is not a regular file; refusing to read it",
            path.display()
        )));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(MimintError::Config(format!(
            "identity key file {} has permissions 0o{:o}: too open (readable beyond its owner); \
             fix with: chmod 600 {}",
            path.display(),
            mode,
            path.display()
        )));
    }
    let mut der = Vec::new();
    file.read_to_end(&mut der).map_err(|err| {
        MimintError::Config(format!(
            "cannot read identity key file {}: {err}",
            path.display()
        ))
    })?;
    if der.is_empty() {
        return Err(MimintError::Config(format!(
            "identity key file {} is empty",
            path.display()
        )));
    }
    Ok(Zeroizing::new(der))
}

#[cfg(not(unix))]
fn read_checked_key_file(path: &Path) -> Result<Zeroizing<Vec<u8>>, MimintError> {
    let _ = path;
    Err(MimintError::Config(
        "identity key permission checks require Unix; refusing to read a key file on this platform"
            .to_string(),
    ))
}

/// Create a new key file with mode `0600` from the first write.
/// Fails when the file already exists.
#[cfg(unix)]
fn create_key_file_bytes(path: &Path, contents: &[u8]) -> Result<(), MimintError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            MimintError::Config(format!(
                "cannot create directory {}: {err}",
                parent.display()
            ))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| {
            MimintError::Config(format!("cannot create key file {}: {err}", path.display()))
        })?;
    file.write_all(contents).map_err(|err| {
        MimintError::Config(format!("cannot write key file {}: {err}", path.display()))
    })?;
    file.sync_all().map_err(|err| {
        MimintError::Config(format!("cannot sync key file {}: {err}", path.display()))
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn create_key_file_bytes(path: &Path, _contents: &[u8]) -> Result<(), MimintError> {
    let _ = path;
    Err(MimintError::Config(
        "identity key creation requires Unix file modes; unsupported on this platform".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "mimint-identity-{}-{nanos}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// §3.8: the key file is 0600; symlinks and world-readable
    /// files are refused; the key never appears in a sync
    /// payload (locked by `sync_canary_secret_never_in_payload`
    /// in the sync module, which also plants this file's bytes
    /// as a canary).
    #[cfg(unix)]
    #[test]
    fn device_key_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let store = DeviceKeyStore::in_data_dir(&dir);
        assert!(!store.exists());
        let identity = store.ensure().expect("ensure creates the key");
        assert!(store.exists());
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "identity key file must be 0600");
        // Reloading yields the same identity.
        let reloaded = store.load().expect("reload");
        assert_eq!(reloaded.device_id(), identity.device_id());
        assert_eq!(reloaded.fingerprint(), identity.fingerprint());
        // A second ensure() does not overwrite.
        let again = store.ensure().expect("ensure again");
        assert_eq!(again.device_id(), identity.device_id());

        // Too-open permissions are refused.
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load().is_err());
        std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.load().is_ok());

        // A symlink to a good key file is refused.
        let link = dir.join("link.key");
        std::os::unix::fs::symlink(store.path(), &link).unwrap();
        assert!(DeviceKeyStore::at(&link).load().is_err());

        // Rotation changes the identity and keeps 0600.
        let rotated = store.rotate().expect("rotate");
        assert_ne!(rotated.device_id(), identity.device_id());
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(store.load().unwrap().device_id(), rotated.device_id());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_debug_never_shows_key_material() {
        let identity = Identity::generate();
        let debug = format!("{identity:?}");
        assert!(debug.contains(identity.device_id()));
        assert!(!debug.contains(&identity.public_key_hex()));
    }
}
