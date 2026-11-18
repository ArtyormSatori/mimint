//! MCP client (Phase 6, §6 + ADR 0001): STDIO transport only,
//! NDJSON JSON-RPC 2.0, legacy handshake generation.
//!
//! Protocol: the client offers `2025-06-18` and accepts
//! `2025-06-18`/`2025-11-25`. The stateless `2026-07-28`
//! generation and every unknown version are refused with a
//! clear message — no faked handshake, no silent downgrade
//! (see `docs/adr/0001-mcp-legacy-protocol.md`).
//!
//! Typestate: only [`ConnectedClient`] — obtainable solely via
//! a successful `initialize` — exposes `list_tools`/`call`.
//! There is no code path to `tools/list` without a completed
//! handshake; a behavioural test (a stateless fake server)
//! proves no `tools/list` is ever sent.
//!
//! Trust: nothing from a server is believed. Schemas pass a
//! clamp (depth ≤ 8, description ≤ 512 bytes, catalog ≤ 32 KiB
//! per server, `required` must name declared properties);
//! arguments are validated recursively before sending; every
//! MCP tool is `Consequential`; descriptions and outputs are
//! framed as untrusted data naming the server. Binary
//! integrity is NOT verified (F6D-21): no hash pinning in
//! Phase 6 — see the operations docs.
//!
//! On Linux, servers run inside the same bwrap sandbox as
//! `execute_code` (§5.1). The server binary is ro-bound when
//! it lives outside the read-only prefixes, and so is any
//! absolute-path argument naming an existing regular file
//! (a script entrypoint, e.g. a Python MCP server): without
//! that bind, script-based servers could not load at all.
//! This is a deliberate extension of the §6.3 bind rule — the
//! design names only the binary — documented here and in the
//! final report. Servers needing files beyond their binary,
//! their script, and the read-only prefixes cannot run
//! sandboxed; that is a known limitation, not a fallback.
//!
//! Unsandboxed servers (D-DESIGN-2) exist only on macOS /
//! Windows, only per-server opt-in with a server-level
//! confirmation, and never as a fallback when the sandbox
//! fails. Linux refuses `unsandboxed = true` at config
//! validation; Android has no MCP at all.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::agents::AGENT_SYSTEM;
use crate::audit::AuditLog;
use crate::config::{McpServerConfig, MAX_MCP_SERVERS};
use crate::error::MimintError;
use crate::sandbox::{
    bwrap_argv, create_workdir_under, discover_init, home_dir, spawn_bwrap_child,
    spawn_direct_child, terminate_child, unique_suffix, ChildProcessRegistry, Sandbox,
};
use crate::tools::Tool;

/// Hard cap per raw NDJSON frame, read via `take()` before
/// allocating the frame buffer (F6D-05).
pub const MCP_FRAME_CAP_BYTES: usize = 256 * 1024;

/// Stderr ring buffer: only the last bytes are kept (F6D-05).
pub const MCP_STDERR_RING_BYTES: usize = 16 * 1024;

/// Max tools per server (§6.2).
pub const MCP_MAX_TOOLS_PER_SERVER: usize = 100;

/// Max total catalog (schema JSON + descriptions) per server
/// (F6D-06).
pub const MCP_CATALOG_CAP_BYTES: usize = 32 * 1024;

/// Max tool description, in bytes (F6D-06).
pub const MCP_DESCRIPTION_CAP_BYTES: usize = 512;

/// Max schema nesting depth and max argument nesting depth
/// (F6D-13).
pub const MCP_MAX_DEPTH: usize = 8;

/// Max serialized `tools/call` arguments (F6D-13).
pub const MCP_ARGS_CAP_BYTES: usize = 64 * 1024;

/// Max combined `tools/call` output (text + structured) (§6.2).
pub const MCP_OUTPUT_CAP_BYTES: usize = 64 * 1024;

/// Max full tool name (`mcp_<server>_<tool>`), in chars (§6.2).
pub const MCP_MAX_TOOL_NAME_LEN: usize = 64;

/// Protocol version the client offers in `initialize`.
pub const MCP_PROTOCOL_VERSION_OFFERED: &str = "2025-06-18";

/// Protocol versions the client accepts (D-DESIGN-3).
pub const MCP_PROTOCOL_VERSIONS_ACCEPTED: [&str; 2] = ["2025-06-18", "2025-11-25"];

/// Max extra read-only binds for server files (binary +
/// script args) in the sandboxed spawn path.
const MAX_EXTRA_BINDS: usize = 16;

/// Recently-cancelled request ids remembered so a late
/// response is ignored instead of treated as a protocol
/// error.
const CANCELLED_ID_MEMORY: usize = 128;

/// Client identification sent in `initialize`.
const CLIENT_NAME: &str = "mimint";
const CLIENT_VERSION: &str = "0.1.0";

/// MCP failure. Converted to [`MimintError::Tool`] with a
/// reason prefix at the tool boundary.
#[derive(Debug)]
pub enum McpError {
    /// Malformed frames, wrong ids, batching, bad handshake
    /// content — the server is dead to us.
    Protocol(String),
    /// The server process died or its pipes broke.
    ServerDead(String),
    /// A call exceeded its timeout (the caller kills the
    /// server; the dispatcher already sent `cancelled`).
    Timeout(String),
    /// The server requires an unsupported protocol version.
    VersionRejected { requested: String },
    /// A JSON-RPC error response to one call: a tool error,
    /// NOT server death. The caller must not kill the server.
    CallFailed(Value),
    /// The server is dead and already used its one restart
    /// for this turn (§6.4): it rests until the next turn.
    RestartRefused,
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(msg) => write!(f, "mcp_protocol: {msg}"),
            Self::ServerDead(msg) => write!(f, "mcp_server_dead: {msg}"),
            Self::Timeout(msg) => write!(f, "timeout: {msg}"),
            Self::VersionRejected { requested } => write!(
                f,
                "server yêu cầu phiên bản không được hỗ trợ (server: {requested}; hỗ trợ: {})",
                MCP_PROTOCOL_VERSIONS_ACCEPTED.join(", ")
            ),
            Self::CallFailed(error) => write!(f, "mcp call failed: {error}"),
            Self::RestartRefused => write!(
                f,
                "mcp_server_dead: restart already used this turn; the server rests until the next turn"
            ),
        }
    }
}

impl std::error::Error for McpError {}

impl From<McpError> for MimintError {
    fn from(err: McpError) -> Self {
        MimintError::Tool(err.to_string())
    }
}

/// Validate a server-returned protocol version (D-DESIGN-3).
/// Accepts exactly the legacy handshake generation.
pub fn validate_protocol_version(version: &str) -> Result<&str, McpError> {
    if MCP_PROTOCOL_VERSIONS_ACCEPTED.contains(&version) {
        Ok(version)
    } else {
        Err(McpError::VersionRejected {
            requested: version.to_string(),
        })
    }
}

/// Whether MCP exists at all on an OS. Android: no MCP, no
/// exception (§6.3).
pub fn mcp_supported_on_os(target_os: &str) -> bool {
    target_os != "android"
}

/// Pure unsandboxed gate (D-DESIGN-2): Linux always refuses;
/// macOS/Windows require a server-level confirmation;
/// Android refuses everything.
pub fn unsandboxed_connect_allowed(
    target_os: &str,
    unsandboxed: bool,
    confirmed: bool,
) -> Result<(), String> {
    if !mcp_supported_on_os(target_os) {
        return Err("MCP servers are not available on Android".to_string());
    }
    if unsandboxed && target_os == "linux" {
        return Err(
            "unsandboxed=true is rejected on Linux: MCP servers must run in the sandbox"
                .to_string(),
        );
    }
    if unsandboxed && !confirmed {
        return Err(
            "unsandboxed server requires a server-level confirmation before its first start"
                .to_string(),
        );
    }
    Ok(())
}

/// Whether a config file path is outside every directory a
/// tool can write (D-DESIGN-2): outside both sandbox workdir
/// roots and outside the system temp dir. The model must
/// never add a server by editing the config file.
pub fn config_path_outside_workdirs(config_path: &Path, home: &Path) -> bool {
    let absolute = if config_path.is_absolute() {
        config_path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| home.to_path_buf())
            .join(config_path)
    };
    if absolute.starts_with(home.join(".mimint-code-runs")) {
        return false;
    }
    if absolute.starts_with(home.join(".mimint-mcp-runs")) {
        return false;
    }
    if absolute.starts_with(std::env::temp_dir()) {
        return false;
    }
    true
}

