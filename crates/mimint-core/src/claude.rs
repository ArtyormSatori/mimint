//! Claude adapter: Anthropic Messages API behind the common [`Provider`]
//! trait (D4: Claude second, after OpenAI-compatible).
//!
//! Translation is two-way and happens only at this boundary:
//! - Request: OpenAI-style messages + tools become Anthropic `system` +
//!   `messages` content blocks (`tool_use` / `tool_result`) + `tools`
//!   with `input_schema`.
//! - Response: Anthropic `content` blocks become one [`ChatMessage`]
//!   with text and/or OpenAI-style `tool_calls`.
//!
//! The adapter speaks plain HTTPS to `https://api.anthropic.com` by
//! default; the base URL is overridable (custom endpoints and tests).
//! Keys come from the vault via the caller and are never logged.
//! `max_tokens` is required by the Anthropic API; the default is the
//! documented constant [`DEFAULT_MAX_TOKENS`], overridable per backend.
//! The model name has no default, like every other backend.

use crate::egress::{url_is_loopback, GuardedHttpClient};
use crate::error::MimintError;
use crate::provider::{
    extract_error_message, http_status_error, ChatMessage, FunctionCall, Provider, ToolCall,
};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;

/// Default Anthropic API root (no path suffix; `/v1/messages` is appended).
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic API version header sent with every request.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default `max_tokens` when the backend does not set one. The Anthropic
/// API requires this field; 1024 keeps one turn cheap and short.
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Client for one Anthropic Messages API endpoint.
pub struct ClaudeClient {
    name: String,
    endpoint_url: String,
    model: String,
    api_key: Option<String>,
    max_tokens: u32,
    http: GuardedHttpClient,
    paid: bool,
}

impl ClaudeClient {
    /// Build a client, validating the endpoint before any request is made.
    ///
    /// `base_url` is the API root: it may end with `/v1` or `/v1/messages`
    /// or neither; the messages path is appended exactly once. The
    /// endpoint is validated against the guarded client's egress policy
    /// at construction, and again on every request.
    pub fn new(
        name: &str,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        max_tokens: u32,
        http: GuardedHttpClient,
    ) -> Result<Self, MimintError> {
        let base_url = base_url.trim().trim_end_matches('/');
        if base_url.is_empty() {
            return Err(MimintError::Config(
                "empty base URL for Claude backend".to_string(),
            ));
        }
        if model.trim().is_empty() {
            return Err(MimintError::Config(
                "empty model for Claude backend (no default model)".to_string(),
            ));
        }
        if max_tokens == 0 {
            return Err(MimintError::Config(
                "max_tokens must be at least 1 for Claude backend".to_string(),
            ));
        }
        let endpoint_url = if base_url.ends_with("/v1/messages") {
            base_url.to_string()
        } else if base_url.ends_with("/v1") {
            format!("{base_url}/messages")
        } else {
            format!("{base_url}/v1/messages")
        };
        http.validate(&endpoint_url)?;
        let loopback = url_is_loopback(&endpoint_url)?;
        let key = api_key.filter(|key| !key.trim().is_empty());
        if key.is_none() && !loopback {
            return Err(MimintError::Config(
                "missing API key for Claude backend: set ANTHROPIC_API_KEY/MIMINT_API_KEY or configure key_env/key_file".to_string(),
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
            max_tokens,
            http,
            paid: !loopback,
        })
    }

    async fn chat_inner(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> Result<ChatMessage, MimintError> {
        let body = to_anthropic_request(&self.model, self.max_tokens, messages, tools)?;
        let mut headers: Vec<(String, String)> = vec![(
            "anthropic-version".to_string(),
            ANTHROPIC_VERSION.to_string(),
        )];
        if let Some(key) = &self.api_key {
            headers.push(("x-api-key".to_string(), key.clone()));
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
        let parsed: Value = response
            .json()
            .await
            .map_err(|e| MimintError::Provider(format!("cannot parse provider response: {e}")))?;
        from_anthropic_response(&parsed)
    }
}

impl Provider for ClaudeClient {
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
        tools: &'a [Value],
    ) -> Pin<Box<dyn Future<Output = Result<ChatMessage, MimintError>> + Send + 'a>> {
        Box::pin(async move { self.chat_inner(messages, tools).await })
    }
}

/// Translate OpenAI-style messages + tools into an Anthropic request body.
fn to_anthropic_request(
    model: &str,
    max_tokens: u32,
    messages: &[ChatMessage],
    tools: &[Value],
) -> Result<Value, MimintError> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut converted: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    let flush_tool_results = |converted: &mut Vec<Value>, pending: &mut Vec<Value>| {
        if !pending.is_empty() {
            let blocks = std::mem::take(pending);
            converted.push(json!({"role": "user", "content": blocks}));
        }
    };

