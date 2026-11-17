//! `mimint-cli`: one-turn chat CLI/headless front end for Mimint core.
//!
//! Arguments are parsed by hand (no CLI parser dependency, by design).
//! Reads one prompt from argv, runs one turn, prints the final text plus
//! a short summary. No REPL.
//!
//! Backend selection, in order:
//! 1. `--base-url` / `--model` flags (missing piece filled from env
//!    `MIMINT_BASE_URL` / `MIMINT_MODEL`): one ad-hoc OpenAI-compatible
//!    backend.
//! 2. `--backend NAME`: one backend from the config file.
//! 3. Config file backends: `fallback.chain`, else `default_backend`.
//! 4. Env `MIMINT_BASE_URL` + `MIMINT_MODEL` (both): ad-hoc backend.
//! 5. Nothing configured: probe loopback for Ollama/LM Studio, print a
//!    configuration suggestion, and stop. Nothing is changed.
//!
//! Privacy Mode defaults to ON (config file, then `--privacy-mode`
//! flag, override it). The cost cap from the config file applies in
//! every mode: flags select a backend, they never bypass policy.

use mimint_core::{
    build_chain, default_config_path, default_vault, load_config_file, probe_local_backends,
    suggestion_text, Approver, AuditLog, BackendKind, BackendSpec, Brain, CostCap, EgressPolicy,
    FallbackChain, MimintError, Notebook, ResolvedConfig, DEFAULT_TIMEOUT_SECS,
};
use serde_json::{json, Value};
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USAGE: &str = "mimint-cli: run one Mimint turn (Phase 2).\n\nUsage:\n  mimint-cli [options] <prompt...>\n\nOptions:\n  --base-url URL     Ad-hoc OpenAI-compatible base URL, or env MIMINT_BASE_URL\n  --model MODEL      Ad-hoc model name, or env MIMINT_MODEL (required, no default)\n  --backend NAME     Use one backend from the config file (not with --base-url/--model)\n  --config PATH      Config file (default: $XDG_CONFIG_HOME/mimint/config.toml or ~/.config/mimint/config.toml)\n  --key-file PATH    Key file for the ad-hoc backend (0600-checked; config backends use key_env/key_file)\n  --privacy-mode on|off  Privacy Mode: ON allows loopback backends only (default: on)\n  --db PATH          Notebook SQLite path (default: mimint.db)\n  --audit PATH       Audit JSONL path (default: mimint-audit.jsonl)\n  --auto-approve     Approve consequential tools automatically (test mode only; recorded in audit)\n  --max-steps N      Max brain steps (default: 5)\n  --timeout SECONDS  Per-request timeout, overriding backend timeouts (default: 30 ad-hoc, else per backend)\n  --help             Show this help\n\nThe API key comes from the vault: env MIMINT_API_KEY by default, or the backend's key_env/key_file.\nKeys are never printed or written to the audit log.\n";

struct CliArgs {
    base_url: Option<String>,
    model: Option<String>,
    backend: Option<String>,
    config: Option<PathBuf>,
    key_file: Option<PathBuf>,
    privacy_mode: Option<bool>,
    db: PathBuf,
    audit: PathBuf,
    auto_approve: bool,
    max_steps: usize,
    timeout_secs: Option<u64>,
    prompt: String,
}

