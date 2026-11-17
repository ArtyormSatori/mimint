//! Backend registry: named backends, kinds, presets, and construction.
//!
//! A backend is one named entry: a kind, a model (always required, never
//! defaulted), an optional base-URL override, a key source, and timeouts.
//! The registry resolves presets, reads keys only through the vault,
//! enforces the P0-11 cost-cap lock for paid backends, and builds the
//! providers behind the common [`Provider`] trait.
//!
//! Kinds:
//! - `openai` / `openai-compatible`: generic OpenAI-compatible endpoint;
//!   `base_url` is required (no magic default).
//! - `custom`: same wire protocol as `openai`, an explicit user endpoint.
//! - `claude` / `anthropic`: Anthropic Messages API; defaults to
//!   `https://api.anthropic.com`, overridable.
//! - `ollama`: OpenAI-compatible, preset `http://127.0.0.1:11434/v1`.
//! - `lmstudio` / `lm-studio`: OpenAI-compatible, preset
//!   `http://127.0.0.1:1234/v1`.
//!
//! Paid vs free/local (P0-11): a backend is paid when its resolved
//! endpoint host is not loopback. Paid backends are LOCKED unless the
//! configuration opts in with `[cost] cap_usd > 0`: a missing cap locks
//! paid backends exactly like a zero cap. The lock is decided before any
//! secret is read. Spend metering (token usage to dollars) is not
//! implemented; the cap is an opt-in gate, not a meter — see the Phase 2
//! report and `docs/privacy-and-cost.md`. The field keeps the name
//! `cap_usd` for schema compatibility; a rename to an explicit
//! allow-paid flag is planned for a later version.

use crate::claude::{ClaudeClient, ANTHROPIC_BASE_URL, DEFAULT_MAX_TOKENS};
use crate::egress::{url_is_loopback, EgressPolicy, GuardedHttpClient};
use crate::error::MimintError;
use crate::provider::{Provider, ProviderClient};
use crate::vault::{EnvVault, FileVault, Vault};
use std::path::PathBuf;
use std::time::Duration;
use zeroize::Zeroizing;

/// Default per-request timeout for a backend (seconds).
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Ollama preset base URL (loopback).
pub const OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434/v1";

/// LM Studio preset base URL (loopback).
pub const LM_STUDIO_BASE_URL: &str = "http://127.0.0.1:1234/v1";

/// Fallback environment variable for a backend key when the backend sets
/// neither `key_env` nor `key_file` (Phase 1 compatibility, documented).
pub const DEFAULT_KEY_ENV: &str = "MIMINT_API_KEY";

/// Backend kinds the registry knows how to build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Generic OpenAI-compatible endpoint; `base_url` required.
    OpenAi,
    /// Anthropic Messages API.
    Claude,
    /// Ollama preset (loopback, OpenAI-compatible).
    Ollama,
    /// LM Studio preset (loopback, OpenAI-compatible).
    LmStudio,
    /// Explicit user endpoint, OpenAI-compatible wire protocol.
    Custom,
}

impl BackendKind {
    /// Parse a kind name from configuration. The error lists every valid
    /// spelling so a typo is diagnosable at a glance.
    pub fn parse(text: &str) -> Result<Self, MimintError> {
        match text.trim().to_ascii_lowercase().as_str() {
            "openai" | "openai-compatible" | "openai_compatible" => Ok(Self::OpenAi),
            "claude" | "anthropic" => Ok(Self::Claude),
            "ollama" => Ok(Self::Ollama),
            "lmstudio" | "lm-studio" | "lm_studio" => Ok(Self::LmStudio),
            "custom" => Ok(Self::Custom),
            other => Err(MimintError::Config(format!(
                "unknown backend kind '{other}': expected one of \
                 openai, claude, ollama, lmstudio, custom"
            ))),
        }
    }

