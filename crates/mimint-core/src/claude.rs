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

use crate::egress::{split_endpoint, url_is_loopback, GuardedHttpClient};
use crate::error::MimintError;
use crate::provider::{
    check_plaintext_auth, extract_error_message, http_status_error_with_retry_after,
    normalize_tool_call_args, parse_retry_after, tool_args_as_object, BoxFuture, CallContext,
    ChatMessage, FunctionCall, Provider, ToolCall,
};
use serde_json::{json, Value};
use zeroize::Zeroizing;

/// Default Anthropic API root (no path suffix; `/v1/messages` is appended).
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic API version header sent with every request.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default `max_tokens` when the backend does not set one. The Anthropic
/// API requires this field; 4096 is the safe standard for tool use, where
/// 1024 cut responses off mid-`tool_use` too easily. Overridable per
/// backend via `max_tokens` in the config file.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Client for one Anthropic Messages API endpoint.
///
/// The key is held in a [`Zeroizing`] wrapper so the stored copy is
/// wiped when the client drops (see the same note on `ProviderClient`).
pub struct ClaudeClient {
    name: String,
    endpoint_url: String,
    model: String,
    api_key: Option<Zeroizing<String>>,
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
    /// `allow_plaintext_auth` must be true to send a key over plaintext
    /// http to a loopback endpoint; see [`check_plaintext_auth`].
    pub fn new(
        name: &str,
        base_url: &str,
        model: &str,
        api_key: Option<Zeroizing<String>>,
        max_tokens: u32,
        allow_plaintext_auth: bool,
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
        let (scheme, _) = split_endpoint(&endpoint_url)?;
        let key = api_key.filter(|key| !key.trim().is_empty());
        if key.is_none() && !loopback {
            return Err(MimintError::Config(
                "missing API key for Claude backend: set ANTHROPIC_API_KEY/MIMINT_API_KEY or configure key_env/key_file".to_string(),
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
            headers.push(("x-api-key".to_string(), key.as_str().to_string()));
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
        _ctx: &'a CallContext<'a>,
        messages: &'a [ChatMessage],
        tools: &'a [Value],
    ) -> BoxFuture<'a, Result<ChatMessage, MimintError>> {
        // Leaf providers do not audit; the fallback chain owns attempt
        // records, so the call context is intentionally unused here.
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
                        // Never fail the whole request on one bad
                        // argument string: invalid or non-object
                        // arguments are wrapped (see
                        // `tool_args_as_object`), so a bad call from a
                        // previous provider cannot poison this one.
                        let input = tool_args_as_object(&call.function.arguments);
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
///
/// A `stop_reason` of `max_tokens` is a truncation, not a success: the
/// response may end mid-`tool_use` with cut-off JSON, so it is reported
/// as a provider error (permanent: retrying with the same `max_tokens`
/// would truncate again) telling the user to raise `max_tokens`. Other
/// stop reasons (`end_turn`, `tool_use`, `stop_sequence`, ...) and a
/// missing `stop_reason` (stubs) are accepted as normal responses.
fn from_anthropic_response(body: &Value) -> Result<ChatMessage, MimintError> {
    if body.get("type").and_then(Value::as_str) == Some("error") {
        let detail = extract_error_message(&body.to_string());
        return Err(MimintError::Provider(format!(
            "Claude backend returned an error response: {detail}"
        )));
    }
    if body.get("stop_reason").and_then(Value::as_str) == Some("max_tokens") {
        return Err(MimintError::Provider(
            "Claude response was truncated (stop_reason=max_tokens): the reply may end mid-tool-call. \
             Raise max_tokens for this backend (default 4096) and retry"
                .to_string(),
        ));
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
                let encoded = serde_json::to_string(&input).map_err(|err| {
                    MimintError::Provider(format!("cannot encode tool input: {err}"))
                })?;
                // Normalize at intake, like the OpenAI-compatible
                // provider: the history must never carry arguments that
                // break a later translation.
                let arguments = normalize_tool_call_args(&encoded);
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

    fn key(text: &str) -> Option<Zeroizing<String>> {
        Some(Zeroizing::new(text.to_string()))
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
            // Plaintext opt-in is on so the loopback case with a key
            // builds; this test is about path appending, not the gate.
            let client = ClaudeClient::new("c", base, "m", key("k"), 10, true, http_off())
                .unwrap_or_else(|err| panic!("{base}: {err}"));
            assert_eq!(client.endpoint_url(), expected);
        }
    }

    #[test]
    fn remote_requires_key_loopback_does_not() {
        assert!(ClaudeClient::new(
            "c",
            "https://api.anthropic.com",
            "m",
            None,
            10,
            false,
            http_off()
        )
        .is_err());
        let client = ClaudeClient::new("c", "http://127.0.0.1:9", "m", None, 10, false, http_off())
            .expect("loopback without key");
        assert!(!client.is_paid());
        assert!(ClaudeClient::new(
            "c",
            "https://api.anthropic.com",
            "",
            key("k"),
            10,
            false,
            http_off()
        )
        .is_err());
        assert!(ClaudeClient::new(
            "c",
            "https://api.anthropic.com",
            "m",
            key("k"),
            0,
            false,
            http_off()
        )
        .is_err());
    }

    #[test]
    fn plaintext_loopback_key_requires_opt_in() {
        assert!(ClaudeClient::new(
            "c",
            "http://127.0.0.1:9",
            "m",
            key("k"),
            10,
            false,
            http_off()
        )
        .is_err());
        assert!(ClaudeClient::new(
            "c",
            "http://127.0.0.1:9",
            "m",
            key("k"),
            10,
            true,
            http_off()
        )
        .is_ok());
        assert_eq!(DEFAULT_MAX_TOKENS, 4096);
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

    #[test]
    fn contract_fixtures_stop_reason_and_error_shapes() {
        // Fixtures shaped after the Anthropic Messages API documentation:
        // a truncated response, a refusal, and an error body. The
        // truncation fixture ends mid-tool_use: it must fail loudly,
        // never surface as a partial success.
        let truncated = json!({
            "id": "msg_trunc",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Saving"},
                {"type": "tool_use", "id": "toolu_cut", "name": "notebook_add", "input": {"title": "Cu"}},
            ],
            "stop_reason": "max_tokens",
            "usage": {"input_tokens": 10, "output_tokens": 4096},
        });
        let err = from_anthropic_response(&truncated).expect_err("max_tokens must fail");
        assert!(err.to_string().contains("max_tokens"), "{err}");
        assert!(!err.is_transient(), "truncation is permanent, not retried");

        let end_turn = json!({
            "id": "msg_ok",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Done."}],
            "stop_reason": "end_turn",
        });
        assert!(from_anthropic_response(&end_turn).is_ok());

        let no_stop_reason = json!({
            "id": "msg_stub",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Stub text."}],
        });
        assert!(from_anthropic_response(&no_stop_reason).is_ok());

        // Error body with the documented shape: type=error, typed error
        // object with a message.
        let rate_limit = json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "Rate limited. Please try again."},
        });
        let err = from_anthropic_response(&rate_limit).expect_err("error body");
        assert!(err.to_string().contains("Rate limited"), "{err}");
    }

    #[test]
    fn request_translation_wraps_invalid_tool_args_instead_of_failing() {
        // History from a previous provider (e.g. a small local model)
        // carrying broken tool-call arguments must not kill the Claude
        // call: the arguments are wrapped, the request goes out, and
        // the tool fails validation so the model can self-correct.
        let messages = vec![
            ChatMessage::user("Save a note."),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![
                    ToolCall {
                        id: "call_broken".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "notebook_add".to_string(),
                            arguments: "{broken json".to_string(),
                        },
                    },
                    ToolCall {
                        id: "call_array".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "notebook_search".to_string(),
                            arguments: "[1, 2]".to_string(),
                        },
                    },
                ]),
                tool_call_id: None,
            },
        ];
        let body = to_anthropic_request("claude-test", 42, &messages, &[]).expect("translate");
        let converted = body["messages"].as_array().unwrap();
        let blocks = converted[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["input"]["_mimint_invalid_args"], "{broken json");
        assert_eq!(blocks[1]["input"]["_mimint_invalid_args"], json!([1, 2]));
    }
}
