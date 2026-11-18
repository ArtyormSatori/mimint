//! Sandbox proof tests (§5.6): the real `Sandbox` code runs
//! the proof cases on vega. When preflight fails (no bwrap),
//! the sandbox tests SKIP with a printed note — they never
//! pass by running code unsandboxed.

use mimint_core::{
    sha256_hex, Approver, AuditLog, CancelToken, ChildProcessRegistry, CodeRunRequest,
    ExecuteCodeTool, GuardedExecutor, MimintError, Notebook, Registry, RunDirTracker, Sandbox,
    SandboxConfig, SandboxPolicy, Tool,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn default_policy() -> SandboxPolicy {
    SandboxPolicy::from_config(&SandboxConfig::default())
}

fn temp_stem(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}/mimint-sandbox-{}-{}-{}",
        std::env::temp_dir().display(),
        tag,
        std::process::id(),
        nanos
    )
}

/// A sandbox whose preflight passed, or `None` (skip) with a
/// printed reason when the machine cannot sandbox.
fn available_sandbox(policy: SandboxPolicy) -> Option<Sandbox> {
    let sandbox = Sandbox::new(policy);
    match sandbox.preflight() {
        Ok(()) => Some(sandbox),
        Err(reason) => {
            eprintln!("SKIP: sandbox preflight failed ({reason}); machine cannot sandbox");
            None
        }
    }
}

fn run_request(language: &str, code: &str) -> CodeRunRequest {
    CodeRunRequest {
        language: language.to_string(),
        code: code.to_string(),
        inputs: Vec::new(),
        agent_id: "agent-001".to_string(),
        cancel_tokens: Vec::new(),
        registry: None,
        kill_ledger: None,
        run_dirs: None,
    }
}

#[test]
fn sandbox_proof_on_vega() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let stem = temp_stem("proof");
    let tag = stem.rsplit('/').next().unwrap();
    let home = std::env::var("HOME").expect("HOME set");
    let home_sentinel = format!("{home}/.mimint-proof-{tag}");
    let tmp_sentinel = format!("{stem}-tmp");
    std::fs::write(&home_sentinel, "home-secret").unwrap();
    std::fs::write(&tmp_sentinel, "tmp-secret").unwrap();
    // The sentinel paths contain only [a-z0-9._/-] (generated
    // names), so embedding them in double quotes is safe.
    let code = format!(
        r#"
import socket, os
try:
    open("{home_sentinel}", "r"); print("HOME_VISIBLE")
except Exception:
    print("HOME_DENIED")
try:
    open("{tmp_sentinel}", "r"); print("TMP_VISIBLE")
except Exception:
    print("TMP_DENIED")
print("X11_ABSENT" if not os.path.exists("/tmp/.X11-unix") else "X11_PRESENT")
open("/work/wrote.txt", "w").write("hello")
print("WORK_WRITE_OK" if open("/work/wrote.txt").read() == "hello" else "WORK_BAD")
try:
    s = socket.create_connection(("1.1.1.1", 80), timeout=3); s.close(); print("NET_OPEN")
except OSError as e:
    print("NET_DENIED", e.errno)
"#,
    );
    let output = sandbox
        .run(run_request("python3", &code))
        .expect("proof run executes");
    let _ = std::fs::remove_file(&home_sentinel);
    let _ = std::fs::remove_file(&tmp_sentinel);
    assert_eq!(output.exit_code, Some(0), "stderr: {}", output.stderr);
    assert!(output.stdout.contains("HOME_DENIED"), "{}", output.stdout);
    assert!(output.stdout.contains("TMP_DENIED"), "{}", output.stdout);
    assert!(output.stdout.contains("X11_ABSENT"), "{}", output.stdout);
    assert!(output.stdout.contains("WORK_WRITE_OK"), "{}", output.stdout);
    // 101 = ENETUNREACH: the namespace has no route out.
    assert!(
        output.stdout.contains("NET_DENIED 101"),
        "network must be unreachable, got: {}",
        output.stdout
    );
    assert_eq!(output.sandbox_verdict, "bwrap");
    let artifact = output
        .artifacts
        .iter()
        .find(|a| a.name == "wrote.txt")
        .expect("wrote.txt listed as an artifact");
    assert!(artifact.sha256.is_some());
}

