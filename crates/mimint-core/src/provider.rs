//! OpenAI-compatible provider client (D4: OpenAI-compatible first).
//!
//! - TLS via rustls only; plaintext HTTP is rejected except for loopback
//!   endpoints explicitly specified by the user, which are logged clearly.
//! - The API key comes only from the caller (the CLI reads `MIMINT_API_KEY`);
//!   it is never written to any log or file. Logs record only key presence.
//! - No default model: the caller must always supply one.

use crate::error::MimintError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A chat message (request and response shape).
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
    endpoint_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl ProviderClient {
    /// Build a client, validating the endpoint before any request is made.
    ///
    /// `base_url` may end with `/v1` (Ollama convention) or not; the
    /// chat-completions path is appended exactly once either way.
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        timeout: Duration,
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
        let (scheme, host) = split_endpoint(base_url)?;
        let loopback = is_loopback_host(&host);
        match scheme.as_str() {
            "https" => {}
            "http" if loopback => {
                eprintln!("plaintext loopback endpoint (user-specified local): {base_url}");
            }
            "http" => {
                return Err(MimintError::Config(format!(
                    "refusing plaintext http endpoint for non-loopback host '{host}': use https or a loopback endpoint"
                )));
            }
            other => {
                return Err(MimintError::Config(format!(
                    "unsupported URL scheme '{other}': use https (or http only for loopback)"
                )));
            }
        }
        let key = api_key.filter(|key| !key.trim().is_empty());
        if key.is_none() && !loopback {
            return Err(MimintError::Config(
                "missing API key for remote endpoint: set MIMINT_API_KEY".to_string(),
            ));
        }
        // Never log the key value; only whether one is present.
        eprintln!(
            "provider endpoint: {base_url}; model: {model}; api key present: {}",
            if key.is_some() { "yes" } else { "no" }
        );
        let endpoint_url = if base_url.ends_with("/v1") {
            format!("{base_url}/chat/completions")
        } else {
            format!("{base_url}/v1/chat/completions")
        };
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| MimintError::Provider(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            endpoint_url,
            model: model.to_string(),
            api_key: key,
            client,
        })
    }

    /// Model name sent with every request.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Full chat-completions URL. Contains no secret.
    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    /// One chat-completions call with tool-calling enabled when tools exist.
    pub async fn chat(
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
        let mut request = self.client.post(&self.endpoint_url).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(500).collect();
            return Err(MimintError::Provider(format!(
                "provider returned HTTP {status}: {snippet}"
            )));
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

/// Split `scheme://authority/...` into a lowercase scheme and host (no port).
fn split_endpoint(base_url: &str) -> Result<(String, String), MimintError> {
    let Some((scheme, rest)) = base_url.split_once("://") else {
        return Err(MimintError::Config(
            "base URL must include a scheme, e.g. https://example.com".to_string(),
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
                    "base URL has a malformed bracketed host".to_string(),
                ));
            }
        }
    } else if hostport.matches(':').count() == 1 {
        hostport.split(':').next().unwrap_or(hostport)
    } else {
        hostport
    };
    if host.is_empty() {
        return Err(MimintError::Config("base URL has no host".to_string()));
    }
    Ok((scheme, host.to_ascii_lowercase()))
}

/// Loopback hosts allowed for user-specified plaintext HTTP: exactly the
/// three forms in the brief, nothing else (notably not `0.0.0.0`).
fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_client(base_url: &str, key: Option<&str>) -> Result<ProviderClient, MimintError> {
        ProviderClient::new(
            base_url,
            "test-model",
            key.map(str::to_string),
            Duration::from_secs(1),
        )
    }

    #[test]
    fn http_non_loopback_is_rejected() {
        let err = new_client("http://example.com", Some("k"))
            .err()
            .expect("http non-loopback must fail");
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
        }
    }

    #[test]
    fn https_remote_without_key_is_rejected() {
        let err = new_client("https://example.com", None)
            .err()
            .expect("https remote without key must fail");
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
            Some("k".to_string()),
            Duration::from_secs(1)
        )
        .is_err());
        assert!(
            ProviderClient::new("", "m", Some("k".to_string()), Duration::from_secs(1)).is_err()
        );
    }
}
