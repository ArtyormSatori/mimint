//! Guard tests: deny-by-default enforcement (RED first, then GREEN).
//!
//! These tests are written before enforcement exists. With the unguarded
//! executor they must FAIL with real assertion failures (not compile
//! errors); with enforcement they must pass.

use mimint_core::{
    default_registry, ApprovalDecision, ApprovalOutcome, ApprovalRequest, Approver, AuditLog,
    CancelToken, GuardedExecutor, MimintError, Notebook,
};
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

/// Approver returning one fixed decision through `approve_request`.
struct FixedOutcomeApprover {
    decision: ApprovalDecision,
}

impl Approver for FixedOutcomeApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(self.decision == ApprovalDecision::Allow)
    }

    fn approve_request(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome, MimintError> {
        Ok(ApprovalOutcome {
            request_id: request.id,
            agent_id: request.agent_id.clone(),
            decision: self.decision,
        })
    }

    fn label(&self) -> &'static str {
        "test fixed outcome"
    }
}

/// Approver answering with the WRONG request id.
struct WrongIdApprover;
impl Approver for WrongIdApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(true)
    }

    fn approve_request(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome, MimintError> {
        Ok(ApprovalOutcome::allow(
            request.id + 100,
            request.agent_id.clone(),
        ))
    }
}

/// Approver that allows — but cancels the shared token while
/// deciding, simulating STOP pressed after the card appeared and
/// before execution.
struct CancellingAllowApprover {
    token: CancelToken,
}

impl Approver for CancellingAllowApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(true)
    }

    fn approve_request(&self, request: &ApprovalRequest) -> Result<ApprovalOutcome, MimintError> {
        self.token.cancel();
        Ok(ApprovalOutcome::allow(request.id, request.agent_id.clone()))
    }
}

#[test]
fn consequential_without_approval_is_denied_and_db_unchanged() {
    let (notebook, audit, db_path, audit_path) = setup("no-approver");
    let registry = default_registry(Arc::clone(&notebook));
    let executor = GuardedExecutor::new(&registry, None, &audit, "turn-guard-a", "agent-001");
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
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-b",
        "agent-001",
    );
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
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-c",
        "agent-001",
    );
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
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-e",
        "agent-001",
    );
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
    let executor = GuardedExecutor::new(&registry, None, &audit, "turn-guard-d", "agent-001");
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
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-f",
        "agent-001",
    );
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

#[test]
fn timeout_is_audited_distinct_from_deny_and_does_not_run() {
    let (notebook, audit, db_path, audit_path) = setup("timeout");
    let registry = default_registry(Arc::clone(&notebook));
    let approver = FixedOutcomeApprover {
        decision: ApprovalDecision::Timeout,
    };
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-timeout",
        "agent-001",
    );
    let result = executor.execute("notebook_add", &json!({"title": "x", "body": "y"}));
    assert!(result.is_err(), "timeout must deny");
    assert_eq!(notebook.count().unwrap(), 0);
    let records = audit_records(&audit_path);
    assert!(
        has_decision(&records, "notebook_add", "timeout"),
        "timeout must be audited as its own decision: {records:?}"
    );
    assert!(
        records
            .iter()
            .any(|r| r["detail"]["reason"] == "approval_timeout"),
        "timeout reason must be approval_timeout: {records:?}"
    );
    cleanup(&db_path, &audit_path);
}

#[test]
fn cancelled_approval_is_audited_distinct_from_deny_and_does_not_run() {
    let (notebook, audit, db_path, audit_path) = setup("cancelled");
    let registry = default_registry(Arc::clone(&notebook));
    let approver = FixedOutcomeApprover {
        decision: ApprovalDecision::Cancelled,
    };
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-cancelled",
        "agent-001",
    );
    let err = executor
        .execute("notebook_add", &json!({"title": "x", "body": "y"}))
        .expect_err("cancelled approval must not run");
    assert!(
        err.is_cancelled(),
        "cancelled outcome must surface as Cancelled: {err}"
    );
    assert_eq!(notebook.count().unwrap(), 0);
    let records = audit_records(&audit_path);
    assert!(
        has_decision(&records, "notebook_add", "cancelled"),
        "cancelled must be audited as its own decision: {records:?}"
    );
    cleanup(&db_path, &audit_path);
}

#[test]
fn mismatched_request_id_is_denied_and_audited() {
    let (notebook, audit, db_path, audit_path) = setup("mismatch");
    let registry = default_registry(Arc::clone(&notebook));
    let approver = WrongIdApprover;
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-mismatch",
        "agent-001",
    );
    let result = executor.execute("notebook_add", &json!({"title": "x", "body": "y"}));
    assert!(result.is_err(), "wrong-id decision must deny");
    assert_eq!(notebook.count().unwrap(), 0);
    let records = audit_records(&audit_path);
    assert!(
        records
            .iter()
            .any(|r| r["detail"]["reason"] == "approval_id_mismatch"),
        "mismatch must be audited: {records:?}"
    );
    cleanup(&db_path, &audit_path);
}

