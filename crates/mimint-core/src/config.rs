//! Configuration file (TOML): backends, Privacy Mode, cost cap,
//! fallback chain, retry policy.
//!
//! Location: `$XDG_CONFIG_HOME/mimint/config.toml`, else
//! `$HOME/.config/mimint/config.toml` (Linux/macOS), else
//! `%USERPROFILE%/.config/mimint/config.toml` (Windows fallback).
//! `--config` overrides the path; `MIMINT_CONFIG` is NOT read (the path
//! is explicit or default, never ambient).
//!
//! Example (top-level keys come before any `[section]` header):
//! ```toml
//! default_backend = "local"
//!
//! [privacy]
//! enabled = true          # default when absent: true
//!
//! [cost]
//! cap_usd = 0.0           # 0 locks paid backends; absent locks too (opt-in)
//!
//! [[backend]]
//! name = "local"
//! kind = "ollama"
//! model = "qwen2.5:3b"
//!
//! [[backend]]
//! name = "cloud"
//! kind = "claude"
//! model = "claude-sonnet-4-5"
//! key_env = "ANTHROPIC_API_KEY"
//! timeout_secs = 60
//!
//! [fallback]
//! chain = ["local", "cloud"]
//! max_attempts = 3
//! max_elapsed_secs = 120
//! ```
//!
//! Rules: unknown fields are rejected (typos fail loudly); a backend's
//! model is always required; keys are NEVER written in this file —
//! `key_env` / `key_file` / `key_keychain` name where the key lives,
//! and a literal `api_key` field is rejected as an unknown field.

use crate::error::MimintError;
use crate::fallback::RetryPolicy;
use crate::registry::{BackendKind, BackendSpec, CostCap, DEFAULT_TIMEOUT_SECS};
use serde::Deserialize;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default config file location. `None` when no home directory can be
/// found; the CLI then treats configuration as absent.
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("mimint").join("config.toml"));
        }
    }
    if let Some(home) = env::var_os("HOME") {
        if !home.is_empty() {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("mimint")
                    .join("config.toml"),
            );
        }
    }
    if let Some(profile) = env::var_os("USERPROFILE") {
        if !profile.is_empty() {
            return Some(
                PathBuf::from(profile)
                    .join(".config")
                    .join("mimint")
                    .join("config.toml"),
            );
        }
    }
    None
}

/// Privacy section. Absent section means Privacy Mode ON.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacySection {
    /// Privacy Mode switch. Default: true.
    #[serde(default = "default_privacy_enabled")]
    pub enabled: bool,
}

fn default_privacy_enabled() -> bool {
    true
}

impl Default for PrivacySection {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Cost section (P0-11). Absent section means paid backends are locked
/// (opt-in required); only `cap_usd > 0` allows them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostSection {
    /// Monthly cap in USD. 0 locks paid backends. Absent locks too.
    /// The value is an opt-in gate, not spending metering.
    pub cap_usd: Option<f64>,
}

/// Fallback + retry section. All fields optional; defaults are the
/// [`RetryPolicy`] defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackSection {
    /// Ordered backend names. When absent, `[default_backend]` is used.
    pub chain: Option<Vec<String>>,
    /// Attempts per provider. Default: [`RetryPolicy::DEFAULT_MAX_ATTEMPTS`].
    pub max_attempts: Option<usize>,
    /// Per-call budget in seconds (fresh per `chat()` call). Default: [`RetryPolicy::DEFAULT_MAX_ELAPSED_SECS`].
    pub max_elapsed_secs: Option<u64>,
    /// First backoff delay in milliseconds. Default: [`RetryPolicy::DEFAULT_BASE_DELAY_MS`].
    pub base_delay_ms: Option<u64>,
    /// Backoff cap in milliseconds. Default: [`RetryPolicy::DEFAULT_MAX_DELAY_MS`].
    pub max_delay_ms: Option<u64>,
}

/// One `[[backend]]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendEntry {
    /// Unique backend name.
    pub name: String,
    /// Kind name; parsed by [`BackendKind::parse`].
    pub kind: String,
    /// Model name. Required (serde) and non-empty (validation).
    pub model: String,
    /// Base URL override; kind preset when absent.
    pub base_url: Option<String>,
    /// Environment variable holding the key.
    pub key_env: Option<String>,
    /// Key file path (0600-checked).
    pub key_file: Option<String>,
    /// macOS Keychain account holding the key (service `ai.mimint`).
    /// Fails closed on other platforms.
    pub key_keychain: Option<String>,
    /// Per-request timeout in seconds. Default: 30.
    pub timeout_secs: Option<u64>,
    /// Claude-only `max_tokens`. Default: 4096 (see [`crate::claude`]).
    pub max_tokens: Option<u32>,
    /// Opt-in to send this backend's key over plaintext http to a
    /// loopback endpoint. Default: false.
    pub allow_plaintext_auth: Option<bool>,
}

