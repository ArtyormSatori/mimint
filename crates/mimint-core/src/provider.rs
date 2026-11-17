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

use crate::egress::{url_is_loopback, GuardedHttpClient};
use crate::error::MimintError;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

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
}

impl ChatMessage {
    /// System message.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// User message.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Tool result message answering one tool call.
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
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
/// chains hold `Box<dyn Provider>` over mixed backend types.
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

    /// One chat call with tool-calling enabled when tools exist.
    fn chat<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [serde_json::Value],
    ) -> Pin<Box<dyn Future<Output = Result<ChatMessage, MimintError>> + Send + 'a>>;
}

/// Build the error for a non-success HTTP status. HTTP 429 and 5xx are
/// transient (the fallback chain may retry them); anything else is a
/// permanent provider error.
pub(crate) fn http_status_error(status: reqwest::StatusCode, snippet: &str) -> MimintError {
    let message = extract_error_message(snippet);
    let text = format!("provider returned HTTP {status}: {message}");
    if status.as_u16() == 429 || status.is_server_error() {
        MimintError::ProviderTransient(text)
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
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
}

/// Client for one OpenAI-compatible chat-completions endpoint.
pub struct ProviderClient {
    name: String,
    endpoint_url: String,
    model: String,
    api_key: Option<String>,
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
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        http: GuardedHttpClient,
    ) -> Result<Self, MimintError> {
        Self::new_named("openai-compatible", base_url, model, api_key, http)
    }

    /// Build a client with an explicit backend name (from configuration).
    pub fn new_named(
        name: &str,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
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
            headers.push(("Authorization".to_string(), format!("Bearer {key}")));
        }
        let response = self
            .http
            .post_json(&self.endpoint_url, &body, &headers)
            .await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(http_status_error(status, &text));
        }
        let parsed: ChatCompletionResponse = response
            .json()
            .await
            .map_err(|e| MimintError::Provider(format!("cannot parse provider response: {e}")))?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .ok_or_else(|| MimintError::Provider("provider returned no choices".to_string()))
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
        messages: &'a [ChatMessage],
        tools: &'a [serde_json::Value],
    ) -> Pin<Box<dyn Future<Output = Result<ChatMessage, MimintError>> + Send + 'a>> {
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
        ProviderClient::new(base_url, "test-model", key.map(str::to_string), http_off())
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
        assert!(
            ProviderClient::new("https://example.com", "", Some("k".to_string()), http_off())
                .is_err()
        );
        assert!(ProviderClient::new("", "m", Some("k".to_string()), http_off()).is_err());
    }

    #[test]
    fn privacy_on_rejects_remote_at_construction() {
        let err = expect_error(
            ProviderClient::new("https://example.com", "m", Some("k".to_string()), http_on()),
            "remote under Privacy Mode must fail at construction",
        );
        assert!(err.to_string().contains("Privacy Mode"), "{err}");
        let client = ProviderClient::new("http://127.0.0.1:11434", "m", None, http_on())
            .expect("loopback under Privacy Mode");
        assert!(!client.is_paid());
    }

    #[test]
    fn error_classification_429_and_5xx_are_transient() {
        let transient = http_status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, "slow down");
        assert!(transient.is_transient());
        let transient = http_status_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "boom");
        assert!(transient.is_transient());
        let permanent = http_status_error(reqwest::StatusCode::UNAUTHORIZED, "bad key");
        assert!(!permanent.is_transient());
        let permanent = http_status_error(reqwest::StatusCode::NOT_FOUND, "no model");
        assert!(!permanent.is_transient());
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
