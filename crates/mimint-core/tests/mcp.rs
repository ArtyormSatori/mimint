//! MCP integration tests (§6.6): a fake MCP server (one
//! Python fixture, stdlib only) runs inside the same bwrap
//! sandbox as production servers. Tests that need a live
//! sandbox skip with a note when the preflight fails.

use mimint_core::{
    config_path_outside_workdirs, load_config_file, AuditLog, ChildProcessRegistry, McpError,
    McpManager, McpServerConfig, Risk, Sandbox, SandboxConfig, Tool, AGENT_DEFAULT, AGENT_SYSTEM,
    MCP_CATALOG_CAP_BYTES, MCP_OUTPUT_CAP_BYTES, MCP_STDERR_RING_BYTES,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_mcp_server.py")
}

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "mimint-mcp-test-{}-{}-{}",
        tag,
        std::process::id(),
        n
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// True when the sandbox preflight passes on this machine.
fn sandbox_available() -> bool {
    Sandbox::from_config(&SandboxConfig::default())
        .preflight()
        .is_ok()
}

fn server_config(
    name: &str,
    mode: &str,
    env: &[(&str, &str)],
    timeout_secs: u64,
) -> McpServerConfig {
    McpServerConfig {
        name: name.to_string(),
        command: "/usr/bin/python3".to_string(),
        args: vec![
            "-u".to_string(),
            fixture_path().to_string_lossy().into_owned(),
            mode.to_string(),
        ],
        env: env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        timeout_secs,
        unsandboxed: false,
    }
}

struct Harness {
    manager: McpManager,
    registry: ChildProcessRegistry,
    audit_path: PathBuf,
    _dir: PathBuf,
}

fn harness(configs: Vec<McpServerConfig>) -> Harness {
    let dir = temp_dir("harness");
    let audit_path = dir.join("audit.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
    let sandbox = Arc::new(Sandbox::from_config(&SandboxConfig::default()));
    let registry = ChildProcessRegistry::new();
    let manager = McpManager::new(configs, audit, sandbox, registry.clone()).unwrap();
    Harness {
        manager,
        registry,
        audit_path,
        _dir: dir,
    }
}

fn read_audit(path: &PathBuf) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

fn records_of<'a>(records: &'a [Value], kind: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|r| r["kind"].as_str() == Some(kind))
        .collect()
}

fn find_tool<'a>(tools: &'a [mimint_core::McpTool], name: &str) -> &'a mimint_core::McpTool {
    tools
        .iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("tool {name} not registered"))
}

fn stderr_text(h: &Harness, name: &str) -> String {
    let tail = h.manager.handle(name).unwrap().stderr_tail();
    String::from_utf8_lossy(&tail).into_owned()
}

/// Poll the stderr ring until `needle` appears or the
/// budget runs out: the stderr drain thread runs
/// asynchronously, so a single read right after connect
/// can miss bytes the server just wrote.
fn wait_for_stderr(h: &Harness, name: &str, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let text = stderr_text(h, name);
        if text.contains(needle) || Instant::now() >= deadline {
            return text;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn mcp_version_negotiation_matrix() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    for version in ["2025-06-18", "2025-11-25"] {
        let h = harness(vec![server_config(
            "srv",
            "echo",
            &[("FAKE_MCP_VERSION", version)],
            30,
        )]);
        let outcomes = h.manager.connect_all();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0].1,
            Ok(2),
            "version {version} must connect: {:?}",
            outcomes[0].1
        );
        assert_eq!(
            h.manager
                .handle("srv")
                .unwrap()
                .protocol_version()
                .as_deref(),
            Some(version)
        );
        h.manager.disconnect_all();
    }
    for version in ["2026-07-28", "2024-11-05", "bogus"] {
        let h = harness(vec![server_config(
            "srv",
            "echo",
            &[("FAKE_MCP_VERSION", version)],
            30,
        )]);
        let outcomes = h.manager.connect_all();
        let err = outcomes[0].1.as_ref().unwrap_err();
        assert!(
            err.contains("server yêu cầu phiên bản không được hỗ trợ"),
            "version {version}: {err}"
        );
        assert!(err.contains(version), "{err}");
        let records = read_audit(&h.audit_path);
        let failures = records_of(&records, "mcp_server_failed");
        assert_eq!(failures.len(), 1, "{records:?}");
        assert_eq!(
            failures[0]["detail"]["protocol_version"].as_str(),
            Some(version)
        );
        let supported = failures[0]["detail"]["supported_versions"]
            .as_array()
            .expect("supported_versions is an array");
        assert!(supported.contains(&json!("2025-06-18")));
        assert!(supported.contains(&json!("2025-11-25")));
        assert!(h.manager.tools_for(AGENT_DEFAULT).is_empty());
        h.manager.disconnect_all();
    }
}

