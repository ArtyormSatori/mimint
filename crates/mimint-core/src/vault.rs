//! Vault: where API keys come from, and only from there.
//!
//! - [`EnvVault`]: keys from environment variables (Phase 1 discipline,
//!   kept). A key is never written to a log or file by this module.
//! - [`FileVault`]: one key in one file, readable only by its owner.
//!   The file is created with mode `0600` from the first write, and its
//!   permissions are checked on every read; a file readable by group or
//!   others is refused.
//!
//! Limits, stated plainly: a file vault protects a key from other local
//! users, not from malware running as the same user. Per-platform OS
//! keyring integration (macOS Keychain, Windows Credential Manager,
//! Linux Secret Service) is body work for Phase 3/4. Keys are held as
//! plain `String`s in memory in Phase 2; no zeroizing wrapper yet.
//!
//! This module never logs a key value. It logs nothing at all.

use crate::error::MimintError;
use std::env;
use std::path::{Path, PathBuf};

/// A source of secrets. Implementations must never log a value.
pub trait Vault: Send + Sync {
    /// Look up a secret by name. `Ok(None)` means absent (not an error).
    /// What `name` means depends on the vault; see each implementation.
    fn get(&self, name: &str) -> Result<Option<String>, MimintError>;

    /// Short label for diagnostics (never includes a value).
    fn label(&self) -> &'static str {
        "vault"
    }
}

/// Secrets from environment variables: `name` is the variable name.
/// Empty values count as absent.
#[derive(Debug)]
pub struct EnvVault;

impl Vault for EnvVault {
    fn get(&self, name: &str) -> Result<Option<String>, MimintError> {
        match env::var_os(name) {
            None => Ok(None),
            Some(value) => match value.into_string() {
                Ok(text) if !text.trim().is_empty() => Ok(Some(text)),
                Ok(_) => Ok(None),
                Err(_) => Err(MimintError::Config(format!(
                    "environment variable {name} is not valid UTF-8"
                ))),
            },
        }
    }

    fn label(&self) -> &'static str {
        "environment vault"
    }
}

/// One key in one file. The file holds the raw key (surrounding
/// whitespace trimmed on read). Permissions are checked on every read.
/// `Debug` is safe: only the path is stored, never the key.
#[derive(Debug)]
pub struct FileVault {
    path: PathBuf,
}

impl FileVault {
    /// Open an existing key file. Fails when the file is missing or its
    /// permissions are too open. Permissions are re-checked on each read.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MimintError> {
        let vault = Self {
            path: path.as_ref().to_path_buf(),
        };
        vault.check_permissions()?;
        Ok(vault)
    }

    /// Create a new key file with mode `0600` from the first write.
    /// Fails when the file already exists: existing key files are never
    /// overwritten through this API.
    pub fn create(path: impl AsRef<Path>, contents: &str) -> Result<Self, MimintError> {
        let path = path.as_ref().to_path_buf();
        create_key_file(&path, contents)?;
        Ok(Self { path })
    }

    /// Path of the key file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the key, checking permissions first. `Ok(None)` when the file
    /// holds only whitespace.
    pub fn read_key(&self) -> Result<Option<String>, MimintError> {
        self.check_permissions()?;
        let text = std::fs::read_to_string(&self.path).map_err(|err| {
            MimintError::Config(format!(
                "cannot read key file {}: {err}",
                self.path.display()
            ))
        })?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    /// Refuse when the file is missing or readable beyond its owner.
    fn check_permissions(&self) -> Result<(), MimintError> {
        check_key_file_permissions(&self.path)
    }
}

impl Vault for FileVault {
    /// A file vault holds exactly one secret, so `name` is ignored and
    /// the file's key is returned. Callers that need per-name lookup
    /// use one `FileVault` per key file.
    fn get(&self, _name: &str) -> Result<Option<String>, MimintError> {
        self.read_key()
    }

    fn label(&self) -> &'static str {
        "file vault"
    }
}

#[cfg(unix)]
fn check_key_file_permissions(path: &Path) -> Result<(), MimintError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).map_err(|err| {
        MimintError::Config(format!("cannot access key file {}: {err}", path.display()))
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(MimintError::Config(format!(
            "key file {} has permissions 0o{:o}: too open (readable beyond its owner); \
             fix with: chmod 600 {}",
            path.display(),
            mode,
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_key_file_permissions(path: &Path) -> Result<(), MimintError> {
    let _ = path;
    Err(MimintError::Config(
        "file vault permission checks require Unix; refusing to read a key file on this platform"
            .to_string(),
    ))
}

#[cfg(unix)]
fn create_key_file(path: &Path, contents: &str) -> Result<(), MimintError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| {
            MimintError::Config(format!("cannot create key file {}: {err}", path.display()))
        })?;
    file.write_all(contents.as_bytes()).map_err(|err| {
        MimintError::Config(format!("cannot write key file {}: {err}", path.display()))
    })?;
    file.sync_all().map_err(|err| {
        MimintError::Config(format!("cannot sync key file {}: {err}", path.display()))
    })?;
    Ok(())
}

#[cfg(not(unix))]
fn create_key_file(path: &Path, _contents: &str) -> Result<(), MimintError> {
    let _ = path;
    Err(MimintError::Config(
        "file vault creation requires Unix file modes; unsupported on this platform".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "mimint-vault-{}-{nanos}-{tag}.key",
            std::process::id()
        ))
    }

    #[test]
    fn env_vault_reads_and_treats_empty_as_absent() {
        let name = format!(
            "MIMINT_TEST_VAULT_{}_{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let vault = EnvVault;
        // The variable name is unique to this test.
        env::set_var(&name, "test-key-value");
        assert_eq!(vault.get(&name).unwrap().as_deref(), Some("test-key-value"));
        env::set_var(&name, "   ");
        assert_eq!(vault.get(&name).unwrap(), None);
        env::remove_var(&name);
        assert_eq!(vault.get(&name).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn file_vault_create_is_0600_and_reads_trimmed() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("create");
        let vault = FileVault::create(&path, "  secret-key\n").expect("create");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file must be created 0600");
        assert_eq!(vault.read_key().unwrap().as_deref(), Some("secret-key"));
        assert_eq!(vault.get("ignored").unwrap().as_deref(), Some("secret-key"));
        // Never overwrite an existing key file.
        assert!(FileVault::create(&path, "other").is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn file_vault_refuses_too_open_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("open");
        let vault = FileVault::create(&path, "secret").expect("create");
        for mode in [0o644, 0o640, 0o604, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let err = vault.read_key().expect_err("too-open must fail");
            assert!(err.to_string().contains("too open"), "{err}");
            assert!(FileVault::open(&path).is_err());
        }
        // Stricter than 0600 is fine.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(vault.read_key().unwrap().as_deref(), Some("secret"));
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn file_vault_missing_file_errors_clearly() {
        let path = temp_path("missing");
        let err = FileVault::open(&path).expect_err("missing must fail");
        assert!(err.to_string().contains("cannot access key file"), "{err}");
    }
}
