//! Fallback/retry tests against scripted stubs: 500, then a timeout,
//! then success; all-fail; permanent errors skipping retry; and the
//! wall-clock budget bound. Every attempt must appear in the audit log.

use mimint_core::{
    AuditLog, CallContext, ChatMessage, EgressPolicy, FallbackChain, GuardedHttpClient,
    MimintError, Provider, ProviderClient, RetryPolicy,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct Behavior {
    status: u16,
    delay: Duration,
    body: String,
}

fn success_body(text: &str) -> String {
    json!({
        "id": "chatcmpl-stub",
        "object": "chat.completion",
        "created": 0,
        "model": "stub",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
    })
    .to_string()
}

struct Stub {
    port: u16,
    count: Arc<AtomicUsize>,
}

fn run_stub(
    listener: TcpListener,
    count: Arc<AtomicUsize>,
    script: Arc<Mutex<VecDeque<Behavior>>>,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        // One thread per connection: a delayed behavior must not block
        // later requests (the retry test's third attempt arrives while
        // the second behavior is still sleeping).
        let count = Arc::clone(&count);
        let script = Arc::clone(&script);
        std::thread::spawn(move || handle_connection(stream, &count, &script));
    }
}

fn handle_connection(
    mut stream: std::net::TcpStream,
    count: &AtomicUsize,
    script: &Mutex<VecDeque<Behavior>>,
) {
    count.fetch_add(1, Ordering::SeqCst);
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
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
        return;
    }
    if expect_continue {
        let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    let behavior = {
        let mut script = script.lock().unwrap();
        if script.len() > 1 {
            script.pop_front().unwrap()
        } else {
            script.front().unwrap().clone()
        }
    };
    if !behavior.delay.is_zero() {
        std::thread::sleep(behavior.delay);
    }
    let reason = if behavior.status == 200 {
        "OK"
    } else {
        "Error"
    };
    let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            behavior.status,
            reason,
            behavior.body.len(),
            behavior.body
        );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn start_stub(script: Vec<Behavior>) -> Stub {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
    let port = listener.local_addr().expect("stub addr").port();
    let count = Arc::new(AtomicUsize::new(0));
    let count_clone = Arc::clone(&count);
    let script = Arc::new(Mutex::new(VecDeque::from(script)));
    std::thread::spawn(move || run_stub(listener, count_clone, script));
    Stub { port, count }
}

fn temp_stem(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!(
        "{}/mimint-fallback-{}-{nanos}-{count}-{tag}",
        std::env::temp_dir().display(),
        std::process::id()
    )
}

fn provider_for(name: &str, port: u16, timeout: Duration, key: Option<&str>) -> Box<dyn Provider> {
    let client = ProviderClient::new_named(
        name,
        &format!("http://127.0.0.1:{port}"),
        "stub-model",
        key.map(|k| Zeroizing::new(k.to_string())),
        // Tests send keys to loopback stubs over http: explicit opt-in.
        true,
        GuardedHttpClient::new(EgressPolicy::privacy_on(), timeout).expect("guarded client"),
    )
    .expect("provider");
    Box::new(client)
}

fn read_audit(path: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(path).expect("read audit");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("audit line is JSON"))
        .collect()
}

fn attempts(records: &[Value]) -> Vec<&Value> {
    records
        .iter()
        .filter(|r| r.get("kind").and_then(Value::as_str) == Some("provider_attempt"))
        .collect()
}

fn fast_retry(max_attempts: usize) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        max_elapsed: Duration::from_secs(30),
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(20),
    }
}

