//! Sandboxed `execute_code` (Phase 6, §5).
//!
//! On Linux, code runs inside bubblewrap with the EXACT flag set
//! proven on vega (`04-sandbox-proof-v2.log`), in the frozen order.
//! No bwrap, or a failed preflight, means fail-closed
//! `sandbox_unavailable` — code never runs outside the sandbox.
//! macOS and Windows fail closed with `sandbox_unverified_on_*`
//! until a real 3-case proof exists on those machines (F6D-22).
//! Android does not register the tool at all (§5.4).
//!
//! Resource limits: `RLIMIT_FSIZE`/`RLIMIT_AS` are set via
//! `pre_exec` before exec'ing bwrap (inherited by the sandbox,
//! exactly like the `prlimit` wrapper used in the proof).
//! `RLIMIT_NPROC` is NEVER set before bwrap: the negative result
//! in `05-rlimit-scope.log` shows tight NPROC breaks bwrap spawn
//! itself. Fork-bomb defence is the PID namespace +
//! `--die-with-parent` + timeout + the AS ceiling.
//!
//! Killing is namespace-first (§5.1, F6D-03): the sandbox init
//! (PID 1 of the new PID namespace) is signalled, which ends the
//! whole namespace; process-group kill is the second layer only.
//! With `--new-session`, group-kill on the bwrap pid alone does
//! NOT reach the sandboxed tree (the sandbox init setsid'd into
//! a new session), so the init pid is discovered via
//! `/proc/<bwrap>/task/*/children` and signalled directly.
//!
//! FD discipline (hazard E2): bwrap does NOT close inherited fds,
//! so the spawn path configures stdio explicitly, inherits
//! nothing else, and every fd Rust `std` opens carries CLOEXEC.
//! No non-CLOEXEC fd may be opened on the spawn path.
//!
//! Child environment: the spawn path passes exactly
//! `PATH=/usr/bin:/bin`, `HOME=/work`, `TMPDIR=/work` (plus
//! explicitly configured extras, of which there are none by
//! default). Two more variables appear inside the child, both
//! added after spawn and both host-data-free: `PWD=/work`
//! (the shell/runtime reports the cwd) and `LC_CTYPE=C.UTF-8`
//! (CPython locale coercion, PEP 538). Verified by direct
//! bwrap probes on vega; see `execute_code_no_ambient_env`.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::audit::sha256_hex;
use crate::cancel::{CancelToken, KillLedger};
use crate::config::SandboxConfig;
use crate::error::MimintError;
use crate::tools::Tool;

/// Tmpfs size for BOTH `--tmpfs` mounts (HOME and /tmp), in
/// bytes. Frozen from the proof: one `--size` directly before
/// each `--tmpfs`.
pub const TMPFS_SIZE_BYTES: u64 = 67_108_864;

/// Files larger than this are listed as artifacts without a
/// hash (the R4 re-hash then reports Failed honestly, rather
/// than pretending an unread file was verified).
const ARTIFACT_HASH_CAP_BYTES: u64 = 16 * 1024 * 1024;

/// How long the wait loop polls for the sandbox init pid and
/// namespace inode after spawn.
const INIT_DISCOVERY_BUDGET: Duration = Duration::from_millis(200);

/// Grace between SIGTERM and SIGKILL (§10).
const TERM_GRACE: Duration = Duration::from_secs(2);

/// How long output-reader threads get to drain after the child
/// exits before the run proceeds with what arrived.
const READER_DRAIN_BUDGET: Duration = Duration::from_secs(5);

/// Preflight timeout: the mini-proof must be quick.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// Verdict recorded for a run that executed inside bwrap.
pub const VERDICT_BWRAP: &str = "bwrap";

/// Pure OS gate: `Some(reason)` when `execute_code` must refuse
/// on this OS without spawning anything. Linux returns `None`
/// (the bwrap path decides); unknown OSes fail closed as
/// `sandbox_unavailable`.
pub fn os_sandbox_denial(target_os: &str) -> Option<&'static str> {
    match target_os {
        "linux" => None,
        "macos" => Some("sandbox_unverified_on_macos"),
        "windows" => Some("sandbox_unverified_on_windows"),
        _ => Some("sandbox_unavailable"),
    }
}

/// Interpreter + script file name for a supported language.
fn interpreter_for(language: &str) -> Option<(&'static str, &'static str)> {
    match language {
        "python3" => Some(("python3", "code.py")),
        "sh" => Some(("sh", "code.sh")),
        _ => None,
    }
}

/// The frozen bwrap argument vector (§5.1), in the proven
/// order. `extra_ro_binds` (MCP server binaries/scripts,
/// §6) are appended AFTER the `--size`/`--tmpfs` pairs:
/// a bind under $HOME placed before the tmpfs would be
/// hidden by it. The base order never changes.
/// `command` is the sandboxed command (interpreter + script).
pub fn bwrap_argv(
    home: &Path,
    workdir: &Path,
    extra_ro_binds: &[(PathBuf, PathBuf)],
    command: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "--unshare-all".into(),
        "--die-with-parent".into(),
        "--new-session".into(),
        "--dev".into(),
        "/dev".into(),
        "--proc".into(),
        "/proc".into(),
        "--ro-bind".into(),
        "/usr".into(),
        "/usr".into(),
        "--ro-bind".into(),
        "/lib".into(),
        "/lib".into(),
        "--ro-bind".into(),
        "/lib64".into(),
        "/lib64".into(),
        "--ro-bind".into(),
        "/bin".into(),
        "/bin".into(),
        "--ro-bind".into(),
        "/etc".into(),
        "/etc".into(),
    ];
    // Extra read-only binds go AFTER the tmpfs mounts: a
    // bind placed before `--tmpfs $HOME` is hidden by the
    // tmpfs when its destination is under $HOME (verified
    // by experiment on vega 2026-11-18: the file was
    // invisible inside the sandbox). bwrap creates the
    // intermediate directories for binds onto a fresh
    // tmpfs (verified the same day). execute_code passes
    // no extra binds, so its frozen flag order is
    // unchanged by this placement.
    argv.push("--size".into());
    argv.push(TMPFS_SIZE_BYTES.to_string());
    argv.push("--tmpfs".into());
    argv.push(home.to_string_lossy().into_owned());
    argv.push("--size".into());
    argv.push(TMPFS_SIZE_BYTES.to_string());
    argv.push("--tmpfs".into());
    argv.push("/tmp".into());
    for (source, dest) in extra_ro_binds {
        argv.push("--ro-bind".into());
        argv.push(source.to_string_lossy().into_owned());
        argv.push(dest.to_string_lossy().into_owned());
    }
    argv.push("--bind".into());
    argv.push(workdir.to_string_lossy().into_owned());
    argv.push("/work".into());
    argv.push("--chdir".into());
    argv.push("/work".into());
    argv.extend(command.iter().cloned());
    argv
}

