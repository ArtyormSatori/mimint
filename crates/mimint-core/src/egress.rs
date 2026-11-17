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
//! - When Privacy Mode is on, the client also uses a custom DNS
//!   resolver ([`LoopbackGuardResolver`]) that checks the RESOLVED
//!   address, not just the hostname string: `localhost` resolves
//!   directly to 127.0.0.1 without a DNS query (so a poisoned
//!   `/etc/hosts` cannot redirect it), and any other name that
//!   resolves to a non-loopback address is refused. The string check
//!   in `validate_url` stays as the first layer.
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
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
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
///
/// Recorded decisions (all fail-closed, asserted by tests):
/// - `LOCALHOST` (any case) is loopback: `split_endpoint` lowercases
///   the host before this check, and DNS names are case-insensitive.
/// - `localhost.` (trailing dot), `127.0.0.1.`, the expanded IPv6 form
///   `[0:0:0:0:0:0:0:1]`, and the mapped form `[::ffff:127.0.0.1]` are
///   NOT loopback here: they are unusual spellings that no configured
///   backend needs, and accepting them would widen the allow-list on
///   string matching alone. Use the canonical forms instead.
/// - `[::1]` is loopback: `split_endpoint` strips the brackets before
///   this check, so the host string is `::1`.
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

/// DNS resolution through the system resolver (`getaddrinfo`), run on a
/// blocking thread because the call blocks. reqwest's own `GaiResolver`
/// is not public, so this is written from scratch on `ToSocketAddrs`.
#[derive(Debug)]
struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::task::spawn_blocking(move || {
                (host.as_str(), 0u16)
                    .to_socket_addrs()
                    .map(|iter| iter.collect())
            })
            .await
            .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?
            .map_err(|err| Box::new(err) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

fn dns_error(message: String) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        message,
    ))
}

/// A DNS resolver that enforces the egress kill-switch at resolution
/// time, on top of the hostname-string check in [`validate_url`].
///
/// - `localhost` (any case) never reaches DNS: it resolves directly to
///   `127.0.0.1`, so a poisoned `/etc/hosts` or a spoofed DNS answer
///   cannot redirect it. No DNS query is issued for it at all.
/// - Any other name resolves through the inner resolver, and when
///   Privacy Mode is on the call fails unless EVERY resolved address
///   is loopback. A name whose string passed the first check but whose
///   address is public is refused before `connect()`.
/// - When Privacy Mode is off, resolution passes through unchanged
///   (this resolver is only installed for Privacy-ON clients).
pub struct LoopbackGuardResolver<R: Resolve> {
    inner: Arc<R>,
    privacy_on: bool,
}

impl<R: Resolve> LoopbackGuardResolver<R> {
    /// Wrap an inner resolver with the loopback guard.
    pub fn new(inner: Arc<R>, privacy_on: bool) -> Self {
        Self { inner, privacy_on }
    }
}

impl LoopbackGuardResolver<SystemResolver> {
    /// Guard around the system resolver (the production wiring).
    /// Crate-visible only: `SystemResolver` is an implementation detail.
    pub(crate) fn system(privacy_on: bool) -> Self {
        Self::new(Arc::new(SystemResolver), privacy_on)
    }
}

