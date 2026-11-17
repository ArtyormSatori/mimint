//! `mimint-cli`: one-turn chat CLI/headless front end for Mimint core (Phase 1).
//!
//! Arguments are parsed by hand (no CLI parser dependency, by design).
//! Reads one prompt from argv, runs one turn, prints the final text plus
//! a short summary. No REPL.

use mimint_core::{
    default_registry, Approver, AuditLog, Brain, MimintError, Notebook, ProviderClient,
};
use serde_json::Value;
use std::env;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const USAGE: &str = "mimint-cli: run one Mimint turn (Phase 1).\n\nUsage:\n  mimint-cli [options] <prompt...>\n\nOptions:\n  --base-url URL     Provider base URL, or env MIMINT_BASE_URL (required)\n  --model MODEL      Model name, or env MIMINT_MODEL (required, no default)\n  --db PATH          Notebook SQLite path (default: mimint.db)\n  --audit PATH       Audit JSONL path (default: mimint-audit.jsonl)\n  --auto-approve     Approve consequential tools automatically (test mode only; recorded in audit)\n  --max-steps N      Max brain steps (default: 5)\n  --timeout SECONDS  Provider timeout (default: 30)\n  --help             Show this help\n\nThe API key comes only from env MIMINT_API_KEY (not needed for loopback endpoints).\n";

struct CliArgs {
    base_url: Option<String>,
    model: Option<String>,
    db: PathBuf,
    audit: PathBuf,
    auto_approve: bool,
    max_steps: usize,
    timeout_secs: u64,
    prompt: String,
}

fn parse_args(argv: Vec<String>) -> Result<CliArgs, MimintError> {
    let mut base_url = None;
    let mut model = None;
    let mut db = PathBuf::from("mimint.db");
    let mut audit = PathBuf::from("mimint-audit.jsonl");
    let mut auto_approve = false;
    let mut max_steps = 5usize;
    let mut timeout_secs = 30u64;
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
            "--base-url" | "--model" | "--db" | "--audit" | "--max-steps" | "--timeout"
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
            "--db" => db = PathBuf::from(value),
            "--audit" => audit = PathBuf::from(value),
            "--max-steps" => {
                max_steps = value
                    .parse()
                    .map_err(|_| MimintError::Config(format!("invalid --max-steps: {value}")))?;
            }
            "--timeout" => {
                timeout_secs = value
                    .parse()
                    .map_err(|_| MimintError::Config(format!("invalid --timeout: {value}")))?;
                if timeout_secs == 0 {
                    return Err(MimintError::Config(
                        "--timeout must be at least 1 second".to_string(),
                    ));
                }
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
                "missing model: pass --model or set MIMINT_MODEL (no default model)".to_string(),
            )
        })?;
    let api_key = nonempty(env::var("MIMINT_API_KEY").ok());
    let notebook = Arc::new(Notebook::open(&args.db)?);
    let audit = AuditLog::open(&args.audit)?;
    let registry = default_registry(Arc::clone(&notebook));
    let provider = ProviderClient::new(
        &base_url,
        &model,
        api_key,
        Duration::from_secs(args.timeout_secs),
    )?;
    let turn_id = format!("turn-{}-{}", std::process::id(), now_ms());
    let stdin_approver = StdinApprover::new();
    let auto_approver = AutoApprover;
    let approver: &dyn Approver = if args.auto_approve {
        &auto_approver
    } else {
        &stdin_approver
    };
    let brain = Brain::new(
        &provider,
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
        assert_eq!(args.timeout_secs, 30);
        assert!(args.base_url.is_none());
        assert!(args.model.is_none());
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
        assert_eq!(args.timeout_secs, 7);
        assert_eq!(args.prompt, "do it");
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
    }
}
