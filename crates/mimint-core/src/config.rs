//! Configuration file (TOML): backends, Privacy Mode, cost cap,
//! fallback chain, retry policy.
//!
//! Location: `$XDG_CONFIG_HOME/mimint/config.toml`, else
//! `$HOME/.config/mimint/config.toml` (Linux/macOS), else
//! `%USERPROFILE%/.config/mimint/config.toml` (Windows fallback).
//! `--config` overrides the path; `MIMINT_CONFIG` is NOT read (the path
//! is explicit or default, never ambient).
//!
//! Example (top-level keys come before any `[section]` header):
//! ```toml
//! default_backend = "local"
//!
//! [privacy]
//! enabled = true          # default when absent: true
//!
//! [cost]
//! cap_usd = 0.0           # 0 locks paid backends; absent locks too (opt-in)
//!
//! [[backend]]
//! name = "local"
//! kind = "ollama"
//! model = "qwen2.5:3b"
//!
//! [[backend]]
//! name = "cloud"
//! kind = "claude"
//! model = "claude-sonnet-4-5"
//! key_env = "ANTHROPIC_API_KEY"
//! timeout_secs = 60
//!
//! [fallback]
//! chain = ["local", "cloud"]
//! max_attempts = 3
//! max_elapsed_secs = 120
//! ```
//!
//! Rules: unknown fields are rejected (typos fail loudly); a backend's
//! model is always required; keys are NEVER written in this file —
//! `key_env` / `key_file` / `key_keychain` name where the key lives,
//! and a literal `api_key` field is rejected as an unknown field.

use crate::error::MimintError;
use crate::fallback::RetryPolicy;
use crate::profile::{ProfileEntry, ProfileSet};
use crate::registry::{BackendKind, BackendSpec, CostCap, DEFAULT_TIMEOUT_SECS};
use serde::Deserialize;
use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default config file location. `None` when no home directory can be
/// found; the CLI then treats configuration as absent.
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("mimint").join("config.toml"));
        }
    }
    if let Some(home) = env::var_os("HOME") {
        if !home.is_empty() {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("mimint")
                    .join("config.toml"),
            );
        }
    }
    if let Some(profile) = env::var_os("USERPROFILE") {
        if !profile.is_empty() {
            return Some(
                PathBuf::from(profile)
                    .join(".config")
                    .join("mimint")
                    .join("config.toml"),
            );
        }
    }
    None
}

/// Privacy section. Absent section means Privacy Mode ON.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacySection {
    /// Privacy Mode switch. Default: true.
    #[serde(default = "default_privacy_enabled")]
    pub enabled: bool,
}

fn default_privacy_enabled() -> bool {
    true
}

impl Default for PrivacySection {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Cost section (P0-11). Absent section means paid backends are locked
/// (opt-in required); only `cap_usd > 0` allows them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CostSection {
    /// Monthly cap in USD. 0 locks paid backends. Absent locks too.
    /// The value is an opt-in gate, not spending metering.
    pub cap_usd: Option<f64>,
}

/// Fallback + retry section. All fields optional; defaults are the
/// [`RetryPolicy`] defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackSection {
    /// Ordered backend names. When absent, `[default_backend]` is used.
    pub chain: Option<Vec<String>>,
    /// Attempts per provider. Default: [`RetryPolicy::DEFAULT_MAX_ATTEMPTS`].
    pub max_attempts: Option<usize>,
    /// Per-call budget in seconds (fresh per `chat()` call). Default: [`RetryPolicy::DEFAULT_MAX_ELAPSED_SECS`].
    pub max_elapsed_secs: Option<u64>,
    /// First backoff delay in milliseconds. Default: [`RetryPolicy::DEFAULT_BASE_DELAY_MS`].
    pub base_delay_ms: Option<u64>,
    /// Backoff cap in milliseconds. Default: [`RetryPolicy::DEFAULT_MAX_DELAY_MS`].
    pub max_delay_ms: Option<u64>,
}

/// Multi-agent section (Phase 6, §12). All fields optional;
/// defaults are single-agent behaviour.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsSection {
    /// Parallel sub-agents. Default: false.
    pub parallelism_enabled: Option<bool>,
    /// Max live agents, 1..=8. Default: 3.
    pub max_agents: Option<usize>,
    /// Max queue WAIT time in seconds, 10..=300. Default: 60.
    /// This is waiting time, not run time (§12, F6D-19).
    pub queue_sla_secs: Option<u64>,
    /// Max queued body actions across all agents. Default: 64.
    pub max_queued_total: Option<usize>,
    /// Max queued body actions per agent. Default: 16.
    pub max_queued_per_agent: Option<usize>,
    /// Per-action run timeout in seconds. Default: 30.
    pub action_timeout_secs: Option<u64>,
}