fn parse_args(argv: Vec<String>) -> Result<CliArgs, MimintError> {
    let mut base_url = None;
    let mut model = None;
    let mut backend = None;
    let mut config = None;
    let mut key_file = None;
    let mut privacy_mode = None;
    let mut db = PathBuf::from("mimint.db");
    let mut audit = PathBuf::from("mimint-audit.jsonl");
    let mut auto_approve = false;
    let mut max_steps = 5usize;
    let mut timeout_secs = None;
    let mut positional: Vec<String> = Vec::new();
    let mut positional_only = false;
    let mut index = 0usize;
    while index < argv.len() {
        let arg = &argv[index];
        if positional_only {
            positional.push(arg.clone());
            index += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if arg == "--auto-approve" {
            auto_approve = true;
            index += 1;
            continue;
        }
        let (flag, inline_value) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let takes_value = matches!(
            flag,
            "--base-url"
                | "--model"
                | "--backend"
                | "--config"
                | "--key-file"
                | "--privacy-mode"
                | "--db"
                | "--audit"
                | "--max-steps"
                | "--timeout"
        );
        if !takes_value {
            if flag.starts_with('-') && flag.len() > 1 {
                return Err(MimintError::Config(format!("unknown argument: {arg}")));
            }
            positional.push(arg.clone());
            index += 1;
            continue;
        }
        let value = match inline_value {
            Some(value) => value,
            None => {
                index += 1;
                if index >= argv.len() {
                    return Err(MimintError::Config(format!("missing value for {flag}")));
                }
                argv[index].clone()
            }
        };
        match flag {
            "--base-url" => base_url = Some(value),
            "--model" => model = Some(value),
            "--backend" => backend = Some(value),
            "--config" => config = Some(PathBuf::from(value)),
            "--key-file" => key_file = Some(PathBuf::from(value)),
            "--privacy-mode" => {
                privacy_mode = Some(match value.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => {
                        return Err(MimintError::Config(format!(
                            "invalid --privacy-mode: {value} (expected on or off)"
                        )));
                    }
                });
            }
            "--db" => db = PathBuf::from(value),
            "--audit" => audit = PathBuf::from(value),
            "--max-steps" => {
                max_steps = value
                    .parse()
                    .map_err(|_| MimintError::Config(format!("invalid --max-steps: {value}")))?;
            }
            "--timeout" => {
                let parsed: u64 = value
                    .parse()
                    .map_err(|_| MimintError::Config(format!("invalid --timeout: {value}")))?;
                if parsed == 0 {
                    return Err(MimintError::Config(
                        "--timeout must be at least 1 second".to_string(),
                    ));
                }
                timeout_secs = Some(parsed);
            }
            _ => {
                return Err(MimintError::Config(format!("unknown argument: {flag}")));
            }
        }
        index += 1;
    }
    let prompt = positional.join(" ");
    if prompt.trim().is_empty() {
        return Err(MimintError::Config(
            "missing prompt: pass the prompt as positional arguments".to_string(),
        ));
    }
    Ok(CliArgs {
        base_url,
        model,
        backend,
        config,
        key_file,
        privacy_mode,
        db,
        audit,
        auto_approve,
        max_steps,
        timeout_secs,
        prompt,
    })
}

/// Asks y/n on stdin. EOF or anything but y/yes denies.
struct StdinApprover {
    input: Mutex<io::BufReader<io::Stdin>>,
}

impl StdinApprover {
    fn new() -> Self {
        Self {
            input: Mutex::new(io::BufReader::new(io::stdin())),
        }
    }
}

impl Approver for StdinApprover {
    fn approve(&self, tool: &str, args: &Value) -> Result<bool, MimintError> {
        eprintln!("Approval required: run tool '{tool}' with args {args}? [y/N]");
        eprint!("> ");
        let _ = io::stderr().flush();
        let mut input = match self.input.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) => Ok(false),
            Ok(_) => Ok(matches!(
                line.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            )),
            Err(err) => Err(MimintError::Tool(format!(
                "cannot read approval answer: {err}"
            ))),
        }
    }

    fn label(&self) -> &'static str {
        "stdin approval"
    }
}

/// Approves everything. Test/stub use only; the audit log says so.
struct AutoApprover;

impl Approver for AutoApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
        Ok(true)
    }

    fn label(&self) -> &'static str {
        "auto-approved (test mode)"
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.trim().is_empty())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One ad-hoc OpenAI-compatible backend from flags/env.
fn adhoc_spec(
    base_url: String,
    model: String,
    key_file: Option<PathBuf>,
    timeout_secs: Option<u64>,
) -> BackendSpec {
    BackendSpec {
        name: "adhoc".to_string(),
        kind: BackendKind::OpenAi,
        model,
        base_url: Some(base_url),
        key_env: None,
        key_file,
        timeout: Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
        max_tokens: None,
    }
}