#[test]
fn sandbox_tmpfs_size_enforced() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let home = std::env::var("HOME").expect("HOME set");
    let code = format!(
        "mount | grep -F ' on {home} ' ; mount | grep -F ' on /tmp ' ; dd if=/dev/zero of=/tmp/big bs=1M count=80 2>/dev/null; echo DD_EXIT=$?; stat -c %s /tmp/big"
    );
    let output = sandbox.run(run_request("sh", &code)).expect("run executes");
    assert_eq!(output.exit_code, Some(0), "stderr: {}", output.stderr);
    // Both tmpfs mounts (HOME and /tmp) carry size=65536k
    // (64 MiB). Other tmpfs mounts (/dev) may also appear.
    let home_mount = output
        .stdout
        .lines()
        .find(|l| l.contains(&format!(" on {home} ")) && l.contains("type tmpfs"));
    let tmp_mount = output
        .stdout
        .lines()
        .find(|l| l.contains(" on /tmp ") && l.contains("type tmpfs"));
    for (what, line) in [("HOME", home_mount), ("/tmp", tmp_mount)] {
        let line = line.unwrap_or_else(|| panic!("{what} tmpfs mount missing: {}", output.stdout));
        assert!(
            line.contains("size=65536k"),
            "{what} tmpfs mount must be 64 MiB: {line}"
        );
    }
    assert!(
        !output.stdout.contains("DD_EXIT=0"),
        "80 MiB write into a 64 MiB tmpfs must fail: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("67108864"),
        "file must stop at exactly 64 MiB: {}",
        output.stdout
    );
}

#[test]
fn sandbox_rlimits_enforced() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    // The configured limits are visible inside, and NPROC is
    // untouched (identical inside and outside).
    let limits = sandbox
        .run(run_request(
            "sh",
            "grep -E 'Max (file size|address space|processes)' /proc/self/limits",
        ))
        .expect("limits run executes");
    assert!(limits.stdout.contains("67108864"), "{}", limits.stdout);
    assert!(limits.stdout.contains("1073741824"), "{}", limits.stdout);
    let outside = std::fs::read_to_string("/proc/self/limits").unwrap();
    let outside_nproc = outside
        .lines()
        .find(|l| l.contains("Max processes"))
        .expect("outside NPROC line");
    let inside_nproc = limits
        .stdout
        .lines()
        .find(|l| l.contains("Max processes"))
        .expect("inside NPROC line");
    assert_eq!(
        inside_nproc.trim(),
        outside_nproc.trim(),
        "NPROC must be inherited untouched, never set before bwrap"
    );
    // Enforcement: FSIZE caps the write, AS caps allocation.
    let code = r#"
import os
try:
    with open("/work/big", "wb") as f:
        f.write(b"\0" * (80 * 1024 * 1024))
    print("WRITE_UNCAPPED")
except OSError as e:
    print("WRITE_CAPPED", e.errno)
print("BIG_SIZE", os.path.getsize("/work/big"))
try:
    x = bytearray(2 * 1024 * 1024 * 1024)
    print("ALLOC_UNCAPPED", len(x))
except MemoryError:
    print("ALLOC_CAPPED")
"#;
    let output = sandbox
        .run(run_request("python3", code))
        .expect("enforcement run executes");
    assert_eq!(output.exit_code, Some(0), "stderr: {}", output.stderr);
    assert!(output.stdout.contains("WRITE_CAPPED"), "{}", output.stdout);
    assert!(
        output.stdout.contains("BIG_SIZE 67108864"),
        "{}",
        output.stdout
    );
    assert!(output.stdout.contains("ALLOC_CAPPED"), "{}", output.stdout);
}