/// Policy for sandboxed runs, from `[sandbox]` config (§12).
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Hard timeout per run.
    pub timeout: Duration,
    /// Combined stdout+stderr cap.
    pub max_output_bytes: usize,
    /// RLIMIT_FSIZE for the child (and its sandbox).
    pub rlimit_fsize_bytes: u64,
    /// RLIMIT_AS for the child (and its sandbox).
    pub rlimit_as_bytes: u64,
    /// bwrap binary. `"bwrap"` (PATH lookup) by default;
    /// overridable for tests that simulate a missing bwrap.
    pub bwrap_path: PathBuf,
    /// Extra environment variables passed to the child, beyond
    /// the minimal PATH/HOME/TMPDIR set. Empty by default (§12
    /// defines no env allow-list field, so nothing is wired).
    pub extra_env: Vec<(String, String)>,
}

impl SandboxPolicy {
    pub fn from_config(config: &SandboxConfig) -> Self {
        Self {
            timeout: Duration::from_secs(config.execute_code_timeout_secs),
            max_output_bytes: config.max_output_bytes,
            rlimit_fsize_bytes: config.rlimit_fsize_bytes,
            rlimit_as_bytes: config.rlimit_as_bytes,
            bwrap_path: PathBuf::from("bwrap"),
            extra_env: Vec::new(),
        }
    }

    pub fn with_bwrap_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.bwrap_path = path.into();
        self
    }
}

/// A run directory tracker: every `execute_code` workdir is
/// registered here and kept until the end of the turn, when
/// the worker/coordinator calls [`RunDirTracker::cleanup`].
#[derive(Debug, Default)]
pub struct RunDirTracker {
    dirs: Mutex<Vec<PathBuf>>,
}

impl RunDirTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn track(&self, path: PathBuf) {
        self.dirs.lock().unwrap().push(path);
    }

    pub fn len(&self) -> usize {
        self.dirs.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove every tracked directory. Best-effort per
    /// directory; returns how many were removed cleanly.
    pub fn cleanup(&self) -> usize {
        let dirs = std::mem::take(&mut *self.dirs.lock().unwrap());
        let mut removed = 0;
        for dir in dirs {
            if std::fs::remove_dir_all(&dir).is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

/// Registry of live child processes by agent (§10): tools
/// register on spawn and unregister on exit; the coordinator's
/// `kill_all` is the backstop when a wait loop cannot kill its
/// own child (it signals only — the owning wait loop reaps).
#[derive(Debug, Clone, Default)]
pub struct ChildProcessRegistry {
    inner: Arc<Mutex<HashMap<String, HashSet<u32>>>>,
}

impl ChildProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, agent_id: &str, pid: u32) {
        self.inner
            .lock()
            .unwrap()
            .entry(agent_id.to_string())
            .or_default()
            .insert(pid);
    }

    pub fn unregister(&self, agent_id: &str, pid: u32) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(set) = guard.get_mut(agent_id) {
            set.remove(&pid);
            if set.is_empty() {
                guard.remove(agent_id);
            }
        }
    }

    pub fn pids(&self, agent_id: &str) -> Vec<u32> {
        self.inner
            .lock()
            .unwrap()
            .get(agent_id)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }

    /// Backstop: SIGTERM every registered pid for the agent,
    /// wait 500ms, then SIGKILL the survivors. Returns the pids
    /// that were signalled. Best-effort; never panics.
    pub fn kill_all(&self, agent_id: &str) -> Vec<u32> {
        let pids = self.pids(agent_id);
        if pids.is_empty() {
            return Vec::new();
        }
        #[cfg(unix)]
        {
            for pid in &pids {
                unsafe {
                    libc::kill(*pid as libc::pid_t, libc::SIGTERM);
                }
            }
            std::thread::sleep(Duration::from_millis(500));
            for pid in &pids {
                let alive = unsafe { libc::kill(*pid as libc::pid_t, 0) == 0 };
                if alive {
                    unsafe {
                        libc::kill(*pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
        }
        pids
    }
}

/// Outcome of a kill sequence (§10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// The child had already exited; nothing was signalled.
    AlreadyExited,
    /// SIGTERM ended the child (and its namespace).
    KilledTerm,
    /// SIGKILL was required.
    KilledKill,
    /// The child survived SIGKILL + the reap budget.
    KillFailed,
}

/// Kill verdict: how the child died, plus how many processes
/// still hold its PID namespace afterwards (strays).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillVerdict {
    pub outcome: KillOutcome,
    /// Processes still in the sandbox PID namespace after the
    /// kill. 0 is the expected value; anything else is a leak.
    pub strays: usize,
    pub elapsed_ms: u64,
}

impl KillVerdict {
    pub fn describe(&self) -> String {
        let outcome = match self.outcome {
            KillOutcome::AlreadyExited => "already_exited",
            KillOutcome::KilledTerm => "killed_term",
            KillOutcome::KilledKill => "killed_kill",
            KillOutcome::KillFailed => "kill_failed",
        };
        format!("{outcome}, strays={}, {}ms", self.strays, self.elapsed_ms)
    }
}

/// One sandboxed code run request.
#[derive(Debug)]
pub struct CodeRunRequest {
    pub language: String,
    pub code: String,
    /// Input files, written into the workdir before spawn.
    /// Names must be plain relative file names (no `/`, no
    /// `..`); content is written verbatim.
    pub inputs: Vec<(String, String)>,
    pub agent_id: String,
    pub cancel_tokens: Vec<CancelToken>,
    pub registry: Option<ChildProcessRegistry>,
    pub kill_ledger: Option<KillLedger>,
    pub run_dirs: Option<Arc<RunDirTracker>>,
}

/// SHA-256 of one `execute_code` input file's content
/// (F6-06): hashed at approval time from the arguments, and
/// again from the bytes actually written into the workdir, so
/// the audit trail can prove the run used the approved
/// inputs. Names are plain relative file names; content is
/// hashed as UTF-8 bytes, exactly as written.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InputFileHash {
    pub name: String,
    pub sha256: String,
}

/// Hash every input in `execute_code` ARGS (approval time).
/// Items without a string `name`/`content` are skipped — the
/// tool itself rejects such arguments before any run, so a
/// skipped item here can never reach execution.
pub fn execute_code_input_hashes(args: &Value) -> Vec<InputFileHash> {
    let Some(list) = args.get("inputs").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?;
            let content = item.get("content")?.as_str()?;
            Some(InputFileHash {
                name: name.to_string(),
                sha256: sha256_hex(content.as_bytes()),
            })
        })
        .collect()
}

/// One file the run left in the workdir (excluding the code
/// file and the inputs), for R4 artifact re-hashing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub name: String,
    /// SHA-256 of the file content, or `None` when the file
    /// exceeds [`ARTIFACT_HASH_CAP_BYTES`].
    pub sha256: Option<String>,
}

