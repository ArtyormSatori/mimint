//! Provider trait and the OpenAI-compatible provider client (D4:
//! OpenAI-compatible first, Claude second).
//!
//! - Every provider sends HTTP only through a [`GuardedHttpClient`],
//!   which enforces the egress policy before any socket opens. No
//!   provider constructs its own HTTP client.
//! - TLS via rustls only; plaintext HTTP is rejected except for loopback
//!   endpoints explicitly specified by the user, which are logged clearly.
//! - The API key comes only from the caller (via the vault); it is never
//!   written to any log or file. Logs record only key presence.
//! - No default model: the caller must always supply one.

use crate::agents::AGENT_DEFAULT;
use crate::audit::AuditLog;
use crate::cancel::CancelToken;
use crate::egress::{url_is_loopback, GuardedHttpClient};
use crate::error::MimintError;
use crate::ledger::{CostLedger, TokenUsage, DEFAULT_MAX_TOKENS};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Boxed future type shared by every [`Provider`] implementation, so no
/// impl repeats the `Pin<Box<dyn Future ...>>` shape by hand. The future
/// must be `Send` and borrow only from the `&self` call.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Per-call context passed to [`Provider::chat`]. Audit state travels
/// with the call, not in the provider: providers (including fallback
/// chains) are stateless across turns and can be built once and reused.
///
/// - `turn_id`: the turn this call belongs to; audit records use it.
/// - `audit`: where the fallback chain writes `provider_attempt` /
///   `provider_failure` records, when attached. Leaf providers do not
///   audit; the chain owns attempt records.
/// - `deadline`: an outer wall-clock bound the caller may set. The
///   fallback chain honors the earlier of this deadline and its own
///   per-call `max_elapsed` budget. `None` means "no outer bound".
/// - `agent_id`: the agent making the call (Phase 6, §1.3); audit
///   and ledger records are attributed to it.
/// - `ledger`: the token cost ledger (Phase 6, §3.3). When attached,
///   the fallback chain reserves before each attempt and settles
///   after it. `None` means "no ledgering" (legacy callers).
/// - `max_tokens`: the reserve basis for one attempt (default 4096).
/// - `cancel_tokens`: checked by the fallback chain between attempts
///   and during backoff sleeps (Phase 6, §10). An HTTP request
///   already in flight is NOT interrupted by these tokens; it is
///   interrupted by dropping the chat future (the brain races the
///   call against a cancel waiter in a `select!`). See
///   `docs/stop-and-approval.md`.
///
/// No `Debug`: `AuditLog` deliberately has none (its path could leak
/// into debug output), and a context carrying it must not either.
#[derive(Clone, Copy)]
pub struct CallContext<'a> {
    pub turn_id: &'a str,
    pub audit: Option<&'a AuditLog>,
    pub deadline: Option<Instant>,
    /// Active profile name (Phase 5, P5.5), tagged onto the
    /// fallback chain's audit records. `None` means untagged.
    pub profile: Option<&'a str>,
    /// The agent making the call. Defaults to the default agent.
    pub agent_id: &'a str,
    /// Token cost ledger, when attached.
    pub ledger: Option<&'a CostLedger>,
    /// Reserve basis for one attempt, in tokens.
    pub max_tokens: u32,
    /// Cancel tokens checked between attempts and in backoff.
    pub cancel_tokens: &'a [CancelToken],
}

impl<'a> CallContext<'a> {
    /// Context for one call: a turn id and an optional audit log. No
    /// outer deadline.
    pub fn new(turn_id: &'a str, audit: Option<&'a AuditLog>) -> Self {
        Self {
            turn_id,
            audit,
            deadline: None,
            profile: None,
            agent_id: AGENT_DEFAULT,
            ledger: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            cancel_tokens: &[],
        }
    }

    /// Context with an outer deadline. The fallback chain stops starting
    /// new attempts once it passes.
    pub fn with_deadline(turn_id: &'a str, audit: Option<&'a AuditLog>, deadline: Instant) -> Self {
        Self {
            turn_id,
            audit,
            deadline: Some(deadline),
            profile: None,
            agent_id: AGENT_DEFAULT,
            ledger: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            cancel_tokens: &[],
        }
    }