#[test]
fn mcp_no_faked_handshake() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config(
        "srv",
        "stateless",
        &[("FAKE_MCP_TRACE", "1")],
        30,
    )]);
    let outcomes = h.manager.connect_all();
    assert!(outcomes[0].1.is_err(), "{:?}", outcomes[0].1);
    // The server saw initialize but never tools/list: the
    // client cannot register tools without a handshake.
    let stderr = stderr_text(&h, "srv");
    assert!(stderr.contains("SAW:initialize"), "{stderr}");
    assert!(!stderr.contains("SAW:tools/list"), "{stderr}");
    assert!(h.manager.tools_for(AGENT_DEFAULT).is_empty());
    h.manager.disconnect_all();
}

#[test]
fn mcp_schema_clamp() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let deep = {
        let mut schema = json!({"type": "string"});
        for _ in 0..8 {
            schema = json!({"type": "object", "properties": {"n": schema}});
        }
        schema
    };
    let tools = json!([
        {"name": "good", "description": "A good tool.",
         "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
        {"name": "noschema", "description": "No schema at all."},
        {"name": "ghost", "description": "Required ghost.",
         "inputSchema": {"type": "object", "properties": {}, "required": ["ghost"]}},
        {"name": "deep", "description": "Too deep.", "inputSchema": deep},
        {"name": "longdesc", "description": "x".repeat(600),
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "dirty", "description": "a\nb\tc",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "Weird Name!", "description": "Weird.",
         "inputSchema": {"type": "object", "properties": {}}},
    ]);
    let h = harness(vec![server_config(
        "srv",
        "echo",
        &[("FAKE_MCP_TOOLS", &tools.to_string())],
        30,
    )]);
    let outcomes = h.manager.connect_all();
    assert_eq!(outcomes[0].1, Ok(4), "{:?}", outcomes[0].1);
    let catalog = h.manager.handle("srv").unwrap().catalog_snapshot().unwrap();
    let names: Vec<&str> = catalog.iter().map(|t| t.full_name.as_str()).collect();
    assert!(names.contains(&"mcp_srv_good"));
    assert!(names.contains(&"mcp_srv_longdesc"));
    assert!(names.contains(&"mcp_srv_dirty"));
    assert!(names.contains(&"mcp_srv_weird_name_"));
    let long = catalog
        .iter()
        .find(|t| t.full_name == "mcp_srv_longdesc")
        .unwrap();
    assert_eq!(long.description.len(), 512);
    assert!(long.description_truncated);
    let dirty = catalog
        .iter()
        .find(|t| t.full_name == "mcp_srv_dirty")
        .unwrap();
    assert_eq!(dirty.description, "abc");
    // The description sent to the model is framed as
    // untrusted server data.
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let good = find_tool(&tools, "mcp_srv_good");
    assert!(good.description().contains("UNTRUSTED"));
    assert!(good.description().contains("srv"));
    // Three tools were rejected, each audited.
    let records = read_audit(&h.audit_path);
    assert_eq!(records_of(&records, "mcp_tool_rejected").len(), 3);
    h.manager.disconnect_all();
}

#[test]
fn mcp_catalog_caps() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    // Over the 32 KiB catalog budget: the whole server is
    // rejected, not just the overflowing tools.
    let mut tools = Vec::new();
    for i in 0..70 {
        tools.push(json!({
            "name": format!("tool{i:02}"),
            "description": "d".repeat(500),
            "inputSchema": {"type": "object", "properties": {}},
        }));
    }
    let h = harness(vec![server_config(
        "srv",
        "echo",
        &[("FAKE_MCP_TOOLS", &json!(tools).to_string())],
        30,
    )]);
    let outcomes = h.manager.connect_all();
    let err = outcomes[0].1.as_ref().unwrap_err();
    assert!(err.contains("catalog"), "{err}");
    assert!(err.contains(&MCP_CATALOG_CAP_BYTES.to_string()), "{err}");
    assert!(h.manager.tools_for(AGENT_DEFAULT).is_empty());
    h.manager.disconnect_all();

    // Two tools colliding after 64-char truncation: the
    // whole server is rejected.
    let colliding = json!([
        {"name": format!("{}a", "t".repeat(70)), "description": "one",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": format!("{}b", "t".repeat(70)), "description": "two",
         "inputSchema": {"type": "object", "properties": {}}},
    ]);
    let h = harness(vec![server_config(
        "srv",
        "echo",
        &[("FAKE_MCP_TOOLS", &colliding.to_string())],
        30,
    )]);
    let outcomes = h.manager.connect_all();
    assert!(outcomes[0].1.is_err(), "{:?}", outcomes[0].1);
    assert!(h.manager.tools_for(AGENT_DEFAULT).is_empty());
    h.manager.disconnect_all();
}

