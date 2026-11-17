//! Guard tests: deny-by-default enforcement (RED first, then GREEN).
//!
//! These tests are written before enforcement exists. With the unguarded
//! executor they must FAIL with real assertion failures (not compile
//! errors); with enforcement they must pass.

use mimint_core::{default_registry, Approver, AuditLog, GuardedExecutor, MimintError, Notebook};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

struct AllowApprover;
impl Approver for AllowApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(true)
    }
    fn label(&self) -> &'static str {
        "test allow"
    }
}

struct DenyApprover;
impl Approver for DenyApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(false)
    }
    fn label(&self) -> &'static str {
        "test deny"
    }
}

struct ErrorApprover;
impl Approver for ErrorApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Err(MimintError::Config("approver exploded".to_string()))
    }
    fn label(&self) -> &'static str {
        "test error"
    }
}

fn temp_stem(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}/mimint-guard-{}-{}-{tag}",
        std::env::temp_dir().display(),
        std::process::id(),
        nanos
    )
}

fn setup(tag: &str) -> (Arc<Notebook>, AuditLog, PathBuf, PathBuf) {
    let stem = temp_stem(tag);
    let db_path = PathBuf::from(format!("{stem}.db"));
    let audit_path = PathBuf::from(format!("{stem}.jsonl"));
    let notebook = Arc::new(Notebook::open(&db_path).expect("open notebook"));
    let audit = AuditLog::open(&audit_path).expect("open audit");
    (notebook, audit, db_path, audit_path)
}

fn cleanup(db_path: &PathBuf, audit_path: &PathBuf) {
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(audit_path);
}

fn audit_records(audit_path: &PathBuf) -> Vec<Value> {
    let text = std::fs::read_to_string(audit_path).expect("read audit log");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("audit line is JSON"))
        .collect()
}

fn has_decision(records: &[Value], tool: &str, decision: &str) -> bool {
    records.iter().any(|record| {
        record.get("kind").and_then(Value::as_str) == Some("decision")
            && record.get("tool").and_then(Value::as_str) == Some(tool)
            && record.get("decision").and_then(Value::as_str) == Some(decision)
    })
}

#[test]
fn consequential_without_approval_is_denied_and_db_unchanged() {
    let (notebook, audit, db_path, audit_path) = setup("no-approver");
    let registry = default_registry(Arc::clone(&notebook));
    let executor = GuardedExecutor::new(&registry, None, &audit, "turn-guard-a");
    let before = notebook.count().unwrap();
    let result = executor.execute("notebook_add", &json!({"title": "x", "body": "y"}));
    assert!(
        result.is_err(),
        "consequential tool without approver must be denied"
    );
    assert_eq!(
        notebook.count().unwrap(),
        before,
        "denied tool must not change the DB"
    );
    let records = audit_records(&audit_path);
    assert!(
        has_decision(&records, "notebook_add", "deny"),
        "deny decision must be audited: {records:?}"
    );
    cleanup(&db_path, &audit_path);
}

#[test]
fn unknown_tool_is_denied_and_audited() {
    let (notebook, audit, db_path, audit_path) = setup("unknown-tool");
    let registry = default_registry(Arc::clone(&notebook));
    let approver = AllowApprover;
    let executor = GuardedExecutor::new(&registry, Some(&approver), &audit, "turn-guard-b");
    let result = executor.execute("no_such_tool", &json!({}));
    assert!(result.is_err(), "unknown tool must be denied");
    let records = audit_records(&audit_path);
    assert!(
        has_decision(&records, "no_such_tool", "deny"),
        "unknown-tool deny must be audited: {records:?}"
    );
    cleanup(&db_path, &audit_path);
}

#[test]
fn refused_approval_is_denied_and_audited() {
    let (notebook, audit, db_path, audit_path) = setup("refused");
    let registry = default_registry(Arc::clone(&notebook));
    let approver = DenyApprover;
    let executor = GuardedExecutor::new(&registry, Some(&approver), &audit, "turn-guard-c");
    let before = notebook.count().unwrap();
    let result = executor.execute("notebook_add", &json!({"title": "x", "body": "y"}));
    assert!(result.is_err(), "refused approval must deny");
    assert_eq!(notebook.count().unwrap(), before);
    let records = audit_records(&audit_path);
    assert!(
        has_decision(&records, "notebook_add", "deny"),
        "refused deny must be audited: {records:?}"
    );
    cleanup(&db_path, &audit_path);
}

#[test]
fn approver_error_denies() {
    let (notebook, audit, db_path, audit_path) = setup("approver-error");
    let registry = default_registry(Arc::clone(&notebook));
    let approver = ErrorApprover;
    let executor = GuardedExecutor::new(&registry, Some(&approver), &audit, "turn-guard-e");
    let before = notebook.count().unwrap();
    let result = executor.execute("notebook_add", &json!({"title": "x", "body": "y"}));
    assert!(result.is_err(), "approver error must deny");
    assert_eq!(notebook.count().unwrap(), before);
    cleanup(&db_path, &audit_path);
}

#[test]
fn read_only_tools_run_without_approval() {
    let (notebook, audit, db_path, audit_path) = setup("read-only");
    let id = notebook.add("Seed", "seed body").unwrap();
    let registry = default_registry(Arc::clone(&notebook));
    let executor = GuardedExecutor::new(&registry, None, &audit, "turn-guard-d");
    let found = executor
        .execute("notebook_search", &json!({"query": "Seed"}))
        .expect("read-only search must run without approval");
    assert_eq!(found["results"].as_array().map(Vec::len), Some(1));
    let read = executor
        .execute("notebook_read", &json!({"id": id}))
        .expect("read-only read must run without approval");
    assert_eq!(read["note"]["title"], "Seed");
    cleanup(&db_path, &audit_path);
}

#[test]
fn approved_consequential_tool_runs_and_is_audited() {
    let (notebook, audit, db_path, audit_path) = setup("approved");
    let registry = default_registry(Arc::clone(&notebook));
    let approver = AllowApprover;
    let executor = GuardedExecutor::new(&registry, Some(&approver), &audit, "turn-guard-f");
    let output = executor
        .execute("notebook_add", &json!({"title": "ok", "body": "b"}))
        .expect("approved tool must run");
    assert!(output.get("id").and_then(Value::as_i64).is_some());
    assert_eq!(notebook.count().unwrap(), 1);
    let records = audit_records(&audit_path);
    assert!(
        has_decision(&records, "notebook_add", "allow"),
        "allow decision must be audited: {records:?}"
    );
    cleanup(&db_path, &audit_path);
}
