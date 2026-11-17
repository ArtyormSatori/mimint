//! macOS Keychain vault: API keys stored in the user's login
//! keychain, read through the Security framework.
//!
//! The vault is read-only: keys are stored by the user (Keychain
//! Access, or `security add-generic-password -s ai.mimint -a <name>
//! -w` on the command line), never written by Mimint. `name` is the
//! Keychain account; every item lives under the single service
//! [`KEYCHAIN_SERVICE`].
//!
//! On any platform other than macOS the vault still exists (so config
//! and registry code compiles everywhere) but every lookup fails
//! closed with a clear message naming the alternatives. The macOS
//! implementation itself has never run on a real Mac at the time
//! Phase 3 closes — the Mac verification package (`mac-verify/`)
//! checks a real read before anyone relies on it.
//!
//! Like every vault, this module never logs a key value. It logs
//! nothing at all.

use crate::error::MimintError;
use crate::vault::Vault;
use zeroize::Zeroizing;

/// Keychain service name for all Mimint secrets.
pub const KEYCHAIN_SERVICE: &str = "ai.mimint";

/// Secrets from the macOS login keychain. `name` is the Keychain
/// account. Absent items read as `Ok(None)`; other Keychain errors
/// fail with the Security framework's message.
#[derive(Debug, Default)]
pub struct KeychainVault;

impl Vault for KeychainVault {
    fn get(&self, name: &str) -> Result<Option<Zeroizing<String>>, MimintError> {
        get_from_keychain(name)
    }

    fn label(&self) -> &'static str {
        "macOS Keychain vault"
    }
}

#[cfg(target_os = "macos")]
fn get_from_keychain(name: &str) -> Result<Option<Zeroizing<String>>, MimintError> {
    /// `errSecItemNotFound` from SecBase.h: no matching item exists.
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, name) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).map_err(|_| {
                MimintError::Config(format!(
                    "Keychain item '{name}' (service '{KEYCHAIN_SERVICE}') is not valid UTF-8"
                ))
            })?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(Zeroizing::new(trimmed.to_string())))
            }
        }
        Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(err) => Err(MimintError::Config(format!(
            "cannot read Keychain item '{name}' (service '{KEYCHAIN_SERVICE}'): {err}"
        ))),
    }
}

#[cfg(not(target_os = "macos"))]
fn get_from_keychain(_name: &str) -> Result<Option<Zeroizing<String>>, MimintError> {
    Err(MimintError::Config(
        "macOS Keychain is not available on this platform; \
         use key_env or key_file instead"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_names_the_keychain() {
        assert_eq!(KeychainVault.label(), "macOS Keychain vault");
        assert_eq!(KEYCHAIN_SERVICE, "ai.mimint");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_lookup_fails_closed_with_alternatives() {
        let err = KeychainVault
            .get("anything")
            .expect_err("Keychain must not silently succeed off macOS");
        let message = err.to_string();
        assert!(message.contains("not available"), "{message}");
        assert!(message.contains("key_env"), "{message}");
        assert!(message.contains("key_file"), "{message}");
    }

    // Read-only probe against the real login keychain: a random
    // account name cannot exist, so the lookup must report absence
    // rather than error. Runs only on macOS, only in the Mac
    // verification package's `cargo test` run.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_absent_item_reads_as_none() {
        let account = format!(
            "mimint-absent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        assert!(KeychainVault.get(&account).unwrap().is_none());
    }
}
