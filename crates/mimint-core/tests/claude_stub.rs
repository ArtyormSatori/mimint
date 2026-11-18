//! Claude adapter tests against a stub that speaks the Anthropic
//! Messages API protocol. No real Anthropic API is called: the stub is
//! in-process, bound to 127.0.0.1, and records every request so the
//! translation can be asserted in both directions.

use mimint_core::{
    default_registry, Approver, AuditLog, Brain, CallContext, ChatMessage, ClaudeClient,
    EgressPolicy, GuardedHttpClient, MimintError, Notebook, Provider,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestApprover;

impl Approver for TestApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(true)
    }
    fn label(&self) -> &'static str {
        "auto-approved (test mode)"
    }
}

#[derive(Clone)]
struct RecordedRequest {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

fn anthropic_tool_use_response() -> String {
    json!({
        "id": "msg_stub_1",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "text", "text": "Saving it."},
            {"type": "tool_use", "id": "toolu_stub_1", "name": "notebook_add",
             "input": {"title": "Stub note", "body": "Written by stub e2e"}},
        ],
        "model": "stub",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })
    .to_string()
}

fn anthropic_text_response(text: &str) -> String {
    json!({
        "id": "msg_stub_2",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "model": "stub",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })
    .to_string()
}

fn anthropic_truncated_response() -> String {
    json!({
        "id": "msg_stub_3",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "text", "text": "Partial answer that ran out of"},
            {"type": "tool_use", "id": "toolu_stub_9", "name": "notebook_add",
             "input": {"title": "Truncated"}},
        ],
        "model": "stub",
        "stop_reason": "max_tokens",
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })
    .to_string()
}