    /// Short label used in diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Claude => "claude",
            Self::Ollama => "ollama",
            Self::LmStudio => "lmstudio",
            Self::Custom => "custom",
        }
    }

    /// Default base URL, when the kind has one. `None` means the backend
    /// must set `base_url` explicitly.
    pub fn default_base_url(&self) -> Option<&'static str> {
        match self {
            Self::OpenAi | Self::Custom => None,
            Self::Claude => Some(ANTHROPIC_BASE_URL),
            Self::Ollama => Some(OLLAMA_BASE_URL),
            Self::LmStudio => Some(LM_STUDIO_BASE_URL),
        }
    }

    /// True for the OpenAI-compatible wire protocol (everything but Claude).
    pub fn is_openai_compatible(&self) -> bool {
        !matches!(self, Self::Claude)
    }
}

/// Cost cap from configuration (P0-11). `None` means no cap was
/// configured, which LOCKS paid backends (opt-in required); `Some(0.0)`
/// locks them too. Only `Some(cap)` with `cap > 0` allows paid backends,
/// and the value is an opt-in gate, not a spending meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostCap {
    /// Monthly cap in USD, when configured. Must be >= 0.
    pub cap_usd: Option<f64>,
}

impl CostCap {
    /// No cap configured: paid backends are locked until the user opts
    /// in with `[cost] cap_usd > 0`.
    pub fn none() -> Self {
        Self { cap_usd: None }
    }

    /// Validate the cap value; negative, NaN, and infinite caps are
    /// configuration errors.
    pub fn new(cap_usd: Option<f64>) -> Result<Self, MimintError> {
        if let Some(cap) = cap_usd {
            if !cap.is_finite() || cap < 0.0 {
                return Err(MimintError::Config(format!(
                    "cost cap must be a finite number >= 0, got {cap}"
                )));
            }
        }
        Ok(Self { cap_usd })
    }

    /// True only when the cap is configured and greater than 0. A
    /// missing cap locks paid backends: paying for a provider must be
    /// an explicit choice, matching the local-first default.
    pub fn allows_paid(&self) -> bool {
        self.cap_usd.map(|cap| cap > 0.0).unwrap_or(false)
    }
}

impl Default for CostCap {
    fn default() -> Self {
        Self::none()
    }
}

/// One validated backend specification, ready to build.
#[derive(Debug, Clone)]
pub struct BackendSpec {
    /// Unique backend name, used in the fallback chain and audit records.
    pub name: String,
    /// Backend kind.
    pub kind: BackendKind,
    /// Model name. Always required; never defaulted.
    pub model: String,
    /// Explicit base URL override. When `None`, the kind preset applies
    /// (kinds without a preset fail validation instead).
    pub base_url: Option<String>,
    /// Environment variable holding the key (mutually exclusive with
    /// `key_file`; when neither is set, `MIMINT_API_KEY` is tried).
    pub key_env: Option<String>,
    /// Key file holding the key (mutually exclusive with `key_env`).
    pub key_file: Option<PathBuf>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Claude-only `max_tokens`. Must be `None` for other kinds.
    pub max_tokens: Option<u32>,
    /// Explicit opt-in to send this backend's key over plaintext http
    /// to a loopback endpoint. Default false: without it, a keyed
    /// plaintext-loopback backend fails at construction (confused
    /// deputy). Only meaningful for loopback `http://` endpoints.
    pub allow_plaintext_auth: bool,
}

impl BackendSpec {
    /// Validate the spec's own fields (name, model, key-source
    /// exclusivity, Claude-only options). Endpoint and key resolution
    /// happen in [`build_provider`].
    pub fn validate(&self) -> Result<(), MimintError> {
        if self.name.trim().is_empty() {
            return Err(MimintError::Config(
                "backend name must not be empty".to_string(),
            ));
        }
        if self.model.trim().is_empty() {
            return Err(MimintError::Config(format!(
                "backend '{}': model must not be empty (no default model)",
                self.name
            )));
        }
        if self.key_env.is_some() && self.key_file.is_some() {
            return Err(MimintError::Config(format!(
                "backend '{}': specify only one of key_env and key_file",
                self.name
            )));
        }
        if self.timeout.is_zero() {
            return Err(MimintError::Config(format!(
                "backend '{}': timeout must be at least 1 second",
                self.name
            )));
        }
        if self.kind != BackendKind::Claude && self.max_tokens.is_some() {
            return Err(MimintError::Config(format!(
                "backend '{}': max_tokens only applies to kind=claude",
                self.name
            )));
        }
        if let Some(max_tokens) = self.max_tokens {
            if max_tokens == 0 {
                return Err(MimintError::Config(format!(
                    "backend '{}': max_tokens must be at least 1",
                    self.name
                )));
            }
        }
        Ok(())
    }