/// Result of a sandboxed run that executed (including
/// non-zero exits — those are data for the model, not errors).
#[derive(Debug)]
pub struct CodeRunOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    /// Signal that killed the child, if it died by signal.
    pub terminated_by_signal: Option<i32>,
    pub truncated: bool,
    pub duration_ms: u64,
    pub sandbox_verdict: &'static str,
    pub code_sha256: String,
    /// Hashes of the input files actually written into the
    /// workdir for this run (F6-06), in argument order.
    pub input_files_sha256: Vec<InputFileHash>,
    pub language: String,
    pub code_len: usize,
    pub workdir: PathBuf,
    pub artifacts: Vec<Artifact>,
}

impl CodeRunOutput {
    /// Tool output for the model. The audit layer redacts
    /// this to a summary; the model sees the real output.
    pub fn to_json(&self) -> Value {
        json!({
            "stdout": self.stdout,
            "stderr": self.stderr,
            "exit_code": self.exit_code,
            "terminated_by_signal": self.terminated_by_signal,
            "truncated": self.truncated,
            "duration_ms": self.duration_ms,
            "sandbox_verdict": self.sandbox_verdict,
            "code_sha256": self.code_sha256,
            "input_files_sha256": self.input_files_sha256.iter().map(|h| json!({
                "name": h.name,
                "sha256": h.sha256,
            })).collect::<Vec<_>>(),
            "language": self.language,
            "code_len": self.code_len,
            "workdir": self.workdir.to_string_lossy(),
            "artifacts": self.artifacts.iter().map(|a| json!({
                "name": a.name,
                "sha256": a.sha256,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Paths an `execute_code` run failed to access because the
/// sandbox hides them (F6-17), detected from the TOOL RESULT:
/// a non-zero exit code, stderr carrying an access-error
/// keyword, and stderr naming an absolute path under one of
/// the trees the sandbox overlays with an empty tmpfs
/// (`/home/`, `/root/`, `/tmp/`). Only those trees count: a
/// genuinely missing file inside `/work` (relative paths, or
/// a missing `/work/...` path) is NOT a sandbox denial, and
/// traceback frames from the read-only `/usr/lib` tree must
/// never false-positive. Returns the denied paths in order of
/// first mention, deduplicated. A missing or zero exit code
/// detects nothing — the tool's own report decides.
pub(crate) fn sandbox_denied_paths(output: &Value) -> Vec<String> {
    let exit_denied = matches!(
        output.get("exit_code").and_then(Value::as_i64),
        Some(code) if code != 0
    );
    if !exit_denied {
        return Vec::new();
    }
    let Some(stderr) = output.get("stderr").and_then(Value::as_str) else {
        return Vec::new();
    };
    const KEYWORDS: [&str; 5] = [
        "No such file or directory",
        "Permission denied",
        "Operation not permitted",
        "PermissionError",
        "FileNotFoundError",
    ];
    if !KEYWORDS.iter().any(|kw| stderr.contains(kw)) {
        return Vec::new();
    }
    const TREES: [&str; 3] = ["/home/", "/root/", "/tmp/"];
    let mut paths: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let trimmed = raw.trim_matches(|c| matches!(c, '\'' | '"' | ',' | '.' | ')' | ':' | ';'));
        if TREES.iter().any(|tree| trimmed.starts_with(tree))
            && !paths.contains(&trimmed.to_string())
        {
            paths.push(trimmed.to_string());
        }
    };
    // Quoted paths first (tracebacks, shell errors), then
    // bare whitespace-separated tokens.
    let mut rest = stderr;
    while let Some(start) = rest.find(['\'', '"']) {
        let quote = rest.as_bytes()[start] as char;
        let after = &rest[start + 1..];
        if let Some(end) = after.find(quote) {
            push(&after[..end]);
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    for token in stderr.split_whitespace() {
        push(token);
    }
    paths
}

/// The sandbox: policy + cached preflight verdict + the run
/// path. Cheap to share (`Arc<Sandbox>`); the preflight cache
/// is shared too, so a fleet of agents preflights once.
#[derive(Debug)]
pub struct Sandbox {
    policy: SandboxPolicy,
    preflight: Mutex<Option<Result<(), String>>>,
}

impl Sandbox {
    pub fn new(policy: SandboxPolicy) -> Self {
        Self {
            policy,
            preflight: Mutex::new(None),
        }
    }

    pub fn from_config(config: &SandboxConfig) -> Self {
        Self::new(SandboxPolicy::from_config(config))
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    /// Cached preflight verdict (§5.1). Runs the mini-proof
    /// once (full flag set); later calls return the cached
    /// result. A failed preflight is cached as a failure —
    /// there is no silent downgrade, and a config rebuild
    /// creates a fresh `Sandbox` (fresh cache).
    pub fn preflight(&self) -> Result<(), String> {
        if let Some(cached) = self.preflight.lock().unwrap().clone() {
            return cached;
        }
        let result = self.run_preflight();
        *self.preflight.lock().unwrap() = Some(result.clone());
        result
    }

    /// The preflight verdict if one has been computed, without
    /// running it. Used by the UI availability indicator.
    pub fn preflight_status(&self) -> Option<Result<(), String>> {
        self.preflight.lock().unwrap().clone()
    }

    fn run_preflight(&self) -> Result<(), String> {
        if let Some(denial) = os_sandbox_denial(std::env::consts::OS) {
            return Err(denial.to_string());
        }
        #[cfg(not(unix))]
        {
            return Err("sandbox_unavailable".to_string());
        }
        #[cfg(unix)]
        {
            self.run_preflight_unix()
        }
    }

    #[cfg(unix)]
    fn run_preflight_unix(&self) -> Result<(), String> {
        // `bwrap --version` first: a missing binary fails fast.
        let version = Command::new(&self.policy.bwrap_path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("sandbox_unavailable: cannot run bwrap: {e}"))?;
        if !version.status.success() {
            return Err("sandbox_unavailable: bwrap --version failed".to_string());
        }
        let home = home_dir().ok_or_else(|| "sandbox_unavailable: HOME is not set".to_string())?;
        let unique = unique_suffix();
        let workdir = create_workdir(&home, &unique)?;
        let home_sentinel = home.join(format!(".mimint-preflight-{unique}"));
        let tmp_sentinel = std::env::temp_dir().join(format!(".mimint-preflight-{unique}"));
        // The proof script: the HOME sentinel and the /tmp
        // sentinel must both be unreadable, the workdir file
        // must be readable. Absolute paths are embedded; the
        // names are generated (alphanumeric + `-`), never
        // user input, so no injection is possible here.
        let script = format!(
            "if [ -r '{home_sentinel}' ]; then echo HOME_VISIBLE; exit 10; fi\n\
             if [ -r '{tmp_sentinel}' ]; then echo TMP_VISIBLE; exit 11; fi\n\
             if ! grep -q allowed /work/allowed.txt; then echo WORK_UNREADABLE; exit 12; fi\n\
             echo PROOF_OK\n",
            home_sentinel = home_sentinel.to_string_lossy(),
            tmp_sentinel = tmp_sentinel.to_string_lossy(),
        );
        let result = (|| -> Result<(), String> {
            std::fs::write(&home_sentinel, "home-secret").map_err(|e| e.to_string())?;
            std::fs::write(&tmp_sentinel, "tmp-secret").map_err(|e| e.to_string())?;
            std::fs::write(workdir.join("allowed.txt"), "allowed").map_err(|e| e.to_string())?;
            std::fs::write(workdir.join("proof.sh"), &script).map_err(|e| e.to_string())?;
            let argv = bwrap_argv(
                &home,
                &workdir,
                &[],
                &["sh".to_string(), "/work/proof.sh".to_string()],
            );
            let raw = run_sandboxed_once(&self.policy, &argv, PREFLIGHT_TIMEOUT, &[], None, None)
                .map_err(|e| format!("sandbox_unavailable: preflight spawn failed: {e}"))?;
            if raw.status.success() && raw.stdout.contains("PROOF_OK") {
                Ok(())
            } else {
                Err(format!(
                    "sandbox_unavailable: preflight proof failed (exit {:?}, stdout {:?}, stderr {:?})",
                    raw.status.code(),
                    raw.stdout.trim(),
                    raw.stderr.trim(),
                ))
            }
        })();
        let _ = std::fs::remove_file(&home_sentinel);
        let _ = std::fs::remove_file(&tmp_sentinel);
        let _ = std::fs::remove_dir_all(&workdir);
        result
    }

    /// Run code in the sandbox. Errors are reason-prefixed
    /// (`sandbox_unavailable: ...`, `unsupported_language: ...`,
    /// `timeout: ...`, `invalid_arguments: ...`) so the audit
    /// layer can record the verdict; cancellation is
    /// [`MimintError::Cancelled`].
    pub fn run(&self, request: CodeRunRequest) -> Result<CodeRunOutput, MimintError> {
        let (interpreter, script_name) = interpreter_for(&request.language).ok_or_else(|| {
            MimintError::Tool(format!(
                "unsupported_language: {} (supported: python3, sh)",
                request.language
            ))
        })?;
        if let Some(denial) = os_sandbox_denial(std::env::consts::OS) {
            return Err(MimintError::Tool(format!(
                "{denial}: execute_code is not available on this OS"
            )));
        }
        self.preflight().map_err(|reason| {
            // The preflight reason is already a full verdict
            // reason ("sandbox_unavailable: ..." or an OS
            // denial); use it as-is, never double-prefix.
            MimintError::Tool(reason)
        })?;
        #[cfg(not(unix))]
        {
            return Err(MimintError::Tool(
                "sandbox_unavailable: sandbox spawn needs Unix".to_string(),
            ));
        }
        #[cfg(unix)]
        {
            self.run_unix(request, interpreter, script_name)
        }
    }

    #[cfg(unix)]
    fn run_unix(
        &self,
        request: CodeRunRequest,
        interpreter: &str,
        script_name: &str,
    ) -> Result<CodeRunOutput, MimintError> {
        let home = home_dir()
            .ok_or_else(|| MimintError::Tool("sandbox_unavailable: HOME is not set".to_string()))?;
        // Validate input names BEFORE creating the workdir, so
        // a rejected run leaves nothing behind.
        let mut input_names: HashSet<String> = HashSet::new();
        for (name, _) in &request.inputs {
            if !is_safe_input_name(name) || name == script_name || !input_names.insert(name.clone())
            {
                return Err(MimintError::Tool(format!(
                    "invalid_arguments: unsafe or duplicate input name: {name:?}"
                )));
            }
        }
        let unique = unique_suffix();
        let workdir = create_workdir(&home, &unique).map_err(MimintError::Tool)?;
        if let Some(tracker) = &request.run_dirs {
            tracker.track(workdir.clone());
        }
        for (name, content) in &request.inputs {
            write_private_file(&workdir.join(name), content.as_bytes())
                .map_err(MimintError::Tool)?;
        }
        write_private_file(&workdir.join(script_name), request.code.as_bytes())
            .map_err(MimintError::Tool)?;
        let argv = bwrap_argv(
            &home,
            &workdir,
            &[],
            &[interpreter.to_string(), format!("/work/{script_name}")],
        );
        let started = Instant::now();
        let raw = run_sandboxed_once(
            &self.policy,
            &argv,
            self.policy.timeout,
            &request.cancel_tokens,
            request
                .registry
                .as_ref()
                .map(|r| (request.agent_id.as_str(), r)),
            request.kill_ledger.as_ref(),
        )?;
        let duration_ms = started.elapsed().as_millis() as u64;
        let artifacts = list_artifacts(&workdir, script_name, &input_names);
        // Hash the input bytes as written (F6-06): the same
        // content the tool wrote verbatim into the workdir.
        let input_files_sha256: Vec<InputFileHash> = request
            .inputs
            .iter()
            .map(|(name, content)| InputFileHash {
                name: name.clone(),
                sha256: sha256_hex(content.as_bytes()),
            })
            .collect();
        Ok(CodeRunOutput {
            stdout: raw.stdout,
            stderr: raw.stderr,
            exit_code: raw.status.code(),
            terminated_by_signal: {
                use std::os::unix::process::ExitStatusExt;
                raw.status.signal()
            },
            truncated: raw.truncated,
            duration_ms,
            sandbox_verdict: VERDICT_BWRAP,
            code_sha256: sha256_hex(request.code.as_bytes()),
            input_files_sha256,
            language: request.language,
            code_len: request.code.len(),
            workdir,
            artifacts,
        })
    }
}

/// Spawn a child inside bwrap with the frozen flags, the
/// minimal environment, explicit stdio, its own process
/// group, and the FSIZE/AS limits. Shared by `execute_code`
/// and sandboxed MCP servers (§5.1/§6.3). The caller owns
/// waiting, killing, and output draining.
#[cfg(unix)]
pub(crate) fn spawn_bwrap_child(
    policy: &SandboxPolicy,
    argv: &[String],
    extra_env: &[(String, String)],
    stdin: Stdio,
) -> std::io::Result<Child> {
    let mut cmd = Command::new(&policy.bwrap_path);
    cmd.args(argv);
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("HOME", "/work");
    cmd.env("TMPDIR", "/work");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let fsize = policy.rlimit_fsize_bytes;
    let as_bytes = policy.rlimit_as_bytes;
    unsafe {
        use std::os::unix::process::CommandExt;
        // Own process group, so the group-kill layer has a
        // group to signal (best-effort; the namespace is the
        // real mechanism).
        cmd.process_group(0);
        // RLIMIT_FSIZE + RLIMIT_AS only. NEVER RLIMIT_NPROC
        // before bwrap (negative result, 05-rlimit-scope.log).
        cmd.pre_exec(move || {
            set_rlimit(libc::RLIMIT_FSIZE as libc::c_int, fsize)?;
            set_rlimit(libc::RLIMIT_AS as libc::c_int, as_bytes)?;
            Ok(())
        });
    }
    cmd.spawn()
}

/// Spawn a child directly (no sandbox): the unsandboxed MCP
/// exception path (§6.3, macOS/Windows only, never Linux).
/// Same stdio/fd discipline as the sandboxed path: explicit
/// stdio, own process group, filtered environment.
#[cfg(unix)]
pub(crate) fn spawn_direct_child(
    program: &str,
    args: &[String],
    env: &[(String, String)],
) -> std::io::Result<Child> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.env_clear();
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
    cmd.spawn()
}

#[cfg(not(unix))]
pub(crate) fn spawn_direct_child(
    program: &str,
    args: &[String],
    env: &[(String, String)],
) -> std::io::Result<Child> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.env_clear();
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.spawn()
}
struct RawOutput {
    stdout: String,
    stderr: String,
    status: std::process::ExitStatus,
    truncated: bool,
}

/// Spawn bwrap with the frozen flags, wait with a deadline
/// and cancel polling, kill namespace-first on timeout or
/// cancel. The ONLY spawn path for sandboxed children.
#[cfg(unix)]
fn run_sandboxed_once(
    policy: &SandboxPolicy,
    argv: &[String],
    timeout: Duration,
    cancel_tokens: &[CancelToken],
    registry: Option<(&str, &ChildProcessRegistry)>,
    kill_ledger: Option<&KillLedger>,
) -> Result<RawOutput, MimintError> {
    let mut child = spawn_bwrap_child(policy, argv, &policy.extra_env, Stdio::null())
        .map_err(|e| MimintError::Tool(format!("sandbox_unavailable: cannot spawn bwrap: {e}")))?;
    let pid = child.id();
    if let Some((agent_id, registry)) = registry {
        registry.register(agent_id, pid);
    }
    let stdout_pipe = child.stdout.take().expect("stdout was piped");
    let stderr_pipe = child.stderr.take().expect("stderr was piped");
    let cap = Arc::new(SharedCap::new(policy.max_output_bytes));
    let stdout_cap = spawn_reader(stdout_pipe, Arc::clone(&cap));
    let stderr_cap = spawn_reader(stderr_pipe, Arc::clone(&cap));
    // Discover the sandbox init + its PID namespace while the
    // child runs, so a later kill can be namespace-first.
    let (init_pid, ns_inode) = discover_init(pid);
    let deadline = Instant::now() + timeout;
    let outcome: Result<std::process::ExitStatus, MimintError> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let verdict = terminate_child(
                        &mut child,
                        pid,
                        init_pid,
                        ns_inode.as_deref(),
                        kill_ledger,
                    );
                    break Err(MimintError::Tool(format!(
                        "timeout: execute_code exceeded {}s ({})",
                        timeout.as_secs(),
                        verdict.describe()
                    )));
                }
                if cancel_tokens.iter().any(CancelToken::is_cancelled) {
                    let _verdict = terminate_child(
                        &mut child,
                        pid,
                        init_pid,
                        ns_inode.as_deref(),
                        kill_ledger,
                    );
                    break Err(MimintError::Cancelled(
                        "execute_code run cancelled".to_string(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => break Err(MimintError::Tool(format!("sandbox wait failed: {e}"))),
        }
    };
    if let Some((agent_id, registry)) = registry {
        registry.unregister(agent_id, pid);
    }
    let status = outcome?;
    // Drain output: the readers finish at EOF, which arrives
    // when the namespace dies. Bounded wait; partial output
    // is kept if a reader stalls.
    let drain_deadline = Instant::now() + READER_DRAIN_BUDGET;
    while !(stdout_cap.is_done() && stderr_cap.is_done()) && Instant::now() < drain_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(RawOutput {
        stdout: String::from_utf8_lossy(&stdout_cap.take()).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_cap.take()).into_owned(),
        status,
        truncated: cap.was_truncated(),
    })
}

#[cfg(unix)]
fn set_rlimit(resource: libc::c_int, value: u64) -> std::io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    let rc = unsafe { libc::setrlimit(resource as _, &limit) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Combined stdout+stderr byte budget, shared by both
/// readers. Past the cap, readers keep draining (so the child
/// never blocks on a full pipe) but keep nothing more.
struct SharedCap {
    remaining: Mutex<usize>,
    truncated: AtomicBool,
}

impl SharedCap {
    fn new(cap: usize) -> Self {
        Self {
            remaining: Mutex::new(cap),
            truncated: AtomicBool::new(false),
        }
    }

    /// How many bytes of `wanted` may be kept.
    fn take(&self, wanted: usize) -> usize {
        let mut remaining = self.remaining.lock().unwrap();
        let kept = (*remaining).min(wanted);
        *remaining -= kept;
        if kept < wanted {
            self.truncated.store(true, Ordering::SeqCst);
        }
        kept
    }

    fn was_truncated(&self) -> bool {
        self.truncated.load(Ordering::SeqCst)
    }
}

struct StreamCapture {
    buf: Mutex<Vec<u8>>,
    done: AtomicBool,
}

impl StreamCapture {
    fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.buf.lock().unwrap())
    }
}

#[cfg(unix)]
fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    cap: Arc<SharedCap>,
) -> Arc<StreamCapture> {
    let capture = Arc::new(StreamCapture {
        buf: Mutex::new(Vec::new()),
        done: AtomicBool::new(false),
    });
    let capture_clone = Arc::clone(&capture);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let kept = cap.take(n);
                    if kept > 0 {
                        capture_clone
                            .buf
                            .lock()
                            .unwrap()
                            .extend_from_slice(&chunk[..kept]);
                    }
                    // The rest of the chunk is drained and
                    // dropped: the pipe must not fill up.
                }
                Err(_) => break,
            }
        }
        capture_clone.done.store(true, Ordering::SeqCst);
    });
    capture
}