#[test]
fn sandbox_child_fd_hygiene() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    // The parent holds an open fd on a sensitive file while
    // spawning (the E2 hazard): the child must not see it.
    let stem = temp_stem("fd");
    let sentinel_path = format!("{stem}-secret");
    std::fs::write(&sentinel_path, "do-not-read").unwrap();
    let _held = std::fs::File::open(&sentinel_path).unwrap();
    let code = r#"
import os
for fd in sorted(os.listdir("/proc/self/fd")):
    try:
        print(fd, os.readlink("/proc/self/fd/" + fd))
    except OSError:
        pass
"#;
    let output = sandbox
        .run(run_request("python3", code))
        .expect("fd run executes");
    let _ = std::fs::remove_file(&sentinel_path);
    assert_eq!(output.exit_code, Some(0), "stderr: {}", output.stderr);
    assert!(
        !output.stdout.contains("secret"),
        "no fd may point at the sentinel: {}",
        output.stdout
    );
    let fd_count = output.stdout.lines().count();
    assert!(
        fd_count <= 6,
        "child must see only stdio + transient fds, saw {fd_count}: {}",
        output.stdout
    );
}

#[test]
fn sandbox_preflight_uses_new_flags() {
    // A fake bwrap that refuses --new-session: preflight
    // runs the full flag set, so it must fail, and every run
    // must then deny fail-closed.
    let stem = temp_stem("fakebwrap");
    let dir = format!("{stem}-dir");
    std::fs::create_dir_all(&dir).unwrap();
    let fake = format!("{dir}/bwrap");
    std::fs::write(
        &fake,
        "#!/bin/sh\nfor a in \"$@\"; do if [ \"$a\" = \"--new-session\" ]; then exit 1; fi; done\nexec /usr/bin/bwrap \"$@\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let policy = default_policy().with_bwrap_path(fake);
    let sandbox = Sandbox::new(policy);
    let err = sandbox.preflight().expect_err("preflight must fail");
    assert!(err.contains("sandbox_unavailable"), "{err}");
    let err = sandbox
        .run(run_request("sh", "true"))
        .expect_err("run must deny");
    assert!(err.message().starts_with("sandbox_unavailable:"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn execute_code_refuses_without_sandbox() {
    let registry = ChildProcessRegistry::new();
    let tracker = Arc::new(RunDirTracker::new());
    let policy = default_policy().with_bwrap_path("/nonexistent/bwrap");
    let sandbox = Arc::new(Sandbox::new(policy));
    let tool = ExecuteCodeTool::new(
        "agent-001",
        Arc::clone(&sandbox),
        Arc::clone(&tracker),
        registry.clone(),
    );
    let err = tool
        .execute(&json!({"language": "sh", "code": "id"}))
        .expect_err("missing bwrap must deny");
    assert!(err.message().starts_with("sandbox_unavailable:"), "{err}");
    assert!(registry.is_empty(), "no child may have been spawned");
    assert!(tracker.is_empty(), "no workdir may have been created");
    assert!(sandbox.preflight_status().is_some_and(|r| r.is_err()));
}

#[test]
fn execute_code_timeout_kills_process_group() {
    let Some(sandbox) = available_sandbox({
        let mut policy = default_policy();
        policy.timeout = Duration::from_secs(1);
        policy
    }) else {
        return;
    };
    let registry = ChildProcessRegistry::new();
    let ledger = mimint_core::KillLedger::new();
    let started = Instant::now();
    let err = sandbox
        .run(CodeRunRequest {
            language: "sh".to_string(),
            code: "sleep 120".to_string(),
            inputs: Vec::new(),
            agent_id: "agent-001".to_string(),
            cancel_tokens: Vec::new(),
            registry: Some(registry.clone()),
            kill_ledger: Some(ledger.clone()),
            run_dirs: None,
        })
        .expect_err("sleep 120 must time out");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "timeout + kill must be quick, took {:?}",
        started.elapsed()
    );
    let message = err.message().to_string();
    assert!(err.message().starts_with("timeout:"), "{message}");
    assert!(
        message.contains("strays=0"),
        "no process may survive in the namespace: {message}"
    );
    assert!(registry.is_empty(), "registry must be clear after kill");
    assert!(
        !ledger.entries().is_empty(),
        "the kill must be recorded in the ledger"
    );
}

#[test]
fn level3_cancel_kills_child_processes() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let registry = ChildProcessRegistry::new();
    let token = CancelToken::new();
    let canceller = token.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        canceller.cancel();
    });
    let err = sandbox
        .run(CodeRunRequest {
            language: "sh".to_string(),
            code: "sleep 120".to_string(),
            inputs: Vec::new(),
            agent_id: "agent-001".to_string(),
            cancel_tokens: vec![token],
            registry: Some(registry.clone()),
            kill_ledger: None,
            run_dirs: None,
        })
        .expect_err("cancel must abort the run");
    assert!(err.is_cancelled(), "{err}");
    assert!(registry.is_empty(), "registry must be clear after cancel");
}