    /// Resolve the effective base URL: explicit override, else kind
    /// preset, else a clear error for kinds without a preset.
    pub fn resolve_base_url(&self) -> Result<String, MimintError> {
        if let Some(base_url) = &self.base_url {
            if base_url.trim().is_empty() {
                return Err(MimintError::Config(format!(
                    "backend '{}': base_url must not be empty",
                    self.name
                )));
            }
            return Ok(base_url.trim().to_string());
        }
        if let Some(preset) = self.kind.default_base_url() {
            return Ok(preset.to_string());
        }
        Err(MimintError::Config(format!(
            "backend '{}' (kind={}): base_url is required, e.g. base_url = \"https://api.openai.com/v1\"",
            self.name,
            self.kind.label()
        )))
    }
}

/// Resolve a backend's key through the vault, without ever logging it.
///
/// Order: `key_file` (0600-checked file) > `key_env` (named variable) >
/// `MIMINT_API_KEY` (default variable). Absent everywhere means `None`;
/// whether a key is required is decided by the provider constructor.
///
/// Exception: for LOOPBACK endpoints, the default-variable fallback does
/// NOT apply. A loopback backend only gets a key when the backend
/// explicitly declares `key_env`/`key_file`, so a stray ambient
/// `MIMINT_API_KEY` is never sent to a local port (where any process
/// could be listening). Explicitly declared keys for plaintext loopback
/// endpoints additionally require `allow_plaintext_auth` (enforced by
/// the provider constructors).
fn resolve_key(
    spec: &BackendSpec,
    vault: &dyn Vault,
    loopback: bool,
) -> Result<Option<Zeroizing<String>>, MimintError> {
    if let Some(path) = &spec.key_file {
        let file_vault = FileVault::open(path).map_err(|err| {
            MimintError::Config(format!("backend '{}': {}", spec.name, err.message()))
        })?;
        return file_vault.read_key();
    }
    if let Some(var) = &spec.key_env {
        return vault.get(var);
    }
    if loopback {
        return Ok(None);
    }
    vault.get(DEFAULT_KEY_ENV)
}

/// Build one provider from a spec: validate, resolve the endpoint,
/// enforce the cost-cap lock, resolve the key, construct.
///
/// The egress policy is baked into a fresh [`GuardedHttpClient`] built
/// here with the backend's timeout; the provider keeps only that client.
/// Rebuilding (for example after a config reload) rebuilds the client
/// and re-validates the endpoint.
pub fn build_provider(
    spec: &BackendSpec,
    vault: &dyn Vault,
    policy: EgressPolicy,
    cost_cap: CostCap,
) -> Result<Box<dyn Provider>, MimintError> {
    spec.validate()?;
    let base_url = spec.resolve_base_url()?;
    // Cost-cap lock (P0-11): decided from the resolved endpoint before
    // any secret is read. Loopback endpoints are free/local.
    let loopback = url_is_loopback(&base_url).map_err(|err| {
        MimintError::Config(format!("backend '{}': {}", spec.name, err.message()))
    })?;
    if !loopback && !cost_cap.allows_paid() {
        let reason = match cost_cap.cap_usd {
            None => "no [cost] section is configured (paid backends require explicit opt-in)"
                .to_string(),
            Some(_) => "cost cap is 0".to_string(),
        };
        return Err(MimintError::Config(format!(
            "paid backend '{}' (kind={}, endpoint {base_url}) is locked: {reason}. \
             Set [cost] cap_usd > 0 to allow paid backends, or use a local backend (Ollama/LM Studio). \
             WARNING: cap_usd is an opt-in gate, not spending metering — actual spend is not measured",
            spec.name,
            spec.kind.label()
        )));
    }
    let key = resolve_key(spec, vault, loopback)?;
    let http = GuardedHttpClient::new(policy, spec.timeout)?;
    if spec.kind == BackendKind::Claude {
        let max_tokens = spec.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let client = ClaudeClient::new(
            &spec.name,
            &base_url,
            &spec.model,
            key,
            max_tokens,
            spec.allow_plaintext_auth,
            http,
        )
        .map_err(|err| {
            MimintError::Config(format!("backend '{}': {}", spec.name, err.message()))
        })?;
        Ok(Box::new(client))
    } else {
        let client = ProviderClient::new_named(
            &spec.name,
            &base_url,
            &spec.model,
            key,
            spec.allow_plaintext_auth,
            http,
        )
        .map_err(|err| {
            MimintError::Config(format!("backend '{}': {}", spec.name, err.message()))
        })?;
        Ok(Box::new(client))
    }
}