#[test]
fn mcp_pagination_and_tool_cap() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let tools = json!([
        {"name": "one", "description": "1", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "two", "description": "2", "inputSchema": {"type": "object", "properties": {}}},
        {"name": "three", "description": "3", "inputSchema": {"type": "object", "properties": {}}},
    ]);
    let h = harness(vec![server_config(
        "srv",
        "echo",
        &[
            ("FAKE_MCP_TOOLS", &tools.to_string()),
            ("FAKE_MCP_PAGE_SIZE", "2"),
        ],
        30,
    )]);
    let outcomes = h.manager.connect_all();
    assert_eq!(outcomes[0].1, Ok(3), "{:?}", outcomes[0].1);
    h.manager.disconnect_all();

    // 101 tools: over the per-server cap.
    let many: Vec<Value> = (0..101)
        .map(|i| {
            json!({"name": format!("tool{i:03}"), "description": "x",
                   "inputSchema": {"type": "object", "properties": {}}})
        })
        .collect();
    let h = harness(vec![server_config(
        "srv",
        "echo",
        &[("FAKE_MCP_TOOLS", &json!(many).to_string())],
        30,
    )]);
    let outcomes = h.manager.connect_all();
    assert!(outcomes[0].1.is_err(), "{:?}", outcomes[0].1);
    assert!(h.manager.tools_for(AGENT_DEFAULT).is_empty());
    h.manager.disconnect_all();
}

#[test]
fn mcp_recursive_args_validation() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "echo", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let echo = find_tool(&tools, "mcp_srv_echo");
    // Valid call reaches the server.
    let out = echo.execute(&json!({"text": "hello"})).unwrap();
    assert!(out["text"].as_str().unwrap().contains("echo:"));
    assert!(out["text"].as_str().unwrap().contains("hello"));
    // Wrong primitive type: rejected before sending.
    let err = echo.execute(&json!({"text": 42})).unwrap_err();
    assert!(
        err.message().contains("expected string"),
        "{}",
        err.message()
    );
    // Missing required field.
    assert!(echo.execute(&json!({})).is_err());
    // Deeper than 8 levels: rejected before sending.
    let mut deep = json!("leaf");
    for _ in 0..9 {
        deep = json!({"n": deep});
    }
    let err = echo.execute(&json!({"text": deep})).unwrap_err();
    assert!(err.message().contains("deeper"), "{}", err.message());
    h.manager.disconnect_all();
}

#[test]
fn mcp_frame_cap_256k() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "huge", &[], 30)]);
    let outcomes = h.manager.connect_all();
    assert!(outcomes[0].1.is_err(), "{:?}", outcomes[0].1);
    assert!(h.registry.is_empty(), "server must be reaped");
    assert!(h.manager.tools_for(AGENT_DEFAULT).is_empty());
    h.manager.disconnect_all();
}

#[test]
fn mcp_garbage_frame_kills_server() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "garbage", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let echo = find_tool(&tools, "mcp_srv_echo");
    assert!(echo.execute(&json!({"text": "x"})).is_err());
    assert!(h.registry.is_empty(), "server must be reaped");
    h.manager.disconnect_all();
}

#[test]
fn mcp_batch_response_rejected() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "batch", &[], 30)]);
    let outcomes = h.manager.connect_all();
    assert!(outcomes[0].1.is_err(), "{:?}", outcomes[0].1);
    assert!(h.registry.is_empty());
    h.manager.disconnect_all();
}

