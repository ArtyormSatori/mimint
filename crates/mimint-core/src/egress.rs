//! Egress kill-switch (P0-12, P0-18): Privacy Mode enforced at the HTTP
//! client layer, for every provider.
//!
//! Design:
//! - `EgressPolicy` is fixed when a `GuardedHttpClient` is built. There is
//!   no setter and no interior mutability: the policy cannot change
//!   without rebuilding the client, and rebuilding re-validates.
//! - Every request goes through `GuardedHttpClient::post_json` / `get`,
//!   which call `validate_url` BEFORE any socket is opened. A denied URL
//!   never reaches DNS or `connect()`.
//! - The underlying reqwest client is built with redirects disabled and
//!   proxies disabled, so a loopback server cannot bounce a request to
//!   an external URL via `Location`, and no proxy environment variable
//!   can reroute traffic.
//! - This module is the ONLY place a reqwest client is constructed in
//!   the workspace (providers never touch reqwest directly). Changing
//!   `base_url` in a config file has no effect until the process
//!   rebuilds its providers through the registry with a fresh client.
//!
//! Privacy Mode defaults to ON (`EgressPolicy::default`). When it is on,
//! only loopback hosts are reachable. Loopback means exactly the three
//! forms `127.0.0.1`, `localhost`, `::1` — nothing else (notably not
//! `0.0.0.0`, not `127.1`, not `*.localhost` lookalikes).

use crate::error::MimintError;
use std::time::Duration;

/// Fixed egress policy baked into a [`GuardedHttpClient`] at build time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EgressPolicy {
    privacy_mode: bool,
}

impl EgressPolicy {
    /// Privacy Mode on: only loopback hosts are reachable.
    pub fn privacy_on() -> Self {
        Self { privacy_mode: true }
    }

    /// Privacy Mode off: https anywhere, plaintext http only for loopback.
    pub fn privacy_off() -> Self {
        Self {
            privacy_mode: false,
        }
    }

    /// Build from an explicit boolean (config file / CLI flag).
    pub fn new(privacy_mode: bool) -> Self {
        Self { privacy_mode }
    }

    /// True when Privacy Mode is on.
    pub fn privacy_mode(&self) -> bool {
        self.privacy_mode
    }
}

impl Default for EgressPolicy {
    /// Privacy Mode defaults to ON (decided 2026-11-15, §7 of the plan).
    fn default() -> Self {
        Self::privacy_on()
    }
}

/// Split `scheme://authority/...` into a lowercase scheme and host (no port).
pub(crate) fn split_endpoint(url: &str) -> Result<(String, String), MimintError> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(MimintError::Config(
            "URL must include a scheme, e.g. https://example.com".to_string(),
        ));
    };
    let scheme = scheme.to_ascii_lowercase();
    let authority = rest.split('/').next().unwrap_or("");
    let authority = authority.split('?').next().unwrap_or(authority);
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(bracketed) = hostport.strip_prefix('[') {
        match bracketed.find(']') {
            Some(end) => &bracketed[..end],
            None => {
                return Err(MimintError::Config(
                    "URL has a malformed bracketed host".to_string(),
                ));
            }
        }
    } else if hostport.matches(':').count() == 1 {
        hostport.split(':').next().unwrap_or(hostport)
    } else {
        hostport
    };
    if host.is_empty() {
        return Err(MimintError::Config("URL has no host".to_string()));
    }
    Ok((scheme, host.to_ascii_lowercase()))
}

/// Loopback hosts allowed for user-specified plaintext HTTP and for all
/// traffic while Privacy Mode is on: exactly the three forms in the
/// brief, nothing else (notably not `0.0.0.0`).
pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// True when the URL's host is a loopback host (see [`is_loopback_host`]).
pub fn url_is_loopback(url: &str) -> Result<bool, MimintError> {
    let (_, host) = split_endpoint(url)?;
    Ok(is_loopback_host(&host))
}

/// Validate one request URL against the policy, before any I/O.
///
/// Order matters: the Privacy Mode check runs first so that, while it is
/// on, every denial names Privacy Mode and the offending host.
pub fn validate_url(url: &str, policy: EgressPolicy) -> Result<(), MimintError> {
    let (scheme, host) = split_endpoint(url)?;
    let loopback = is_loopback_host(&host);
    if policy.privacy_mode() && !loopback {
        return Err(MimintError::Config(format!(
            "Privacy Mode is ON: refusing non-loopback URL host '{host}'. \
             Use a loopback backend (Ollama/LM Studio) or turn Privacy Mode off explicitly (--privacy-mode off)"
        )));
    }
    match scheme.as_str() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        "http" => Err(MimintError::Config(format!(
            "refusing plaintext http endpoint for non-loopback host '{host}': use https or a loopback endpoint"
        ))),
        other => Err(MimintError::Config(format!(
            "unsupported URL scheme '{other}': use https (or http only for loopback)"
        ))),
    }
}