    for message in messages {
        match message.role.as_str() {
            "system" => {
                flush_tool_results(&mut converted, &mut pending_tool_results);
                if let Some(content) = &message.content {
                    if !content.trim().is_empty() {
                        system_parts.push(content.clone());
                    }
                }
            }
            "tool" => {
                let Some(tool_call_id) = &message.tool_call_id else {
                    return Err(MimintError::Provider(
                        "tool result message without tool_call_id cannot be sent to Claude"
                            .to_string(),
                    ));
                };
                pending_tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": message.content.clone().unwrap_or_default(),
                }));
            }
            "user" => {
                flush_tool_results(&mut converted, &mut pending_tool_results);
                converted.push(json!({
                    "role": "user",
                    "content": message.content.clone().unwrap_or_default(),
                }));
            }
            "assistant" => {
                flush_tool_results(&mut converted, &mut pending_tool_results);
                let calls = message.tool_calls.clone().unwrap_or_default();
                if calls.is_empty() {
                    converted.push(json!({
                        "role": "assistant",
                        "content": message.content.clone().unwrap_or_default(),
                    }));
                } else {
                    let mut blocks: Vec<Value> = Vec::new();
                    if let Some(content) = &message.content {
                        if !content.is_empty() {
                            blocks.push(json!({"type": "text", "text": content}));
                        }
                    }
                    for call in calls {
                        let input: Value =
                            serde_json::from_str(&call.function.arguments).map_err(|err| {
                                MimintError::Provider(format!(
                                    "tool arguments for '{}' are not valid JSON: {err}",
                                    call.function.name
                                ))
                            })?;
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.function.name,
                            "input": input,
                        }));
                    }
                    converted.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            other => {
                return Err(MimintError::Provider(format!(
                    "unsupported message role '{other}' for Claude backend"
                )));
            }
        }
    }
    flush_tool_results(&mut converted, &mut pending_tool_results);
    if converted.is_empty() {
        return Err(MimintError::Provider(
            "no messages to send to Claude backend".to_string(),
        ));
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": converted,
    });
    if !system_parts.is_empty() {
        body["system"] = Value::String(system_parts.join("\n\n"));
    }
    if !tools.is_empty() {
        let mut anthropic_tools: Vec<Value> = Vec::new();
        for tool in tools {
            anthropic_tools.push(convert_tool(tool)?);
        }
        body["tools"] = Value::Array(anthropic_tools);
    }
    Ok(body)
}

/// Convert one OpenAI-style tool schema to Anthropic form.
fn convert_tool(tool: &Value) -> Result<Value, MimintError> {
    let function = tool.get("function").ok_or_else(|| {
        MimintError::Provider("tool schema without 'function' cannot be sent to Claude".to_string())
    })?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| MimintError::Provider("tool schema without a function name".to_string()))?;
    let description = function
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let input_schema = function.get("parameters").cloned().unwrap_or(json!({
        "type": "object",
        "properties": {},
    }));
    Ok(json!({
        "name": name,
        "description": description,
        "input_schema": input_schema,
    }))
}