async fn run() -> Result<(), MimintError> {
    let argv: Vec<String> = env::args().skip(1).collect();
    if argv
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        print!("{USAGE}");
        return Ok(());
    }
    let args = parse_args(argv)?;
    if args.backend.is_some() && (args.base_url.is_some() || args.model.is_some()) {
        return Err(MimintError::Config(
            "cannot combine --backend with --base-url/--model: pick one backend source".to_string(),
        ));
    }

    // Config file: explicit --config is required; the default path is
    // optional (absent file means an empty configuration).
    let config_path = args.config.clone().or_else(default_config_path);
    let config_required = args.config.is_some();
    let config_existed = config_path.as_ref().is_some_and(|path| path.exists());
    let resolved: ResolvedConfig = match &config_path {
        Some(path) => load_config_file(path, config_required)?.validate()?,
        None => mimint_core::FileConfig::empty().validate()?,
    };
    let config_display = config_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(no config file: home directory not found)".to_string());

    // Privacy Mode: flag > config file > default ON. The effective mode
    // is recorded in the audit log on every run.
    let (privacy_enabled, privacy_source) = match args.privacy_mode {
        Some(enabled) => (enabled, "flag"),
        None if config_existed => (resolved.privacy_enabled, "config file"),
        None => (resolved.privacy_enabled, "default"),
    };
    let policy = EgressPolicy::new(privacy_enabled);
    if privacy_enabled {
        eprintln!(
            "Privacy Mode: ON (source: {privacy_source}); only loopback backends are reachable."
        );
    } else {
        eprintln!("Privacy Mode: OFF (source: {privacy_source}); remote endpoints are reachable.");
    }

    let notebook = Arc::new(Notebook::open(&args.db)?);
    let audit = Arc::new(AuditLog::open(&args.audit)?);
    let turn_id = format!("turn-{}-{}", std::process::id(), now_ms());
    audit.log(
        &turn_id,
        "privacy_mode",
        None,
        Some(if privacy_enabled { "on" } else { "off" }),
        json!({
            "enabled": privacy_enabled,
            "source": privacy_source,
            "config_path": config_display,
        }),
    )?;

    // Backend selection (see the module docs for the order).
    let mut specs: Vec<BackendSpec>;
    let order: Vec<String>;
    if args.base_url.is_some() || args.model.is_some() {
        let base_url = nonempty(args.base_url)
            .or_else(|| nonempty(env::var("MIMINT_BASE_URL").ok()))
            .ok_or_else(|| {
                MimintError::Config(
                    "missing base URL: pass --base-url or set MIMINT_BASE_URL".to_string(),
                )
            })?;
        let model = nonempty(args.model)
            .or_else(|| nonempty(env::var("MIMINT_MODEL").ok()))
            .ok_or_else(|| {
                MimintError::Config(
                    "missing model: pass --model or set MIMINT_MODEL (no default model)"
                        .to_string(),
                )
            })?;
        specs = vec![adhoc_spec(
            base_url,
            model,
            args.key_file,
            args.timeout_secs,
        )];
        order = vec!["adhoc".to_string()];
    } else if let Some(name) = &args.backend {
        if resolved.backends.is_empty() {
            return Err(MimintError::Config(format!(
                "no backends are defined in {config_display}; --backend '{name}' cannot be resolved"
            )));
        }
        if !resolved.backends.iter().any(|spec| &spec.name == name) {
            let defined: Vec<&str> = resolved
                .backends
                .iter()
                .map(|spec| spec.name.as_str())
                .collect();
            return Err(MimintError::Config(format!(
                "unknown backend '{name}'; defined backends: {}",
                defined.join(", ")
            )));
        }
        specs = resolved.backends.clone();
        order = vec![name.clone()];
    } else if !resolved.backends.is_empty() {
        specs = resolved.backends.clone();
        order = resolved.chain.clone();
    } else if let (Some(base_url), Some(model)) = (
        nonempty(env::var("MIMINT_BASE_URL").ok()),
        nonempty(env::var("MIMINT_MODEL").ok()),
    ) {
        specs = vec![adhoc_spec(
            base_url,
            model,
            args.key_file,
            args.timeout_secs,
        )];
        order = vec!["adhoc".to_string()];
    } else {
        // Onboarding: suggest, never change anything.
        let probe = probe_local_backends(policy).await;
        eprintln!("{}", suggestion_text(&probe, &config_display));
        return Err(MimintError::Config(
            "no backend configured: add a backend to the config file or pass --base-url and --model".to_string(),
        ));
    }

    // A --timeout flag overrides every backend's timeout.
    if let Some(timeout_secs) = args.timeout_secs {
        for spec in &mut specs {
            spec.timeout = Duration::from_secs(timeout_secs);
        }
    }

    // The cost cap applies in every mode (see module docs).
    let cost_cap: CostCap = resolved.cost_cap;
    let vault = default_vault();
    let providers = build_chain(&specs, &order, &vault, policy, cost_cap)?;
    let chain_names: Vec<String> = providers
        .iter()
        .map(|provider| provider.name().to_string())
        .collect();

    let turn_id_for_chain = turn_id.clone();
    let registry = mimint_core::default_registry(Arc::clone(&notebook));
    let chain = FallbackChain::new(
        providers,
        resolved.retry,
        Some(Arc::clone(&audit)),
        turn_id_for_chain,
    )?;
    let stdin_approver = StdinApprover::new();
    let auto_approver = AutoApprover;
    let approver: &dyn Approver = if args.auto_approve {
        &auto_approver
    } else {
        &stdin_approver
    };
    let brain = Brain::new(
        &chain,
        &registry,
        Some(approver),
        &audit,
        Arc::clone(&notebook),
        turn_id,
        args.max_steps,
    )?;
    let outcome = brain.run(&args.prompt).await?;
    println!("{}", outcome.text);
    println!("---");
    println!("providers: {}", chain_names.join(" -> "));
    println!(
        "privacy mode: {}",
        if privacy_enabled { "on" } else { "off" }
    );
    println!("steps: {}", outcome.steps);
    println!(
        "tools used: {}",
        if outcome.tools_used.is_empty() {
            "(none)".to_string()
        } else {
            outcome.tools_used.join(", ")
        }
    );
    println!("partial: {}", outcome.partial);
    println!("audit: {}", args.audit.display());
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn defaults_and_positional_prompt() {
        let args = parse_args(argv(&["hello", "world"])).unwrap();
        assert_eq!(args.prompt, "hello world");
        assert_eq!(args.db, PathBuf::from("mimint.db"));
        assert_eq!(args.audit, PathBuf::from("mimint-audit.jsonl"));
        assert!(!args.auto_approve);
        assert_eq!(args.max_steps, 5);
        assert_eq!(args.timeout_secs, None);
        assert!(args.base_url.is_none());
        assert!(args.model.is_none());
        assert!(args.backend.is_none());
        assert!(args.config.is_none());
        assert!(args.key_file.is_none());
        assert_eq!(args.privacy_mode, None);
    }

    #[test]
    fn flags_space_and_equals_forms() {
        let args = parse_args(argv(&[
            "--base-url",
            "http://127.0.0.1:9",
            "--model=m",
            "--db",
            "/tmp/x.db",
            "--audit=/tmp/a.jsonl",
            "--auto-approve",
            "--max-steps",
            "3",
            "--timeout=7",
            "--privacy-mode",
            "off",
            "--key-file",
            "/tmp/k.key",
            "do",
            "it",
        ]))
        .unwrap();
        assert_eq!(args.base_url.as_deref(), Some("http://127.0.0.1:9"));
        assert_eq!(args.model.as_deref(), Some("m"));
        assert_eq!(args.db, PathBuf::from("/tmp/x.db"));
        assert_eq!(args.audit, PathBuf::from("/tmp/a.jsonl"));
        assert!(args.auto_approve);
        assert_eq!(args.max_steps, 3);
        assert_eq!(args.timeout_secs, Some(7));
        assert_eq!(args.privacy_mode, Some(false));
        assert_eq!(args.key_file, Some(PathBuf::from("/tmp/k.key")));
        assert_eq!(args.prompt, "do it");
    }

    #[test]
    fn backend_and_config_flags() {
        let args = parse_args(argv(&[
            "--backend",
            "local",
            "--config=/tmp/c.toml",
            "--privacy-mode=on",
            "hi",
        ]))
        .unwrap();
        assert_eq!(args.backend.as_deref(), Some("local"));
        assert_eq!(args.config, Some(PathBuf::from("/tmp/c.toml")));
        assert_eq!(args.privacy_mode, Some(true));
    }

    #[test]
    fn separator_keeps_dash_prompt_positional() {
        let args = parse_args(argv(&["--", "--not-a-flag", "x"])).unwrap();
        assert_eq!(args.prompt, "--not-a-flag x");
    }

    #[test]
    fn missing_prompt_unknown_flag_and_bad_numbers_error() {
        assert!(parse_args(argv(&["--base-url", "http://127.0.0.1:9"])).is_err());
        assert!(parse_args(argv(&["--bogus", "hi"])).is_err());
        assert!(parse_args(argv(&["--max-steps", "abc", "hi"])).is_err());
        assert!(parse_args(argv(&["--timeout", "0", "hi"])).is_err());
        assert!(parse_args(argv(&["--model"])).is_err());
        assert!(parse_args(argv(&["--privacy-mode", "maybe", "hi"])).is_err());
        assert!(parse_args(argv(&["--privacy-mode"])).is_err());
    }
}