#[test]
fn g6_2_cross_agent_replay_refused() {
    let (notebook, audit, db_path, audit_path) = setup("mismatch-agent");
    let registry = default_registry(Arc::clone(&notebook));
    // An Allow carrying another agent's id must deny, exactly
    // like a wrong request id: the pair is the binding.
    struct WrongAgentApprover;
    impl Approver for WrongAgentApprover {
        fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
            Ok(true)
        }

        fn approve_request(
            &self,
            request: &ApprovalRequest,
        ) -> Result<ApprovalOutcome, MimintError> {
            Ok(ApprovalOutcome::allow(request.id, "agent-999".to_string()))
        }

        fn label(&self) -> &'static str {
            "test wrong agent"
        }
    }
    let approver = WrongAgentApprover;
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-mismatch-agent",
        "agent-001",
    );
    let result = executor.execute("notebook_add", &json!({"title": "x", "body": "y"}));
    assert!(result.is_err(), "wrong-agent decision must deny");
    assert_eq!(notebook.count().unwrap(), 0);
    let records = audit_records(&audit_path);
    let mismatch = records
        .iter()
        .find(|r| r["detail"]["reason"] == "approval_id_mismatch")
        .expect("mismatch must be audited");
    assert_eq!(mismatch["detail"]["outcome_agent_id"], "agent-999");
    cleanup(&db_path, &audit_path);
}

#[test]
fn stop_after_approval_blocks_execution_and_audits_cancelled() {
    let (notebook, audit, db_path, audit_path) = setup("stop-after-approval");
    let registry = default_registry(Arc::clone(&notebook));
    let token = CancelToken::new();
    let approver = CancellingAllowApprover {
        token: token.clone(),
    };
    let mut executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-stop",
        "agent-001",
    );
    executor.add_cancel_token(token);
    let err = executor
        .execute("notebook_add", &json!({"title": "x", "body": "y"}))
        .expect_err("approved-then-stopped tool must not run");
    assert!(err.is_cancelled(), "{err}");
    assert_eq!(notebook.count().unwrap(), 0, "tool must not have run");
    let records = audit_records(&audit_path);
    assert!(
        records
            .iter()
            .any(|r| r["detail"]["reason"] == "cancelled_after_approval"),
        "cancelled_after_approval must be audited: {records:?}"
    );
    assert!(
        !has_decision(&records, "notebook_add", "allow"),
        "no allow may be recorded for a stopped tool: {records:?}"
    );
    cleanup(&db_path, &audit_path);
}

#[test]
fn pre_cancelled_token_blocks_before_the_card_and_before_read_only_tools() {
    let (notebook, audit, db_path, audit_path) = setup("pre-cancelled");
    let id = notebook.add("Seed", "seed body").unwrap();
    let registry = default_registry(Arc::clone(&notebook));
    let token = CancelToken::new();
    token.cancel();
    let approver = AllowApprover;
    let mut executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-precancel",
        "agent-001",
    );
    executor.add_cancel_token(token);
    let err = executor
        .execute("notebook_add", &json!({"title": "x", "body": "y"}))
        .expect_err("pre-cancelled consequential tool must not run");
    assert!(err.is_cancelled(), "{err}");
    assert_eq!(notebook.count().unwrap(), 1);
    let err = executor
        .execute("notebook_read", &json!({"id": id}))
        .expect_err("pre-cancelled read-only tool must not run either");
    assert!(err.is_cancelled(), "{err}");
    cleanup(&db_path, &audit_path);
}

#[test]
fn request_ids_increase_across_requests() {
    let (notebook, audit, db_path, audit_path) = setup("request-ids");
    let registry = default_registry(Arc::clone(&notebook));
    let approver = AllowApprover;
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-guard-ids",
        "agent-001",
    );
    executor
        .execute("notebook_add", &json!({"title": "a", "body": "b"}))
        .unwrap();
    executor
        .execute("notebook_add", &json!({"title": "c", "body": "d"}))
        .unwrap();
    let records = audit_records(&audit_path);
    let decision_ids: Vec<u64> = records
        .iter()
        .filter(|r| r["kind"].as_str() == Some("decision"))
        .filter_map(|r| r["detail"]["request_id"].as_u64())
        .collect();
    assert_eq!(
        decision_ids,
        vec![1, 2],
        "decision request ids must be 1, 2 in order: {decision_ids:?}"
    );
    // Execution entries bind to their decision's id (Phase 4):
    // the trail pairs decision↔execution without guessing.
    let execution_ids: Vec<u64> = records
        .iter()
        .filter(|r| r["kind"].as_str() == Some("execution"))
        .filter_map(|r| r["detail"]["request_id"].as_u64())
        .collect();
    assert_eq!(
        execution_ids,
        vec![1, 2],
        "execution entries must carry their decision's request id: {execution_ids:?}"
    );
    cleanup(&db_path, &audit_path);
}