/// Whole config file as parsed from TOML.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Privacy section; default ON when absent.
    #[serde(default)]
    pub privacy: PrivacySection,
    /// Cost section; paid locked when absent.
    #[serde(default)]
    pub cost: CostSection,
    /// Backend used when no fallback chain is set.
    pub default_backend: Option<String>,
    /// Backend entries (`[[backend]]` tables).
    #[serde(default)]
    pub backend: Vec<BackendEntry>,
    /// Fallback + retry section.
    #[serde(default)]
    pub fallback: FallbackSection,
}

/// A validated, resolved configuration ready for the registry.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// Effective Privacy Mode from the file (CLI flags override later).
    pub privacy_enabled: bool,
    /// Effective cost cap.
    pub cost_cap: CostCap,
    /// Validated backend specs, in file order.
    pub backends: Vec<BackendSpec>,
    /// Ordered fallback chain (backend names). Empty when no backend is
    /// configured at all; the CLI then runs onboarding.
    pub chain: Vec<String>,
    /// Retry policy for the chain.
    pub retry: RetryPolicy,
}

impl FileConfig {
    /// An empty configuration: Privacy ON, paid locked, no backends.
    /// Used when no config file exists.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse TOML text. Errors name the problem; the caller adds the path.
    pub fn parse(text: &str) -> Result<Self, MimintError> {
        toml::from_str(text)
            .map_err(|err| MimintError::Config(format!("invalid config TOML: {err}")))
    }