    /// Context with a profile name attached for audit tagging.
    pub fn with_profile(mut self, profile: &'a str) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Context attributed to one agent.
    pub fn with_agent_id(mut self, agent_id: &'a str) -> Self {
        self.agent_id = agent_id;
        self
    }

    /// Context with a token cost ledger attached.
    pub fn with_ledger(mut self, ledger: &'a CostLedger) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Context with a reserve basis for each attempt.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Context with cancel tokens for between-attempt checks.
    pub fn with_cancel_tokens(mut self, tokens: &'a [CancelToken]) -> Self {
        self.cancel_tokens = tokens;
        self
    }

    /// True when any attached cancel token is cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_tokens.iter().any(|t| t.is_cancelled())
    }
}

/// A chat message (request and response shape), in OpenAI-style form.
/// This is the common in-memory shape: protocol adapters (Claude, ...)
/// translate to and from their wire format at the boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    /// Null for assistant messages that only carry tool calls.
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Token usage parsed from the provider response (Phase 6,
    /// §3.3). `None` for request messages and for responses that
    /// carried no usable usage block. Never serialized into a
    /// request: providers strip it at the boundary by
    /// construction (it is skipped when serializing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

impl ChatMessage {
    /// System message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        }
    }

    /// User message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        }
    }

    /// Tool result message answering one tool call.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            usage: None,
        }
    }
}

/// One tool call requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_tool_call_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn default_tool_call_type() -> String {
    "function".to_string()
}

/// The function part of a tool call. `arguments` is a JSON-encoded string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// One LLM backend behind the common chat interface.
///
/// The `chat` method returns a boxed future (instead of `async fn`) so
/// the trait stays object-safe without an extra dependency: fallback
/// chains hold `Box<dyn Provider>` over mixed backend types. This is the
/// long-term shape: `async-trait` would generate exactly this form, and
/// `async fn` in traits is not dyn-compatible on stable Rust.
///
/// Streaming contract (sketch, not implemented): streaming needs a
/// separate method (for example `chat_stream` returning delta chunks)
/// with a default impl that buffers into one full response. Retry and
/// fallback are only valid BEFORE the first byte is emitted: once a
/// delta has gone out, switching providers mid-stream would repeat or
/// skew content. Per-chunk guards, per-chunk timeouts, cancellation,
/// and reassembly of streamed tool-call JSON must be designed with that
/// method; the shape must be settled before a fourth provider is added.
pub trait Provider: Send + Sync {
    /// Backend name from configuration (used in audit records and errors).
    fn name(&self) -> &str;

    /// Model name sent with every request.
    fn model(&self) -> &str;

    /// Full request URL. Contains no secret.
    fn endpoint_url(&self) -> &str;

    /// True when this backend bills per use: any non-loopback endpoint.
    /// Loopback backends (Ollama, LM Studio, local gateways) are
    /// classified free/local. This drives the P0-11 cost-cap lock.
    fn is_paid(&self) -> bool;

    /// One chat call with tool-calling enabled when tools exist. The
    /// call context carries the turn id and audit log for this call;
    /// providers must not retain it past the returned future.
    fn chat<'a>(
        &'a self,
        ctx: &'a CallContext<'a>,
        messages: &'a [ChatMessage],
        tools: &'a [serde_json::Value],
    ) -> BoxFuture<'a, Result<ChatMessage, MimintError>>;
}

/// Tool-call arguments as a JSON object, never failing the caller.
///
/// Tool arguments arrive as a JSON-encoded string in the OpenAI-shaped
/// history. That string may be invalid JSON (small local models do this
/// regularly) or valid JSON that is not an object. Adapters that need an
/// object (Claude's `tool_use.input`) must use this helper instead of
/// parsing inline: invalid or non-object arguments are wrapped in
/// `{"_mimint_invalid_args": ...}` so the request still goes out, the
/// tool then fails argument validation, and the model sees a tool error
/// it can correct — instead of one bad call poisoning every later call.
/// Valid objects pass through unchanged.
pub(crate) fn tool_args_as_object(arguments: &str) -> Value {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(map)) => Value::Object(map),
        Ok(other) => serde_json::json!({"_mimint_invalid_args": other}),
        Err(_) => serde_json::json!({"_mimint_invalid_args": arguments}),
    }
}