#[test]
fn mcp_wrong_id_kills_server() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "wrong-id", &[], 30)]);
    let outcomes = h.manager.connect_all();
    assert!(outcomes[0].1.is_err(), "{:?}", outcomes[0].1);
    assert!(h.registry.is_empty());
    h.manager.disconnect_all();
}

#[test]
fn mcp_stderr_ring_bounded() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "stderr-flood", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    let tail = h.manager.handle("srv").unwrap().stderr_tail();
    assert!(!tail.is_empty());
    assert!(
        tail.len() <= MCP_STDERR_RING_BYTES,
        "ring holds {} bytes",
        tail.len()
    );
    h.manager.disconnect_all();
}

#[test]
fn mcp_all_tools_consequential() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "echo", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    assert_eq!(tools.len(), 2);
    for tool in &tools {
        assert_eq!(tool.risk(), Risk::Consequential, "{}", tool.name());
    }
    // Without an approver, a consequential MCP tool is denied.
    let mut registry = mimint_core::Registry::new();
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    for tool in tools {
        registry.register(Box::new(tool));
    }
    let audit = Arc::new(AuditLog::open(h.audit_path.with_extension("guard.jsonl")).unwrap());
    let executor =
        mimint_core::GuardedExecutor::new(&registry, None, &audit, "turn-1", AGENT_DEFAULT);
    let err = executor
        .execute("mcp_srv_echo", &json!({"text": "hi"}))
        .unwrap_err();
    assert!(err.message().contains("no approver"), "{}", err.message());
    h.manager.disconnect_all();
}

#[test]
fn mcp_timeout_kills_server() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![
        server_config("slow", "hang", &[], 1),
        server_config("fast", "echo", &[], 30),
    ]);
    let outcomes = h.manager.connect_all();
    assert_eq!(outcomes[0].1, Ok(2), "{:?}", outcomes[0].1);
    assert_eq!(outcomes[1].1, Ok(2), "{:?}", outcomes[1].1);
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let slow = find_tool(&tools, "mcp_slow_echo");
    let started = Instant::now();
    let err = slow.execute(&json!({"text": "x"})).unwrap_err();
    let elapsed = started.elapsed();
    assert!(err.message().contains("timeout"), "{}", err.message());
    assert!(
        elapsed < Duration::from_secs(15),
        "timeout path took {elapsed:?}"
    );
    // The hung server is gone; the healthy one still works.
    assert_eq!(h.registry.pids(AGENT_SYSTEM).len(), 1);
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let fast = find_tool(&tools, "mcp_fast_echo");
    assert!(fast.execute(&json!({"text": "ok"})).is_ok());
    h.manager.disconnect_all();
    assert!(h.registry.is_empty());
}

#[test]
fn g6_9_mcp_death_isolates_single() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![
        server_config("dying", "exit-mid-call", &[], 30),
        server_config("healthy", "echo", &[], 30),
    ]);
    assert_eq!(h.manager.connect_all().len(), 2);
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let dying = find_tool(&tools, "mcp_dying_echo");
    assert!(dying.execute(&json!({"text": "x"})).is_err());
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let healthy = find_tool(&tools, "mcp_healthy_echo");
    assert!(healthy.execute(&json!({"text": "ok"})).is_ok());
    // The dead server's tools stay registered; the next
    // turn may restart it once.
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    assert!(tools.iter().any(|t| t.name() == "mcp_dying_echo"));
    h.manager.disconnect_all();
}