/// Strip control characters (including newlines) from a
/// server-supplied string before it enters a prompt (F6D-06).
pub fn strip_control_chars(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Sanitize one tool-name segment to `[a-z0-9_]`: lowercase
/// ASCII, every other character becomes `_`.
pub fn sanitize_tool_name(raw: &str) -> String {
    strip_control_chars(raw)
        .chars()
        .map(|c| {
            let lower = c.to_ascii_lowercase();
            if lower.is_ascii_lowercase() || lower.is_ascii_digit() || lower == '_' {
                lower
            } else {
                '_'
            }
        })
        .collect()
}

/// Truncate to at most `cap` bytes, at a char boundary.
/// Returns the truncated string and whether truncation happened.
pub fn truncate_bytes(text: &str, cap: usize) -> (String, bool) {
    if text.len() <= cap {
        return (text.to_string(), false);
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Full registry name `mcp_<server>_<tool>`, truncated to
/// [`MCP_MAX_TOOL_NAME_LEN`] chars. Duplicate checks happen
/// AFTER this truncation (§6.2).
pub fn full_tool_name(server: &str, tool: &str) -> String {
    let full = format!("mcp_{}_{}", server, sanitize_tool_name(tool));
    if full.chars().count() <= MCP_MAX_TOOL_NAME_LEN {
        full
    } else {
        full.chars().take(MCP_MAX_TOOL_NAME_LEN).collect()
    }
}

/// Frame a server description as untrusted data naming its
/// server, for the prompt (F6D-06).
pub fn frame_description(server: &str, description: &str) -> String {
    format!("[UNTRUSTED MCP DATA — server \"{server}\"] {description}")
}

/// Frame tool output text as untrusted data naming its
/// server and tool (F6D-06).
pub fn frame_output_text(server: &str, tool: &str, text: &str) -> String {
    format!(
        "[UNTRUSTED_MCP_OUTPUT server=\"{server}\" tool=\"{tool}\"]{text}[/UNTRUSTED_MCP_OUTPUT]"
    )
}

/// Schema nesting depth: 1 for a leaf schema, +1 per level
/// of `properties`/`items` nesting. Reject when > 8 (§6.2).
fn schema_depth(schema: &Value) -> usize {
    let mut deepest_child = 0;
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for child in properties.values() {
            deepest_child = deepest_child.max(schema_depth(child));
        }
    }
    if let Some(items) = schema.get("items") {
        deepest_child = deepest_child.max(schema_depth(items));
    }
    1 + deepest_child
}

/// A server tool definition after the client-side clamp (§6.2).
#[derive(Debug, Clone)]
pub struct ClampedTool {
    /// Original server-side tool name (sent in `tools/call`).
    pub name: String,
    /// Registry name, sanitized + truncated (§6.2).
    pub full_name: String,
    /// Description after control-strip + 512-byte truncation.
    pub description: String,
    /// Whether the description was truncated.
    pub description_truncated: bool,
    /// Clamped input schema: exactly `type`/`properties`/
    /// `required`; nothing else from the server survives.
    pub input_schema: Value,
    /// Catalog budget consumed: clamped schema JSON bytes +
    /// description bytes.
    pub catalog_bytes: usize,
}

/// Clamp one raw tool definition from `tools/list`. Rejects
/// malformed schemas; truncates (never rejects) long
/// descriptions.
pub fn clamp_tool_schema(server: &str, raw: &Value) -> Result<ClampedTool, String> {
    let raw = raw
        .as_object()
        .ok_or_else(|| "tool definition is not an object".to_string())?;
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tool definition lacks a string name".to_string())?;
    if name.is_empty() {
        return Err("tool definition has an empty name".to_string());
    }
    let sanitized = sanitize_tool_name(name);
    if sanitized.trim_matches('_').is_empty() {
        return Err(format!("tool name {name:?} sanitizes to nothing usable"));
    }
    let description = match raw.get("description") {
        None => String::new(),
        Some(Value::String(text)) => strip_control_chars(text),
        Some(_) => return Err(format!("tool {name:?} has a non-string description")),
    };
    let (description, description_truncated) =
        truncate_bytes(&description, MCP_DESCRIPTION_CAP_BYTES);
    let schema = raw
        .get("inputSchema")
        .ok_or_else(|| format!("tool {name:?} lacks inputSchema"))?;
    let schema_obj = schema
        .as_object()
        .ok_or_else(|| format!("tool {name:?} inputSchema is not an object"))?;
    let has_properties = schema_obj.contains_key("properties");
    match schema_obj.get("type").and_then(Value::as_str) {
        Some("object") => {}
        None if has_properties => {}
        Some(other) => {
            return Err(format!(
                "tool {name:?} inputSchema type is {other:?}, not object"
            ));
        }
        None => {
            return Err(format!(
                "tool {name:?} inputSchema has neither type nor properties"
            ));
        }
    }
    let properties = match schema_obj.get("properties") {
        None => serde_json::Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => {
            return Err(format!("tool {name:?} inputSchema properties is not a map"));
        }
    };
    if schema_depth(schema) > MCP_MAX_DEPTH {
        return Err(format!(
            "tool {name:?} inputSchema nests deeper than {MCP_MAX_DEPTH}"
        ));
    }
    let mut required: Vec<String> = Vec::new();
    if let Some(required_raw) = schema_obj.get("required") {
        let list = required_raw
            .as_array()
            .ok_or_else(|| format!("tool {name:?} required is not an array"))?;
        for entry in list {
            let entry = entry
                .as_str()
                .ok_or_else(|| format!("tool {name:?} required holds a non-string"))?;
            if !properties.contains_key(entry) {
                return Err(format!(
                    "tool {name:?} requires undeclared property {entry:?}"
                ));
            }
            required.push(entry.to_string());
        }
    }
    let input_schema = json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });
    let catalog_bytes = serde_json::to_string(&input_schema)
        .map(|s| s.len())
        .unwrap_or(0)
        + description.len();
    Ok(ClampedTool {
        name: name.to_string(),
        full_name: full_tool_name(server, name),
        description,
        description_truncated,
        input_schema,
        catalog_bytes,
    })
}

/// Catalog budget accumulator: the running total must stay
/// within [`MCP_CATALOG_CAP_BYTES`] per server (F6D-06).
/// Returns the new total, or an error rejecting the server.
pub fn check_catalog_budget(used: usize, additional: usize) -> Result<usize, String> {
    let total = used + additional;
    if total > MCP_CATALOG_CAP_BYTES {
        return Err(format!(
            "server catalog exceeds {MCP_CATALOG_CAP_BYTES} bytes ({total})"
        ));
    }
    Ok(total)
}

/// Data nesting depth: 0 for scalars, 1 + max child depth
/// for objects/arrays.
fn data_depth(value: &Value) -> usize {
    match value {
        Value::Object(map) => 1 + map.values().map(data_depth).max().unwrap_or(0),
        Value::Array(items) => 1 + items.iter().map(data_depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Validate `tools/call` arguments against a clamped schema
/// (F6D-13): total size, depth, `required` presence, and the
/// primitive types of declared properties, recursively.
/// Full JSON Schema validation (enum/pattern/format/`$ref`)
/// is explicitly out of scope — the server's job.
pub fn validate_args(schema: &Value, args: &Value) -> Result<(), String> {
    let serialized = serde_json::to_string(args).map_err(|e| e.to_string())?;
    if serialized.len() > MCP_ARGS_CAP_BYTES {
        return Err(format!(
            "arguments exceed {MCP_ARGS_CAP_BYTES} bytes ({})",
            serialized.len()
        ));
    }
    if data_depth(args) > MCP_MAX_DEPTH {
        return Err(format!("arguments nest deeper than {MCP_MAX_DEPTH}"));
    }
    let args = args
        .as_object()
        .ok_or_else(|| "arguments must be a JSON object".to_string())?;
    check_object(schema, args, 1)
}

fn check_object(
    schema: &Value,
    args: &serde_json::Map<String, Value>,
    depth: usize,
) -> Result<(), String> {
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for entry in required {
            let entry = entry.as_str().unwrap_or("");
            if !args.contains_key(entry) {
                return Err(format!("missing required argument: {entry}"));
            }
        }
    }
    for (key, value) in args {
        if let Some(prop_schema) = properties.get(key) {
            check_value(prop_schema, value, depth).map_err(|e| format!("{key}: {e}"))?;
        }
        // Undeclared properties are allowed: the client
        // checks declared ones only (§6.2).
    }
    Ok(())
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true, // Unknown type names are not enforced.
    }
}

fn check_value(schema: &Value, value: &Value, depth: usize) -> Result<(), String> {
    if depth > MCP_MAX_DEPTH {
        return Err(format!("value nests deeper than {MCP_MAX_DEPTH}"));
    }
    match schema.get("type") {
        Some(Value::String(expected)) => {
            if !type_matches(expected, value) {
                return Err(format!("expected {expected}, got {value}"));
            }
        }
        Some(Value::Array(options)) => {
            let any = options.iter().any(|o| {
                o.as_str()
                    .map(|expected| type_matches(expected, value))
                    .unwrap_or(false)
            });
            if !any {
                return Err(format!("expected one of {options:?}, got {value}"));
            }
        }
        _ => {}
    }
    if let Some(object) = value.as_object() {
        if schema.get("properties").is_some() {
            check_object(schema, object, depth + 1)?;
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for item in items {
                check_value(item_schema, item, depth + 1)?;
            }
        }
    }
    Ok(())
}

// ---- Transport: bounded frames, reader + dispatcher threads ----

/// One parsed inbound message.
enum IncomingMsg {
    Response {
        id: u64,
        result: Option<Value>,
        error: Option<Value>,
    },
    ServerRequest {
        id: Value,
        method: String,
        // Parsed to validate the frame shape; never acted
        // on. Sampling/roots get -32601 regardless of params.
        #[allow(dead_code)]
        params: Value,
    },
    Notification {
        method: String,
        // Parsed to validate the frame shape; notifications
        // are counted, never logged or acted on.
        #[allow(dead_code)]
        params: Value,
    },
    Eof,
    ProtocolError(String),
}

/// Outcome of one bounded frame read.
#[derive(Debug)]
enum FrameRead {
    Frame(Vec<u8>),
    Eof,
    TooLarge,
}

/// Read one NDJSON frame through `take()` BEFORE allocating
/// the frame buffer (F6D-05): at most cap+1 bytes are ever
/// pulled from the pipe for one frame. A frame that fills
/// the take budget without a newline is oversized.
fn read_frame<R: BufRead>(reader: &mut R) -> std::io::Result<FrameRead> {
    let mut buf = Vec::new();
    let n = reader
        .by_ref()
        .take((MCP_FRAME_CAP_BYTES + 1) as u64)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(FrameRead::Eof);
    }
    if n > MCP_FRAME_CAP_BYTES && !buf.ends_with(b"\n") {
        return Ok(FrameRead::TooLarge);
    }
    if buf.ends_with(b"\n") {
        buf.pop();
        if buf.ends_with(b"\r") {
            buf.pop();
        }
    }
    Ok(FrameRead::Frame(buf))
}