/// Hierarchy section (Phase 6, §12). Two independent flags:
/// `enabled` requires `agents.parallelism_enabled`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HierarchySection {
    /// Sub-agent delegation (`delegate` tool). Default: false.
    pub enabled: Option<bool>,
    /// Max concurrent children, 1..=4 and <= max_agents - 1.
    /// Default: 2.
    pub max_children: Option<usize>,
    /// Default step limit per child, 1..=50. Default: 10.
    pub default_child_steps: Option<usize>,
    /// Default wall-clock limit per child in seconds, 60..=1800.
    /// Default: 300.
    pub default_child_time_secs: Option<u64>,
}

/// Sandbox section (Phase 6, §12) for `execute_code`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSection {
    /// Hard timeout per code run in seconds, 5..=300. Default: 30.
    pub execute_code_timeout_secs: Option<u64>,
    /// Combined stdout+stderr cap in bytes. Default: 1 MiB.
    /// The 1 MiB ceiling is a safety constant: configuration
    /// may only tighten it, never loosen it (§12).
    pub max_output_bytes: Option<usize>,
    /// RLIMIT_FSIZE for the child in bytes. Default: 64 MiB.
    pub rlimit_fsize_bytes: Option<u64>,
    /// RLIMIT_AS for the child in bytes. Default: 1 GiB.
    pub rlimit_as_bytes: Option<u64>,
}

/// Routing section (Phase 6, §12) for the R6 batch-loop guard.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingSection {
    /// Identical body actions in a row before the turn ends
    /// with `gui_batch_loop`, 2..=50. Default: 5.
    pub gui_batch_threshold: Option<usize>,
}

/// One `[[mcp-server]]` table (Phase 6, §12). Servers come ONLY
/// from this allow-list in the config file — never from a tool,
/// a prompt, or a workdir (§6.3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerEntry {
    /// Server name: `[a-z0-9_]+`, unique. Used in tool names
    /// (`mcp_<server>_<tool>`).
    pub name: String,
    /// Absolute path to the server binary. Relative paths are
    /// rejected: the binary must not resolve through PATH.
    pub command: String,
    /// Arguments passed to the server.
    pub args: Option<Vec<String>>,
    /// Extra environment variables for the child. The child
    /// otherwise inherits only `PATH` and `HOME` (§6.3).
    pub env: Option<std::collections::HashMap<String, String>>,
    /// Per-call timeout in seconds, 5..=300. Default: 30.
    pub timeout_secs: Option<u64>,
    /// Run outside the sandbox. Default: false. REJECTED on
    /// Linux (configuration error); opt-in per server on
    /// macOS/Windows with a per-server confirmation (§6.3,
    /// D-DESIGN-2); MCP does not exist on Android.
    pub unsandboxed: Option<bool>,
}

/// Validated multi-agent configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsConfig {
    pub parallelism_enabled: bool,
    pub max_agents: usize,
    pub queue_sla_secs: u64,
    pub max_queued_total: usize,
    pub max_queued_per_agent: usize,
    pub action_timeout_secs: u64,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            parallelism_enabled: false,
            max_agents: 3,
            queue_sla_secs: 60,
            max_queued_total: 64,
            max_queued_per_agent: 16,
            action_timeout_secs: 30,
        }
    }
}

/// Validated hierarchy configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyConfig {
    pub enabled: bool,
    pub max_children: usize,
    pub default_child_steps: usize,
    pub default_child_time_secs: u64,
}

impl Default for HierarchyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_children: 2,
            default_child_steps: 10,
            default_child_time_secs: 300,
        }
    }
}

/// Validated sandbox configuration for `execute_code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    pub execute_code_timeout_secs: u64,
    pub max_output_bytes: usize,
    pub rlimit_fsize_bytes: u64,
    pub rlimit_as_bytes: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            execute_code_timeout_secs: 30,
            max_output_bytes: 1_048_576,
            rlimit_fsize_bytes: 67_108_864,
            rlimit_as_bytes: 1_073_741_824,
        }
    }
}

/// Validated routing configuration (R6 guard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingConfig {
    pub gui_batch_threshold: usize,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            gui_batch_threshold: 5,
        }
    }
}

/// Validated MCP server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::HashMap<String, String>,
    pub timeout_secs: u64,
    pub unsandboxed: bool,
}

/// Maximum MCP servers (§6.2).
pub const MAX_MCP_SERVERS: usize = 8;