#[test]
fn execute_code_output_capped() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let output = sandbox
        .run(run_request("python3", "print('x' * (5 * 1024 * 1024))"))
        .expect("run executes");
    assert!(
        output.truncated,
        "5 MiB of output must be flagged truncated"
    );
    assert!(
        output.stdout.len() <= 1_048_576,
        "stdout capped at 1 MiB, got {}",
        output.stdout.len()
    );
    assert_eq!(output.stdout.len(), 1_048_576, "cap is exact");
}

#[test]
fn sandbox_proc_shows_only_sandbox_pids() {
    // F6-11: inside the sandbox, /proc lists only processes
    // in the sandbox's PID namespace. The sandboxed
    // process sees its own PID and never the host test
    // process's PID.
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let host_pid = std::process::id();
    let code = "import os, json; print(json.dumps({'pid': os.getpid(), 'pids': sorted(os.listdir('/proc'))}))";
    let output = sandbox
        .run(run_request("python3", code))
        .expect("sandboxed run succeeds");
    assert_eq!(output.exit_code, Some(0), "stderr: {}", output.stderr);
    let seen: Value =
        serde_json::from_str(output.stdout.trim()).expect("the child prints one JSON line");
    let own_pid = seen["pid"].as_u64().expect("child pid");
    let pids: Vec<String> = seen["pids"]
        .as_array()
        .expect("pid list")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(
        pids.contains(&own_pid.to_string()),
        "the child must see its own PID in /proc: {pids:?}"
    );
    assert_ne!(
        own_pid as u32, host_pid,
        "the child runs in its own PID namespace"
    );
    assert!(
        !pids.contains(&host_pid.to_string()),
        "the host test process PID {host_pid} must not appear in the sandboxed /proc: {pids:?}"
    );
}

#[test]
fn execute_code_audit_has_hash_not_text() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let stem = temp_stem("audit");
    let db_path = format!("{stem}.db");
    let audit_path = format!("{stem}.jsonl");
    let notebook = Arc::new(Notebook::open(&db_path).unwrap());
    let audit = AuditLog::open(&audit_path).unwrap();
    let tracker = Arc::new(RunDirTracker::new());
    let tool = ExecuteCodeTool::new(
        "agent-001",
        Arc::new(sandbox),
        Arc::clone(&tracker),
        ChildProcessRegistry::new(),
    );
    let mut registry = Registry::new();
    registry.register(Box::new(tool));
    struct AllowApprover;
    impl Approver for AllowApprover {
        fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
            Ok(true)
        }
        fn label(&self) -> &'static str {
            "test allow"
        }
    }
    let approver = AllowApprover;
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-sandbox-audit",
        "agent-001",
    );
    let code = "print('SENTINEL_SECRET_XYZ')";
    let output = executor
        .execute(
            "execute_code",
            &json!({"language": "python3", "code": code}),
        )
        .expect("approved run executes");
    assert!(output["stdout"]
        .as_str()
        .unwrap()
        .contains("SENTINEL_SECRET_XYZ"));
    drop(executor);
    drop(audit);
    drop(notebook);
    let log = std::fs::read_to_string(&audit_path).unwrap();
    assert!(
        !log.contains("SENTINEL_SECRET_XYZ"),
        "audit must never contain code text"
    );
    assert!(log.contains("code_sha256"), "audit must carry the hash");
    assert!(
        log.contains("code_execution"),
        "code_execution record present"
    );
    assert!(log.contains("\"sandbox_verdict\":\"bwrap\""), "{log}");
    tracker.cleanup();
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&audit_path);
}