/// Translate an Anthropic response body into one [`ChatMessage`].
fn from_anthropic_response(body: &Value) -> Result<ChatMessage, MimintError> {
    if body.get("type").and_then(Value::as_str) == Some("error") {
        let detail = extract_error_message(&body.to_string());
        return Err(MimintError::Provider(format!(
            "Claude backend returned an error response: {detail}"
        )));
    }
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            MimintError::Provider("Claude response has no 'content' array".to_string())
        })?;
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(text.to_string());
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        MimintError::Provider("Claude tool_use block without id".to_string())
                    })?
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        MimintError::Provider("Claude tool_use block without name".to_string())
                    })?
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                let arguments = serde_json::to_string(&input).map_err(|err| {
                    MimintError::Provider(format!("cannot encode tool input: {err}"))
                })?;
                tool_calls.push(ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: FunctionCall { name, arguments },
                });
            }
            // Unknown block types are ignored for forward compatibility.
            _ => {}
        }
    }
    if text_parts.is_empty() && tool_calls.is_empty() {
        return Err(MimintError::Provider(
            "Claude response had no usable content".to_string(),
        ));
    }
    let text = text_parts.concat();
    Ok(ChatMessage {
        role: "assistant".to_string(),
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
    })
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

    #[test]
    fn endpoint_path_is_appended_exactly_once() {
        let cases = [
            (
                "https://api.anthropic.com",
                "https://api.anthropic.com/v1/messages",
            ),
            (
                "https://api.anthropic.com/v1",
                "https://api.anthropic.com/v1/messages",
            ),
            (
                "https://api.anthropic.com/v1/messages",
                "https://api.anthropic.com/v1/messages",
            ),
            ("http://127.0.0.1:9/", "http://127.0.0.1:9/v1/messages"),
        ];
        for (base, expected) in cases {
            let client = ClaudeClient::new("c", base, "m", Some("k".into()), 10, http_off())
                .unwrap_or_else(|err| panic!("{base}: {err}"));
            assert_eq!(client.endpoint_url(), expected);
        }
    }

    #[test]
    fn remote_requires_key_loopback_does_not() {
        assert!(
            ClaudeClient::new("c", "https://api.anthropic.com", "m", None, 10, http_off()).is_err()
        );
        let client = ClaudeClient::new("c", "http://127.0.0.1:9", "m", None, 10, http_off())
            .expect("loopback without key");
        assert!(!client.is_paid());
        assert!(ClaudeClient::new(
            "c",
            "https://api.anthropic.com",
            "",
            Some("k".into()),
            10,
            http_off()
        )
        .is_err());
        assert!(ClaudeClient::new(
            "c",
            "https://api.anthropic.com",
            "m",
            Some("k".into()),
            0,
            http_off()
        )
        .is_err());
    }

    #[test]
    fn request_translation_system_tools_and_tool_flow() {
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "notebook_add",
                "description": "Add a note.",
                "parameters": {"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]},
            },
        })];
        let messages = vec![
            ChatMessage::system("Be brief."),
            ChatMessage::system("Second instruction."),
            ChatMessage::user("Save a note."),
            ChatMessage {
                role: "assistant".to_string(),
                content: Some("Saving it.".to_string()),
                tool_calls: Some(vec![ToolCall {
                    id: "toolu_1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "notebook_add".to_string(),
                        arguments: r#"{"title":"Hi"}"#.to_string(),
                    },
                }]),
                tool_call_id: None,
            },
            ChatMessage::tool_result("toolu_1", r#"{"id":7}"#),
            ChatMessage::tool_result("toolu_2", r#"{"id":8}"#),
        ];
        let body = to_anthropic_request("claude-test", 42, &messages, &tools).expect("translate");
        assert_eq!(body["model"], "claude-test");
        assert_eq!(body["max_tokens"], 42);
        assert_eq!(body["system"], "Be brief.\n\nSecond instruction.");
        assert_eq!(body["tools"][0]["name"], "notebook_add");
        assert_eq!(body["tools"][0]["description"], "Add a note.");
        assert_eq!(
            body["tools"][0]["input_schema"]["properties"]["title"]["type"],
            "string"
        );
        assert!(body["tools"][0].get("function").is_none());
        let converted = body["messages"].as_array().unwrap();
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0]["role"], "user");
        // Assistant turn: text block + tool_use block.
        let blocks = converted[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["name"], "notebook_add");
        assert_eq!(blocks[1]["input"]["title"], "Hi");
        // Consecutive tool results merge into one user turn.
        let results = converted[2]["content"].as_array().unwrap();
        assert_eq!(converted[2]["role"], "user");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[0]["tool_use_id"], "toolu_1");
        assert_eq!(results[1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn response_translation_text_and_tool_use() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Here: "},
                {"type": "text", "text": "done."},
                {"type": "tool_use", "id": "toolu_9", "name": "notebook_add", "input": {"title": "T", "body": "B"}},
            ],
            "stop_reason": "tool_use",
        });
        let message = from_anthropic_response(&body).expect("translate response");
        assert_eq!(message.content.as_deref(), Some("Here: done."));
        let calls = message.tool_calls.expect("tool calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_9");
        assert_eq!(calls[0].function.name, "notebook_add");
        let args: Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["title"], "T");
    }

    #[test]
    fn response_translation_rejects_empty_and_error_shapes() {
        let empty = json!({"type": "message", "content": []});
        assert!(from_anthropic_response(&empty).is_err());
        let error = json!({"type": "error", "error": {"type": "auth", "message": "bad key"}});
        let err = from_anthropic_response(&error).expect_err("error shape");
        assert!(err.to_string().contains("bad key"), "{err}");
        let no_content = json!({"type": "message"});
        assert!(from_anthropic_response(&no_content).is_err());
    }
}