/// True when `name` is a valid MCP server name: non-empty,
/// `[a-z0-9_]` only. Server names become part of tool names,
/// which must stay in that alphabet (§6.2).
pub fn is_valid_mcp_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Pure check for the unsandboxed rule (D-DESIGN-2), written
/// against an explicit OS string so it is testable on Linux:
/// `unsandboxed=true` is a configuration error on Linux, and
/// MCP does not exist on Android at all.
pub fn validate_mcp_unsandboxed(unsandboxed: bool, target_os: &str) -> Result<(), MimintError> {
    if target_os == "android" {
        return Err(MimintError::Config(
            "MCP servers are not available on Android".to_string(),
        ));
    }
    if unsandboxed && target_os == "linux" {
        return Err(MimintError::Config(
            "mcp-server unsandboxed=true is rejected on Linux: MCP servers must run in the sandbox (D-DESIGN-2)".to_string(),
        ));
    }
    Ok(())
}

/// One `[[backend]]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendEntry {
    /// Unique backend name.
    pub name: String,
    /// Kind name; parsed by [`BackendKind::parse`].
    pub kind: String,
    /// Model name. Required (serde) and non-empty (validation).
    pub model: String,
    /// Base URL override; kind preset when absent.
    pub base_url: Option<String>,
    /// Environment variable holding the key.
    pub key_env: Option<String>,
    /// Key file path (0600-checked).
    pub key_file: Option<String>,
    /// macOS Keychain account holding the key (service `ai.mimint`).
    /// Fails closed on other platforms.
    pub key_keychain: Option<String>,
    /// Per-request timeout in seconds. Default: 30.
    pub timeout_secs: Option<u64>,
    /// Claude-only `max_tokens`. Default: 4096 (see [`crate::claude`]).
    pub max_tokens: Option<u32>,
    /// Opt-in to send this backend's key over plaintext http to a
    /// loopback endpoint. Default: false.
    pub allow_plaintext_auth: Option<bool>,
}

/// Whole config file as parsed from TOML.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    /// Privacy section; default ON when absent.
    #[serde(default)]
    pub privacy: PrivacySection,
    /// Cost section; paid locked when absent.
    #[serde(default)]
    pub cost: CostSection,
    /// Backend used when no fallback chain is set.
    pub default_backend: Option<String>,
    /// Backend entries (`[[backend]]` tables).
    #[serde(default)]
    pub backend: Vec<BackendEntry>,
    /// Fallback + retry section.
    #[serde(default)]
    pub fallback: FallbackSection,
    /// Profile entries (`[[profile]]` tables, Phase 5). Absent
    /// means one implicit "Mặc định" profile from the top-level
    /// values.
    #[serde(default)]
    pub profile: Vec<ProfileEntry>,
    /// Name of the default profile. Absent means the first
    /// profile (or the implicit default).
    pub default_profile: Option<String>,
    /// Multi-agent section (Phase 6). Absent means single-agent.
    #[serde(default)]
    pub agents: AgentsSection,
    /// Hierarchy section (Phase 6). Absent means disabled.
    #[serde(default)]
    pub hierarchy: HierarchySection,
    /// Sandbox section (Phase 6). Absent means the defaults.
    #[serde(default)]
    pub sandbox: SandboxSection,
    /// Routing section (Phase 6). Absent means the defaults.
    #[serde(default)]
    pub routing: RoutingSection,
    /// MCP server allow-list (`[[mcp-server]]` tables, Phase 6).
    /// Absent means no MCP servers.
    #[serde(default, rename = "mcp-server")]
    pub mcp_server: Vec<McpServerEntry>,
}

/// A validated, resolved configuration ready for the registry.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// Effective Privacy Mode from the file (CLI flags override later).
    pub privacy_enabled: bool,
    /// Effective cost cap.
    pub cost_cap: CostCap,
    /// Validated backend specs, in file order.
    pub backends: Vec<BackendSpec>,
    /// Ordered fallback chain (backend names). Empty when no backend is
    /// configured at all; the CLI then runs onboarding.
    pub chain: Vec<String>,
    /// Retry policy for the chain.
    pub retry: RetryPolicy,
    /// Validated profile set (Phase 5). Always has at least one
    /// profile; see [`ProfileSet`] for the fail-closed rules and
    /// warnings.
    pub profiles: ProfileSet,
    /// Validated multi-agent configuration (Phase 6).
    pub agents: AgentsConfig,
    /// Validated hierarchy configuration (Phase 6).
    pub hierarchy: HierarchyConfig,
    /// Validated sandbox configuration (Phase 6).
    pub sandbox: SandboxConfig,
    /// Validated routing configuration (Phase 6).
    pub routing: RoutingConfig,
    /// Validated MCP server allow-list (Phase 6).
    pub mcp_servers: Vec<McpServerConfig>,
}