struct Stub {
    port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

/// One scripted response: status, body, and extra response headers
/// (e.g. `Retry-After`) to send before the standard headers.
type Scripted = (u16, String, Vec<(String, String)>);

fn run_stub(
    listener: TcpListener,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    script: Arc<Mutex<Vec<Scripted>>>,
) {
    let counter = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let _call = counter.fetch_add(1, Ordering::SeqCst) + 1;
        let Ok(clone) = stream.try_clone() else {
            continue;
        };
        let mut reader = BufReader::new(clone);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        }
        let mut content_length = 0usize;
        let mut expect_continue = false;
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut headers_ok = true;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    headers_ok = false;
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some((key, value)) = trimmed.split_once(':') {
                        if key.trim().eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap_or(0);
                        } else if key.trim().eq_ignore_ascii_case("expect")
                            && value.trim().eq_ignore_ascii_case("100-continue")
                        {
                            expect_continue = true;
                        }
                        headers.push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
                    }
                }
            }
        }
        if !headers_ok {
            continue;
        }
        if expect_continue {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        requests.lock().unwrap().push(RecordedRequest {
            path,
            headers,
            body: parsed,
        });
        let (status, payload, extra_headers) = {
            let mut script = script.lock().unwrap();
            if script.len() > 1 {
                script.remove(0)
            } else {
                script[0].clone()
            }
        };
        let reason = if status == 200 { "OK" } else { "Error" };
        let mut extra = String::new();
        for (name, value) in &extra_headers {
            extra.push_str(&format!("{name}: {value}\r\n"));
        }
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\n{extra}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

fn start_stub(script: Vec<Scripted>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().expect("stub addr").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = Arc::clone(&requests);
    let script = Arc::new(Mutex::new(script));
    std::thread::spawn(move || run_stub(listener, requests_clone, script));
    Stub { port, requests }
}

fn temp_stem(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!(
        "{}/mimint-claude-{}-{nanos}-{count}-{tag}",
        std::env::temp_dir().display(),
        std::process::id()
    )
}

fn claude_client(port: u16, key: Option<&str>) -> ClaudeClient {
    ClaudeClient::new(
        "claude-stub",
        &format!("http://127.0.0.1:{port}"),
        "stub-model",
        key.map(|k| Zeroizing::new(k.to_string())),
        64,
        // Tests send a key to a loopback stub over http: explicit opt-in.
        true,
        GuardedHttpClient::new(EgressPolicy::privacy_on(), Duration::from_secs(10))
            .expect("guarded client"),
    )
    .expect("claude client")
}

fn header<'a>(request: &'a RecordedRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn claude_turn_through_anthropic_stub() {
    let stub = start_stub(vec![
        (200, anthropic_tool_use_response(), vec![]),
        (
            200,
            anthropic_text_response("Stub done: note saved."),
            vec![],
        ),
    ]);
    let stem = temp_stem("turn");
    let db_path = format!("{stem}.db");
    let audit_path = format!("{stem}.jsonl");
    let notebook = Arc::new(Notebook::open(&db_path).expect("open notebook"));
    let audit = AuditLog::open(&audit_path).expect("open audit");
    let registry = default_registry(Arc::clone(&notebook));
    let provider = claude_client(stub.port, Some("test-key"));
    assert_eq!(
        provider.endpoint_url(),
        format!("http://127.0.0.1:{}/v1/messages", stub.port)
    );
    let approver: Arc<dyn Approver> = Arc::new(TestApprover);
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let registry = Arc::new(registry);
    let audit = Arc::new(audit);
    let brain = Brain::new(
        Arc::clone(&provider),
        Arc::clone(&registry),
        Some(Arc::clone(&approver)),
        Arc::clone(&audit),
        Arc::clone(&notebook),
        "turn-claude-stub",
        "agent-001",
        5,
    )
    .expect("brain");
    let outcome = brain
        .run("Save a note titled Stub note.")
        .await
        .expect("turn succeeds");
    assert_eq!(outcome.text, "Stub done: note saved.");
    assert!(!outcome.partial);
    assert_eq!(outcome.tools_used, vec!["notebook_add".to_string()]);
    assert_eq!(outcome.steps, 2);

    let notes = notebook.search("Stub note").expect("search");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].body, "Written by stub e2e");

    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(header(request, "x-api-key"), Some("test-key"));
        assert_eq!(header(request, "anthropic-version"), Some("2023-06-01"));
    }
    // First request: system field, user message, Anthropic tool schemas.
    let first = &requests[0].body;
    assert_eq!(first["model"], "stub-model");
    assert_eq!(first["max_tokens"], 64);
    assert!(
        first["system"]
            .as_str()
            .is_some_and(|s| s.contains("Mimint")),
        "system field must carry the system prompt: {first}"
    );
    assert!(first["tools"][0].get("function").is_none());
    assert_eq!(first["tools"][0]["name"], "notebook_search");
    assert_eq!(
        first["tools"][0]["input_schema"]["properties"]["query"]["type"],
        "string"
    );
    // Second request: assistant tool_use block + user tool_result block.
    let second = &requests[1].body;
    let messages = second["messages"].as_array().expect("messages array");
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant turn in history");
    let blocks = assistant["content"].as_array().expect("content blocks");
    assert!(blocks.iter().any(|b| b["type"] == "tool_use"
        && b["id"] == "toolu_stub_1"
        && b["name"] == "notebook_add"));
    let tool_result_turn = messages
        .iter()
        .find(|m| {
            m["role"] == "user"
                && m["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"))
        })
        .expect("tool_result turn in history");
    let result_blocks = tool_result_turn["content"].as_array().unwrap();
    assert_eq!(result_blocks[0]["tool_use_id"], "toolu_stub_1");

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn claude_error_mapping_transient_and_permanent() {
    let stub_500 = start_stub(vec![(500, "boom".to_string(), vec![])]);
    let provider = claude_client(stub_500.port, None);
    let ctx = CallContext::new("turn-claude-err", None);
    let err = provider
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect_err("500 must fail");
    assert!(err.is_transient(), "{err}");

    let stub_401 = start_stub(vec![(
        401,
        json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}).to_string(),
        vec![],
    )]);
    let provider = claude_client(stub_401.port, Some("bad"));
    let err = provider
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect_err("401 must fail");
    assert!(!err.is_transient(), "{err}");
    assert!(err.to_string().contains("invalid x-api-key"), "{err}");
}

#[tokio::test]
async fn claude_retry_after_header_is_carried_on_transient_errors() {
    let stub = start_stub(vec![(
        429,
        json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}})
            .to_string(),
        vec![("Retry-After".to_string(), "2".to_string())],
    )]);
    let provider = claude_client(stub.port, Some("test-key"));
    let ctx = CallContext::new("turn-claude-retry-after", None);
    let err = provider
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect_err("429 must fail");
    assert!(err.is_transient(), "{err}");
    assert_eq!(err.retry_after(), Some(Duration::from_secs(2)), "{err}");
}

#[tokio::test]
async fn claude_max_tokens_truncation_is_an_error_not_success() {
    let stub = start_stub(vec![(200, anthropic_truncated_response(), vec![])]);
    let provider = claude_client(stub.port, Some("test-key"));
    let ctx = CallContext::new("turn-claude-truncated", None);
    let err = provider
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect_err("max_tokens stop_reason must fail");
    assert!(!err.is_transient(), "{err}");
    assert!(err.to_string().contains("max_tokens"), "{err}");
}

#[tokio::test]
async fn claude_text_only_response_has_no_tool_calls() {
    let stub = start_stub(vec![(200, anthropic_text_response("Just text."), vec![])]);
    let provider = claude_client(stub.port, None);
    let ctx = CallContext::new("turn-claude-text", None);
    let message = provider
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect("text response");
    assert_eq!(message.content.as_deref(), Some("Just text."));
    assert!(message.tool_calls.is_none());
}