/// Parse one frame into a message. Arrays (batching), scalar
/// frames, wrong `jsonrpc` versions, and id-less/method-less
/// messages are protocol errors. Every parse is fallible;
/// nothing from the server is ever unwrapped.
fn parse_message(bytes: &[u8]) -> Result<IncomingMsg, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON frame: {e}"))?;
    if value.is_array() {
        return Err("batched JSON-RPC message".to_string());
    }
    let obj = value
        .as_object()
        .ok_or_else(|| "frame is not a JSON object".to_string())?;
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err("missing or wrong jsonrpc version".to_string());
    }
    let id = obj.get("id");
    let method = obj.get("method");
    let result = obj.get("result");
    let error = obj.get("error");
    match (id, method) {
        (Some(id), Some(method)) => {
            let method = method
                .as_str()
                .ok_or_else(|| "request method is not a string".to_string())?;
            Ok(IncomingMsg::ServerRequest {
                id: id.clone(),
                method: method.to_string(),
                params: obj.get("params").cloned().unwrap_or(Value::Null),
            })
        }
        (Some(id), None) => {
            if result.is_none() && error.is_none() {
                return Err("response without result or error".to_string());
            }
            let id = id
                .as_u64()
                .ok_or_else(|| "response id is not a number".to_string())?;
            Ok(IncomingMsg::Response {
                id,
                result: result.cloned(),
                error: error.cloned(),
            })
        }
        (None, Some(method)) => {
            let method = method
                .as_str()
                .ok_or_else(|| "notification method is not a string".to_string())?;
            Ok(IncomingMsg::Notification {
                method: method.to_string(),
                params: obj.get("params").cloned().unwrap_or(Value::Null),
            })
        }
        (None, None) => Err("message has neither id nor method".to_string()),
    }
}

type ReplyTx = std::sync::mpsc::SyncSender<Result<Value, McpError>>;

/// Messages into the dispatcher thread: parsed inbound
/// messages from the reader, and outgoing calls from tools.
enum DispatcherMsg {
    Incoming(IncomingMsg),
    Call {
        token: u64,
        method: String,
        params: Value,
        reply: ReplyTx,
    },
    Notify {
        payload: Value,
    },
    Cancel {
        token: u64,
    },
}

struct PendingEntry {
    token: u64,
    reply: ReplyTx,
}

/// The dispatcher thread: owns the server's stdin and the
/// pending map (ids assigned monotonically here). It dies —
/// failing every pending call — on EOF, protocol error, or
/// a broken stdin pipe. It never touches the network and
/// never parses on the tool threads.
fn dispatcher_loop(
    mut stdin: ChildStdin,
    rx: std::sync::mpsc::Receiver<DispatcherMsg>,
    unknown_notifications: Arc<AtomicU64>,
    on_death: Arc<dyn Fn() + Send + Sync>,
) {
    let mut pending: HashMap<u64, PendingEntry> = HashMap::new();
    let mut cancelled: VecDeque<u64> = VecDeque::new();
    let mut next_id: u64 = 1;
    let die = |pending: &mut HashMap<u64, PendingEntry>, reason: McpError| {
        for (_, entry) in pending.drain() {
            let _ = entry.reply.send(Err(match &reason {
                McpError::Protocol(msg) => McpError::Protocol(msg.clone()),
                McpError::ServerDead(msg) => McpError::ServerDead(msg.clone()),
                other => McpError::ServerDead(other.to_string()),
            }));
        }
        on_death();
    };
    for msg in rx {
        match msg {
            DispatcherMsg::Call {
                token,
                method,
                params,
                reply,
            } => {
                let id = next_id;
                next_id += 1;
                let frame = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                });
                if write_frame(&mut stdin, &frame).is_err() {
                    let _ = reply.send(Err(McpError::ServerDead(
                        "failed to write to the server".to_string(),
                    )));
                    die(
                        &mut pending,
                        McpError::ServerDead("stdin pipe broke".to_string()),
                    );
                    return;
                }
                pending.insert(id, PendingEntry { token, reply });
            }
            DispatcherMsg::Notify { payload } => {
                if write_frame(&mut stdin, &payload).is_err() {
                    die(
                        &mut pending,
                        McpError::ServerDead("stdin pipe broke".to_string()),
                    );
                    return;
                }
            }
            DispatcherMsg::Cancel { token } => {
                if let Some((id, _)) = pending.iter().find(|(_, e)| e.token == token) {
                    let id = *id;
                    pending.remove(&id);
                    // Best-effort `notifications/cancelled`
                    // with the real request id (§6.4).
                    let frame = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/cancelled",
                        "params": {"requestId": id, "reason": "cancelled"},
                    });
                    let _ = write_frame(&mut stdin, &frame);
                    cancelled.push_back(id);
                    while cancelled.len() > CANCELLED_ID_MEMORY {
                        cancelled.pop_front();
                    }
                }
            }
            DispatcherMsg::Incoming(IncomingMsg::Response { id, result, error }) => {
                if let Some(entry) = pending.remove(&id) {
                    let outcome = if let Some(error) = error {
                        Err(McpError::CallFailed(error))
                    } else if let Some(result) = result {
                        Ok(result)
                    } else {
                        Err(McpError::Protocol(
                            "response without result or error".to_string(),
                        ))
                    };
                    let _ = entry.reply.send(outcome);
                } else if cancelled.contains(&id) {
                    // Late response to a cancelled call: ignore.
                } else {
                    die(
                        &mut pending,
                        McpError::Protocol(format!("response for unknown id {id}")),
                    );
                    return;
                }
            }
            DispatcherMsg::Incoming(IncomingMsg::ServerRequest {
                id,
                method,
                params: _,
            }) => {
                // The client offers no capabilities (§6.1):
                // every reverse request except `ping` gets a
                // well-formed -32601 (F6D-08), never silence.
                let frame = if method == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
                };
                if write_frame(&mut stdin, &frame).is_err() {
                    die(
                        &mut pending,
                        McpError::ServerDead("stdin pipe broke".to_string()),
                    );
                    return;
                }
            }
            DispatcherMsg::Incoming(IncomingMsg::Notification { method, params: _ }) => {
                // `notifications/tools/list_changed` changes
                // nothing mid-turn (§6.4); every other
                // unknown notification is counted, never
                // logged (content is untrusted server data).
                let _ = &method;
                unknown_notifications.fetch_add(1, Ordering::SeqCst);
            }
            DispatcherMsg::Incoming(IncomingMsg::Eof) => {
                die(
                    &mut pending,
                    McpError::ServerDead("server closed stdout".to_string()),
                );
                return;
            }
            DispatcherMsg::Incoming(IncomingMsg::ProtocolError(reason)) => {
                die(&mut pending, McpError::Protocol(reason));
                return;
            }
        }
    }
    // All senders dropped: fail whatever is left and die.
    die(
        &mut pending,
        McpError::ServerDead("dispatcher channel closed".to_string()),
    );
}

fn write_frame(stdin: &mut ChildStdin, frame: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *stdin, frame)?;
    stdin.write_all(b"\n")?;
    stdin.flush()
}

/// The reader thread: bounded frames off stdout, parsed
/// fallibly, forwarded to the dispatcher.
fn reader_loop(stdout: ChildStdout, tx: std::sync::mpsc::Sender<DispatcherMsg>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_frame(&mut reader) {
            Ok(FrameRead::Eof) => {
                let _ = tx.send(DispatcherMsg::Incoming(IncomingMsg::Eof));
                break;
            }
            Ok(FrameRead::TooLarge) => {
                let _ = tx.send(DispatcherMsg::Incoming(IncomingMsg::ProtocolError(
                    format!("frame exceeds {MCP_FRAME_CAP_BYTES} bytes"),
                )));
                break;
            }
            Ok(FrameRead::Frame(bytes)) => {
                if bytes.is_empty() {
                    continue; // Blank lines are skipped, not errors.
                }
                match parse_message(&bytes) {
                    Ok(message) => {
                        if tx.send(DispatcherMsg::Incoming(message)).is_err() {
                            break;
                        }
                    }
                    Err(reason) => {
                        let _ =
                            tx.send(DispatcherMsg::Incoming(IncomingMsg::ProtocolError(reason)));
                        break;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(DispatcherMsg::Incoming(IncomingMsg::ProtocolError(
                    format!("stdout read failed: {e}"),
                )));
                break;
            }
        }
    }
}

/// The stderr drain thread: the pipe is drained continuously
/// (a full stderr pipe would deadlock the server) into a
/// 16 KiB ring buffer holding only the tail (F6D-05).
fn stderr_loop(stderr: std::process::ChildStderr, ring: Arc<Mutex<VecDeque<u8>>>) {
    let mut reader = stderr;
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let mut ring = ring.lock().unwrap();
                ring.extend(&chunk[..n]);
                let excess = ring.len().saturating_sub(MCP_STDERR_RING_BYTES);
                if excess > 0 {
                    ring.drain(..excess);
                }
            }
            Err(_) => break,
        }
    }
}