impl FileConfig {
    /// An empty configuration: Privacy ON, paid locked, no backends.
    /// Used when no config file exists.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Parse TOML text. Errors name the problem; the caller adds the path.
    pub fn parse(text: &str) -> Result<Self, MimintError> {
        toml::from_str(text)
            .map_err(|err| MimintError::Config(format!("invalid config TOML: {err}")))
    }

    /// Validate and resolve into a [`ResolvedConfig`]. Every error names
    /// the backend or field at fault.
    pub fn validate(self) -> Result<ResolvedConfig, MimintError> {
        let cost_cap = CostCap::new(self.cost.cap_usd)?;
        let mut backends: Vec<BackendSpec> = Vec::new();
        for entry in &self.backend {
            let kind = BackendKind::parse(&entry.kind).map_err(|err| {
                MimintError::Config(format!("backend '{}': {}", entry.name, err.message()))
            })?;
            let spec = BackendSpec {
                name: entry.name.clone(),
                kind,
                model: entry.model.clone(),
                base_url: entry.base_url.clone(),
                key_env: entry.key_env.clone(),
                key_file: entry.key_file.clone().map(PathBuf::from),
                key_keychain: entry.key_keychain.clone(),
                timeout: Duration::from_secs(entry.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
                max_tokens: entry.max_tokens,
                allow_plaintext_auth: entry.allow_plaintext_auth.unwrap_or(false),
            };
            spec.validate()?;
            if backends.iter().any(|other| other.name == spec.name) {
                return Err(MimintError::Config(format!(
                    "duplicate backend name '{}' in config (backend names must be unique)",
                    spec.name
                )));
            }
            // Resolve the base URL now so a missing required URL fails
            // at config load, not mid-turn.
            spec.resolve_base_url()?;
            backends.push(spec);
        }

        let retry = RetryPolicy {
            max_attempts: self
                .fallback
                .max_attempts
                .unwrap_or(RetryPolicy::DEFAULT_MAX_ATTEMPTS),
            max_elapsed: Duration::from_secs(
                self.fallback
                    .max_elapsed_secs
                    .unwrap_or(RetryPolicy::DEFAULT_MAX_ELAPSED_SECS),
            ),
            base_delay: Duration::from_millis(
                self.fallback
                    .base_delay_ms
                    .unwrap_or(RetryPolicy::DEFAULT_BASE_DELAY_MS),
            ),
            max_delay: Duration::from_millis(
                self.fallback
                    .max_delay_ms
                    .unwrap_or(RetryPolicy::DEFAULT_MAX_DELAY_MS),
            ),
        };
        retry.validate()?;

        let defined: Vec<&str> = backends.iter().map(|spec| spec.name.as_str()).collect();
        let chain = match (&self.fallback.chain, &self.default_backend) {
            (Some(chain), default) => {
                if chain.is_empty() {
                    return Err(MimintError::Config(
                        "fallback.chain must not be empty: name at least one backend".to_string(),
                    ));
                }
                for name in chain {
                    if !defined.contains(&name.as_str()) {
                        return Err(MimintError::Config(format!(
                            "fallback.chain names unknown backend '{name}'; defined backends: {}",
                            defined.join(", ")
                        )));
                    }
                }
                if let Some(default) = default {
                    if chain.first().map(String::as_str) != Some(default.as_str()) {
                        return Err(MimintError::Config(format!(
                            "default_backend '{default}' must be the first entry of fallback.chain \
                             (chain starts with '{}')",
                            chain[0]
                        )));
                    }
                }
                chain.clone()
            }
            (None, Some(default)) => {
                if !defined.contains(&default.as_str()) {
                    return Err(MimintError::Config(format!(
                        "default_backend '{default}' is not a defined backend; defined backends: {}",
                        defined.join(", ")
                    )));
                }
                vec![default.clone()]
            }
            (None, None) => {
                if backends.is_empty() {
                    Vec::new()
                } else {
                    return Err(MimintError::Config(format!(
                        "no backend selected: set default_backend or fallback.chain \
                         (defined backends: {})",
                        defined.join(", ")
                    )));
                }
            }
        };

        let profiles = ProfileSet::resolve(
            &self.profile,
            self.default_profile.as_deref(),
            &backends,
            &chain,
            self.privacy.enabled,
            cost_cap,
        );

        let agents = validate_agents_section(&self.agents)?;
        let hierarchy = validate_hierarchy_section(&self.hierarchy, &agents)?;
        let sandbox = validate_sandbox_section(&self.sandbox)?;
        let routing = validate_routing_section(&self.routing)?;
        let mcp_servers = validate_mcp_servers(&self.mcp_server)?;

        Ok(ResolvedConfig {
            privacy_enabled: self.privacy.enabled,
            cost_cap,
            backends,
            chain,
            retry,
            profiles,
            agents,
            hierarchy,
            sandbox,
            routing,
            mcp_servers,
        })
    }
}

fn range_check(what: &str, value: u64, min: u64, max: u64) -> Result<(), MimintError> {
    if value < min || value > max {
        return Err(MimintError::Config(format!(
            "{what} must be {min}..={max}, got {value}"
        )));
    }
    Ok(())
}

fn validate_agents_section(section: &AgentsSection) -> Result<AgentsConfig, MimintError> {
    let defaults = AgentsConfig::default();
    let max_agents = section.max_agents.unwrap_or(defaults.max_agents);
    range_check("agents.max_agents", max_agents as u64, 1, 8)?;
    let queue_sla_secs = section.queue_sla_secs.unwrap_or(defaults.queue_sla_secs);
    range_check("agents.queue_sla_secs", queue_sla_secs, 10, 300)?;
    let max_queued_total = section
        .max_queued_total
        .unwrap_or(defaults.max_queued_total);
    range_check("agents.max_queued_total", max_queued_total as u64, 1, 4096)?;
    let max_queued_per_agent = section
        .max_queued_per_agent
        .unwrap_or(defaults.max_queued_per_agent);
    range_check(
        "agents.max_queued_per_agent",
        max_queued_per_agent as u64,
        1,
        1024,
    )?;
    if max_queued_per_agent > max_queued_total {
        return Err(MimintError::Config(format!(
            "agents.max_queued_per_agent ({max_queued_per_agent}) must not exceed agents.max_queued_total ({max_queued_total})"
        )));
    }
    let action_timeout_secs = section
        .action_timeout_secs
        .unwrap_or(defaults.action_timeout_secs);
    range_check("agents.action_timeout_secs", action_timeout_secs, 1, 600)?;
    Ok(AgentsConfig {
        parallelism_enabled: section
            .parallelism_enabled
            .unwrap_or(defaults.parallelism_enabled),
        max_agents,
        queue_sla_secs,
        max_queued_total,
        max_queued_per_agent,
        action_timeout_secs,
    })
}

fn validate_hierarchy_section(
    section: &HierarchySection,
    agents: &AgentsConfig,
) -> Result<HierarchyConfig, MimintError> {
    let defaults = HierarchyConfig::default();
    let enabled = section.enabled.unwrap_or(defaults.enabled);
    if enabled && !agents.parallelism_enabled {
        return Err(MimintError::Config(
            "hierarchy.enabled requires agents.parallelism_enabled: hierarchy is two independent flags (§12)".to_string(),
        ));
    }
    let max_children = section.max_children.unwrap_or(defaults.max_children);
    range_check("hierarchy.max_children", max_children as u64, 1, 4)?;
    if enabled && agents.max_agents < 2 {
        return Err(MimintError::Config(
            "hierarchy.enabled requires agents.max_agents >= 2: a parent needs room for at least one child".to_string(),
        ));
    }
    if enabled && max_children > agents.max_agents.saturating_sub(1) {
        return Err(MimintError::Config(format!(
            "hierarchy.max_children ({max_children}) must not exceed agents.max_agents - 1 ({})",
            agents.max_agents - 1
        )));
    }
    let default_child_steps = section
        .default_child_steps
        .unwrap_or(defaults.default_child_steps);
    range_check(
        "hierarchy.default_child_steps",
        default_child_steps as u64,
        1,
        50,
    )?;
    let default_child_time_secs = section
        .default_child_time_secs
        .unwrap_or(defaults.default_child_time_secs);
    range_check(
        "hierarchy.default_child_time_secs",
        default_child_time_secs,
        60,
        1800,
    )?;
    Ok(HierarchyConfig {
        enabled,
        max_children,
        default_child_steps,
        default_child_time_secs,
    })
}

fn validate_sandbox_section(section: &SandboxSection) -> Result<SandboxConfig, MimintError> {
    let defaults = SandboxConfig::default();
    let execute_code_timeout_secs = section
        .execute_code_timeout_secs
        .unwrap_or(defaults.execute_code_timeout_secs);
    range_check(
        "sandbox.execute_code_timeout_secs",
        execute_code_timeout_secs,
        5,
        300,
    )?;
    let max_output_bytes = section
        .max_output_bytes
        .unwrap_or(defaults.max_output_bytes);
    // The 1 MiB ceiling may only be tightened, never loosened (§12).
    range_check(
        "sandbox.max_output_bytes",
        max_output_bytes as u64,
        1,
        defaults.max_output_bytes as u64,
    )?;
    let rlimit_fsize_bytes = section
        .rlimit_fsize_bytes
        .unwrap_or(defaults.rlimit_fsize_bytes);
    if rlimit_fsize_bytes == 0 {
        return Err(MimintError::Config(
            "sandbox.rlimit_fsize_bytes must be greater than 0".to_string(),
        ));
    }
    let rlimit_as_bytes = section.rlimit_as_bytes.unwrap_or(defaults.rlimit_as_bytes);
    if rlimit_as_bytes == 0 {
        return Err(MimintError::Config(
            "sandbox.rlimit_as_bytes must be greater than 0".to_string(),
        ));
    }
    Ok(SandboxConfig {
        execute_code_timeout_secs,
        max_output_bytes,
        rlimit_fsize_bytes,
        rlimit_as_bytes,
    })
}

fn validate_routing_section(section: &RoutingSection) -> Result<RoutingConfig, MimintError> {
    let defaults = RoutingConfig::default();
    let gui_batch_threshold = section
        .gui_batch_threshold
        .unwrap_or(defaults.gui_batch_threshold);
    range_check(
        "routing.gui_batch_threshold",
        gui_batch_threshold as u64,
        2,
        50,
    )?;
    Ok(RoutingConfig {
        gui_batch_threshold,
    })
}

fn validate_mcp_servers(entries: &[McpServerEntry]) -> Result<Vec<McpServerConfig>, MimintError> {
    if entries.len() > MAX_MCP_SERVERS {
        return Err(MimintError::Config(format!(
            "at most {MAX_MCP_SERVERS} mcp-server entries are allowed, got {}",
            entries.len()
        )));
    }
    let target_os = std::env::consts::OS;
    let mut servers = Vec::new();
    for entry in entries {
        if !is_valid_mcp_server_name(&entry.name) {
            return Err(MimintError::Config(format!(
                "mcp-server name '{}' must be non-empty and [a-z0-9_] only",
                entry.name
            )));
        }
        if servers
            .iter()
            .any(|s: &McpServerConfig| s.name == entry.name)
        {
            return Err(MimintError::Config(format!(
                "duplicate mcp-server name '{}' in config (server names must be unique)",
                entry.name
            )));
        }
        if !Path::new(&entry.command).is_absolute() {
            return Err(MimintError::Config(format!(
                "mcp-server '{}': command '{}' must be an absolute path",
                entry.name, entry.command
            )));
        }
        let timeout_secs = entry.timeout_secs.unwrap_or(30);
        range_check(
            &format!("mcp-server '{}' timeout_secs", entry.name),
            timeout_secs,
            5,
            300,
        )?;
        let unsandboxed = entry.unsandboxed.unwrap_or(false);
        validate_mcp_unsandboxed(unsandboxed, target_os).map_err(|err| {
            MimintError::Config(format!("mcp-server '{}': {}", entry.name, err.message()))
        })?;
        servers.push(McpServerConfig {
            name: entry.name.clone(),
            command: entry.command.clone(),
            args: entry.args.clone().unwrap_or_default(),
            env: entry.env.clone().unwrap_or_default(),
            timeout_secs,
            unsandboxed,
        });
    }
    Ok(servers)
}

/// Load a config file. When `required` is false, a missing file means an
/// empty configuration; when true, a missing file is an error. Parse and
/// validation errors always fail, and name the file.
pub fn load_config_file(path: &Path, required: bool) -> Result<FileConfig, MimintError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(FileConfig::empty());
        }
        Err(err) => {
            return Err(MimintError::Config(format!(
                "cannot read config file {}: {err}",
                path.display()
            )));
        }
    };
    FileConfig::parse(&text)
        .map_err(|err| MimintError::Config(format!("{}: {}", path.display(), err.message())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
default_backend = "local"

[privacy]
enabled = true

[cost]
cap_usd = 0.0

[[backend]]
name = "local"
kind = "ollama"
model = "qwen2.5:3b"

[[backend]]
name = "cloud"
kind = "claude"
model = "claude-sonnet-4-5"
key_env = "ANTHROPIC_API_KEY"
timeout_secs = 60

[fallback]
chain = ["local", "cloud"]
max_attempts = 2
"#;

    #[test]
    fn valid_config_resolves() {
        let resolved = FileConfig::parse(VALID).unwrap().validate().unwrap();
        assert!(resolved.privacy_enabled);
        assert_eq!(resolved.cost_cap.cap_usd, Some(0.0));
        assert_eq!(resolved.backends.len(), 2);
        assert_eq!(
            resolved.chain,
            vec!["local".to_string(), "cloud".to_string()]
        );
        assert_eq!(resolved.retry.max_attempts, 2);
        assert_eq!(resolved.backends[1].timeout, Duration::from_secs(60));
    }

    #[test]
    fn keychain_source_parses_and_resolves() {
        let text = r#"
default_backend = "cloud"

[[backend]]
name = "cloud"
kind = "claude"
model = "claude-sonnet-4-5"
key_keychain = "anthropic"
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert_eq!(
            resolved.backends[0].key_keychain.as_deref(),
            Some("anthropic")
        );
        assert!(resolved.backends[0].key_env.is_none());
        assert!(resolved.backends[0].key_file.is_none());
    }

    #[test]
    fn privacy_defaults_on_and_cap_defaults_absent() {
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert!(resolved.privacy_enabled);
        assert_eq!(resolved.cost_cap.cap_usd, None);
        assert_eq!(resolved.chain, vec!["local".to_string()]);
    }

    #[test]
    fn unknown_fields_rejected_including_literal_api_key() {
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
api_key = "never-allowed-here"
"#;
        let err = FileConfig::parse(text).expect_err("api_key must fail");
        assert!(err.to_string().contains("api_key"), "{err}");
        let text = "[privacy]\nenabled = true\ntypo_field = 1\n";
        assert!(FileConfig::parse(text).is_err());
    }

    #[test]
    fn missing_model_missing_base_url_and_duplicates_fail() {
        let no_model = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
"#;
        assert!(FileConfig::parse(no_model).is_err());
        let no_base = r#"
default_backend = "c"
[[backend]]
name = "c"
kind = "openai"
model = "m"
"#;
        let err = FileConfig::parse(no_base)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("base_url"), "{err}");
        let dup = r#"
default_backend = "a"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
"#;
        let err = FileConfig::parse(dup).unwrap().validate().err().unwrap();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn chain_and_default_selection_errors_name_defined_backends() {
        let unknown_chain = r#"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
[fallback]
chain = ["ghost"]
"#;
        let err = FileConfig::parse(unknown_chain)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("ghost"), "{err}");
        assert!(err.to_string().contains('a'), "{err}");

        let mismatch = r#"
default_backend = "b"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
[[backend]]
name = "b"
kind = "ollama"
model = "m"
[fallback]
chain = ["a", "b"]
"#;
        let err = FileConfig::parse(mismatch)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("first entry"), "{err}");

        let none_selected = r#"
[[backend]]
name = "a"
kind = "ollama"
model = "m"
"#;
        let err = FileConfig::parse(none_selected)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("no backend selected"), "{err}");
    }

    #[test]
    fn invalid_cap_and_retry_values_fail() {
        let bad_cap = "default_backend = \"a\"\n[cost]\ncap_usd = -5.0\n[[backend]]\nname = \"a\"\nkind = \"ollama\"\nmodel = \"m\"\n";
        assert!(FileConfig::parse(bad_cap).unwrap().validate().is_err());
        let bad_retry = "default_backend = \"a\"\n[[backend]]\nname = \"a\"\nkind = \"ollama\"\nmodel = \"m\"\n[fallback]\nmax_attempts = 0\n";
        assert!(FileConfig::parse(bad_retry).unwrap().validate().is_err());
    }

    #[test]
    fn allow_plaintext_auth_parses_and_defaults_false() {
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
key_env = "MIMINT_TEST_KEY"
allow_plaintext_auth = true
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert!(resolved.backends[0].allow_plaintext_auth);
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert!(!resolved.backends[0].allow_plaintext_auth);
    }

    #[test]
    fn empty_config_has_no_backends_and_empty_chain() {
        let resolved = FileConfig::empty().validate().unwrap();
        assert!(resolved.backends.is_empty());
        assert!(resolved.chain.is_empty());
        assert!(resolved.privacy_enabled);
    }

    #[test]
    fn phase6_sections_default_to_single_agent() {
        let resolved = FileConfig::empty().validate().unwrap();
        assert!(!resolved.agents.parallelism_enabled);
        assert_eq!(resolved.agents.max_agents, 3);
        assert_eq!(resolved.agents.queue_sla_secs, 60);
        assert!(!resolved.hierarchy.enabled);
        assert_eq!(resolved.sandbox.execute_code_timeout_secs, 30);
        assert_eq!(resolved.sandbox.max_output_bytes, 1_048_576);
        assert_eq!(resolved.routing.gui_batch_threshold, 5);
        assert!(resolved.mcp_servers.is_empty());
    }

    #[test]
    fn phase6_sections_parse_and_validate() {
        let text = r#"
default_backend = "local"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
[agents]
parallelism_enabled = true
max_agents = 4
queue_sla_secs = 90
[hierarchy]
enabled = true
max_children = 2
[sandbox]
execute_code_timeout_secs = 45
max_output_bytes = 524288
[routing]
gui_batch_threshold = 8
[[mcp-server]]
name = "notes"
command = "/usr/local/bin/notes-mcp"
args = ["--stdio"]
timeout_secs = 20
"#;
        let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
        assert!(resolved.agents.parallelism_enabled);
        assert_eq!(resolved.agents.max_agents, 4);
        assert_eq!(resolved.agents.queue_sla_secs, 90);
        assert!(resolved.hierarchy.enabled);
        assert_eq!(resolved.sandbox.execute_code_timeout_secs, 45);
        assert_eq!(resolved.sandbox.max_output_bytes, 524_288);
        assert_eq!(resolved.routing.gui_batch_threshold, 8);
        assert_eq!(resolved.mcp_servers.len(), 1);
        assert_eq!(resolved.mcp_servers[0].name, "notes");
        assert!(!resolved.mcp_servers[0].unsandboxed);
    }

    #[test]
    fn hierarchy_without_parallelism_fails() {
        let text = "default_backend = \"a\"\n[[backend]]\nname = \"a\"\nkind = \"ollama\"\nmodel = \"m\"\n[hierarchy]\nenabled = true\n";
        let err = FileConfig::parse(text).unwrap().validate().err().unwrap();
        assert!(err.to_string().contains("parallelism_enabled"), "{err}");
    }

    #[test]
    fn out_of_range_phase6_values_fail() {
        let base = "default_backend = \"a\"\n[[backend]]\nname = \"a\"\nkind = \"ollama\"\nmodel = \"m\"\n";
        for section in [
            "[agents]\nmax_agents = 9\n",
            "[agents]\nqueue_sla_secs = 5\n",
            "[agents]\nmax_queued_per_agent = 100\nmax_queued_total = 10\n",
            "[routing]\ngui_batch_threshold = 1\n",
            "[routing]\ngui_batch_threshold = 51\n",
            "[sandbox]\nexecute_code_timeout_secs = 301\n",
            // The 1 MiB output ceiling cannot be loosened.
            "[sandbox]\nmax_output_bytes = 2097152\n",
            "[sandbox]\nrlimit_fsize_bytes = 0\n",
        ] {
            let text = format!("{base}{section}");
            assert!(
                FileConfig::parse(&text).unwrap().validate().is_err(),
                "section must fail: {section}"
            );
        }
    }

    #[test]
    fn mcp_server_names_commands_and_duplicates_fail() {
        let base = "default_backend = \"a\"\n[[backend]]\nname = \"a\"\nkind = \"ollama\"\nmodel = \"m\"\n";
        let bad_name = format!("{base}[[mcp-server]]\nname = \"Notes!\"\ncommand = \"/bin/x\"\n");
        assert!(FileConfig::parse(&bad_name).unwrap().validate().is_err());
        let relative = format!("{base}[[mcp-server]]\nname = \"notes\"\ncommand = \"notes-mcp\"\n");
        let err = FileConfig::parse(&relative)
            .unwrap()
            .validate()
            .err()
            .unwrap();
        assert!(err.to_string().contains("absolute"), "{err}");
        let dup = format!(
            "{base}[[mcp-server]]\nname = \"notes\"\ncommand = \"/bin/x\"\n[[mcp-server]]\nname = \"notes\"\ncommand = \"/bin/y\"\n"
        );
        let err = FileConfig::parse(&dup).unwrap().validate().err().unwrap();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn unsandboxed_rule_is_pure_and_os_dependent() {
        assert!(validate_mcp_unsandboxed(false, "linux").is_ok());
        assert!(validate_mcp_unsandboxed(true, "linux").is_err());
        assert!(validate_mcp_unsandboxed(true, "macos").is_ok());
        assert!(validate_mcp_unsandboxed(true, "windows").is_ok());
        assert!(validate_mcp_unsandboxed(false, "android").is_err());
        assert!(is_valid_mcp_server_name("notes_v2"));
        assert!(!is_valid_mcp_server_name("Notes"));
        assert!(!is_valid_mcp_server_name("notes-x"));
        assert!(!is_valid_mcp_server_name(""));
    }

    #[test]
    fn unknown_fields_rejected_in_phase6_sections() {
        let text = "[agents]\nparallelism_enabled = true\ntypo = 1\n";
        assert!(FileConfig::parse(text).is_err());
        let text = "[[mcp-server]]\nname = \"x\"\ncommand = \"/bin/x\"\ntypo = 1\n";
        assert!(FileConfig::parse(text).is_err());
    }
}