#[tokio::test]
async fn retry_500_then_timeout_then_success() {
    let stub = start_stub(vec![
        Behavior {
            status: 500,
            delay: Duration::ZERO,
            body: "boom".to_string(),
        },
        Behavior {
            status: 200,
            delay: Duration::from_millis(1500),
            body: success_body("too slow"),
        },
        Behavior {
            status: 200,
            delay: Duration::ZERO,
            body: success_body("Fallback success."),
        },
    ]);
    let audit_path = format!("{}.jsonl", temp_stem("retry"));
    let audit = Arc::new(AuditLog::open(&audit_path).expect("open audit"));
    let chain = FallbackChain::new(
        vec![provider_for(
            "only",
            stub.port,
            Duration::from_millis(300),
            None,
        )],
        fast_retry(3),
    )
    .expect("chain");
    let ctx = CallContext::new("turn-retry", Some(audit.as_ref()));
    let message = chain
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect("third attempt succeeds");
    assert_eq!(message.content.as_deref(), Some("Fallback success."));
    assert_eq!(stub.count.load(Ordering::SeqCst), 3);

    let records = read_audit(&audit_path);
    let tries = attempts(&records);
    assert_eq!(tries.len(), 3, "every attempt must be audited");
    assert_eq!(tries[0]["decision"], "transient_error");
    assert_eq!(tries[1]["decision"], "transient_error");
    assert_eq!(tries[2]["decision"], "success");
    assert!(tries.iter().all(|r| r["detail"]["provider"] == "only"));
    assert!(tries.iter().all(|r| r["detail"]["paid"] == false));
    assert!(tries.iter().all(|r| r["turn_id"] == "turn-retry"));
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn all_providers_fail_with_clear_error_and_full_audit() {
    let first = start_stub(vec![Behavior {
        status: 500,
        delay: Duration::ZERO,
        body: "first boom".to_string(),
    }]);
    let second = start_stub(vec![Behavior {
        status: 503,
        delay: Duration::ZERO,
        body: "second boom".to_string(),
    }]);
    let audit_path = format!("{}.jsonl", temp_stem("allfail"));
    let audit = Arc::new(AuditLog::open(&audit_path).expect("open audit"));
    let chain = FallbackChain::new(
        vec![
            provider_for("first", first.port, Duration::from_secs(5), None),
            provider_for("second", second.port, Duration::from_secs(5), None),
        ],
        fast_retry(2),
    )
    .expect("chain");
    let ctx = CallContext::new("turn-allfail", Some(audit.as_ref()));
    let err = chain
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect_err("all providers must fail");
    let text = err.to_string();
    assert!(text.contains("all 2 provider(s) failed"), "{text}");
    assert!(text.contains("first"), "{text}");
    assert!(text.contains("second"), "{text}");
    assert_eq!(first.count.load(Ordering::SeqCst), 2);
    assert_eq!(second.count.load(Ordering::SeqCst), 2);

    let records = read_audit(&audit_path);
    assert_eq!(attempts(&records).len(), 4);
    assert!(records.iter().any(|r| r.get("kind").and_then(Value::as_str)
        == Some("provider_failure")
        && r.get("decision").and_then(Value::as_str) == Some("all_failed")));
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn permanent_error_skips_retry_and_moves_on() {
    let bad_key = start_stub(vec![Behavior {
        status: 401,
        delay: Duration::ZERO,
        body: json!({"error": {"message": "bad key"}}).to_string(),
    }]);
    let good = start_stub(vec![Behavior {
        status: 200,
        delay: Duration::ZERO,
        body: success_body("Second provider wins."),
    }]);
    let chain = FallbackChain::new(
        vec![
            provider_for("bad-key", bad_key.port, Duration::from_secs(5), Some("bad")),
            provider_for("good", good.port, Duration::from_secs(5), None),
        ],
        fast_retry(3),
    )
    .expect("chain");
    let ctx = CallContext::new("turn-permanent", None);
    let message = chain
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect("second provider succeeds");
    assert_eq!(message.content.as_deref(), Some("Second provider wins."));
    // The 401 was tried exactly once: permanent errors are not retried.
    assert_eq!(bad_key.count.load(Ordering::SeqCst), 1);
    assert_eq!(good.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn wall_clock_budget_bounds_total_time() {
    let slow = start_stub(vec![Behavior {
        status: 200,
        delay: Duration::from_millis(400),
        body: success_body("too slow"),
    }]);
    let chain = FallbackChain::new(
        vec![provider_for(
            "slow",
            slow.port,
            Duration::from_millis(100),
            None,
        )],
        RetryPolicy {
            max_attempts: 100,
            max_elapsed: Duration::from_millis(600),
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
        },
    )
    .expect("chain");
    let ctx = CallContext::new("turn-budget", None);
    let started = Instant::now();
    let err = chain
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect_err("budget must eventually fail");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "chain ran too long: {:?}",
        started.elapsed()
    );
    assert!(err.to_string().contains("budget exhausted"), "{err}");
    assert!(
        slow.count.load(Ordering::SeqCst) < 100,
        "attempts must be bounded by the budget"
    );
}

#[tokio::test]
async fn key_value_never_appears_in_audit() {
    let stub = start_stub(vec![Behavior {
        status: 200,
        delay: Duration::ZERO,
        body: success_body("ok"),
    }]);
    let audit_path = format!("{}.jsonl", temp_stem("canary"));
    let audit = Arc::new(AuditLog::open(&audit_path).expect("open audit"));
    let chain = FallbackChain::new(
        vec![provider_for(
            "keyed",
            stub.port,
            Duration::from_secs(5),
            Some("CANARY-KEY-9f8e7d6c"),
        )],
        fast_retry(1),
    )
    .expect("chain");
    let ctx = CallContext::new("turn-canary", Some(audit.as_ref()));
    chain
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect("turn succeeds");
    drop(chain);
    drop(audit);
    let text = std::fs::read_to_string(&audit_path).expect("read audit");
    assert!(
        !text.contains("CANARY-KEY-9f8e7d6c"),
        "audit log must never contain a key value"
    );
    assert!(text.contains("provider_attempt"), "{text}");
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn chain_is_reusable_across_turns_with_fresh_budget() {
    // Scripted [500, 200]: with max_attempts=1 the first call fails
    // consuming the 500, the second call gets a fresh budget and the
    // 200. The same chain serves two turns with different turn ids.
    let stub = start_stub(vec![
        Behavior {
            status: 500,
            delay: Duration::ZERO,
            body: "first boom".to_string(),
        },
        Behavior {
            status: 200,
            delay: Duration::ZERO,
            body: success_body("Second turn wins."),
        },
    ]);
    let audit_path = format!("{}.jsonl", temp_stem("reuse"));
    let audit = Arc::new(AuditLog::open(&audit_path).expect("open audit"));
    let chain = FallbackChain::new(
        vec![provider_for(
            "only",
            stub.port,
            Duration::from_secs(5),
            None,
        )],
        fast_retry(1),
    )
    .expect("chain");

    let ctx1 = CallContext::new("turn-reuse-1", Some(audit.as_ref()));
    let err = chain
        .chat(&ctx1, &[ChatMessage::user("hi")], &[])
        .await
        .expect_err("first call fails on the 500");
    assert!(err.to_string().contains("all 1 provider"), "{err}");

    let ctx2 = CallContext::new("turn-reuse-2", Some(audit.as_ref()));
    let message = chain
        .chat(&ctx2, &[ChatMessage::user("hi")], &[])
        .await
        .expect("second call succeeds with fresh budget");
    assert_eq!(message.content.as_deref(), Some("Second turn wins."));
    assert_eq!(stub.count.load(Ordering::SeqCst), 2);

    let records = read_audit(&audit_path);
    let tries = attempts(&records);
    assert_eq!(tries.len(), 2);
    assert_eq!(tries[0]["turn_id"], "turn-reuse-1");
    assert_eq!(tries[1]["turn_id"], "turn-reuse-2");
    let _ = std::fs::remove_file(&audit_path);
}

#[test]
fn config_errors_are_not_transient_so_chains_never_retry_them() {
    let err = MimintError::Config("bad config".to_string());
    assert!(!err.is_transient());
    let err = MimintError::ProviderTransient("timeout".to_string());
    assert!(err.is_transient());
}

#[tokio::test]
async fn progress_hook_reports_each_attempt_and_last_used_tracks_the_winner() {
    // First provider always 500s (permanent? no: 500 is transient,
    // retried once, then the chain moves on); second succeeds.
    let failing = start_stub(vec![
        Behavior {
            status: 500,
            delay: Duration::ZERO,
            body: "boom".to_string(),
        },
        Behavior {
            status: 500,
            delay: Duration::ZERO,
            body: "boom".to_string(),
        },
    ]);
    let winning = start_stub(vec![Behavior {
        status: 200,
        delay: Duration::ZERO,
        body: success_body("winner answers"),
    }]);
    let audit_path = format!("{}.jsonl", temp_stem("progress"));
    let audit = Arc::new(AuditLog::open(&audit_path).expect("open audit"));
    let mut chain = FallbackChain::new(
        vec![
            provider_for("first", failing.port, Duration::from_secs(5), None),
            provider_for("second", winning.port, Duration::from_secs(5), None),
        ],
        fast_retry(2),
    )
    .expect("chain");
    assert_eq!(chain.last_used_provider(), None);
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = Arc::clone(&seen);
    chain.set_progress_hook(Arc::new(move |name| {
        seen_clone.lock().unwrap().push(name.to_string());
    }));
    let ctx = CallContext::new("turn-progress", Some(audit.as_ref()));
    let message = chain
        .chat(&ctx, &[ChatMessage::user("hi")], &[])
        .await
        .expect("second provider answers");
    assert_eq!(message.content.as_deref(), Some("winner answers"));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["first", "first", "second"],
        "hook fires per attempt: {seen:?}"
    );
    assert_eq!(chain.last_used_provider().as_deref(), Some("second"));
    let _ = std::fs::remove_file(&audit_path);
}