impl<R: Resolve + 'static> Resolve for LoopbackGuardResolver<R> {
    fn resolve(&self, name: Name) -> Resolving {
        if name.as_str().eq_ignore_ascii_case("localhost") {
            let addr = SocketAddr::from(([127, 0, 0, 1], 0));
            return Box::pin(async move {
                Ok::<Addrs, Box<dyn std::error::Error + Send + Sync>>(Box::new(std::iter::once(
                    addr,
                )))
            });
        }
        let inner = Arc::clone(&self.inner);
        let privacy_on = self.privacy_on;
        let host = name.as_str().to_string();
        Box::pin(async move {
            let resolved = inner.resolve(name).await?;
            let collected: Vec<SocketAddr> = resolved.collect();
            if collected.is_empty() {
                return Err(dns_error(format!(
                    "DNS resolution for '{host}' returned no addresses"
                )));
            }
            if privacy_on {
                if let Some(bad) = collected.iter().find(|addr| !addr.ip().is_loopback()) {
                    return Err(dns_error(format!(
                        "Privacy Mode is ON: '{host}' resolved to non-loopback address {bad}; refusing to connect"
                    )));
                }
            }
            Ok(Box::new(collected.into_iter()) as Addrs)
        })
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
    /// When Privacy Mode is on, DNS resolution goes through
    /// [`LoopbackGuardResolver`] so resolved addresses are checked too;
    /// when it is off, reqwest's default resolver is kept.
    #[allow(clippy::disallowed_methods)]
    pub fn new(policy: EgressPolicy, timeout: Duration) -> Result<Self, MimintError> {
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
        if policy.privacy_mode() {
            builder = builder.dns_resolver(Arc::new(LoopbackGuardResolver::system(true)));
        }
        let client = builder
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[test]
    fn hostname_parsing_decisions_fail_closed() {
        // Canonical forms pass.
        assert!(url_is_loopback("http://[::1]:1/x").unwrap());
        assert!(url_is_loopback("http://LocalHost:1/x").unwrap());
        // Unusual spellings fail closed, even when they denote loopback.
        for url in [
            "http://localhost.:1/x",
            "http://127.0.0.1.:1/x",
            "http://[0:0:0:0:0:0:0:1]:1/x",
            "http://[::ffff:127.0.0.1]:1/x",
            "http://127.1:1/x",
            "http://sub.localhost:1/x",
        ] {
            assert!(!url_is_loopback(url).unwrap(), "{url}");
            assert!(
                validate_url(url, EgressPolicy::privacy_on()).is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn client_timeout_round_trips() {
        let client =
            GuardedHttpClient::new(EgressPolicy::privacy_on(), Duration::from_secs(7)).unwrap();
        assert_eq!(client.timeout(), Duration::from_secs(7));
    }

    /// Fake inner resolver returning fixed addresses and counting calls.
    struct FakeResolver {
        addrs: Vec<SocketAddr>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeResolver {
        fn returning(addrs: Vec<SocketAddr>) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    addrs,
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }
    }

    impl Resolve for FakeResolver {
        fn resolve(&self, _name: Name) -> Resolving {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let addrs = self.addrs.clone();
            Box::pin(async move {
                Ok::<Addrs, Box<dyn std::error::Error + Send + Sync>>(Box::new(addrs.into_iter()))
            })
        }
    }

    fn resolve_name(name: &str) -> Name {
        name.parse().expect("parse Name")
    }

    #[tokio::test]
    async fn resolver_refuses_public_ip_when_privacy_on() {
        let public: SocketAddr = "93.184.216.34:0".parse().unwrap();
        let (fake, _) = FakeResolver::returning(vec![public]);
        let guard = LoopbackGuardResolver::new(Arc::new(fake), true);
        let err = match guard.resolve(resolve_name("example.com")).await {
            Ok(_) => panic!("public IP must be refused"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("Privacy Mode"), "{err}");
        assert!(err.to_string().contains("93.184.216.34"), "{err}");
    }

    #[tokio::test]
    async fn resolver_passes_public_ip_when_privacy_off() {
        let public: SocketAddr = "93.184.216.34:0".parse().unwrap();
        let (fake, _) = FakeResolver::returning(vec![public]);
        let guard = LoopbackGuardResolver::new(Arc::new(fake), false);
        let addrs: Vec<SocketAddr> = guard
            .resolve(resolve_name("example.com"))
            .await
            .expect("privacy off passes through")
            .collect();
        assert_eq!(addrs, vec![public]);
    }

    #[tokio::test]
    async fn resolver_refuses_mixed_loopback_and_public() {
        let addrs: Vec<SocketAddr> = vec![
            "127.0.0.1:0".parse().unwrap(),
            "93.184.216.34:0".parse().unwrap(),
        ];
        let (fake, _) = FakeResolver::returning(addrs);
        let guard = LoopbackGuardResolver::new(Arc::new(fake), true);
        assert!(guard.resolve(resolve_name("mixed.example")).await.is_err());
    }

    #[tokio::test]
    async fn localhost_short_circuits_without_dns() {
        let public: SocketAddr = "93.184.216.34:0".parse().unwrap();
        let (fake, calls) = FakeResolver::returning(vec![public]);
        let guard = LoopbackGuardResolver::new(Arc::new(fake), true);
        for spelling in ["localhost", "LOCALHOST", "LocalHost"] {
            let addrs: Vec<SocketAddr> = guard
                .resolve(resolve_name(spelling))
                .await
                .expect("localhost resolves")
                .collect();
            assert_eq!(addrs, vec![SocketAddr::from(([127, 0, 0, 1], 0))]);
        }
        // The inner resolver was never consulted: no DNS query, no
        // /etc/hosts influence, even for a poisoned answer.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn resolver_rejects_empty_answer() {
        let (fake, _) = FakeResolver::returning(vec![]);
        let guard = LoopbackGuardResolver::new(Arc::new(fake), true);
        assert!(guard.resolve(resolve_name("empty.example")).await.is_err());
    }
}