    /// Validate and resolve into a [`ResolvedConfig`]. Every error names
    /// the backend or field at fault.
    pub fn validate(self) -> Result<ResolvedConfig, MimintError> {
        let cost_cap = CostCap::new(self.cost.cap_usd)?;
        let mut backends: Vec<BackendSpec> = Vec::new();
        for entry in &self.backend {
            let kind = BackendKind::parse(&entry.kind).map_err(|err| {
                MimintError::Config(format!("backend '{}': {}", entry.name, err.message()))
            })?;
            let spec = BackendSpec {
                name: entry.name.clone(),
                kind,
                model: entry.model.clone(),
                base_url: entry.base_url.clone(),
                key_env: entry.key_env.clone(),
                key_file: entry.key_file.clone().map(PathBuf::from),
                key_keychain: entry.key_keychain.clone(),
                timeout: Duration::from_secs(entry.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
                max_tokens: entry.max_tokens,
                allow_plaintext_auth: entry.allow_plaintext_auth.unwrap_or(false),
            };
            spec.validate()?;
            if backends.iter().any(|other| other.name == spec.name) {
                return Err(MimintError::Config(format!(
                    "duplicate backend name '{}' in config (backend names must be unique)",
                    spec.name
                )));
            }
            // Resolve the base URL now so a missing required URL fails
            // at config load, not mid-turn.
            spec.resolve_base_url()?;
            backends.push(spec);
        }

        let retry = RetryPolicy {
            max_attempts: self
                .fallback
                .max_attempts
                .unwrap_or(RetryPolicy::DEFAULT_MAX_ATTEMPTS),
            max_elapsed: Duration::from_secs(
                self.fallback
                    .max_elapsed_secs
                    .unwrap_or(RetryPolicy::DEFAULT_MAX_ELAPSED_SECS),
            ),
            base_delay: Duration::from_millis(
                self.fallback
                    .base_delay_ms
                    .unwrap_or(RetryPolicy::DEFAULT_BASE_DELAY_MS),
            ),
            max_delay: Duration::from_millis(
                self.fallback
                    .max_delay_ms
                    .unwrap_or(RetryPolicy::DEFAULT_MAX_DELAY_MS),
            ),
        };
        retry.validate()?;

        let defined: Vec<&str> = backends.iter().map(|spec| spec.name.as_str()).collect();
        let chain = match (&self.fallback.chain, &self.default_backend) {
            (Some(chain), default) => {
                if chain.is_empty() {
                    return Err(MimintError::Config(
                        "fallback.chain must not be empty: name at least one backend".to_string(),
                    ));
                }
                for name in chain {
                    if !defined.contains(&name.as_str()) {
                        return Err(MimintError::Config(format!(
                            "fallback.chain names unknown backend '{name}'; defined backends: {}",
                            defined.join(", ")
                        )));
                    }
                }
                if let Some(default) = default {
                    if chain.first().map(String::as_str) != Some(default.as_str()) {
                        return Err(MimintError::Config(format!(
                            "default_backend '{default}' must be the first entry of fallback.chain \
                             (chain starts with '{}')",
                            chain[0]
                        )));
                    }
                }
                chain.clone()
            }
            (None, Some(default)) => {
                if !defined.contains(&default.as_str()) {
                    return Err(MimintError::Config(format!(
                        "default_backend '{default}' is not a defined backend; defined backends: {}",
                        defined.join(", ")
                    )));
                }
                vec![default.clone()]
            }
            (None, None) => {
                if backends.is_empty() {
                    Vec::new()
                } else {
                    return Err(MimintError::Config(format!(
                        "no backend selected: set default_backend or fallback.chain \
                         (defined backends: {})",
                        defined.join(", ")
                    )));
                }
            }
        };

        Ok(ResolvedConfig {
            privacy_enabled: self.privacy.enabled,
            cost_cap,
            backends,
            chain,
            retry,
        })
    }
}

/// Load a config file. When `required` is false, a missing file means an
/// empty configuration; when true, a missing file is an error. Parse and
/// validation errors always fail, and name the file.
pub fn load_config_file(path: &Path, required: bool) -> Result<FileConfig, MimintError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(FileConfig::empty());
        }
        Err(err) => {
            return Err(MimintError::Config(format!(
                "cannot read config file {}: {err}",
                path.display()
            )));
        }
    };
    FileConfig::parse(&text)
        .map_err(|err| MimintError::Config(format!("{}: {}", path.display(), err.message())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
default_backend = "local"

[privacy]
enabled = true

[cost]
cap_usd = 0.0

[[backend]]
name = "local"
kind = "ollama"
model = "qwen2.5:3b"

[[backend]]
name = "cloud"
kind = "claude"
model = "claude-sonnet-4-5"
key_env = "ANTHROPIC_API_KEY"
timeout_secs = 60

[fallback]
chain = ["local", "cloud"]
max_attempts = 2
"#;

    #[test]
    fn valid_config_resolves() {
        let resolved = FileConfig::parse(VALID).unwrap().validate().unwrap();
        assert!(resolved.privacy_enabled);
        assert_eq!(resolved.cost_cap.cap_usd, Some(0.0));
        assert_eq!(resolved.backends.len(), 2);
        assert_eq!(
            resolved.chain,
            vec!["local".to_string(), "cloud".to_string()]
        );
        assert_eq!(resolved.retry.max_attempts, 2);
        assert_eq!(resolved.backends[1].timeout, Duration::from_secs(60));
    }

    #[test]
    fn keychain_source_parses_and_resolves() {
        let text = r#"
default_backend = "cloud"

[[backend]]
name = "cloud"
kind = "claude"
model = "claude-sonnet-4-5"
key_keychain = "anthropic"
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert_eq!(
            resolved.backends[0].key_keychain.as_deref(),
            Some("anthropic")
        );
        assert!(resolved.backends[0].key_env.is_none());
        assert!(resolved.backends[0].key_file.is_none());
    }

    #[test]
    fn privacy_defaults_on_and_cap_defaults_absent() {
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert!(resolved.privacy_enabled);
        assert_eq!(resolved.cost_cap.cap_usd, None);
        assert_eq!(resolved.chain, vec!["local".to_string()]);
    }

    #[test]
    fn unknown_fields_rejected_including_literal_api_key() {
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
api_key = "never-allowed-here"
"#;
        let err = FileConfig::parse(text).expect_err("api_key must fail");
        assert!(err.to_string().contains("api_key"), "{err}");
        let text = "[privacy]\nenabled = true\ntypo_field = 1\n";
        assert!(FileConfig::parse(text).is_err());
    }

    #[test]
    fn missing_model_missing_base_url_and_duplicates_fail() {
        let no_model = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
"#;
        assert!(FileConfig::parse(no_model).is_err());
        let no_base = r#"
default_backend = "c"
[[backend]]
name = "c"
kind = "openai"
model = "m"
"#;
        let err = FileConfig::parse(no_base)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("base_url"), "{err}");
        let dup = r#"
default_backend = "a"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
"#;
        let err = FileConfig::parse(dup).unwrap().validate().err().unwrap();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn chain_and_default_selection_errors_name_defined_backends() {
        let unknown_chain = r#"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
[fallback]
chain = ["ghost"]
"#;
        let err = FileConfig::parse(unknown_chain)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("ghost"), "{err}");
        assert!(err.to_string().contains('a'), "{err}");

        let mismatch = r#"
default_backend = "b"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
[[backend]]
name = "b"
kind = "ollama"
model = "m"
[fallback]
chain = ["a", "b"]
"#;
        let err = FileConfig::parse(mismatch)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("first entry"), "{err}");

        let none_selected = r#"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
"#;
        let err = FileConfig::parse(none_selected)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("no backend selected"), "{err}");
    }

    #[test]
    fn invalid_cap_and_retry_values_fail() {
        let bad_cap = "default_backend = \"a\"\n[cost]\ncap_usd = -5.0\n[[backend]]\nname = \"a\"\nkind = \"ollama\"\nmodel = \"m\"\n";
        assert!(FileConfig::parse(bad_cap).unwrap().validate().is_err());
        let bad_retry = "default_backend = \"a\"\n[[backend]]\nname = \"a\"\nkind = \"ollama\"\nmodel = \"m\"\n[fallback]\nmax_attempts = 0\n";
        assert!(FileConfig::parse(bad_retry).unwrap().validate().is_err());
    }

    #[test]
    fn allow_plaintext_auth_parses_and_defaults_false() {
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
key_env = "MIMINT_TEST_KEY"
allow_plaintext_auth = true
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert!(resolved.backends[0].allow_plaintext_auth);
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert!(!resolved.backends[0].allow_plaintext_auth);
    }

    #[test]
    fn empty_config_has_no_backends_and_empty_chain() {
        let resolved = FileConfig::empty().validate().unwrap();
        assert!(resolved.backends.is_empty());
        assert!(resolved.chain.is_empty());
        assert!(resolved.privacy_enabled);
    }
}