/// Normalize one tool call's argument string at response intake, so the
/// history never carries a value that breaks a later provider.
///
/// Valid JSON objects are returned byte-identical. Anything else is
/// rewritten to the wrapped-object form (see [`tool_args_as_object`]).
/// Providers call this on every tool call they parse out of a response,
/// before the message enters the history.
pub(crate) fn normalize_tool_call_args(arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(Value::Object(_)) => arguments.to_string(),
        _ => serde_json::to_string(&tool_args_as_object(arguments))
            .unwrap_or_else(|_| "{\"_mimint_invalid_args\":null}".to_string()),
    }
}

/// Refuse to send an API key over plaintext HTTP to a loopback endpoint
/// unless the backend explicitly opted in.
///
/// Any local process can bind an unused loopback port, so a key sent to
/// `http://127.0.0.1:<port>` may reach an impostor service (confused
/// deputy) instead of the intended local server. The opt-in lives on the
/// backend (`allow_plaintext_auth`), never ambient: without it, a
/// loopback backend with a key fails at construction with a clear error.
pub(crate) fn check_plaintext_auth(
    backend_name: &str,
    base_url: &str,
    scheme: &str,
    loopback: bool,
    key_present: bool,
    allow_plaintext_auth: bool,
) -> Result<(), MimintError> {
    if scheme == "http" && loopback && key_present && !allow_plaintext_auth {
        return Err(MimintError::Config(format!(
            "refusing to send an API key over plaintext http to loopback backend '{backend_name}' ({base_url}): \
             another local process could be listening on that port. Remove the key from this backend, \
             or set allow_plaintext_auth = true on the backend to opt in explicitly"
        )));
    }
    Ok(())
}

/// Parse a `Retry-After` header value (delay-seconds form only) into a
/// wait duration. The HTTP-date form, negative values, and garbage are
/// ignored (`None`): the caller falls back to exponential backoff.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

/// Build the error for a non-success HTTP status. HTTP 429 and 5xx are
/// transient (the fallback chain may retry them); anything else is a
/// permanent provider error. HTTP 529 (Anthropic overloaded) is transient
/// via the 5xx rule.
/// Build a provider error from an HTTP status. Transient statuses (429,
/// 5xx) produce a retryable error that also carries the provider's
/// `Retry-After` wait when one was supplied, so the fallback chain
/// honors it instead of guessing a backoff. Everything else is
/// permanent: retrying cannot fix it.
pub(crate) fn http_status_error_with_retry_after(
    status: reqwest::StatusCode,
    snippet: &str,
    retry_after: Option<Duration>,
) -> MimintError {
    let message = extract_error_message(snippet);
    let text = format!("provider returned HTTP {status}: {message}");
    if status.as_u16() == 429 || status.is_server_error() {
        match retry_after {
            Some(delay) => MimintError::ProviderTransientDelayed(text, delay),
            None => MimintError::ProviderTransient(text),
        }
    } else {
        MimintError::Provider(text)
    }
}

/// Pull a readable message out of a provider error body when possible.
/// Understands `{"error": {"message": ...}}` (OpenAI) and
/// `{"error": {"message": ...}}` (Anthropic); falls back to the raw body,
/// truncated to 500 characters.
pub(crate) fn extract_error_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(message) = parsed
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(serde_json::Value::as_str)
        {
            return message.chars().take(500).collect();
        }
        if let Some(message) = parsed.get("error").and_then(serde_json::Value::as_str) {
            return message.chars().take(500).collect();
        }
    }
    body.chars().take(500).collect()
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
    /// OpenAI-style usage block, when the provider sends one.
    /// Both fields must be present for the usage to count.
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