/// Find bwrap's direct child (the sandbox init) and its PID
/// namespace inode, polling briefly after spawn. Shared with
/// the MCP kill path.
#[cfg(unix)]
pub(crate) fn discover_init(bwrap_pid: u32) -> (Option<u32>, Option<String>) {
    let deadline = Instant::now() + INIT_DISCOVERY_BUDGET;
    while Instant::now() < deadline {
        if let Some(init) = read_children(bwrap_pid).into_iter().next() {
            let inode = std::fs::read_link(format!("/proc/{init}/ns/pid"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            return (Some(init), inode);
        }
        // The child may already have exited (fast command):
        // then there is nothing to discover, and nothing to
        // kill later either.
        if !std::path::Path::new(&format!("/proc/{bwrap_pid}")).exists() {
            return (None, None);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    (None, None)
}

#[cfg(unix)]
fn read_children(pid: u32) -> Vec<u32> {
    let mut children = Vec::new();
    let task_dir = format!("/proc/{pid}/task");
    if let Ok(entries) = std::fs::read_dir(&task_dir) {
        for entry in entries.flatten() {
            let path = entry.path().join("children");
            if let Ok(text) = std::fs::read_to_string(&path) {
                for token in text.split_ascii_whitespace() {
                    if let Ok(child) = token.parse::<u32>() {
                        children.push(child);
                    }
                }
            }
        }
    }
    children
}

/// Namespace-first kill (§5.1/§10): SIGTERM the bwrap pid,
/// the sandbox init, and the process group; after the grace,
/// SIGKILL the same set; reap bounded; then scan for strays
/// still holding the namespace. Shared with the MCP kill path.
#[cfg(unix)]
pub(crate) fn terminate_child(
    child: &mut Child,
    bwrap_pid: u32,
    init_pid: Option<u32>,
    ns_inode: Option<&str>,
    kill_ledger: Option<&KillLedger>,
) -> KillVerdict {
    let started = Instant::now();
    let mut signalled_kill = false;
    let signal = |sig: libc::c_int, targets: &[(u32, bool)]| {
        for (pid, is_group) in targets {
            let target = if *is_group {
                -(*pid as libc::pid_t)
            } else {
                *pid as libc::pid_t
            };
            unsafe {
                libc::kill(target, sig);
            }
        }
    };
    let mut targets: Vec<(u32, bool)> = vec![(bwrap_pid, false), (bwrap_pid, true)];
    if let Some(init) = init_pid {
        targets.push((init, false));
    }
    // Fast path: already gone.
    if let Ok(Some(_)) = child.try_wait() {
        return KillVerdict {
            outcome: KillOutcome::AlreadyExited,
            strays: count_strays(ns_inode),
            elapsed_ms: started.elapsed().as_millis() as u64,
        };
    }
    signal(libc::SIGTERM, &targets);
    let term_deadline = Instant::now() + TERM_GRACE;
    let mut outcome = KillOutcome::KilledTerm;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= term_deadline {
                    signal(libc::SIGKILL, &targets);
                    signalled_kill = true;
                    outcome = KillOutcome::KilledKill;
                    let kill_deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => {
                                if Instant::now() >= kill_deadline {
                                    outcome = KillOutcome::KillFailed;
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            Err(_) => {
                                outcome = KillOutcome::KillFailed;
                                break;
                            }
                        }
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                outcome = KillOutcome::KillFailed;
                break;
            }
        }
    }
    if let Some(ledger) = kill_ledger {
        let method = if signalled_kill { "SIGKILL" } else { "SIGTERM" };
        ledger.record(bwrap_pid, method);
        if let Some(init) = init_pid {
            ledger.record(init, method);
        }
    }
    // A short grace lets the kernel finish namespace teardown
    // before the stray scan, so a slow teardown is not
    // misread as a leak.
    std::thread::sleep(Duration::from_millis(100));
    KillVerdict {
        outcome,
        strays: count_strays(ns_inode),
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

/// Count live processes holding the given PID namespace
/// inode. `None` (unknown namespace) means "cannot verify":
/// reported as 0 strays with no claim — the kill outcome
/// above is the real signal.
#[cfg(unix)]
fn count_strays(ns_inode: Option<&str>) -> usize {
    let Some(inode) = ns_inode else {
        return 0;
    };
    let mut strays = 0;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if let Ok(link) = std::fs::read_link(format!("/proc/{name}/ns/pid")) {
                if link.to_string_lossy() == inode {
                    strays += 1;
                }
            }
        }
    }
    strays
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

static WORKDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let counter = WORKDIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{}-{}-{}", std::process::id(), nanos, counter)
}

/// Create a fresh workdir OUTSIDE /tmp, under
/// `$HOME/.mimint-code-runs/`, mode 0700 (§5.1).
fn create_workdir(home: &Path, unique: &str) -> Result<PathBuf, String> {
    create_workdir_under(home, ".mimint-code-runs", unique)
}

/// Create a fresh private dir OUTSIDE /tmp under
/// `$HOME/<root>/`, mode 0700. Shared with MCP server
/// workdirs (§6.3).
pub(crate) fn create_workdir_under(
    home: &Path,
    root: &str,
    unique: &str,
) -> Result<PathBuf, String> {
    let root = home.join(root);
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create run root: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    }
    let dir = root.join(format!("run-{unique}"));
    std::fs::create_dir(&dir).map_err(|e| format!("cannot create workdir: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot chmod workdir: {e}"))?;
    }
    Ok(dir)
}

fn write_private_file(path: &Path, content: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    file.write_all(content)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(())
}

fn is_safe_input_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

/// Files the run left behind (excluding the code file and the
/// inputs), with hashes for R4 re-verification.
fn list_artifacts(
    workdir: &Path,
    script_name: &str,
    input_names: &HashSet<String>,
) -> Vec<Artifact> {
    let mut artifacts = Vec::new();
    let Ok(entries) = std::fs::read_dir(workdir) else {
        return artifacts;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == script_name || input_names.contains(&name) {
            continue;
        }
        let sha256 = (|| -> Option<String> {
            let meta = entry.metadata().ok()?;
            if meta.len() > ARTIFACT_HASH_CAP_BYTES {
                return None;
            }
            let bytes = std::fs::read(entry.path()).ok()?;
            Some(sha256_hex(&bytes))
        })();
        artifacts.push(Artifact { name, sha256 });
    }
    artifacts.sort_by(|a, b| a.name.cmp(&b.name));
    artifacts
}

/// The `execute_code` tool: Consequential, always approved,
/// always sandboxed on Linux, fail-closed elsewhere.
pub struct ExecuteCodeTool {
    agent_id: String,
    sandbox: Arc<Sandbox>,
    run_dirs: Arc<RunDirTracker>,
    registry: ChildProcessRegistry,
    kill_ledger: Mutex<Option<KillLedger>>,
    cancel_tokens: Mutex<Vec<CancelToken>>,
}

impl ExecuteCodeTool {
    pub fn new(
        agent_id: impl Into<String>,
        sandbox: Arc<Sandbox>,
        run_dirs: Arc<RunDirTracker>,
        registry: ChildProcessRegistry,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            sandbox,
            run_dirs,
            registry,
            kill_ledger: Mutex::new(None),
            cancel_tokens: Mutex::new(Vec::new()),
        }
    }

    /// The turn's kill ledger (§10.2): kills are recorded
    /// here so the turn-end audit can name them. Set by the
    /// worker/coordinator at turn wiring time.
    pub fn set_kill_ledger(&self, ledger: KillLedger) {
        *self.kill_ledger.lock().unwrap() = Some(ledger);
    }

    /// The turn's cancel tokens: the wait loop polls them so
    /// a cancel kills the child promptly. Set by the
    /// worker/coordinator at turn wiring time.
    pub fn set_cancel_tokens(&self, tokens: Vec<CancelToken>) {
        *self.cancel_tokens.lock().unwrap() = tokens;
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

impl Tool for ExecuteCodeTool {
    fn name(&self) -> &'static str {
        "execute_code"
    }

    fn description(&self) -> &'static str {
        "Run python3 or sh code inside a bubblewrap sandbox (no network, no display, workdir only). Always requires approval. Returns stdout, stderr, exit code, and output files."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "language": {"type": "string", "description": "python3 or sh"},
                "code": {"type": "string", "description": "Code to run inside the sandbox"},
                "inputs": {
                    "type": "array",
                    "description": "Optional input files written into the workdir before the run",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "content": {"type": "string"}
                        },
                        "required": ["name", "content"]
                    }
                }
            },
            "required": ["language", "code"]
        })
    }

    fn risk(&self) -> crate::guards::Risk {
        crate::guards::Risk::Consequential
    }

    fn execute(&self, args: &Value) -> Result<Value, MimintError> {
        let language = args
            .get("language")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MimintError::Tool("invalid_arguments: execute_code needs a language".to_string())
            })?;
        let code = args.get("code").and_then(Value::as_str).ok_or_else(|| {
            MimintError::Tool("invalid_arguments: execute_code needs code".to_string())
        })?;
        let mut inputs = Vec::new();
        if let Some(list) = args.get("inputs") {
            let list = list.as_array().ok_or_else(|| {
                MimintError::Tool("invalid_arguments: inputs must be an array".to_string())
            })?;
            for item in list {
                let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                    MimintError::Tool("invalid_arguments: input needs a name".to_string())
                })?;
                let content = item.get("content").and_then(Value::as_str).ok_or_else(|| {
                    MimintError::Tool("invalid_arguments: input needs content".to_string())
                })?;
                inputs.push((name.to_string(), content.to_string()));
            }
        }
        let request = CodeRunRequest {
            language: language.to_string(),
            code: code.to_string(),
            inputs,
            agent_id: self.agent_id.clone(),
            cancel_tokens: self.cancel_tokens.lock().unwrap().clone(),
            registry: Some(self.registry.clone()),
            kill_ledger: self.kill_ledger.lock().unwrap().clone(),
            run_dirs: Some(Arc::clone(&self.run_dirs)),
        };
        let output = self.sandbox.run(request)?;
        Ok(output.to_json())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_denied_paths_detects_only_overlayed_trees() {
        // Python traceback: the /home path counts, the
        // /usr/lib frame does not.
        let out = json!({
            "exit_code": 1,
            "stderr": "  File \"/usr/lib/python3.11/runpy.py\", line 198\nFileNotFoundError: [Errno 2] No such file or directory: '/home/adminis/g2-secret.txt'",
        });
        assert_eq!(
            sandbox_denied_paths(&out),
            vec!["/home/adminis/g2-secret.txt".to_string()]
        );
        // Shell form, bare token with a trailing colon.
        let out = json!({
            "exit_code": 1,
            "stderr": "cat: /root/secret.txt: Permission denied",
        });
        assert_eq!(
            sandbox_denied_paths(&out),
            vec!["/root/secret.txt".to_string()]
        );
        // Deduplicated, in order of first mention.
        let out = json!({
            "exit_code": 1,
            "stderr": "open('/tmp/a.txt') failed; open('/tmp/a.txt') failed; open('/tmp/b.txt') failed: No such file or directory",
        });
        assert_eq!(
            sandbox_denied_paths(&out),
            vec!["/tmp/a.txt".to_string(), "/tmp/b.txt".to_string()]
        );
        // Zero exit: nothing, even with the same stderr.
        let out = json!({
            "exit_code": 0,
            "stderr": "FileNotFoundError: '/home/adminis/x.txt'",
        });
        assert!(sandbox_denied_paths(&out).is_empty());
        // Missing exit code: nothing.
        let out = json!({"stderr": "Permission denied: '/home/adminis/x.txt'"});
        assert!(sandbox_denied_paths(&out).is_empty());
        // No access-error keyword: nothing.
        let out = json!({
            "exit_code": 1,
            "stderr": "saw path /home/adminis/x.txt in a log line",
        });
        assert!(sandbox_denied_paths(&out).is_empty());
        // A missing file inside /work is not a denial.
        let out = json!({
            "exit_code": 1,
            "stderr": "FileNotFoundError: [Errno 2] No such file or directory: 'missing.txt'",
        });
        assert!(sandbox_denied_paths(&out).is_empty());
        let out = json!({
            "exit_code": 1,
            "stderr": "cat: /work/nope.txt: No such file or directory",
        });
        assert!(sandbox_denied_paths(&out).is_empty());
    }

    #[test]
    fn execute_code_input_hashes_hash_name_and_content() {
        let args = json!({
            "language": "python3",
            "code": "print(1)",
            "inputs": [
                {"name": "data.txt", "content": "hello"},
                {"name": "other.txt", "content": ""},
            ],
        });
        let hashes = execute_code_input_hashes(&args);
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes[0].name, "data.txt");
        assert_eq!(hashes[0].sha256, sha256_hex(b"hello"));
        assert_eq!(hashes[1].sha256, sha256_hex(b""));
        // No inputs key, or a non-array, hashes nothing.
        assert!(execute_code_input_hashes(&json!({"code": "x"})).is_empty());
        assert!(execute_code_input_hashes(&json!({"inputs": {"a": "b"}})).is_empty());
        // The output JSON carries the hashes for the audit layer.
        let out = CodeRunOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            terminated_by_signal: None,
            truncated: false,
            duration_ms: 1,
            sandbox_verdict: VERDICT_BWRAP,
            code_sha256: sha256_hex(b"print(1)"),
            input_files_sha256: hashes,
            language: "python3".to_string(),
            code_len: 8,
            workdir: PathBuf::from("/work"),
            artifacts: Vec::new(),
        };
        let rendered = out.to_json();
        assert_eq!(rendered["input_files_sha256"][0]["name"], "data.txt");
        assert_eq!(
            rendered["input_files_sha256"][0]["sha256"],
            sha256_hex(b"hello")
        );
    }

    #[test]
    fn bwrap_argv_matches_the_proven_flag_order() {
        let argv = bwrap_argv(
            Path::new("/home/tester"),
            Path::new("/home/tester/.mimint-code-runs/run-1"),
            &[],
            &["python3".to_string(), "/work/code.py".to_string()],
        );
        let expected: Vec<String> = [
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/lib",
            "/lib",
            "--ro-bind",
            "/lib64",
            "/lib64",
            "--ro-bind",
            "/bin",
            "/bin",
            "--ro-bind",
            "/etc",
            "/etc",
            "--size",
            "67108864",
            "--tmpfs",
            "/home/tester",
            "--size",
            "67108864",
            "--tmpfs",
            "/tmp",
            "--bind",
            "/home/tester/.mimint-code-runs/run-1",
            "/work",
            "--chdir",
            "/work",
            "python3",
            "/work/code.py",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(argv, expected);
    }

    #[test]
    fn bwrap_argv_extra_binds_land_after_the_tmpfs_pairs() {
        let argv = bwrap_argv(
            Path::new("/home/tester"),
            Path::new("/home/tester/work"),
            &[(PathBuf::from("/opt/server"), PathBuf::from("/opt/server"))],
            &["sh".to_string()],
        );
        let bind_pos = argv
            .iter()
            .position(|a| a == "/opt/server")
            .expect("extra bind present");
        // Both --tmpfs pairs precede the extra bind, and the
        // workdir bind follows it: binds under $HOME must
        // land after the tmpfs or the tmpfs hides them.
        let last_tmpfs = argv.iter().rposition(|a| a == "--tmpfs").unwrap();
        let workdir_bind = argv.iter().position(|a| a == "--bind").unwrap();
        assert!(bind_pos > last_tmpfs, "extra binds follow --tmpfs");
        assert!(bind_pos < workdir_bind, "extra binds precede --bind /work");
    }

    #[test]
    fn os_denial_matrix() {
        assert_eq!(os_sandbox_denial("linux"), None);
        assert_eq!(
            os_sandbox_denial("macos"),
            Some("sandbox_unverified_on_macos")
        );
        assert_eq!(
            os_sandbox_denial("windows"),
            Some("sandbox_unverified_on_windows")
        );
        assert_eq!(os_sandbox_denial("freebsd"), Some("sandbox_unavailable"));
        assert_eq!(os_sandbox_denial(""), Some("sandbox_unavailable"));
    }

    #[test]
    fn unsupported_language_is_denied_before_any_spawn() {
        // A bogus bwrap path proves the language check runs
        // before preflight/spawn: the error is about the
        // language, not the sandbox.
        let policy = SandboxPolicy::from_config(&SandboxConfig::default())
            .with_bwrap_path("/nonexistent/bwrap");
        let sandbox = Sandbox::new(policy);
        let err = sandbox
            .run(CodeRunRequest {
                language: "ruby".to_string(),
                code: "puts 1".to_string(),
                inputs: Vec::new(),
                agent_id: "agent-001".to_string(),
                cancel_tokens: Vec::new(),
                registry: None,
                kill_ledger: None,
                run_dirs: None,
            })
            .expect_err("ruby must be denied");
        assert!(err.message().starts_with("unsupported_language:"), "{err}");
    }

    #[test]
    fn preflight_uses_new_flags_and_never_sets_nproc() {
        // Gate test (§5.6): the source itself must carry the
        // full new flag set, and the only setrlimit calls
        // must be FSIZE/AS — NPROC before bwrap broke spawn
        // (05-rlimit-scope.log) and is banned.
        let source = include_str!("sandbox.rs");
        for flag in [
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--tmpfs",
            "--size",
            "0o700",
        ] {
            assert!(source.contains(flag), "source must contain {flag}");
        }
        let mut setrlimit_calls = 0;
        for line in source.lines() {
            // Only real call statements (a trimmed line
            // starting with the call); this gate's own
            // scanning lines mention the name but are not
            // calls.
            let trimmed = line.trim_start();
            if trimmed.starts_with("set_rlimit(") {
                setrlimit_calls += 1;
                assert!(
                    line.contains("RLIMIT_FSIZE") || line.contains("RLIMIT_AS"),
                    "setrlimit call must be FSIZE or AS only: {line}"
                );
            }
        }
        assert_eq!(setrlimit_calls, 2, "exactly FSIZE + AS");
    }

    #[test]
    fn input_name_validation_matrix() {
        assert!(is_safe_input_name("data.txt"));
        assert!(is_safe_input_name("a-b_c.1"));
        assert!(!is_safe_input_name(""));
        assert!(!is_safe_input_name("."));
        assert!(!is_safe_input_name(".."));
        assert!(!is_safe_input_name("../escape"));
        assert!(!is_safe_input_name("sub/file"));
        assert!(!is_safe_input_name("a\\b"));
        assert!(!is_safe_input_name("a\0b"));
    }

    #[test]
    fn workdir_is_under_home_and_0700() {
        let fake_home =
            std::env::temp_dir().join(format!("mimint-sandbox-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&fake_home);
        std::fs::create_dir_all(&fake_home).unwrap();
        let dir = create_workdir(&fake_home, "unit-test").unwrap();
        assert!(dir.starts_with(&fake_home));
        assert!(dir.starts_with(fake_home.join(".mimint-code-runs")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "workdir must be 0700");
        }
        let _ = std::fs::remove_dir_all(&fake_home);
    }

    #[test]
    fn kill_verdict_describe_format() {
        let verdict = KillVerdict {
            outcome: KillOutcome::KilledTerm,
            strays: 0,
            elapsed_ms: 120,
        };
        assert_eq!(verdict.describe(), "killed_term, strays=0, 120ms");
    }

    #[test]
    fn run_dir_tracker_cleanup_removes_dirs() {
        let tracker = RunDirTracker::new();
        let dir = std::env::temp_dir().join(format!("mimint-tracker-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x"), b"x").unwrap();
        tracker.track(dir.clone());
        assert_eq!(tracker.len(), 1);
        assert_eq!(tracker.cleanup(), 1);
        assert!(!dir.exists());
        assert!(tracker.is_empty());
        assert_eq!(tracker.cleanup(), 0);
    }

    #[test]
    fn child_registry_tracks_and_clears() {
        let registry = ChildProcessRegistry::new();
        assert!(registry.is_empty());
        registry.register("agent-001", 4242);
        registry.register("agent-001", 4243);
        registry.register("agent-002", 5000);
        assert_eq!(registry.pids("agent-001").len(), 2);
        registry.unregister("agent-001", 4242);
        assert_eq!(registry.pids("agent-001"), vec![4243]);
        registry.unregister("agent-001", 4243);
        registry.unregister("agent-002", 5000);
        assert!(registry.is_empty());
        // kill_all on an empty agent is a silent no-op.
        assert!(registry.kill_all("agent-001").is_empty());
    }
}
