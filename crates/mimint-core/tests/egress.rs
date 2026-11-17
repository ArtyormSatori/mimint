//! Egress kill-switch tests: Privacy Mode denies non-loopback traffic
//! before any socket opens, redirects are not followed, and proxy
//! environment variables are ignored. No real network is used: every
//! reachable endpoint is a loopback stub in this process.

use mimint_core::{
    default_registry, Approver, AuditLog, Brain, EgressPolicy, GuardedHttpClient, MimintError,
    Notebook, Provider, ProviderClient,
};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct StubResponse {
    status: u16,
    reason: &'static str,
    extra_headers: Vec<(String, String)>,
    body: String,
}

type Responder = dyn Fn(usize, &RecordedRequest) -> StubResponse + Send + Sync;

struct Stub {
    port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

fn chat_completion_text(text: &str) -> String {
    json!({
        "id": "chatcmpl-stub",
        "object": "chat.completion",
        "created": 0,
        "model": "stub",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
    })
    .to_string()
}

fn chat_completion_tool_call() -> String {
    r#"{"id":"chatcmpl-stub","object":"chat.completion","created":0,"model":"stub","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_stub_1","type":"function","function":{"name":"notebook_add","arguments":"{\"title\":\"Stub note\",\"body\":\"Written by stub e2e\"}"}}]},"finish_reason":"tool_calls"}]}"#.to_string()
}

fn run_stub(
    listener: TcpListener,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    responder: Arc<Responder>,
) {
    let counter = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
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
                        headers.push((key.trim().to_string(), value.trim().to_string()));
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
        let mut parts = request_line.split_whitespace();
        let recorded = RecordedRequest {
            method: parts.next().unwrap_or("").to_string(),
            path: parts.next().unwrap_or("").to_string(),
            headers,
            body,
        };
        requests.lock().unwrap().push(recorded.clone());
        let reply = responder(call, &recorded);
        let mut response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            reply.status,
            reply.reason,
            reply.body.len()
        );
        for (name, value) in &reply.extra_headers {
            response.push_str(&format!("{name}: {value}\r\n"));
        }
        response.push_str(&format!("\r\n{}", reply.body));
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

fn start_stub(responder: Arc<Responder>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().expect("stub addr").port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let requests_clone = Arc::clone(&requests);
    std::thread::spawn(move || run_stub(listener, requests_clone, responder));
    Stub { port, requests }
}

fn temp_stem(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!(
        "{}/mimint-egress-{}-{nanos}-{count}-{tag}",
        std::env::temp_dir().display(),
        std::process::id()
    )
}

fn guarded_on(timeout: Duration) -> GuardedHttpClient {
    GuardedHttpClient::new(EgressPolicy::privacy_on(), timeout).expect("guarded client")
}

#[tokio::test]
async fn privacy_on_denies_non_loopback_before_any_socket() {
    // Both targets would take seconds (or fail differently) if the client
    // actually tried DNS/connect: an unroutable IP and an unresolvable
    // name. A Privacy Mode denial must win instantly, before any socket.
    let http = guarded_on(Duration::from_secs(30));
    for url in [
        "https://10.255.255.1:9/v1/chat/completions",
        "https://nonexistent.invalid/v1/chat/completions",
    ] {
        let started = Instant::now();
        let err = http
            .post_json(url, &json!({}), &[])
            .await
            .expect_err("non-loopback must be denied");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "denial took too long ({:?}): the client may have attempted I/O",
            started.elapsed()
        );
        assert!(err.to_string().contains("Privacy Mode"), "{err}");
        assert!(!err.is_transient(), "egress denial must never be retried");
    }
    let err = http
        .get("https://example.com/")
        .await
        .expect_err("GET must be denied too");
    assert!(err.to_string().contains("Privacy Mode"), "{err}");
}