/// A live server connection: the dispatcher channel, the
/// child handle slot, and the kill-once machinery. Shared
/// across agents; calls multiplex by id in the dispatcher.
pub(crate) struct McpConnection {
    tx: std::sync::mpsc::Sender<DispatcherMsg>,
    call_counter: AtomicU64,
    child_slot: Mutex<Option<Child>>,
    child_pid: u32,
    init_pid: Option<u32>,
    ns_inode: Option<String>,
    sandboxed: bool,
    killed: AtomicBool,
    /// Serializes `kill_once`: a caller arriving while
    /// another thread is mid-kill BLOCKS until the kill
    /// completes, so "the server is dead" is true by the
    /// time any `kill_once` call returns. Without this,
    /// the dispatcher thread's asynchronous kill raced
    /// the API thread's registry checks.
    kill_lock: Mutex<()>,
    stderr_ring: Arc<Mutex<VecDeque<u8>>>,
    unknown_notifications: Arc<AtomicU64>,
    workdir: Option<PathBuf>,
    registry: ChildProcessRegistry,
}

impl McpConnection {
    /// One request/response round trip. On timeout, a
    /// `cancelled` notification is sent best-effort; the
    /// caller decides whether to kill the server.
    fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, McpError> {
        let token = self.call_counter.fetch_add(1, Ordering::SeqCst);
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(DispatcherMsg::Call {
                token,
                method: method.to_string(),
                params,
                reply: reply_tx,
            })
            .map_err(|_| McpError::ServerDead("dispatcher is gone".to_string()))?;
        match reply_rx.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let _ = self.tx.send(DispatcherMsg::Cancel { token });
                Err(McpError::Timeout(format!(
                    "mcp call {method} exceeded {}s",
                    timeout.as_secs()
                )))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(McpError::ServerDead("dispatcher is gone".to_string()))
            }
        }
    }

    fn notify(&self, payload: Value) {
        let _ = self.tx.send(DispatcherMsg::Notify { payload });
    }

    /// Kill the server process exactly once, from any path
    /// (tool timeout, dispatcher death, disconnect). The
    /// first caller signals + reaps; later callers are
    /// no-ops. Sandboxed servers die namespace-first (§5.1);
    /// unsandboxed servers die best-effort by process group.
    fn kill_once(&self) {
        // Serialize with any in-progress kill: callers
        // must observe the kill as complete on return.
        let _kill_guard = self.kill_lock.lock().unwrap();
        if self.killed.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut child_guard = self.child_slot.lock().unwrap();
        if let Some(mut child) = child_guard.take() {
            #[cfg(unix)]
            {
                if self.sandboxed {
                    let _ = terminate_child(
                        &mut child,
                        self.child_pid,
                        self.init_pid,
                        self.ns_inode.as_deref(),
                        None,
                    );
                } else {
                    kill_process_group_best_effort(&mut child, self.child_pid);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        drop(child_guard);
        self.registry.unregister(AGENT_SYSTEM, self.child_pid);
        if let Some(workdir) = &self.workdir {
            let _ = std::fs::remove_dir_all(workdir);
        }
    }

    fn stderr_tail(&self) -> Vec<u8> {
        self.stderr_ring.lock().unwrap().iter().copied().collect()
    }

    fn unknown_notification_count(&self) -> u64 {
        self.unknown_notifications.load(Ordering::SeqCst)
    }
}

/// Best-effort group kill for unsandboxed servers (§5.1):
/// a child that setsid'd or double-forked escapes the group.
/// Documented limitation of the exception path.
#[cfg(unix)]
fn kill_process_group_best_effort(child: &mut Child, pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
        libc::kill(-(pid as libc::pid_t), libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return,
        }
    }
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// A spawned server that has NOT completed the handshake.
/// The only way forward is [`UnconnectedClient::initialize`];
/// there is no `list_tools` here, by construction.
pub struct UnconnectedClient {
    conn: Arc<McpConnection>,
    timeout: Duration,
}

/// A server that completed a valid legacy handshake. Only
/// this type exposes `list_tools` and `call_tool`.
pub struct ConnectedClient {
    conn: Arc<McpConnection>,
    protocol_version: String,
    server_name: String,
    server_version: String,
    timeout: Duration,
}

impl UnconnectedClient {
    /// `initialize` → validate version → `notifications/initialized`.
    /// Any failure leaves the caller to kill the connection.
    pub fn initialize(self) -> Result<ConnectedClient, McpError> {
        let result = self.conn.call(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION_OFFERED,
                "capabilities": {},
                "clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
            }),
            self.timeout,
        )?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                McpError::Protocol("initialize result lacks protocolVersion".to_string())
            })?;
        let version = validate_protocol_version(version)?.to_string();
        let server_info = result.get("serverInfo").and_then(Value::as_object);
        let (server_name, server_version) = match server_info {
            Some(info) => (
                info.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                info.get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ),
            None => (String::new(), String::new()),
        };
        self.conn.notify(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));
        Ok(ConnectedClient {
            conn: self.conn,
            protocol_version: version,
            server_name,
            server_version,
            timeout: self.timeout,
        })
    }

    pub(crate) fn connection(&self) -> &Arc<McpConnection> {
        &self.conn
    }
}

impl ConnectedClient {
    /// Paginated `tools/list`, accumulating at most
    /// [`MCP_MAX_TOOLS_PER_SERVER`] raw definitions.
    pub fn list_tools(&self) -> Result<Vec<Value>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut params = json!({});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let result = self.conn.call("tools/list", params, self.timeout)?;
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    McpError::Protocol("tools/list result lacks a tools array".to_string())
                })?;
            tools.extend(page.iter().cloned());
            if tools.len() > MCP_MAX_TOOLS_PER_SERVER {
                return Err(McpError::Protocol(format!(
                    "server offers more than {MCP_MAX_TOOLS_PER_SERVER} tools"
                )));
            }
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
        }
        Ok(tools)
    }

    /// One `tools/call`. A JSON-RPC error response is
    /// [`McpError::CallFailed`] (a tool error, not death).
    pub fn call_tool(&self, name: &str, args: Value) -> Result<Value, McpError> {
        self.conn.call(
            "tools/call",
            json!({"name": name, "arguments": args}),
            self.timeout,
        )
    }

    pub fn protocol_version(&self) -> &str {
        &self.protocol_version
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn server_version(&self) -> &str {
        &self.server_version
    }
}

// ---- Spawn, handle, manager, tool ----

/// Read-only prefixes already bound by the frozen flag set.
/// A server binary (or script) under one of these needs no
/// extra bind.
fn under_ro_prefix(path: &Path) -> bool {
    path.starts_with("/usr")
        || path.starts_with("/lib")
        || path.starts_with("/lib64")
        || path.starts_with("/bin")
        || path.starts_with("/etc")
}

/// Extra read-only binds for a sandboxed server: the server
/// binary when it lives outside the ro prefixes, plus any
/// absolute-path argument naming an existing regular file
/// (a script entrypoint). Capped; the binary comes first.
fn extra_binds_for_server(command: &str, args: &[String]) -> Vec<(PathBuf, PathBuf)> {
    let mut binds: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut consider = |path: &Path| {
        if binds.len() >= MAX_EXTRA_BINDS {
            return;
        }
        if !path.is_absolute() || under_ro_prefix(path) {
            return;
        }
        if !path.is_file() {
            return;
        }
        if seen.insert(path.to_path_buf()) {
            binds.push((path.to_path_buf(), path.to_path_buf()));
        }
    };
    consider(Path::new(command));
    for arg in args {
        consider(Path::new(arg));
    }
    binds
}

/// Start the reader/dispatcher/stderr threads around a
/// spawned child and return the shared connection. Threads
/// are detached: they exit when the child's pipes close
/// (kill or natural death). Dropping the last `Arc` signals
/// the child (see `Drop`), so an undisconnected connection
/// cannot leak a live server.
#[allow(clippy::too_many_arguments)]
fn start_connection(
    child: Child,
    sandboxed: bool,
    init_pid: Option<u32>,
    ns_inode: Option<String>,
    workdir: Option<PathBuf>,
    registry: ChildProcessRegistry,
) -> Result<Arc<McpConnection>, String> {
    let mut child = child;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "spawned server has no stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "spawned server has no stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "spawned server has no stderr".to_string())?;
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel::<DispatcherMsg>();
    let stderr_ring: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
    let unknown_notifications = Arc::new(AtomicU64::new(0));
    let conn = Arc::new_cyclic(|weak: &std::sync::Weak<McpConnection>| {
        let weak = weak.clone();
        let on_death: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(conn) = weak.upgrade() {
                conn.kill_once();
            }
        });
        let reader_tx = tx.clone();
        std::thread::spawn(move || reader_loop(stdout, reader_tx));
        let ring = Arc::clone(&stderr_ring);
        std::thread::spawn(move || stderr_loop(stderr, ring));
        let unknown = Arc::clone(&unknown_notifications);
        std::thread::spawn(move || dispatcher_loop(stdin, rx, unknown, on_death));
        McpConnection {
            tx,
            call_counter: AtomicU64::new(0),
            child_slot: Mutex::new(Some(child)),
            child_pid: pid,
            init_pid,
            ns_inode,
            sandboxed,
            killed: AtomicBool::new(false),
            kill_lock: Mutex::new(()),
            stderr_ring,
            unknown_notifications,
            workdir,
            registry,
        }
    });
    conn.registry.register(AGENT_SYSTEM, pid);
    Ok(conn)
}