#[test]
fn execute_code_input_hashes_are_audited_and_verified() {
    // F6-06 end to end: a real sandboxed run with an input
    // file. The code_execution record must carry the
    // approved input hash, the executed input hash, and
    // input_hashes_verified=true; the executed hash must
    // equal the hash of the approved content.
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let stem = temp_stem("input-hash");
    let db_path = format!("{stem}.db");
    let audit_path = format!("{stem}.jsonl");
    let notebook = Arc::new(Notebook::open(&db_path).unwrap());
    let audit = AuditLog::open(&audit_path).unwrap();
    let tracker = Arc::new(RunDirTracker::new());
    let tool = ExecuteCodeTool::new(
        "agent-001",
        Arc::new(sandbox),
        Arc::clone(&tracker),
        ChildProcessRegistry::new(),
    );
    let mut registry = Registry::new();
    registry.register(Box::new(tool));
    struct AllowApprover;
    impl Approver for AllowApprover {
        fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
            Ok(true)
        }
        fn label(&self) -> &'static str {
            "test allow"
        }
    }
    let approver = AllowApprover;
    let executor = GuardedExecutor::new(
        &registry,
        Some(&approver),
        &audit,
        "turn-sandbox-input-hash",
        "agent-001",
    );
    let output = executor
        .execute(
            "execute_code",
            &json!({
                "language": "python3",
                "code": "print(open('data.txt').read())",
                "inputs": [{"name": "data.txt", "content": "hello-input"}],
            }),
        )
        .expect("approved run executes");
    assert!(output["stdout"].as_str().unwrap().contains("hello-input"));
    assert_eq!(
        output["input_files_sha256"][0]["sha256"],
        sha256_hex(b"hello-input")
    );
    drop(executor);
    drop(audit);
    drop(notebook);
    let log = std::fs::read_to_string(&audit_path).unwrap();
    assert!(
        !log.contains("hello-input"),
        "audit must carry hashes, never input content"
    );
    let record = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|r| r["kind"] == "code_execution")
        .expect("code_execution record");
    let detail = &record["detail"];
    assert_eq!(detail["input_hashes_verified"], true, "{detail}");
    assert_eq!(detail["input_files_sha256"][0]["name"], "data.txt");
    assert_eq!(
        detail["input_files_sha256"][0]["sha256"],
        sha256_hex(b"hello-input")
    );
    assert_eq!(
        detail["executed_input_files_sha256"], detail["input_files_sha256"],
        "{detail}"
    );
    tracker.cleanup();
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&audit_path);
}