impl OpenAiUsage {
    fn to_token_usage(&self) -> Option<TokenUsage> {
        match (self.prompt_tokens, self.completion_tokens) {
            (Some(prompt_tokens), Some(completion_tokens)) => Some(TokenUsage {
                prompt_tokens,
                completion_tokens,
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
}

/// Client for one OpenAI-compatible chat-completions endpoint.
///
/// The key is held in a [`Zeroizing`] wrapper so the stored copy is
/// wiped when the client drops. Transient copies (the formatted
/// `Authorization` header value, reqwest's internal buffers) are not
/// wiped; that limit is stated, not hidden.
pub struct ProviderClient {
    name: String,
    endpoint_url: String,
    model: String,
    api_key: Option<Zeroizing<String>>,
    http: GuardedHttpClient,
    paid: bool,
}

impl ProviderClient {
    /// Build a client, validating the endpoint before any request is made.
    ///
    /// `base_url` may end with `/v1` (Ollama convention) or not; the
    /// chat-completions path is appended exactly once either way.
    /// The endpoint is validated against the guarded client's egress
    /// policy at construction, and again on every request.
    /// Plaintext loopback auth is NOT allowed: use [`Self::new_named`]
    /// with an explicit opt-in for a keyed loopback backend.
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<Zeroizing<String>>,
        http: GuardedHttpClient,
    ) -> Result<Self, MimintError> {
        Self::new_named("openai-compatible", base_url, model, api_key, false, http)
    }

    /// Build a client with an explicit backend name (from configuration)
    /// and an explicit plaintext-auth opt-in. `allow_plaintext_auth`
    /// must be true to send a key over plaintext http to a loopback
    /// endpoint; see [`check_plaintext_auth`].
    pub fn new_named(
        name: &str,
        base_url: &str,
        model: &str,
        api_key: Option<Zeroizing<String>>,
        allow_plaintext_auth: bool,
        http: GuardedHttpClient,
    ) -> Result<Self, MimintError> {
        let base_url = base_url.trim().trim_end_matches('/');
        if base_url.is_empty() {
            return Err(MimintError::Config(
                "empty base URL: pass --base-url or set MIMINT_BASE_URL".to_string(),
            ));
        }
        if model.trim().is_empty() {
            return Err(MimintError::Config(
                "empty model: pass --model or set MIMINT_MODEL (no default model)".to_string(),
            ));
        }
        let endpoint_url = if base_url.ends_with("/v1") {
            format!("{base_url}/chat/completions")
        } else {
            format!("{base_url}/v1/chat/completions")
        };
        // Egress policy (including Privacy Mode) is enforced here, at
        // construction, and again on every request inside the client.
        http.validate(&endpoint_url)?;
        let loopback = url_is_loopback(&endpoint_url)?;
        let (scheme, _) = crate::egress::split_endpoint(&endpoint_url)?;
        if scheme == "http" && loopback {
            eprintln!("plaintext loopback endpoint (user-specified local): {base_url}");
        }
        let key = api_key.filter(|key| !key.trim().is_empty());
        if key.is_none() && !loopback {
            return Err(MimintError::Config(
                "missing API key for remote endpoint: set MIMINT_API_KEY or configure key_env/key_file".to_string(),
            ));
        }
        check_plaintext_auth(
            name,
            base_url,
            &scheme,
            loopback,
            key.is_some(),
            allow_plaintext_auth,
        )?;
        if scheme == "http" && loopback && key.is_some() {
            eprintln!(
                "WARNING: sending an API key over plaintext http to loopback backend '{name}' (explicit allow_plaintext_auth opt-in)"
            );
        }
        // Never log the key value; only whether one is present.
        eprintln!(
            "provider endpoint: {base_url}; model: {model}; api key present: {}",
            if key.is_some() { "yes" } else { "no" }
        );
        Ok(Self {
            name: name.to_string(),
            endpoint_url,
            model: model.to_string(),
            api_key: key,
            http,
            paid: !loopback,
        })
    }

    /// Full chat-completions URL. Contains no secret.
    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    /// Model name sent with every request.
    pub fn model(&self) -> &str {
        &self.model
    }

    async fn chat_inner(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<ChatMessage, MimintError> {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools.to_vec());
            body["tool_choice"] = serde_json::Value::String("auto".to_string());
        }
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(key) = &self.api_key {
            headers.push((
                "Authorization".to_string(),
                format!("Bearer {}", key.as_str()),
            ));
        }
        let response = self
            .http
            .post_json(&self.endpoint_url, &body, &headers)
            .await?;
        let status = response.status();
        // Read Retry-After before consuming the body: the body read
        // below takes the response by value.
        let retry_after = parse_retry_after(response.headers());
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(http_status_error_with_retry_after(
                status,
                &text,
                retry_after,
            ));
        }
        let parsed: ChatCompletionResponse = response
            .json()
            .await
            .map_err(|e| MimintError::Provider(format!("cannot parse provider response: {e}")))?;
        let mut message = parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .ok_or_else(|| MimintError::Provider("provider returned no choices".to_string()))?;
        // Token usage (§3.3): attached to the message so the
        // fallback chain can settle the attempt's reserve with
        // measured usage. Absent or partial usage stays `None` —
        // the chain then settles the reserve as an estimate.
        message.usage = parsed.usage.and_then(|usage| usage.to_token_usage());
        // Normalize tool-call arguments at intake so the history never
        // carries a value that breaks a later provider (see
        // `normalize_tool_call_args`).
        if let Some(calls) = &mut message.tool_calls {
            for call in calls.iter_mut() {
                call.function.arguments = normalize_tool_call_args(&call.function.arguments);
            }
        }
        Ok(message)
    }
}

impl Provider for ProviderClient {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    fn is_paid(&self) -> bool {
        self.paid
    }