#[tokio::test]
async fn redirect_to_external_url_is_not_followed() {
    let stub = start_stub(Arc::new(|_, _| StubResponse {
        status: 302,
        reason: "Found",
        extra_headers: vec![(
            "Location".to_string(),
            "https://example.com/v1/chat/completions".to_string(),
        )],
        body: String::new(),
    }));
    let provider = ProviderClient::new(
        &format!("http://127.0.0.1:{}", stub.port),
        "stub-model",
        None,
        guarded_on(Duration::from_secs(5)),
    )
    .expect("provider");
    let err = provider
        .chat(&[mimint_core::ChatMessage::user("hi")], &[])
        .await
        .expect_err("redirect must surface as an error, not a follow");
    // The client returned the 302 itself instead of following it.
    assert!(err.to_string().contains("302"), "{err}");
    assert_eq!(stub.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn proxy_environment_is_ignored() {
    let stub = start_stub(Arc::new(|_, _| StubResponse {
        status: 200,
        reason: "OK",
        extra_headers: vec![],
        body: chat_completion_text("proxy ignored"),
    }));
    // Point every proxy variable at a dead proxy: if the client honored
    // any of them, this request would fail.
    let saved: Vec<(String, Option<String>)> = [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
    .iter()
    .map(|name| (name.to_string(), std::env::var(name).ok()))
    .collect();
    for (name, _) in &saved {
        std::env::set_var(name, "http://127.0.0.1:1");
    }
    let outcome = async {
        let provider = ProviderClient::new(
            &format!("http://127.0.0.1:{}", stub.port),
            "stub-model",
            None,
            guarded_on(Duration::from_secs(5)),
        )
        .expect("provider");
        provider
            .chat(&[mimint_core::ChatMessage::user("hi")], &[])
            .await
    }
    .await;
    for (name, original) in saved {
        match original {
            Some(value) => std::env::set_var(&name, value),
            None => std::env::remove_var(&name),
        }
    }
    let message = outcome.expect("loopback request must succeed despite dead proxy env");
    assert_eq!(message.content.as_deref(), Some("proxy ignored"));
}

#[tokio::test]
async fn privacy_on_turn_only_touches_the_loopback_stub() {
    let stub = start_stub(Arc::new(|call, _| StubResponse {
        status: 200,
        reason: "OK",
        extra_headers: vec![],
        body: if call == 1 {
            chat_completion_tool_call()
        } else {
            chat_completion_text("Stub done: note saved.")
        },
    }));
    let stem = temp_stem("turn");
    let db_path = format!("{stem}.db");
    let audit_path = format!("{stem}.jsonl");
    let notebook = Arc::new(Notebook::open(&db_path).expect("open notebook"));
    let audit = AuditLog::open(&audit_path).expect("open audit");
    let registry = default_registry(Arc::clone(&notebook));
    let provider = ProviderClient::new(
        &format!("http://127.0.0.1:{}", stub.port),
        "stub-model",
        None,
        guarded_on(Duration::from_secs(10)),
    )
    .expect("provider");
    let approver = TestApprover;
    let brain = Brain::new(
        &provider,
        &registry,
        Some(&approver),
        &audit,
        Arc::clone(&notebook),
        "turn-egress",
        5,
    )
    .expect("brain");
    let outcome = brain.run("Save a note.").await.expect("turn succeeds");
    assert_eq!(outcome.text, "Stub done: note saved.");
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "exactly two loopback requests expected");
    for request in requests.iter() {
        assert_eq!(request.method, "POST");
        assert!(
            request.path.ends_with("/v1/chat/completions"),
            "unexpected path: {}",
            request.path
        );
        // No key was configured, so no Authorization header may appear.
        assert!(
            request
                .headers
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("authorization")),
            "unexpected Authorization header without a configured key"
        );
        let body: Value = serde_json::from_slice(&request.body).expect("request body is JSON");
        assert_eq!(body["model"], "stub-model");
    }
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&audit_path);
}

#[test]
fn remote_provider_cannot_be_constructed_under_privacy_on() {
    let err = match ProviderClient::new(
        "https://example.com",
        "m",
        Some("k".to_string()),
        guarded_on(Duration::from_secs(1)),
    ) {
        Ok(_) => panic!("remote construction under Privacy Mode must fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("Privacy Mode"), "{err}");
    assert!(!err.is_transient());
}