impl Drop for McpConnection {
    /// Last-resort signalling (never blocking, never
    /// panicking): a connection dropped without disconnect
    /// must not leak a live server. Explicit paths use
    /// `kill_once`, which reaps; this only signals.
    fn drop(&mut self) {
        if self.killed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(mut slot) = self.child_slot.try_lock() {
            if let Some(child) = slot.take() {
                #[cfg(unix)]
                unsafe {
                    libc::kill(self.child_pid as libc::pid_t, libc::SIGTERM);
                    libc::kill(-(self.child_pid as libc::pid_t), libc::SIGTERM);
                    if let Some(init) = self.init_pid {
                        libc::kill(init as libc::pid_t, libc::SIGTERM);
                    }
                }
                #[cfg(not(unix))]
                {
                    let mut child = child;
                    let _ = child.kill();
                }
                // Deliberately not waited: Drop must not block.
                #[cfg(unix)]
                drop(child);
            }
        }
        self.registry.unregister(AGENT_SYSTEM, self.child_pid);
    }
}

enum HandleState {
    Disconnected,
    Connected(Arc<ConnectedClient>),
    Dead,
}

/// One configured server: its connection state, catalog,
/// restart budget, and unsandboxed confirmation. Shared
/// across agents; the tools of every agent call through it.
pub struct McpServerHandle {
    config: McpServerConfig,
    sandbox: Arc<Sandbox>,
    audit: Arc<AuditLog>,
    registry: ChildProcessRegistry,
    turn_cell: Arc<Mutex<String>>,
    confirmed: AtomicBool,
    state: Mutex<HandleState>,
    catalog: Mutex<Option<Vec<ClampedTool>>>,
    last_attempt_turn: Mutex<Option<String>>,
    connection: Mutex<Option<Arc<McpConnection>>>,
}

impl McpServerHandle {
    pub fn new(
        config: McpServerConfig,
        sandbox: Arc<Sandbox>,
        audit: Arc<AuditLog>,
        registry: ChildProcessRegistry,
        turn_cell: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            config,
            sandbox,
            audit,
            registry,
            turn_cell,
            confirmed: AtomicBool::new(false),
            state: Mutex::new(HandleState::Disconnected),
            catalog: Mutex::new(None),
            last_attempt_turn: Mutex::new(None),
            connection: Mutex::new(None),
        }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    /// Negotiated protocol version, when connected.
    pub fn protocol_version(&self) -> Option<String> {
        match &*self.state.lock().unwrap() {
            HandleState::Connected(client) => Some(client.protocol_version().to_string()),
            _ => None,
        }
    }

    pub fn catalog_snapshot(&self) -> Option<Vec<ClampedTool>> {
        self.catalog.lock().unwrap().clone()
    }

    pub fn stderr_tail(&self) -> Vec<u8> {
        self.connection
            .lock()
            .unwrap()
            .as_ref()
            .map(|conn| conn.stderr_tail())
            .unwrap_or_default()
    }

    pub fn unknown_notification_count(&self) -> u64 {
        self.connection
            .lock()
            .unwrap()
            .as_ref()
            .map(|conn| conn.unknown_notification_count())
            .unwrap_or(0)
    }

    /// Whether this server still needs its server-level
    /// unsandboxed confirmation (D-DESIGN-2).
    pub fn needs_unsandboxed_confirmation(&self) -> bool {
        self.config.unsandboxed
            && !self.confirmed.load(Ordering::SeqCst)
            && matches!(std::env::consts::OS, "macos" | "windows")
    }

    /// Record a server-level unsandboxed confirmation,
    /// audited with who confirmed it (D-DESIGN-2). A
    /// no-op for sandboxed servers.
    pub fn confirm_unsandboxed(&self, confirmed_by: &str) -> Result<(), MimintError> {
        if !self.config.unsandboxed {
            return Ok(());
        }
        self.confirmed.store(true, Ordering::SeqCst);
        self.audit.log(
            "ui-startup",
            "mcp_unsandboxed_confirmed",
            None,
            None,
            json!({
                "mcp_server": self.config.name,
                "confirmed_by": confirmed_by,
            }),
        )
    }

    /// Connect if needed, honouring the one-restart-per-turn
    /// budget (§6.4). Failures are audited and leave the
    /// handle `Dead` with no tools registered.
    pub fn ensure_connected(&self) -> Result<Arc<ConnectedClient>, McpError> {
        {
            let state = self.state.lock().unwrap();
            if let HandleState::Connected(client) = &*state {
                return Ok(Arc::clone(client));
            }
        }
        let turn = self.turn_cell.lock().unwrap().clone();
        {
            let mut last = self.last_attempt_turn.lock().unwrap();
            if *last == Some(turn.clone()) {
                return Err(McpError::RestartRefused);
            }
            *last = Some(turn);
        }
        match self.connect_inner() {
            Ok(client) => {
                *self.state.lock().unwrap() = HandleState::Connected(Arc::clone(&client));
                Ok(client)
            }
            Err(err) => {
                *self.state.lock().unwrap() = HandleState::Dead;
                self.audit_failure(&err);
                Err(err)
            }
        }
    }

    fn audit_failure(&self, err: &McpError) {
        let mut detail = json!({
            "mcp_server": self.config.name,
            "reason": err.to_string(),
        });
        if let McpError::VersionRejected { requested } = err {
            detail["protocol_version"] = json!(requested);
            detail["supported_versions"] = json!(MCP_PROTOCOL_VERSIONS_ACCEPTED);
        }
        let _ = self
            .audit
            .log("ui-startup", "mcp_server_failed", None, None, detail);
    }

    fn connect_inner(&self) -> Result<Arc<ConnectedClient>, McpError> {
        unsandboxed_connect_allowed(
            std::env::consts::OS,
            self.config.unsandboxed,
            self.confirmed.load(Ordering::SeqCst),
        )
        .map_err(McpError::Protocol)?;
        let timeout = Duration::from_secs(self.config.timeout_secs);
        let unconnected = if self.config.unsandboxed {
            self.spawn_direct(timeout)?
        } else {
            self.spawn_sandboxed(timeout)?
        };
        let conn = Arc::clone(unconnected.connection());
        *self.connection.lock().unwrap() = Some(Arc::clone(&conn));
        let connected = match unconnected.initialize() {
            Ok(client) => client,
            Err(err) => {
                conn.kill_once();
                return Err(err);
            }
        };
        let raw_tools = match connected.list_tools() {
            Ok(tools) => tools,
            Err(err) => {
                conn.kill_once();
                return Err(err);
            }
        };
        let catalog = match self.clamp_catalog(&raw_tools) {
            Ok(catalog) => catalog,
            Err(reason) => {
                conn.kill_once();
                return Err(McpError::Protocol(reason));
            }
        };
        let tool_count = catalog.len();
        *self.catalog.lock().unwrap() = Some(catalog);
        let _ = self.audit.log(
            "ui-startup",
            "mcp_server_connected",
            None,
            None,
            json!({
                "mcp_server": self.config.name,
                "server_name": connected.server_name(),
                "server_version": connected.server_version(),
                "protocol_version": connected.protocol_version(),
                "tool_count": tool_count,
                "sandboxed": !self.config.unsandboxed,
            }),
        );
        Ok(Arc::new(connected))
    }

    /// Clamp every tool, enforce the catalog budget, and
    /// reject the server on any within-server name collision
    /// (checked after truncation, §6.2). A malformed tool
    /// rejects only itself; a budget overrun or a collision
    /// rejects the whole server.
    fn clamp_catalog(&self, raw_tools: &[Value]) -> Result<Vec<ClampedTool>, String> {
        let mut catalog = Vec::new();
        let mut names: HashSet<String> = HashSet::new();
        let mut used = 0usize;
        for raw in raw_tools {
            let clamped = match clamp_tool_schema(&self.config.name, raw) {
                Ok(clamped) => clamped,
                Err(reason) => {
                    // One bad tool never kills the server;
                    // it is simply not registered.
                    let _ = self.audit.log(
                        "ui-startup",
                        "mcp_tool_rejected",
                        None,
                        None,
                        json!({"mcp_server": self.config.name, "reason": reason}),
                    );
                    continue;
                }
            };
            used = check_catalog_budget(used, clamped.catalog_bytes)?;
            if !names.insert(clamped.full_name.clone()) {
                return Err(format!(
                    "duplicate tool name after truncation: {}",
                    clamped.full_name
                ));
            }
            catalog.push(clamped);
        }
        Ok(catalog)
    }

    #[cfg(unix)]
    fn spawn_sandboxed(&self, timeout: Duration) -> Result<UnconnectedClient, McpError> {
        self.sandbox.preflight().map_err(|reason| {
            McpError::ServerDead(format!(
                "sandbox unavailable, server cannot start: {reason}"
            ))
        })?;
        let home = home_dir().ok_or_else(|| {
            McpError::ServerDead("sandbox unavailable: HOME is not set".to_string())
        })?;
        let workdir = create_workdir_under(&home, ".mimint-mcp-runs", &unique_suffix())
            .map_err(McpError::Protocol)?;
        let mut command: Vec<String> = vec![self.config.command.clone()];
        command.extend(self.config.args.iter().cloned());
        let binds = extra_binds_for_server(&self.config.command, &self.config.args);
        let argv = bwrap_argv(&home, &workdir, &binds, &command);
        let mut extra_env: Vec<(String, String)> = self
            .config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        extra_env.sort();
        let child = spawn_bwrap_child(self.sandbox.policy(), &argv, &extra_env, Stdio::piped())
            .map_err(|e| McpError::ServerDead(format!("cannot spawn server: {e}")))?;
        let pid = child.id();
        let (init_pid, ns_inode) = discover_init(pid);
        let conn = start_connection(
            child,
            true,
            init_pid,
            ns_inode,
            Some(workdir),
            self.registry.clone(),
        )
        .map_err(McpError::ServerDead)?;
        Ok(UnconnectedClient { conn, timeout })
    }

