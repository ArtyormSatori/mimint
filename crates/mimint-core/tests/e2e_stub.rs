//! End-to-end turn through a canned localhost stub.
//!
//! Deterministic: the stub is in-process, std-only, bound to 127.0.0.1 on
//! an ephemeral port, and never calls out. First provider call returns a
//! canned `notebook_add` tool call; the second returns canned final text.

use mimint_core::{
    default_registry, Approver, AuditLog, Brain, MimintError, Notebook, ProviderClient,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct TestApprover;

impl Approver for TestApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(true)
    }

    fn label(&self) -> &'static str {
        "auto-approved (test mode)"
    }
}

fn stub_response(call: usize) -> String {
    if call == 1 {
        r#"{"id":"chatcmpl-stub","object":"chat.completion","created":0,"model":"stub","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_stub_1","type":"function","function":{"name":"notebook_add","arguments":"{\"title\":\"Stub note\",\"body\":\"Written by stub e2e\"}"}}]},"finish_reason":"tool_calls"}]}"#.to_string()
    } else {
        r#"{"id":"chatcmpl-stub","object":"chat.completion","created":0,"model":"stub","choices":[{"index":0,"message":{"role":"assistant","content":"Stub done: note saved."},"finish_reason":"stop"}]}"#.to_string()
    }
}

fn run_stub(listener: TcpListener, counter: Arc<AtomicUsize>) {
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
        let payload = stub_response(call);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }
}

#[tokio::test]
async fn e2e_turn_through_stub() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().expect("stub addr").port();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);
    std::thread::spawn(move || run_stub(listener, counter_clone));

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stem = format!(
        "{}/mimint-e2e-{}-{nanos}",
        std::env::temp_dir().display(),
        std::process::id()
    );
    let db_path = format!("{stem}.db");
    let audit_path = format!("{stem}.jsonl");

    let notebook = Arc::new(Notebook::open(&db_path).expect("open notebook"));
    let audit = AuditLog::open(&audit_path).expect("open audit");
    let registry = default_registry(Arc::clone(&notebook));
    let provider = ProviderClient::new(
        &format!("http://127.0.0.1:{port}"),
        "stub-model",
        None,
        Duration::from_secs(10),
    )
    .expect("provider");
    let approver = TestApprover;
    let brain = Brain::new(
        &provider,
        &registry,
        Some(&approver),
        &audit,
        Arc::clone(&notebook),
        "turn-e2e-stub",
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
    assert_eq!(notes[0].title, "Stub note");
    assert_eq!(notes[0].body, "Written by stub e2e");

    let text = std::fs::read_to_string(&audit_path).expect("read audit");
    let records: Vec<Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("audit line is JSON"))
        .collect();
    for kind in ["decision", "execution", "verification", "reflection"] {
        assert!(
            records
                .iter()
                .any(|record| record.get("kind").and_then(Value::as_str) == Some(kind)),
            "audit missing kind {kind}: {records:?}"
        );
    }
    assert!(
        records.iter().any(|record| {
            record.get("kind").and_then(Value::as_str) == Some("decision")
                && record.get("decision").and_then(Value::as_str) == Some("allow")
                && record
                    .get("detail")
                    .and_then(|detail| detail.get("approver"))
                    .and_then(Value::as_str)
                    == Some("auto-approved (test mode)")
        }),
        "audit must record the test-mode approval: {records:?}"
    );
    assert!(
        records.iter().any(|record| {
            record.get("kind").and_then(Value::as_str) == Some("verification")
                && record.get("decision").and_then(Value::as_str) == Some("verified")
        }),
        "audit must record a verified verification: {records:?}"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&audit_path);
}