    fn chat<'a>(
        &'a self,
        _ctx: &'a CallContext<'a>,
        messages: &'a [ChatMessage],
        tools: &'a [serde_json::Value],
    ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
        // Leaf providers do not audit; the fallback chain owns attempt
        // records, so the call context is intentionally unused here.
        Box::pin(async move { self.chat_inner(messages, tools).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::EgressPolicy;
    use std::time::Duration;

    fn http_off() -> GuardedHttpClient {
        GuardedHttpClient::new(EgressPolicy::privacy_off(), Duration::from_secs(1))
            .expect("build guarded client")
    }

    fn http_on() -> GuardedHttpClient {
        GuardedHttpClient::new(EgressPolicy::privacy_on(), Duration::from_secs(1))
            .expect("build guarded client")
    }

    fn new_client(base_url: &str, key: Option<&str>) -> Result<ProviderClient, MimintError> {
        ProviderClient::new(
            base_url,
            "test-model",
            key.map(|k| Zeroizing::new(k.to_string())),
            http_off(),
        )
    }

    fn new_named_client(
        base_url: &str,
        key: Option<&str>,
        allow_plaintext_auth: bool,
    ) -> Result<ProviderClient, MimintError> {
        ProviderClient::new_named(
            "test",
            base_url,
            "test-model",
            key.map(|k| Zeroizing::new(k.to_string())),
            allow_plaintext_auth,
            http_off(),
        )
    }

    /// `expect_err` without a `Debug` bound on the success type: provider
    /// clients deliberately do not implement `Debug` (they hold keys).
    fn expect_error<T>(result: Result<T, MimintError>, what: &str) -> MimintError {
        match result {
            Ok(_) => panic!("expected error: {what}"),
            Err(err) => err,
        }
    }

    #[test]
    fn http_non_loopback_is_rejected() {
        let err = expect_error(
            new_client("http://example.com", Some("k")),
            "http non-loopback must fail",
        );
        assert!(err.to_string().contains("plaintext"), "{err}");
    }

    #[test]
    fn http_loopback_without_key_is_allowed() {
        for base_url in [
            "http://127.0.0.1:11434",
            "http://localhost:8080",
            "http://[::1]:8080",
        ] {
            let client = new_client(base_url, None).expect("loopback without key");
            assert!(client.endpoint_url().ends_with("/v1/chat/completions"));
            assert!(!client.is_paid(), "{base_url} must be free/local");
        }
    }

    #[test]
    fn https_remote_without_key_is_rejected() {
        let err = expect_error(
            new_client("https://example.com", None),
            "https remote without key must fail",
        );
        assert!(err.to_string().contains("MIMINT_API_KEY"), "{err}");
    }

    #[test]
    fn https_remote_with_key_is_allowed() {
        let client = new_client("https://example.com", Some("secret")).unwrap();
        assert_eq!(
            client.endpoint_url(),
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(client.model(), "test-model");
        assert!(client.is_paid());
    }

    #[test]
    fn base_url_ending_in_v1_is_not_doubled() {
        let client = new_client("http://127.0.0.1:11434/v1", None).unwrap();
        assert_eq!(
            client.endpoint_url(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
        let client = new_client("http://127.0.0.1:11434/v1/", None).unwrap();
        assert_eq!(
            client.endpoint_url(),
            "http://127.0.0.1:11434/v1/chat/completions"
        );
    }

    #[test]
    fn unsupported_scheme_and_empty_model_are_rejected() {
        assert!(new_client("ftp://example.com", Some("k")).is_err());
        assert!(new_client("example.com", Some("k")).is_err());
        assert!(ProviderClient::new(
            "https://example.com",
            "",
            Some(Zeroizing::new("k".to_string())),
            http_off()
        )
        .is_err());
        assert!(
            ProviderClient::new("", "m", Some(Zeroizing::new("k".to_string())), http_off())
                .is_err()
        );
    }

    #[test]
    fn privacy_on_rejects_remote_at_construction() {
        let err = expect_error(
            ProviderClient::new(
                "https://example.com",
                "m",
                Some(Zeroizing::new("k".to_string())),
                http_on(),
            ),
            "remote under Privacy Mode must fail at construction",
        );
        assert!(err.to_string().contains("Privacy Mode"), "{err}");
        let client = ProviderClient::new("http://127.0.0.1:11434", "m", None, http_on())
            .expect("loopback under Privacy Mode");
        assert!(!client.is_paid());
    }

    #[test]
    fn error_classification_429_and_5xx_are_transient() {
        let transient = http_status_error_with_retry_after(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            None,
        );
        assert!(transient.is_transient());
        let transient = http_status_error_with_retry_after(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "boom",
            None,
        );
        assert!(transient.is_transient());
        // 529 (Anthropic overloaded) is transient via the 5xx rule.
        let overloaded = reqwest::StatusCode::from_u16(529).unwrap();
        let transient = http_status_error_with_retry_after(overloaded, "overloaded", None);
        assert!(transient.is_transient());
        assert!(transient.retry_after().is_none());
        let permanent =
            http_status_error_with_retry_after(reqwest::StatusCode::UNAUTHORIZED, "bad key", None);
        assert!(!permanent.is_transient());
        let permanent =
            http_status_error_with_retry_after(reqwest::StatusCode::NOT_FOUND, "no model", None);
        assert!(!permanent.is_transient());
    }

    #[test]
    fn retry_after_is_carried_on_transient_errors_only() {
        let err = http_status_error_with_retry_after(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            Some(Duration::from_secs(7)),
        );
        assert!(err.is_transient());
        assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
        assert!(err.to_string().contains("7s"), "{err}");
        // A Retry-After on a permanent error does not make it transient.
        let err = http_status_error_with_retry_after(
            reqwest::StatusCode::FORBIDDEN,
            "nope",
            Some(Duration::from_secs(7)),
        );
        assert!(!err.is_transient());
        assert!(err.retry_after().is_none());
    }

    #[test]
    fn retry_after_header_parsing_accepts_delay_seconds_only() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("2"),
        );
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(2)));
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("garbage"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn tool_args_normalization_keeps_objects_and_wraps_the_rest() {
        assert_eq!(
            normalize_tool_call_args(r#"{"title":"Hi"}"#),
            r#"{"title":"Hi"}"#
        );
        let wrapped = normalize_tool_call_args("not json {");
        let parsed: Value = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(parsed["_mimint_invalid_args"], "not json {");
        let wrapped = normalize_tool_call_args("[1, 2]");
        let parsed: Value = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(parsed["_mimint_invalid_args"], serde_json::json!([1, 2]));
        let wrapped = normalize_tool_call_args("");
        let parsed: Value = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(parsed["_mimint_invalid_args"], "");
        let object = tool_args_as_object(r#"{"a":1}"#);
        assert_eq!(object, serde_json::json!({"a": 1}));
    }

    #[test]
    fn plaintext_loopback_auth_requires_explicit_opt_in() {
        let err = expect_error(
            new_named_client("http://127.0.0.1:11434", Some("k"), false),
            "key over plaintext loopback without opt-in must fail",
        );
        assert!(err.to_string().contains("allow_plaintext_auth"), "{err}");
        assert!(err.to_string().contains("plaintext"), "{err}");
        // Explicit opt-in builds; no key needs no opt-in.
        assert!(new_named_client("http://127.0.0.1:11434", Some("k"), true).is_ok());
        assert!(new_named_client("http://127.0.0.1:11434", None, false).is_ok());
        // Remote https with a key needs no plaintext opt-in.
        assert!(new_named_client("https://example.com", Some("k"), false).is_ok());
    }

    #[test]
    fn error_message_extraction_prefers_provider_message() {
        assert_eq!(
            extract_error_message(r#"{"error":{"message":"bad key","type":"auth"}}"#),
            "bad key"
        );
        assert_eq!(extract_error_message("plain failure"), "plain failure");
        assert_eq!(extract_error_message(r#"{"error":"short"}"#), "short");
    }
}