#[test]
fn mcp_output_caps_and_types() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "big-output", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(3));
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let flood = find_tool(&tools, "mcp_srv_flood");
    let out = flood.execute(&json!({})).unwrap();
    assert_eq!(out["truncated"], true);
    // Framing adds bytes around the capped text; the raw
    // payload inside is exactly the cap.
    let text = out["text"].as_str().unwrap();
    assert!(text.contains(&"y".repeat(MCP_OUTPUT_CAP_BYTES)));
    assert!(!text.contains(&"y".repeat(MCP_OUTPUT_CAP_BYTES + 1)));
    h.manager.disconnect_all();

    let h = harness(vec![server_config("srv", "image", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(3));
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let shot = find_tool(&tools, "mcp_srv_shot");
    let err = shot.execute(&json!({})).unwrap_err();
    assert!(
        err.message().starts_with("unsupported_content_type:"),
        "{}",
        err.message()
    );
    h.manager.disconnect_all();

    let h = harness(vec![server_config("srv", "echo", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let fail = find_tool(&tools, "mcp_srv_fail");
    let err = fail.execute(&json!({})).unwrap_err();
    assert!(
        err.message().contains("deliberate failure"),
        "{}",
        err.message()
    );
    h.manager.disconnect_all();
}

#[test]
fn mcp_sampling_rejected() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    for (mode, needle) in [("sampling", "-32601"), ("roots", "-32601")] {
        let h = harness(vec![server_config("srv", mode, &[], 30)]);
        assert_eq!(h.manager.connect_all()[0].1, Ok(2), "mode {mode}");
        let stderr = wait_for_stderr(&h, "srv", "RESPONSE:");
        assert!(
            stderr.contains("RESPONSE:"),
            "mode {mode}: no response logged: {stderr}"
        );
        assert!(
            stderr.contains(needle),
            "mode {mode}: expected {needle} in {stderr}"
        );
        h.manager.disconnect_all();
    }
    // ping gets a pong, not an error.
    let h = harness(vec![server_config("srv", "ping", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    let stderr = wait_for_stderr(&h, "srv", "RESPONSE:");
    assert!(stderr.contains("RESPONSE:"), "{stderr}");
    assert!(stderr.contains("\"result\":{}"), "{stderr}");
    assert!(!stderr.contains("-32601"), "{stderr}");
    h.manager.disconnect_all();
}

#[test]
fn mcp_list_changed_is_ignored() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "list-changed", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    assert!(
        h.manager
            .handle("srv")
            .unwrap()
            .unknown_notification_count()
            >= 1,
        "list_changed must be counted, not reloaded mid-turn"
    );
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    assert!(find_tool(&tools, "mcp_srv_echo")
        .execute(&json!({"text": "ok"}))
        .is_ok());
    h.manager.disconnect_all();
}

#[test]
fn mcp_no_ambient_secrets() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    // A sentinel in the parent process environment must
    // never reach the server child.
    std::env::set_var(
        "MIMINT_MCP_PROBE_SECRET",
        format!("probe-secret-{}", std::process::id()),
    );
    let h = harness(vec![server_config(
        "srv",
        "env-dump",
        &[("FAKE_MCP_DECLARED", "yes")],
        30,
    )]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(3));
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    let dump = find_tool(&tools, "mcp_srv_dump_env");
    let out = dump.execute(&json!({})).unwrap();
    let text = out["text"].as_str().unwrap();
    assert!(text.contains("\"FAKE_MCP_DECLARED\": \"yes\""), "{text}");
    assert!(text.contains("\"HOME\": \"/work\""), "{text}");
    assert!(text.contains("\"PATH\""), "{text}");
    assert!(!text.contains("probe-secret-"), "{text}");
    assert!(!text.contains("/home/adminis"), "{text}");
    h.manager.disconnect_all();
}

#[test]
fn mcp_unsandboxed_linux_refused() {
    // Configuration level: unsandboxed=true on Linux is a
    // configuration error, caught at validation.
    let dir = temp_dir("unsandboxed");
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        r#"
[[mcp-server]]
name = "srv"
command = "/usr/bin/python3"
unsandboxed = true
"#,
    )
    .unwrap();
    let loaded = load_config_file(&path, true).unwrap();
    let err = loaded.validate().unwrap_err();
    assert!(err.message().contains("unsandboxed"), "{}", err.message());
    // Connection level: a handle built with unsandboxed=true
    // refuses to connect on Linux even if validation is
    // bypassed.
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let mut config = server_config("srv", "echo", &[], 30);
    config.unsandboxed = true;
    let h = harness(vec![config]);
    let outcomes = h.manager.connect_all();
    let err = outcomes[0].1.as_ref().unwrap_err();
    assert!(err.contains("unsandboxed"), "{err}");
    assert!(h.registry.is_empty());
    h.manager.disconnect_all();
}

#[test]
fn mcp_unsandboxed_requires_server_confirmation() {
    // Pure gate (§6.3, D-DESIGN-2): on macOS/Windows the
    // connection is refused until the server-level
    // confirmation exists; on Linux it is refused always.
    assert!(mimint_core::unsandboxed_connect_allowed("macos", true, false).is_err());
    assert!(mimint_core::unsandboxed_connect_allowed("windows", true, false).is_err());
    assert!(mimint_core::unsandboxed_connect_allowed("macos", true, true).is_ok());
    assert!(mimint_core::unsandboxed_connect_allowed("windows", true, true).is_ok());
    assert!(mimint_core::unsandboxed_connect_allowed("linux", true, true).is_err());
}

#[test]
fn mcp_config_file_out_of_tool_reach() {
    let home = PathBuf::from("/home/tester");
    assert!(config_path_outside_workdirs(
        &PathBuf::from("/home/tester/.config/mimint/config.toml"),
        &home
    ));
    assert!(!config_path_outside_workdirs(
        &PathBuf::from("/home/tester/.mimint-code-runs/x/config.toml"),
        &home
    ));
    assert!(!config_path_outside_workdirs(
        &PathBuf::from("/home/tester/.mimint-mcp-runs/x/config.toml"),
        &home
    ));
}

#[test]
fn mcp_restart_budget_one_per_turn() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "echo", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    let handle = h.manager.handle("srv").unwrap();
    handle.kill_server();
    // Same turn: no retry.
    let err = handle
        .ensure_connected()
        .err()
        .expect("same-turn retry must be refused");
    assert!(
        matches!(err, McpError::RestartRefused),
        "expected RestartRefused, got {err}"
    );
    // Next turn: exactly one retry, which succeeds.
    h.manager.begin_turn("turn-2");
    assert!(handle.ensure_connected().is_ok());
    let records = read_audit(&h.audit_path);
    assert_eq!(records_of(&records, "mcp_server_connected").len(), 2);
    h.manager.disconnect_all();
}

#[test]
fn mcp_server_connected_audit() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    let h = harness(vec![server_config("srv", "echo", &[], 30)]);
    assert_eq!(h.manager.connect_all()[0].1, Ok(2));
    let records = read_audit(&h.audit_path);
    let connected = records_of(&records, "mcp_server_connected");
    assert_eq!(connected.len(), 1);
    let detail = &connected[0]["detail"];
    assert_eq!(detail["server_name"].as_str(), Some("fake"));
    assert_eq!(detail["server_version"].as_str(), Some("1.0"));
    assert_eq!(detail["protocol_version"].as_str(), Some("2025-06-18"));
    assert_eq!(detail["tool_count"], json!(2));
    assert_eq!(detail["sandboxed"], json!(true));
    assert_eq!(connected[0]["agent_id"].as_str(), Some(AGENT_SYSTEM));
    h.manager.disconnect_all();
}

#[test]
fn mcp_name_collision_between_servers_rejects_later() {
    if !sandbox_available() {
        eprintln!("SKIP: sandbox preflight failed on this machine");
        return;
    }
    // Server names whose truncated tool names collide
    // across servers, without colliding within either
    // server: A ("a"*56) + "echo" truncates to
    // "mcp_" + "a"*56 + "_ech"; B ("a"*56 + "_ech") +
    // "solo" truncates to the same 64 chars. B carries a
    // single tool so its own catalog is collision-free
    // and the cross-server check is what rejects it.
    let name_a = "a".repeat(56);
    let name_b = format!("{}_ech", "a".repeat(56));
    let solo = json!([{"name": "solo", "description": "one",
         "inputSchema": {"type": "object", "properties": {}}}]);
    let h = harness(vec![
        server_config(&name_a, "echo", &[], 30),
        server_config(
            &name_b,
            "echo",
            &[("FAKE_MCP_TOOLS", &solo.to_string())],
            30,
        ),
    ]);
    let outcomes = h.manager.connect_all();
    assert_eq!(outcomes[0].1, Ok(2), "{:?}", outcomes[0].1);
    assert!(outcomes[1].1.is_err(), "{:?}", outcomes[1].1);
    let tools = h.manager.tools_for(AGENT_DEFAULT);
    assert_eq!(tools.len(), 2);
    h.manager.disconnect_all();
}

#[test]
fn mcp_manager_rejects_ninth_server() {
    let dir = temp_dir("ninth");
    let audit = Arc::new(AuditLog::open(dir.join("audit.jsonl")).unwrap());
    let sandbox = Arc::new(Sandbox::from_config(&SandboxConfig::default()));
    let configs: Vec<McpServerConfig> = (0..9)
        .map(|i| server_config(&format!("srv{i}"), "echo", &[], 30))
        .collect();
    let err = McpManager::new(configs, audit, sandbox, ChildProcessRegistry::new())
        .err()
        .expect("ninth server must be rejected");
    assert!(err.message().contains('9'), "{}", err.message());
}