/// The single HTTP client every provider uses. The policy is fixed at
/// construction; requests are validated against it on every call.
#[derive(Debug, Clone)]
pub struct GuardedHttpClient {
    policy: EgressPolicy,
    timeout: Duration,
    client: reqwest::Client,
}

impl GuardedHttpClient {
    /// Build a client with a fixed egress policy and request timeout.
    ///
    /// Redirects and proxies are disabled: a response cannot reroute a
    /// request off-loopback, and proxy environment variables are ignored.
    pub fn new(policy: EgressPolicy, timeout: Duration) -> Result<Self, MimintError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| MimintError::Provider(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            policy,
            timeout,
            client,
        })
    }

    /// The fixed egress policy this client enforces.
    pub fn policy(&self) -> EgressPolicy {
        self.policy
    }

    /// The per-request timeout this client was built with.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Validate a URL against this client's policy (no I/O). Providers
    /// call this at construction so a bad endpoint fails before any turn.
    pub fn validate(&self, url: &str) -> Result<(), MimintError> {
        validate_url(url, self.policy)
    }

    /// POST a JSON body, validating the URL first. `headers` are extra
    /// request headers (auth); header values are never logged.
    pub async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
        headers: &[(String, String)],
    ) -> Result<reqwest::Response, MimintError> {
        // Egress check runs BEFORE the request is built or sent: a denied
        // URL never opens a socket and never touches DNS.
        self.validate(url)?;
        let mut request = self.client.post(url).json(body);
        for (name, value) in headers {
            let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| MimintError::Config(format!("invalid HTTP header name: {name}")))?;
            // Never include the value in the error: it may be a key.
            let header_value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
                MimintError::Config(format!("invalid HTTP header value for: {name}"))
            })?;
            request = request.header(header_name, header_value);
        }
        Ok(request.send().await?)
    }

    /// GET a URL, validating it first. Used by onboarding probes.
    pub async fn get(&self, url: &str) -> Result<reqwest::Response, MimintError> {
        self.validate(url)?;
        Ok(self.client.get(url).send().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_on_allows_only_loopback() {
        let policy = EgressPolicy::privacy_on();
        for url in [
            "http://127.0.0.1:11434/v1/chat/completions",
            "http://localhost:8080/x",
            "http://[::1]:8080/x",
            "https://127.0.0.1:11434/x",
            "https://localhost/x",
        ] {
            assert!(validate_url(url, policy).is_ok(), "{url}");
        }
        for url in [
            "https://example.com/v1/x",
            "https://api.anthropic.com/v1/messages",
            "http://example.com/x",
            "http://0.0.0.0:8080/x",
            "http://127.1:8080/x",
            "http://localhost.evil.example/x",
            "http://127.0.0.1.example.com/x",
            "https://2130706433/x",
        ] {
            let err = validate_url(url, policy).expect_err("must deny {url}");
            assert!(
                err.to_string().contains("Privacy Mode"),
                "denial must name Privacy Mode: {err}"
            );
        }
    }

    #[test]
    fn privacy_off_keeps_phase1_https_discipline() {
        let policy = EgressPolicy::privacy_off();
        assert!(validate_url("https://example.com/v1/x", policy).is_ok());
        assert!(validate_url("http://127.0.0.1:11434/x", policy).is_ok());
        let err =
            validate_url("http://example.com/x", policy).expect_err("plaintext remote must fail");
        assert!(err.to_string().contains("plaintext"), "{err}");
        assert!(validate_url("ftp://example.com/x", policy).is_err());
        assert!(validate_url("example.com/x", policy).is_err());
    }

    #[test]
    fn userinfo_cannot_smuggle_a_host() {
        let policy = EgressPolicy::privacy_on();
        // Real host is 127.0.0.1 (userinfo is `evil`): loopback, allowed.
        assert!(validate_url("http://evil@127.0.0.1:8080/x", policy).is_ok());
        // Real host is example.com (userinfo is `127.0.0.1`): denied.
        assert!(validate_url("http://127.0.0.1@example.com/x", policy).is_err());
        assert!(validate_url(
            "http://127.0.0.1@example.com/x",
            EgressPolicy::privacy_off()
        )
        .is_err());
    }

    #[test]
    fn default_policy_is_privacy_on() {
        assert!(EgressPolicy::default().privacy_mode());
    }

    #[test]
    fn url_is_loopback_matches_allow_list() {
        assert!(url_is_loopback("http://127.0.0.1:1/x").unwrap());
        assert!(url_is_loopback("http://LOCALHOST:1/x").unwrap());
        assert!(!url_is_loopback("https://example.com/x").unwrap());
        assert!(!url_is_loopback("http://0.0.0.0/x").unwrap());
        assert!(url_is_loopback("no-scheme").is_err());
    }
}