    #[cfg(not(unix))]
    fn spawn_sandboxed(&self, _timeout: Duration) -> Result<UnconnectedClient, McpError> {
        Err(McpError::ServerDead(
            "sandboxed MCP servers need Unix".to_string(),
        ))
    }

    fn spawn_direct(&self, timeout: Duration) -> Result<UnconnectedClient, McpError> {
        // Unsandboxed exception path: filtered env only —
        // PATH + HOME + declared vars, never ambient secrets.
        let mut env: Vec<(String, String)> = vec![
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string()),
            ),
            (
                "HOME".to_string(),
                std::env::var("HOME").unwrap_or_default(),
            ),
        ];
        let mut declared: Vec<(String, String)> = self
            .config
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        declared.sort();
        env.extend(declared);
        let child = spawn_direct_child(&self.config.command, &self.config.args, &env)
            .map_err(|e| McpError::ServerDead(format!("cannot spawn server: {e}")))?;
        let conn = start_connection(child, false, None, None, None, self.registry.clone())
            .map_err(McpError::ServerDead)?;
        Ok(UnconnectedClient { conn, timeout })
    }

    /// Kill the server, keeping the catalog: the tools stay
    /// registered, and the next turn may restart once (§6.4).
    pub fn kill_server(&self) {
        if let Some(conn) = self.connection.lock().unwrap().as_ref() {
            conn.kill_once();
        }
        let mut state = self.state.lock().unwrap();
        if matches!(*state, HandleState::Connected(_)) {
            *state = HandleState::Dead;
        }
    }

    /// Full teardown: kill, clear the catalog and connection.
    /// Used by manager shutdown and duplicate rejection.
    pub fn disconnect(&self) {
        self.kill_server();
        *self.catalog.lock().unwrap() = None;
        *self.connection.lock().unwrap() = None;
        *self.state.lock().unwrap() = HandleState::Disconnected;
    }
}

/// Parse one `tools/call` result into the framed tool output
/// (§6.2): text + structuredContent under one 64 KiB cap,
/// image/audio/resource rejected, `isError` a tool error.
fn parse_call_result(server: &str, tool: &str, result: Value) -> Result<Value, MimintError> {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut texts = Vec::new();
    for item in &content {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                texts.push(
                    item.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                );
            }
            Some(other) => {
                return Err(MimintError::Tool(format!(
                    "unsupported_content_type: {other}"
                )));
            }
            None => {
                return Err(MimintError::Tool(
                    "unsupported_content_type: content item lacks a type".to_string(),
                ));
            }
        }
    }
    let text = texts.join("\n");
    if is_error {
        return Err(MimintError::Tool(format!("mcp tool error: {text}")));
    }
    let structured = result.get("structuredContent").cloned();
    let (text, structured, truncated) = cap_output(text, structured);
    Ok(json!({
        "mcp_server": server,
        "mcp_tool": tool,
        "text": frame_output_text(server, tool, &text),
        "structured_content": structured,
        "truncated": truncated,
    }))
}

/// Combined output cap (§6.2): text takes the budget first;
/// structured content keeps only the remainder, and is
/// dropped whole (never truncated into invalid JSON) when it
/// does not fit.
fn cap_output(text: String, structured: Option<Value>) -> (String, Option<Value>, bool) {
    if text.len() > MCP_OUTPUT_CAP_BYTES {
        let (text, _) = truncate_bytes(&text, MCP_OUTPUT_CAP_BYTES);
        return (text, None, true);
    }
    let remaining = MCP_OUTPUT_CAP_BYTES - text.len();
    match structured {
        Some(value) => {
            let serialized_len = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
            if serialized_len > remaining {
                (text, None, true)
            } else {
                (text, Some(value), false)
            }
        }
        None => (text, None, false),
    }
}

/// One MCP tool in a `Registry`: always `Consequential`,
/// always validated, always framed (§6.2/§6.3).
pub struct McpTool {
    full_name: &'static str,
    description: &'static str,
    server: String,
    tool: String,
    schema: Value,
    handle: Arc<McpServerHandle>,
    agent_id: String,
}

impl McpTool {
    fn new(
        clamped: &ClampedTool,
        handle: Arc<McpServerHandle>,
        agent_id: impl Into<String>,
    ) -> Self {
        let server = handle.name().to_string();
        Self {
            full_name: Box::leak(clamped.full_name.clone().into_boxed_str()),
            description: Box::leak(
                frame_description(&server, &clamped.description).into_boxed_str(),
            ),
            server,
            tool: clamped.name.clone(),
            schema: clamped.input_schema.clone(),
            handle,
            agent_id: agent_id.into(),
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.full_name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }

    fn risk(&self) -> crate::guards::Risk {
        crate::guards::Risk::Consequential
    }

    fn audit_fields(&self) -> Option<Value> {
        Some(json!({
            "mcp_server": self.server,
            "mcp_tool": self.tool,
            "protocol_version": self.handle.protocol_version().unwrap_or_else(|| "unconnected".to_string()),
            "unsandboxed": self.handle.config().unsandboxed,
        }))
    }

    fn execute(&self, args: &Value) -> Result<Value, MimintError> {
        validate_args(&self.schema, args)
            .map_err(|reason| MimintError::Tool(format!("invalid_arguments: {reason}")))?;
        let client = self.handle.ensure_connected()?;
        match client.call_tool(&self.tool, args.clone()) {
            Ok(result) => parse_call_result(&self.server, &self.tool, result),
            Err(McpError::CallFailed(error)) => {
                Err(MimintError::Tool(format!("mcp tool error: {error}")))
            }
            Err(err @ McpError::Timeout(_)) => {
                // Timeout kills the server (§6.4); the
                // dispatcher already sent `cancelled`.
                self.handle.kill_server();
                Err(err.into())
            }
            Err(err @ (McpError::Protocol(_) | McpError::ServerDead(_))) => {
                self.handle.kill_server();
                Err(err.into())
            }
            Err(err) => Err(err.into()),
        }
    }
}

/// All configured MCP servers: connection, catalogs, the
/// shared turn clock for restart budgets, and the global
/// tool-name registry. One manager per worker.
pub struct McpManager {
    handles: Vec<Arc<McpServerHandle>>,
    turn_cell: Arc<Mutex<String>>,
    audit: Arc<AuditLog>,
    global_names: Mutex<HashSet<String>>,
}

impl McpManager {
    pub fn new(
        configs: Vec<McpServerConfig>,
        audit: Arc<AuditLog>,
        sandbox: Arc<Sandbox>,
        registry: ChildProcessRegistry,
    ) -> Result<Self, MimintError> {
        if configs.len() > MAX_MCP_SERVERS {
            return Err(MimintError::Config(format!(
                "at most {MAX_MCP_SERVERS} mcp-server entries are allowed, got {}",
                configs.len()
            )));
        }
        let turn_cell = Arc::new(Mutex::new("startup".to_string()));
        let handles = configs
            .into_iter()
            .map(|config| {
                Arc::new(McpServerHandle::new(
                    config,
                    Arc::clone(&sandbox),
                    Arc::clone(&audit),
                    registry.clone(),
                    Arc::clone(&turn_cell),
                ))
            })
            .collect();
        Ok(Self {
            handles,
            turn_cell,
            audit,
            global_names: Mutex::new(HashSet::new()),
        })
    }

    /// Advance the shared turn clock (§6.4 restart budgets).
    /// Called by the worker/coordinator at every turn start.
    pub fn begin_turn(&self, turn_id: &str) {
        *self.turn_cell.lock().unwrap() = turn_id.to_string();
    }

    /// Connect every server. One server's failure never
    /// affects the others. Returns per-server outcomes in
    /// config order: the tool count, or the failure reason.
    /// A server whose tool names collide with an earlier
    /// server's (after truncation) is rejected whole (§6.2).
    pub fn connect_all(&self) -> Vec<(String, Result<usize, String>)> {
        let mut outcomes = Vec::new();
        for handle in &self.handles {
            let name = handle.name().to_string();
            match handle.ensure_connected() {
                Ok(_) => {
                    let catalog = handle.catalog_snapshot().unwrap_or_default();
                    let mut collision: Option<String> = None;
                    {
                        let mut global = self.global_names.lock().unwrap();
                        for tool in &catalog {
                            if !global.insert(tool.full_name.clone()) {
                                collision = Some(tool.full_name.clone());
                                break;
                            }
                        }
                    }
                    if let Some(dup) = collision {
                        handle.disconnect();
                        let _ = self.audit.log(
                            "ui-startup",
                            "mcp_server_failed",
                            None,
                            None,
                            json!({
                                "mcp_server": name,
                                "reason": format!("duplicate tool name across servers: {dup}"),
                            }),
                        );
                        outcomes.push((
                            name,
                            Err(format!("duplicate tool name across servers: {dup}")),
                        ));
                    } else {
                        outcomes.push((name, Ok(catalog.len())));
                    }
                }
                Err(err) => outcomes.push((name, Err(err.to_string()))),
            }
        }
        outcomes
    }

    /// Tools for one agent's registry, from every catalog
    /// collected so far (connected or dead-with-catalog).
    /// A server that never connected contributes no tools.
    pub fn tools_for(&self, agent_id: &str) -> Vec<McpTool> {
        let mut tools = Vec::new();
        for handle in &self.handles {
            if let Some(catalog) = handle.catalog_snapshot() {
                for clamped in &catalog {
                    tools.push(McpTool::new(clamped, Arc::clone(handle), agent_id));
                }
            }
        }
        tools
    }

    pub fn server_names(&self) -> Vec<String> {
        self.handles.iter().map(|h| h.name().to_string()).collect()
    }