/// Build an ordered fallback chain from specs. `order` names the specs
/// in chain order; every name must exist, and names must not repeat.
/// An empty `order` is a configuration error.
pub fn build_chain(
    specs: &[BackendSpec],
    order: &[String],
    vault: &dyn Vault,
    policy: EgressPolicy,
    cost_cap: CostCap,
) -> Result<Vec<Box<dyn Provider>>, MimintError> {
    if order.is_empty() {
        return Err(MimintError::Config(
            "fallback chain must not be empty: name at least one backend".to_string(),
        ));
    }
    let mut seen: Vec<&str> = Vec::new();
    let mut providers: Vec<Box<dyn Provider>> = Vec::new();
    for name in order {
        if seen.contains(&name.as_str()) {
            return Err(MimintError::Config(format!(
                "fallback chain repeats backend '{name}'"
            )));
        }
        seen.push(name.as_str());
        let Some(spec) = specs.iter().find(|spec| &spec.name == name) else {
            let defined: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
            return Err(MimintError::Config(format!(
                "fallback chain names unknown backend '{name}'; defined backends: {}",
                defined.join(", ")
            )));
        };
        providers.push(build_provider(spec, vault, policy, cost_cap)?);
    }
    Ok(providers)
}

/// The default vault for key resolution: environment variables.
pub fn default_vault() -> EnvVault {
    EnvVault
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, kind: BackendKind, model: &str) -> BackendSpec {
        BackendSpec {
            name: name.to_string(),
            kind,
            model: model.to_string(),
            base_url: None,
            key_env: None,
            key_file: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_tokens: None,
            allow_plaintext_auth: false,
        }
    }

    #[test]
    fn kind_parsing_accepts_aliases_and_lists_valid() {
        assert_eq!(
            BackendKind::parse("openai-compatible").unwrap(),
            BackendKind::OpenAi
        );
        assert_eq!(
            BackendKind::parse("ANTHROPIC").unwrap(),
            BackendKind::Claude
        );
        assert_eq!(
            BackendKind::parse("lm-studio").unwrap(),
            BackendKind::LmStudio
        );
        let err = BackendKind::parse("gemini").expect_err("unknown kind");
        assert!(err.to_string().contains("openai"), "{err}");
        assert!(err.to_string().contains("claude"), "{err}");
    }

    #[test]
    fn presets_resolve_and_openai_requires_base_url() {
        assert_eq!(
            spec("o", BackendKind::Ollama, "m")
                .resolve_base_url()
                .unwrap(),
            OLLAMA_BASE_URL
        );
        assert_eq!(
            spec("l", BackendKind::LmStudio, "m")
                .resolve_base_url()
                .unwrap(),
            LM_STUDIO_BASE_URL
        );
        assert_eq!(
            spec("c", BackendKind::Claude, "m")
                .resolve_base_url()
                .unwrap(),
            ANTHROPIC_BASE_URL
        );
        assert!(spec("o", BackendKind::OpenAi, "m")
            .resolve_base_url()
            .is_err());
        assert!(spec("x", BackendKind::Custom, "m")
            .resolve_base_url()
            .is_err());
    }

    #[test]
    fn spec_validation_catches_empty_model_key_conflict_and_max_tokens() {
        assert!(spec("x", BackendKind::Ollama, "").validate().is_err());
        let mut both = spec("x", BackendKind::Ollama, "m");
        both.key_env = Some("A".to_string());
        both.key_file = Some(PathBuf::from("/tmp/x.key"));
        assert!(both.validate().is_err());
        let mut wrong = spec("x", BackendKind::Ollama, "m");
        wrong.max_tokens = Some(10);
        assert!(wrong.validate().is_err());
        let mut ok = spec("x", BackendKind::Claude, "m");
        ok.max_tokens = Some(10);
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn cost_cap_zero_locks_paid_and_allows_loopback() {
        let vault = EnvVault;
        let cap_zero = CostCap::new(Some(0.0)).unwrap();
        assert!(!cap_zero.allows_paid());
        // A missing cap locks paid backends too: opt-in is required.
        assert!(!CostCap::none().allows_paid());
        assert!(CostCap::new(Some(5.0)).unwrap().allows_paid());
        assert!(CostCap::new(Some(-1.0)).is_err());
        assert!(CostCap::new(Some(f64::NAN)).is_err());

        let mut remote = spec("cloud", BackendKind::OpenAi, "m");
        remote.base_url = Some("https://example.com/v1".to_string());
        let err = match build_provider(&remote, &vault, EgressPolicy::privacy_off(), cap_zero) {
            Ok(_) => panic!("cap 0 must lock paid backend"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("cost cap is 0"), "{err}");
        assert!(err.to_string().contains("Ollama"), "{err}");
        assert!(err.to_string().contains("not spending metering"), "{err}");

        let local = spec("local", BackendKind::Ollama, "m");
        let provider = build_provider(&local, &vault, EgressPolicy::privacy_on(), cap_zero)
            .expect("cap 0 must allow loopback");
        assert!(!provider.is_paid());
    }

    #[test]
    fn absent_cost_cap_locks_paid_backends() {
        let vault = EnvVault;
        let mut remote = spec("cloud", BackendKind::OpenAi, "m");
        remote.base_url = Some("https://example.com/v1".to_string());
        let err = match build_provider(
            &remote,
            &vault,
            EgressPolicy::privacy_off(),
            CostCap::none(),
        ) {
            Ok(_) => panic!("absent cap must lock paid backend"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("explicit opt-in"), "{err}");
        assert!(err.to_string().contains("cap_usd"), "{err}");

        // Opting in with cap > 0 unlocks (construction then fails only
        // on the missing key, proving the lock itself passed).
        let err = match build_provider(
            &remote,
            &vault,
            EgressPolicy::privacy_off(),
            CostCap::new(Some(5.0)).unwrap(),
        ) {
            Ok(_) => panic!("missing key must still fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("API key"), "{err}");
    }

    #[test]
    fn loopback_backend_with_explicit_key_needs_plaintext_opt_in() {
        let vault = EnvVault;
        let var = "MIMINT_TEST_REGISTRY_PLAINTEXT_KEY";
        std::env::set_var(var, "explicit-key");
        let mut local = spec("local", BackendKind::Ollama, "m");
        local.key_env = Some(var.to_string());
        let err = match build_provider(&local, &vault, EgressPolicy::privacy_on(), CostCap::none())
        {
            Ok(_) => panic!("explicit key over plaintext loopback needs opt-in"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("allow_plaintext_auth"), "{err}");
        local.allow_plaintext_auth = true;
        let provider = build_provider(&local, &vault, EgressPolicy::privacy_on(), CostCap::none())
            .expect("opt-in builds");
        assert!(!provider.is_paid());
        std::env::remove_var(var);
    }

    #[test]
    fn chain_build_rejects_unknown_empty_and_repeated_names() {
        let vault = EnvVault;
        let specs = vec![spec("a", BackendKind::Ollama, "m")];
        let policy = EgressPolicy::privacy_on();
        assert!(build_chain(&specs, &[], &vault, policy, CostCap::none()).is_err());
        assert!(build_chain(
            &specs,
            &["nope".to_string()],
            &vault,
            policy,
            CostCap::none()
        )
        .is_err());
        assert!(build_chain(
            &specs,
            &["a".to_string(), "a".to_string()],
            &vault,
            policy,
            CostCap::none()
        )
        .is_err());
        let chain = build_chain(&specs, &["a".to_string()], &vault, policy, CostCap::none())
            .expect("single-backend chain");
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].name(), "a");
    }
}