#[test]
fn execute_code_no_ambient_env() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    std::env::set_var("MIMINT_TEST_AMBIENT_XYZ", "should-not-leak");
    std::env::set_var("MIMINT_API_KEY", "should-not-leak-either");
    let output = sandbox
        .run(run_request(
            "python3",
            "import os, json; print(json.dumps(dict(os.environ)))",
        ))
        .expect("env run executes");
    std::env::remove_var("MIMINT_TEST_AMBIENT_XYZ");
    std::env::remove_var("MIMINT_API_KEY");
    assert_eq!(output.exit_code, Some(0), "stderr: {}", output.stderr);
    assert!(
        !output.stdout.contains("should-not-leak"),
        "{}",
        output.stdout
    );
    let env: Value = serde_json::from_str(output.stdout.trim()).expect("env JSON");
    let env = env.as_object().expect("env object");
    // The spawn path passes exactly PATH/HOME/TMPDIR. Two
    // more appear inside, both added after spawn and both
    // host-data-free (verified by direct bwrap probes):
    // PWD=/work (the shell/runtime reports the cwd) and
    // LC_CTYPE=C.UTF-8 (CPython locale coercion, PEP 538).
    let mut keys: Vec<&str> = env.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["HOME", "LC_CTYPE", "PATH", "PWD", "TMPDIR"],
        "child env must be the minimal set + interpreter additions: {env:?}"
    );
    assert_eq!(env["HOME"], "/work");
    assert_eq!(env["TMPDIR"], "/work");
    assert_eq!(env["PATH"], "/usr/bin:/bin");
    assert_eq!(env["PWD"], "/work");
    assert_eq!(env["LC_CTYPE"], "C.UTF-8");
    let home = std::env::var("HOME").unwrap();
    for (key, value) in env {
        assert!(
            !value.as_str().unwrap().contains(&home),
            "env {key} must not leak the real HOME path"
        );
    }
}

#[test]
fn execute_code_inputs_and_workdir_are_visible_to_code() {
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let tracker = Arc::new(RunDirTracker::new());
    let output = sandbox
        .run(CodeRunRequest {
            language: "python3".to_string(),
            code: "print(open('/work/data.txt').read().strip())".to_string(),
            inputs: vec![("data.txt".to_string(), "input-content".to_string())],
            agent_id: "agent-001".to_string(),
            cancel_tokens: Vec::new(),
            registry: None,
            kill_ledger: None,
            run_dirs: Some(Arc::clone(&tracker)),
        })
        .expect("input run executes");
    assert_eq!(output.stdout.trim(), "input-content");
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(tracker.len(), 1, "the workdir is tracked for cleanup");
    assert_eq!(tracker.cleanup(), 1);
    assert!(!output.workdir.exists(), "cleanup removes the workdir");
    // Unsafe input names are rejected before spawn.
    let err = sandbox
        .run(CodeRunRequest {
            language: "python3".to_string(),
            code: "print(1)".to_string(),
            inputs: vec![("../escape.txt".to_string(), "x".to_string())],
            agent_id: "agent-001".to_string(),
            cancel_tokens: Vec::new(),
            registry: None,
            kill_ledger: None,
            run_dirs: None,
        })
        .expect_err("path traversal input must be rejected");
    assert!(err.message().starts_with("invalid_arguments:"), "{err}");
}

#[test]
fn r5_code_cannot_reach_display() {
    // R5 is enforced by the namespaces, not by scanning code:
    // the child has no display variables, no X socket, and no
    // network — including loopback to the host's Ollama port.
    let Some(sandbox) = available_sandbox(default_policy()) else {
        return;
    };
    let code = r#"
import os, socket
print("DISPLAY_PRESENT" if "DISPLAY" in os.environ else "DISPLAY_ABSENT")
print("WAYLAND_PRESENT" if "WAYLAND_DISPLAY" in os.environ else "WAYLAND_ABSENT")
print("XSOCK_VISIBLE" if os.path.exists("/tmp/.X11-unix") else "XSOCK_ABSENT")
try:
    s = socket.create_connection(("127.0.0.1", 11434), timeout=2)
    s.close()
    print("NET_REACHABLE")
except Exception:
    print("NET_DENIED")
"#;
    let output = sandbox.run(run_request("python3", code)).unwrap();
    assert_eq!(output.exit_code, Some(0), "stderr: {}", output.stderr);
    assert!(
        output.stdout.contains("DISPLAY_ABSENT"),
        "DISPLAY must be absent: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("WAYLAND_ABSENT"),
        "WAYLAND_DISPLAY must be absent: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("XSOCK_ABSENT"),
        "the X socket dir must not exist in the sandbox /tmp: {}",
        output.stdout
    );
    assert!(
        output.stdout.contains("NET_DENIED"),
        "loopback to the host must fail: {}",
        output.stdout
    );
}