    pub fn handle(&self, name: &str) -> Option<Arc<McpServerHandle>> {
        self.handles
            .iter()
            .find(|h| h.name() == name)
            .map(Arc::clone)
    }

    /// Record a server-level unsandboxed confirmation
    /// (D-DESIGN-2). Unknown server names are an error.
    pub fn confirm_unsandboxed(&self, server: &str, by: &str) -> Result<(), MimintError> {
        let Some(handle) = self.handle(server) else {
            return Err(MimintError::Config(format!("unknown mcp-server: {server}")));
        };
        handle.confirm_unsandboxed(by)
    }

    /// Tear down every server. Called at worker shutdown.
    pub fn disconnect_all(&self) {
        for handle in &self.handles {
            handle.disconnect();
        }
        self.global_names.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation_matrix() {
        assert!(validate_protocol_version("2025-06-18").is_ok());
        assert!(validate_protocol_version("2025-11-25").is_ok());
        for rejected in ["2026-07-28", "2024-11-05", "2025-03-26", "bogus", ""] {
            let err = validate_protocol_version(rejected).expect_err("must reject");
            let message = err.to_string();
            assert!(
                message.contains("server yêu cầu phiên bản không được hỗ trợ"),
                "{message}"
            );
            assert!(message.contains(rejected), "{message}");
            assert!(message.contains("2025-06-18"), "{message}");
            assert!(message.contains("2025-11-25"), "{message}");
        }
    }

    #[test]
    fn os_support_and_unsandboxed_gate_matrix() {
        assert!(mcp_supported_on_os("linux"));
        assert!(mcp_supported_on_os("macos"));
        assert!(mcp_supported_on_os("windows"));
        assert!(!mcp_supported_on_os("android"));
        // Sandboxed everywhere (except Android) needs no confirmation.
        assert!(unsandboxed_connect_allowed("linux", false, false).is_ok());
        assert!(unsandboxed_connect_allowed("macos", false, false).is_ok());
        // Linux never allows unsandboxed, confirmed or not.
        assert!(unsandboxed_connect_allowed("linux", true, true).is_err());
        // macOS/Windows need the server-level confirmation.
        assert!(unsandboxed_connect_allowed("macos", true, false).is_err());
        assert!(unsandboxed_connect_allowed("windows", true, false).is_err());
        assert!(unsandboxed_connect_allowed("macos", true, true).is_ok());
        assert!(unsandboxed_connect_allowed("windows", true, true).is_ok());
        // Android refuses everything.
        assert!(unsandboxed_connect_allowed("android", false, false).is_err());
        assert!(unsandboxed_connect_allowed("android", true, true).is_err());
    }

    #[test]
    fn config_path_must_be_outside_workdirs() {
        let home = Path::new("/home/tester");
        assert!(config_path_outside_workdirs(
            Path::new("/home/tester/.config/mimint/config.toml"),
            home
        ));
        assert!(!config_path_outside_workdirs(
            Path::new("/home/tester/.mimint-code-runs/run-1/config.toml"),
            home
        ));
        assert!(!config_path_outside_workdirs(
            Path::new("/home/tester/.mimint-mcp-runs/notes/config.toml"),
            home
        ));
        let tmp_config = std::env::temp_dir().join("mimint-config.toml");
        assert!(!config_path_outside_workdirs(&tmp_config, home));
    }

    #[test]
    fn name_sanitizing_truncation_and_stripping() {
        assert_eq!(sanitize_tool_name("EchoText"), "echotext");
        assert_eq!(sanitize_tool_name("read-file.v2"), "read_file_v2");
        assert_eq!(sanitize_tool_name("a b\tc\nd"), "a_bcd");
        assert_eq!(sanitize_tool_name("café"), "caf_");
        assert_eq!(strip_control_chars("a\nb\tc"), "abc");
        assert_eq!(full_tool_name("notes", "Echo"), "mcp_notes_echo");
        let long = full_tool_name("srv", &"t".repeat(100));
        assert_eq!(long.chars().count(), MCP_MAX_TOOL_NAME_LEN);
        assert!(long.starts_with("mcp_srv_"));
        let (text, truncated) = truncate_bytes("aébc", 3);
        assert_eq!(text, "aé");
        assert!(truncated);
        let (text, truncated) = truncate_bytes("ab", 5);
        assert_eq!(text, "ab");
        assert!(!truncated);
    }

    #[test]
    fn framing_names_the_server_and_tool() {
        let desc = frame_description("notes", "reads notes");
        assert!(desc.contains("notes"));
        assert!(desc.contains("UNTRUSTED"));
        assert!(desc.contains("reads notes"));
        let out = frame_output_text("notes", "read", "hello");
        assert!(out.contains("notes"));
        assert!(out.contains("read"));
        assert!(out.contains("hello"));
        assert!(out.starts_with("[UNTRUSTED_MCP_OUTPUT"));
        assert!(out.ends_with("[/UNTRUSTED_MCP_OUTPUT]"));
    }

    fn raw_tool(name: &str, schema: Value, description: &str) -> Value {
        json!({"name": name, "description": description, "inputSchema": schema})
    }

    #[test]
    fn schema_clamp_accepts_valid_and_rejects_malformed() {
        let ok = raw_tool(
            "Echo",
            json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
            "Echoes.",
        );
        let clamped = clamp_tool_schema("srv", &ok).unwrap();
        assert_eq!(clamped.name, "Echo");
        assert_eq!(clamped.full_name, "mcp_srv_echo");
        assert_eq!(clamped.input_schema["required"], json!(["text"]));
        // Missing type + properties is fine (object implied).
        let implied = raw_tool("f", json!({"properties": {"a": {"type": "string"}}}), "");
        assert!(clamp_tool_schema("srv", &implied).is_ok());
        // Missing type AND properties: reject.
        let bare = raw_tool("f", json!({}), "");
        assert!(clamp_tool_schema("srv", &bare).is_err());
        // Non-object type: reject.
        let array = raw_tool("f", json!({"type": "array", "items": {}}), "");
        assert!(clamp_tool_schema("srv", &array).is_err());
        // Properties not a map: reject.
        let bad_props = raw_tool("f", json!({"type": "object", "properties": []}), "");
        assert!(clamp_tool_schema("srv", &bad_props).is_err());
        // Required naming an undeclared property: reject.
        let bad_required = raw_tool(
            "f",
            json!({"type": "object", "properties": {}, "required": ["ghost"]}),
            "",
        );
        assert!(clamp_tool_schema("srv", &bad_required).is_err());
        // Non-string description: reject.
        let bad_desc = json!({"name": "f", "description": 42, "inputSchema": {"type": "object"}});
        assert!(clamp_tool_schema("srv", &bad_desc).is_err());
        // Missing inputSchema: reject.
        let no_schema = json!({"name": "f"});
        assert!(clamp_tool_schema("srv", &no_schema).is_err());
    }

    fn nested_schema(levels: usize) -> Value {
        let mut schema = json!({"type": "string"});
        for _ in 0..levels {
            schema = json!({"type": "object", "properties": {"n": schema}});
        }
        schema
    }

    #[test]
    fn schema_clamp_enforces_depth_eight() {
        // 7 nesting levels + leaf = depth 8: accepted.
        let ok = raw_tool("f", nested_schema(7), "");
        assert!(clamp_tool_schema("srv", &ok).is_ok());
        // 8 nesting levels + leaf = depth 9: rejected.
        let deep = raw_tool("f", nested_schema(8), "");
        assert!(clamp_tool_schema("srv", &deep).is_err());
    }

    #[test]
    fn schema_clamp_truncates_long_descriptions_and_strips_control() {
        let long = "x".repeat(600);
        let raw = raw_tool("f", json!({"type": "object"}), &long);
        let clamped = clamp_tool_schema("srv", &raw).unwrap();
        assert_eq!(clamped.description.len(), MCP_DESCRIPTION_CAP_BYTES);
        assert!(clamped.description_truncated);
        let dirty = raw_tool("f", json!({"type": "object"}), "a\nb\tc");
        let clamped = clamp_tool_schema("srv", &dirty).unwrap();
        assert_eq!(clamped.description, "abc");
        assert!(!clamped.description_truncated);
    }

    #[test]
    fn catalog_budget_is_exact() {
        assert_eq!(
            check_catalog_budget(0, MCP_CATALOG_CAP_BYTES).unwrap(),
            MCP_CATALOG_CAP_BYTES
        );
        assert!(check_catalog_budget(MCP_CATALOG_CAP_BYTES, 1).is_err());
        assert!(check_catalog_budget(100, MCP_CATALOG_CAP_BYTES).is_err());
    }

    fn echo_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": {"type": "string"},
                "count": {"type": "integer"},
                "nested": {"type": "object", "properties": {"flag": {"type": "boolean"}}, "required": ["flag"]},
                "tags": {"type": "array", "items": {"type": "string"}}
            },
            "required": ["text"]
        })
    }

    #[test]
    fn args_validation_matrix() {
        let schema = echo_schema();
        assert!(validate_args(&schema, &json!({"text": "hi"})).is_ok());
        assert!(validate_args(
            &schema,
            &json!({"text": "hi", "count": 3, "nested": {"flag": true}, "tags": ["a"]})
        )
        .is_ok());
        // Missing required.
        assert!(validate_args(&schema, &json!({})).is_err());
        // Wrong primitive type.
        assert!(validate_args(&schema, &json!({"text": 1})).is_err());
        // Integer rejects floats.
        assert!(validate_args(&schema, &json!({"text": "x", "count": 1.5})).is_err());
        // Missing nested required.
        assert!(validate_args(&schema, &json!({"text": "x", "nested": {}})).is_err());
        // Wrong nested type.
        assert!(validate_args(&schema, &json!({"text": "x", "nested": {"flag": "yes"}})).is_err());
        // Wrong array item type.
        assert!(validate_args(&schema, &json!({"text": "x", "tags": [1]})).is_err());
        // Undeclared properties are allowed.
        assert!(validate_args(&schema, &json!({"text": "x", "surprise": true})).is_ok());
        // Non-object args rejected.
        assert!(validate_args(&schema, &json!([1, 2])).is_err());
        // Oversized args rejected.
        let big = json!({"text": "x".repeat(MCP_ARGS_CAP_BYTES)});
        assert!(validate_args(&schema, &big).is_err());
    }

    fn nested_args(levels: usize) -> Value {
        let mut value = json!("leaf");
        for _ in 0..levels {
            value = json!({"n": value});
        }
        value
    }

    #[test]
    fn args_validation_enforces_depth_eight() {
        let schema = json!({"type": "object", "properties": {}});
        // 8 levels of objects: depth 8, accepted (no declared
        // properties, so only the depth/size gates apply).
        assert!(validate_args(&schema, &nested_args(8)).is_ok());
        assert!(validate_args(&schema, &nested_args(9)).is_err());
    }

    #[test]
    fn frame_reader_respects_the_cap_and_eof() {
        use std::io::Cursor;
        // Normal frame + EOF.
        let mut reader = Cursor::new(b"{\"a\":1}\n".to_vec());
        match read_frame(&mut reader).unwrap() {
            FrameRead::Frame(bytes) => assert_eq!(bytes, b"{\"a\":1}"),
            other => panic!("expected a frame, got {other:?}"),
        }
        assert!(matches!(read_frame(&mut reader).unwrap(), FrameRead::Eof));
        // CRLF is stripped.
        let mut reader = Cursor::new(b"{\"a\":1}\r\n".to_vec());
        match read_frame(&mut reader).unwrap() {
            FrameRead::Frame(bytes) => assert_eq!(bytes, b"{\"a\":1}"),
            other => panic!("expected a frame, got {other:?}"),
        }
        // Oversized frame: cap+1 bytes without a newline.
        let big = vec![b'x'; MCP_FRAME_CAP_BYTES + 100];
        let mut reader = Cursor::new(big);
        assert!(matches!(
            read_frame(&mut reader).unwrap(),
            FrameRead::TooLarge
        ));
        // Exactly cap bytes + newline: allowed.
        let mut exact = vec![b'x'; MCP_FRAME_CAP_BYTES];
        exact.push(b'\n');
        let mut reader = Cursor::new(exact);
        match read_frame(&mut reader).unwrap() {
            FrameRead::Frame(bytes) => assert_eq!(bytes.len(), MCP_FRAME_CAP_BYTES),
            other => panic!("expected a frame, got {other:?}"),
        }
        // Partial frame at EOF is returned (the server died
        // mid-frame; the parse layer decides).
        let mut reader = Cursor::new(b"{\"a\":1}".to_vec());
        match read_frame(&mut reader).unwrap() {
            FrameRead::Frame(bytes) => assert_eq!(bytes, b"{\"a\":1}"),
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn message_parsing_matrix() {
        // Response.
        let msg = parse_message(br#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#).unwrap();
        assert!(matches!(msg, IncomingMsg::Response { id: 3, .. }));
        // Error response.
        let msg = parse_message(br#"{"jsonrpc":"2.0","id":3,"error":{"code":-1,"message":"x"}}"#)
            .unwrap();
        assert!(matches!(
            msg,
            IncomingMsg::Response {
                id: 3,
                error: Some(_),
                ..
            }
        ));
        // Server request (string id is fine; echoed verbatim).
        let msg = parse_message(br#"{"jsonrpc":"2.0","id":"abc","method":"ping"}"#).unwrap();
        assert!(matches!(msg, IncomingMsg::ServerRequest { .. }));
        // Notification.
        let msg = parse_message(br#"{"jsonrpc":"2.0","method":"notifications/foo"}"#).unwrap();
        assert!(matches!(msg, IncomingMsg::Notification { .. }));
        // Batching is a protocol error.
        assert!(parse_message(br#"[{"jsonrpc":"2.0","id":1,"result":{}}]"#).is_err());
        // Scalar frame is a protocol error.
        assert!(parse_message(b"42").is_err());
        // Wrong jsonrpc version.
        assert!(parse_message(br#"{"jsonrpc":"1.0","id":1,"result":{}}"#).is_err());
        // Missing jsonrpc.
        assert!(parse_message(br#"{"id":1,"result":{}}"#).is_err());
        // Response with a string id: we only send numeric ids.
        assert!(parse_message(br#"{"jsonrpc":"2.0","id":"abc","result":{}}"#).is_err());
        // Response without result or error.
        assert!(parse_message(br#"{"jsonrpc":"2.0","id":1}"#).is_err());
        // Neither id nor method.
        assert!(parse_message(br#"{"jsonrpc":"2.0"}"#).is_err());
        // Invalid JSON.
        assert!(parse_message(b"not json").is_err());
    }

    #[test]
    fn output_cap_drops_structured_whole_never_partial() {
        let (text, structured, truncated) = cap_output("hello".to_string(), Some(json!({"a": 1})));
        assert_eq!(text, "hello");
        assert_eq!(structured, Some(json!({"a": 1})));
        assert!(!truncated);
        let big_text = "x".repeat(MCP_OUTPUT_CAP_BYTES + 10);
        let (text, structured, truncated) = cap_output(big_text, Some(json!({"a": 1})));
        assert_eq!(text.len(), MCP_OUTPUT_CAP_BYTES);
        assert_eq!(structured, None);
        assert!(truncated);
        let big_structured = json!({"blob": "y".repeat(MCP_OUTPUT_CAP_BYTES)});
        let (text, structured, truncated) = cap_output("hi".to_string(), Some(big_structured));
        assert_eq!(text, "hi");
        assert_eq!(structured, None);
        assert!(truncated);
    }

    #[test]
    fn call_result_parsing_matrix() {
        let ok = parse_call_result(
            "srv",
            "echo",
            json!({"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}], "structuredContent": {"n": 1}}),
        )
        .unwrap();
        assert!(ok["text"].as_str().unwrap().contains("a\nb"));
        assert_eq!(ok["structured_content"], json!({"n": 1}));
        assert_eq!(ok["truncated"], false);
        // Image content is rejected.
        let err = parse_call_result(
            "srv",
            "shot",
            json!({"content": [{"type": "image", "data": "xxx"}]}),
        )
        .expect_err("image must be rejected");
        assert!(
            err.message().starts_with("unsupported_content_type:"),
            "{}",
            err.message()
        );
        // isError is a tool error carrying the text.
        let err = parse_call_result(
            "srv",
            "fail",
            json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
        )
        .expect_err("isError must fail");
        assert!(err.message().contains("boom"), "{}", err.message());
        // Missing content is an empty result, not an error.
        let empty = parse_call_result("srv", "noop", json!({})).unwrap();
        assert!(empty["text"]
            .as_str()
            .unwrap()
            .contains("UNTRUSTED_MCP_OUTPUT"));
    }

    #[test]
    fn extra_binds_cover_binaries_and_scripts_only() {
        let dir = std::env::temp_dir().join(format!("mimint-binds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(&script, "x").unwrap();
        let binary = dir.join("server-bin");
        std::fs::write(&binary, "x").unwrap();
        // Binary outside the prefixes + script arg: both bound.
        let binds = extra_binds_for_server(
            binary.to_str().unwrap(),
            &[script.to_str().unwrap().to_string(), "--flag".to_string()],
        );
        assert_eq!(binds.len(), 2);
        // /usr/bin/python3 needs no bind; the script still does.
        let binds =
            extra_binds_for_server("/usr/bin/python3", &[script.to_str().unwrap().to_string()]);
        assert_eq!(binds.len(), 1);
        assert_eq!(binds[0].0, script);
        // A nonexistent absolute arg is not bound.
        let binds = extra_binds_for_server(
            "/usr/bin/python3",
            &[dir.join("missing.py").to_str().unwrap().to_string()],
        );
        assert!(binds.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn typestate_gate_no_path_to_tools_list_without_initialize() {
        // Gate (§6.6): in production code, the "tools/list"
        // method string appears ONLY inside `impl
        // ConnectedClient`, and `ConnectedClient` is
        // constructed ONLY in `initialize` (plus its own
        // struct definition). The behavioural half of the
        // gate is `mcp_no_faked_handshake` (integration).
        let source = include_str!("mcp.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("test module marker");
        let impl_start = production
            .find("impl ConnectedClient")
            .expect("impl ConnectedClient exists");
        let impl_end = production[impl_start..]
            .find("\nimpl ")
            .map(|i| impl_start + i)
            .unwrap_or(production.len());
        let mut occurrences = 0;
        for (offset, _) in production.match_indices("\"tools/list\"") {
            occurrences += 1;
            assert!(
                offset >= impl_start && offset < impl_end,
                "\"tools/list\" outside impl ConnectedClient at byte {offset}"
            );
        }
        assert_eq!(occurrences, 1, "exactly one tools/list call site");
        let constructions: Vec<&str> = production
            .lines()
            .filter(|l| l.contains("ConnectedClient {"))
            .filter(|l| !l.trim_start().starts_with("impl "))
            .collect();
        assert_eq!(
            constructions.len(),
            2,
            "struct definition + one construction: {constructions:?}"
        );
        assert!(constructions[0].contains("pub struct ConnectedClient"));
        assert!(constructions[1].contains("Ok(ConnectedClient"));
    }
}
