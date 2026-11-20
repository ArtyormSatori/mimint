//! `mimint-eval`: evaluation harness for Phase 5 (eval, evolve,
//! drift, finetune, skill lifecycle).
//!
//! Arguments are parsed by hand (no CLI parser dependency, by
//! design, like `mimint-cli`). Every scored run uses binary
//! checkers evaluated by code — no model judges — and prints a
//! JSON report plus a human summary. Exit codes: 0 means the
//! command ran; `eval`, `skill validate`, and `drift` exit 1
//! when their checks did not all pass (the report still prints).
//!
//! Provider selection (for the subcommands that need a model):
//! 1. `--base-url` / `--model` flags (missing piece filled from
//!    env `MIMINT_BASE_URL` / `MIMINT_MODEL`): one ad-hoc
//!    OpenAI-compatible backend.
//! 2. `--backend NAME`: one backend from the config file.
//! 3. Config file: the active profile's chain (a `[[profile]]`
//!    chain, else `fallback.chain`, else `default_backend`).
//! 4. Env `MIMINT_BASE_URL` + `MIMINT_MODEL` (both set).
//! 5. Nothing configured: error. (No onboarding probe: eval is a
//!    power tool and fails loudly instead of suggesting.)
//!
//! Eval turns auto-approve ONLY `notebook_add` (the same
//! [`ValidationApprover`] the skill validator uses) and deny
//! every other consequential tool. Eval turns never retrieve
//! from the experience store: measurement must be clean.
//! `eval` RECORDS its checked turns into the experience store
//! by default (the gate in [`ExperienceStore::record_turn`]
//! decides success vs failure); `--no-record` disables that.

use mimint_core::{
    build_chain, build_system_prompt, default_config_path, default_data_dir, default_vault,
    drift_findings, evaluate_finetune, evaluate_golden, evolve_skill, load_config_file,
    load_eval_tasks, probe_finetune, validate_skill, AuditLog, BackendKind, BackendSpec, Brain,
    CostCap, EgressPolicy, EvolveConfig, ExperienceStore, FallbackChain, MimintError, Notebook,
    Provider, ResolvedConfig, SkillDef, SkillStatus, SkillStore, SoulStore, ValidationApprover,
    DEFAULT_TIMEOUT_SECS, GOLDEN_PROMPTS, GOLDEN_PROMPTS_V2,
};
use serde_json::{json, Value};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USAGE: &str = "mimint-eval: Phase 5 evaluation harness.\n\nUsage:\n  mimint-eval <command> [options]\n\nCommands:\n  eval --tasks FILE [--prompt-file FILE] [--notebook PATH] [--report FILE] [--no-record]\n      Run tasks (TOML [[task]] tables with binary checkers) through the\n      model and report pass/fail per task. Tasks share one fresh scratch\n      notebook: use distinct note needles per task. Records checked turns\n      into the experience store unless --no-record is given.\n  evolve --skill NAME --tasks FILE [--run-dir DIR] [--report FILE] [--apply-winner]\n      S4: evaluate skill variants on a held-out task set, report scores,\n      and name the winner (strictly beats base) and eliminated variants.\n      Each (variant, task) pair runs on a fresh notebook. Read-only by\n      default; --apply-winner saves the winner's content as a new draft\n      version (validation is still required before promotion).\n  drift [--soul-file NAME] [--report FILE] [--golden-set v1|v2] [--repeat N]\n      Run the golden identity prompts against the SOUL persona and report\n      drift assertions. Never modifies SOUL (loads it read-only). v2 is\n      the new set with Vietnamese language assertions on every prompt;\n      --repeat N runs each prompt N times and reports per-prompt drift rates.\n      Each prompt reports one of three states: clean / drift / timeout\n      (a turn that does not complete is TIMEOUT, never drift or clean).\n      Each entry records the real model name, the timeout used, and the\n      full must_contain / must_not_contain assertions for that prompt.\n  finetune\n      Print the fine-tune hardware gate status and exit.\n  skill create --name NAME --description TEXT --author TEXT (--content TEXT | --content-file FILE) --tasks-file FILE [--triggers a,b]\n      Create a skill as a draft. Never promotes: run `skill validate` next.\n  skill validate NAME [--notebook PATH]\n      Validate a skill against its own tasks (real model turns). All pass\n      -> promoted; any fail -> quarantined. A negative probe runs first:\n      a checker that passes on empty evidence quarantines the skill with\n      zero model calls. An imported skill that passes stays a draft\n      until its import is approved (see approve-import).\n  skill promote NAME\n      Promote only when the latest validation run passed every task for\n      the skill's current content. No override around the gate.\n  skill approve-import NAME\n      Approve an imported skill (audited). The user step an imported\n      skill needs before promotion; validation is still required.\n  skill demote NAME\n      Demote a skill back to draft.\n  skill list\n      List all skills with status, version, and last validation run.\n  skill import PATH\n      Import a skill TOML file. Always lands as a draft with no history.\n\nShared options (model commands):\n  --config PATH      Config file (default: $XDG_CONFIG_HOME/mimint/config.toml or ~/.config/mimint/config.toml)\n  --data-dir PATH    User data directory (default: $XDG_DATA_HOME/mimint or ~/.local/share/mimint)\n  --audit PATH       Audit JSONL path (default: <data-dir>/eval-audit.jsonl)\n  --backend NAME     One backend from the config file\n  --profile NAME     Named profile (unknown names fall back to the default with a warning)\n  --base-url URL     Ad-hoc OpenAI-compatible base URL, or env MIMINT_BASE_URL\n  --model MODEL      Ad-hoc model name, or env MIMINT_MODEL\n  --privacy-mode on|off  Privacy Mode (default: profile, else on)\n  --max-steps N      Max brain steps per turn (default: 5)\n  --timeout SECONDS  Per-request timeout, overriding backend timeouts\n  --time-budget-secs SECONDS\n      Total time budget for eval/evolve/drift/validate: when it expires\n      before a task starts, the run stops with a partial report (evolve:\n      BudgetExhausted verdict; validate: error, no run recorded).\n      Ctrl-C still kills the process; completed tasks stay in the audit log.\n  --help             Show this help\n";

/// All flags any subcommand accepts, parsed once.
#[derive(Default)]
struct Opts {
    config: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    audit: Option<PathBuf>,
    backend: Option<String>,
    profile: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    privacy_mode: Option<bool>,
    max_steps: usize,
    timeout_secs: Option<u64>,
    time_budget_secs: Option<u64>,
    tasks: Option<PathBuf>,
    prompt_file: Option<PathBuf>,
    report: Option<PathBuf>,
    notebook: Option<PathBuf>,
    run_dir: Option<PathBuf>,
    no_record: bool,
    apply_winner: bool,
    soul_file: Option<String>,
    skill: Option<String>,
    name: Option<String>,
    description: Option<String>,
    author: Option<String>,
    content: Option<String>,
    content_file: Option<PathBuf>,
    tasks_file: Option<PathBuf>,
    triggers: Option<String>,
    golden_set: Option<String>,
    repeat: Option<usize>,
    positional: Vec<String>,
}

const VALUE_FLAGS: [&str; 27] = [
    "--config",
    "--data-dir",
    "--audit",
    "--backend",
    "--profile",
    "--base-url",
    "--model",
    "--privacy-mode",
    "--max-steps",
    "--timeout",
    "--time-budget-secs",
    "--tasks",
    "--prompt-file",
    "--report",
    "--notebook",
    "--run-dir",
    "--soul-file",
    "--skill",
    "--name",
    "--description",
    "--author",
    "--content",
    "--content-file",
    "--tasks-file",
    "--triggers",
    "--golden-set",
    "--repeat",
];

fn wants_help(argv: &[String]) -> bool {
    let mut index = 0usize;
    while index < argv.len() {
        let arg = &argv[index];
        if arg == "--" {
            return false;
        }
        if arg == "--help" || arg == "-h" {
            return true;
        }
        let flag = arg.split_once('=').map(|(flag, _)| flag).unwrap_or(arg);
        if VALUE_FLAGS.contains(&flag) && !arg.contains('=') {
            index += 2;
            continue;
        }
        index += 1;
    }
    false
}

fn parse_opts(argv: &[String]) -> Result<Opts, MimintError> {
    let mut opts = Opts {
        max_steps: 5,
        ..Default::default()
    };
    let mut index = 0usize;
    let mut positional_only = false;
    while index < argv.len() {
        let arg = &argv[index];
        if positional_only {
            opts.positional.push(arg.clone());
            index += 1;
            continue;
        }
        if arg == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if arg == "--no-record" {
            opts.no_record = true;
            index += 1;
            continue;
        }
        if arg == "--apply-winner" {
            opts.apply_winner = true;
            index += 1;
            continue;
        }
        let (flag, inline_value) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if !VALUE_FLAGS.contains(&flag) {
            if flag.starts_with('-') && flag.len() > 1 {
                return Err(MimintError::Config(format!("unknown argument: {arg}")));
            }
            opts.positional.push(arg.clone());
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
            "--config" => opts.config = Some(PathBuf::from(value)),
            "--data-dir" => opts.data_dir = Some(PathBuf::from(value)),
            "--audit" => opts.audit = Some(PathBuf::from(value)),
            "--backend" => opts.backend = Some(value),
            "--profile" => opts.profile = Some(value),
            "--base-url" => opts.base_url = Some(value),
            "--model" => opts.model = Some(value),
            "--privacy-mode" => {
                opts.privacy_mode = Some(match value.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => {
                        return Err(MimintError::Config(format!(
                            "invalid --privacy-mode: {value} (expected on or off)"
                        )));
                    }
                });
            }
            "--max-steps" => {
                opts.max_steps = value
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
                opts.timeout_secs = Some(parsed);
            }
            "--time-budget-secs" => {
                let parsed: u64 = value.parse().map_err(|_| {
                    MimintError::Config(format!("invalid --time-budget-secs: {value}"))
                })?;
                if parsed == 0 {
                    return Err(MimintError::Config(
                        "--time-budget-secs must be at least 1 second".to_string(),
                    ));
                }
                opts.time_budget_secs = Some(parsed);
            }
            "--tasks" => opts.tasks = Some(PathBuf::from(value)),
            "--prompt-file" => opts.prompt_file = Some(PathBuf::from(value)),
            "--report" => opts.report = Some(PathBuf::from(value)),
            "--notebook" => opts.notebook = Some(PathBuf::from(value)),
            "--run-dir" => opts.run_dir = Some(PathBuf::from(value)),
            "--soul-file" => opts.soul_file = Some(value),
            "--skill" => opts.skill = Some(value),
            "--name" => opts.name = Some(value),
            "--description" => opts.description = Some(value),
            "--author" => opts.author = Some(value),
            "--content" => opts.content = Some(value),
            "--content-file" => opts.content_file = Some(PathBuf::from(value)),
            "--tasks-file" => opts.tasks_file = Some(PathBuf::from(value)),
            "--triggers" => opts.triggers = Some(value),
            "--golden-set" => {
                if value != "v1" && value != "v2" {
                    return Err(MimintError::Config(format!(
                        "invalid --golden-set: {value} (expected v1 or v2)"
                    )));
                }
                opts.golden_set = Some(value);
            }
            "--repeat" => {
                let parsed: usize = value
                    .parse()
                    .map_err(|_| MimintError::Config(format!("invalid --repeat: {value}")))?;
                if parsed == 0 {
                    return Err(MimintError::Config(
                        "--repeat must be at least 1".to_string(),
                    ));
                }
                opts.repeat = Some(parsed);
            }
            _ => return Err(MimintError::Config(format!("unknown argument: {flag}"))),
        }
        index += 1;
    }
    Ok(opts)
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

fn time_budget(opts: &Opts) -> Option<Duration> {
    opts.time_budget_secs.map(Duration::from_secs)
}

fn data_dir(opts: &Opts) -> PathBuf {
    opts.data_dir
        .clone()
        .or_else(default_data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn audit_path(opts: &Opts) -> PathBuf {
    opts.audit
        .clone()
        .unwrap_or_else(|| data_dir(opts).join("eval-audit.jsonl"))
}

fn fresh_scratch_db(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "mimint-eval-{}-{}-{tag}.db",
        std::process::id(),
        now_ms()
    ))
}

fn read_file(path: &PathBuf, what: &str) -> Result<String, MimintError> {
    std::fs::read_to_string(path)
        .map_err(|err| MimintError::Config(format!("cannot read {what} {}: {err}", path.display())))
}

fn write_report(report: &Value, path: Option<&PathBuf>) -> Result<(), MimintError> {
    let text = serde_json::to_string_pretty(report)
        .map_err(|err| MimintError::Io(format!("cannot serialize report: {err}")))?;
    match path {
        Some(path) => {
            std::fs::write(path, format!("{text}\n")).map_err(|err| {
                MimintError::Io(format!("cannot write report {}: {err}", path.display()))
            })?;
            eprintln!("Report written to {}", path.display());
        }
        None => println!("{text}"),
    }
    Ok(())
}

/// A built provider chain plus its identity, for reports.
struct ProviderSetup {
    chain: Arc<FallbackChain>,
    names: Vec<String>,
    profile_name: String,
    privacy_enabled: bool,
}

fn adhoc_spec(base_url: String, model: String, timeout_secs: Option<u64>) -> BackendSpec {
    BackendSpec {
        name: "adhoc".to_string(),
        kind: BackendKind::OpenAi,
        model,
        base_url: Some(base_url),
        key_env: None,
        key_file: None,
        key_keychain: None,
        timeout: Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS)),
        max_tokens: None,
        allow_plaintext_auth: false,
    }
}

fn build_provider(opts: &Opts) -> Result<ProviderSetup, MimintError> {
    let config_path = opts.config.clone().or_else(default_config_path);
    let config_required = opts.config.is_some();
    let resolved: ResolvedConfig = match &config_path {
        Some(path) => load_config_file(path, config_required)?.validate()?,
        None => mimint_core::FileConfig::empty().validate()?,
    };
    for warning in &resolved.profiles.warnings {
        eprintln!("Warning: {warning}");
    }
    let (profile, profile_warning) = match &opts.profile {
        Some(name) => {
            let (profile, warning) = resolved.profiles.resolve_or_default(name);
            (profile.clone(), warning)
        }
        None => (resolved.profiles.default_profile().clone(), None),
    };
    if let Some(warning) = profile_warning {
        eprintln!("Warning: {warning}");
    }
    let privacy_enabled = opts.privacy_mode.unwrap_or(profile.privacy);
    let policy = EgressPolicy::new(privacy_enabled);

    let mut specs: Vec<BackendSpec>;
    let order: Vec<String>;
    if opts.base_url.is_some() || opts.model.is_some() {
        let base_url = nonempty(opts.base_url.clone())
            .or_else(|| nonempty(env::var("MIMINT_BASE_URL").ok()))
            .ok_or_else(|| {
                MimintError::Config(
                    "missing base URL: pass --base-url or set MIMINT_BASE_URL".to_string(),
                )
            })?;
        let model = nonempty(opts.model.clone())
            .or_else(|| nonempty(env::var("MIMINT_MODEL").ok()))
            .ok_or_else(|| {
                MimintError::Config(
                    "missing model: pass --model or set MIMINT_MODEL (no default model)"
                        .to_string(),
                )
            })?;
        specs = vec![adhoc_spec(base_url, model, opts.timeout_secs)];
        order = vec!["adhoc".to_string()];
    } else if let Some(name) = &opts.backend {
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
        order = profile.chain.clone();
    } else if let (Some(base_url), Some(model)) = (
        nonempty(env::var("MIMINT_BASE_URL").ok()),
        nonempty(env::var("MIMINT_MODEL").ok()),
    ) {
        specs = vec![adhoc_spec(base_url, model, opts.timeout_secs)];
        order = vec!["adhoc".to_string()];
    } else {
        return Err(MimintError::Config(
            "no backend configured: add a backend to the config file or pass --base-url and --model".to_string(),
        ));
    }
    if let Some(timeout_secs) = opts.timeout_secs {
        for spec in &mut specs {
            spec.timeout = Duration::from_secs(timeout_secs);
        }
    }
    let cost_cap: CostCap = profile.cost_cap;
    let vault = default_vault();
    let providers = build_chain(&specs, &order, &vault, policy, cost_cap)?;
    let names: Vec<String> = providers
        .iter()
        .map(|provider| provider.name().to_string())
        .collect();
    eprintln!("Profile: {}", profile.name);
    eprintln!("Backend: {}", names.join(" -> "));
    let chain = FallbackChain::new(providers, resolved.retry)?;
    Ok(ProviderSetup {
        chain: Arc::new(chain),
        names,
        profile_name: profile.name.clone(),
        privacy_enabled,
    })
}

const DEFAULT_SYSTEM_PROMPT: &str = "You are Mimint, a local-first personal assistant. Use the provided tools when they help answer. Keep answers short.";

/// `eval`: run tasks, report scores, record checked turns.
async fn cmd_eval(opts: &Opts) -> Result<bool, MimintError> {
    let tasks_path = opts
        .tasks
        .clone()
        .ok_or_else(|| MimintError::Config("eval requires --tasks FILE".to_string()))?;
    let tasks = load_eval_tasks(&tasks_path)?;
    let system_prompt = match &opts.prompt_file {
        Some(path) => read_file(path, "prompt file")?,
        None => DEFAULT_SYSTEM_PROMPT.to_string(),
    };
    let setup = build_provider(opts)?;
    let notebook_path = opts
        .notebook
        .clone()
        .unwrap_or_else(|| fresh_scratch_db("eval"));
    eprintln!("Scratch notebook: {}", notebook_path.display());
    let notebook = Arc::new(Notebook::open(&notebook_path)?);
    let audit = Arc::new(AuditLog::open(audit_path(opts))?);
    let experience = if opts.no_record {
        None
    } else {
        Some(ExperienceStore::open(data_dir(opts).join("experience.db"))?)
    };
    let turn_prefix = format!("eval-{}-{}", std::process::id(), now_ms());
    let started = Instant::now();
    let deadline = time_budget(opts).map(|budget| started + budget);
    let mut results = Vec::new();
    let mut passed = 0usize;
    let mut budget_exhausted = false;
    for task in &tasks {
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                budget_exhausted = true;
                eprintln!(
                    "Time budget exhausted after {}/{} task(s); stopping with a partial report.",
                    results.len(),
                    tasks.len()
                );
                break;
            }
        }
        let turn_id = format!("{turn_prefix}-{}", task.id);
        let run = mimint_core::run_one_task_detailed(
            Arc::clone(&setup.chain) as Arc<dyn Provider>,
            &system_prompt,
            task,
            Arc::clone(&notebook),
            Arc::clone(&audit),
            &turn_id,
            opts.max_steps,
        )
        .await?;
        if run.result.passed {
            passed += 1;
        }
        let recorded = match (&experience, &run.outcome, &run.error) {
            (Some(store), Some(outcome), _) => {
                let decision =
                    store.record_turn(&task.prompt, outcome, Some(&task.checker), &notebook)?;
                format!("{decision:?}")
            }
            (Some(store), None, Some(error)) => {
                store.record_error(&task.prompt, error)?;
                "RecordedFailure { reason: turn error }".to_string()
            }
            _ => "not recorded".to_string(),
        };
        // Every learning write is audited (P5.3): the task id and
        // the recording decision, never prompt text.
        if experience.is_some() {
            audit.log(
                &turn_id,
                "experience_recorded",
                None,
                Some(if run.result.passed {
                    "passed"
                } else {
                    "failed"
                }),
                json!({
                    "task": task.id,
                    "recorded": recorded,
                }),
            )?;
        }
        eprintln!(
            "  [{}] {}: {}",
            if run.result.passed { "PASS" } else { "FAIL" },
            task.id,
            run.result.evidence
        );
        results.push(json!({
            "id": task.id,
            "passed": run.result.passed,
            "evidence": run.result.evidence,
            "recorded": recorded,
        }));
    }
    let report = json!({
        "command": "eval",
        "tasks_file": tasks_path.display().to_string(),
        "providers": setup.names,
        "profile": setup.profile_name,
        "privacy_enabled": setup.privacy_enabled,
        "passed": passed,
        "total": tasks.len(),
        "completed": results.len(),
        "budget_exhausted": budget_exhausted,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "results": results,
    });
    eprintln!("Eval: {passed}/{} passed", tasks.len());
    write_report(&report, opts.report.as_ref())?;
    Ok(passed == tasks.len() && !budget_exhausted)
}

/// `evolve`: S4 variant competition on a held-out set.
async fn cmd_evolve(opts: &Opts) -> Result<(), MimintError> {
    let skill_name = opts
        .skill
        .clone()
        .ok_or_else(|| MimintError::Config("evolve requires --skill NAME".to_string()))?;
    let tasks_path = opts
        .tasks
        .clone()
        .ok_or_else(|| MimintError::Config("evolve requires --tasks FILE".to_string()))?;
    let tasks = load_eval_tasks(&tasks_path)?;
    let store = SkillStore::new(data_dir(opts).join("skills"));
    let skill = store.get(&skill_name)?.ok_or_else(|| {
        MimintError::Config(format!(
            "unknown skill {skill_name:?}: no such skill in the store"
        ))
    })?;
    let setup = build_provider(opts)?;
    let run_dir = opts.run_dir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("mimint-evolve-{}-{}", std::process::id(), now_ms()))
    });
    std::fs::create_dir_all(&run_dir).map_err(|err| {
        MimintError::Io(format!(
            "cannot create evolve run dir {}: {err}",
            run_dir.display()
        ))
    })?;
    eprintln!("Evolve run dir: {}", run_dir.display());
    let audit = Arc::new(AuditLog::open(audit_path(opts))?);
    let variants = mimint_core::build_variants(&skill);
    let factory = |key: &str| Ok(Arc::new(Notebook::open(run_dir.join(format!("{key}.db")))?));
    let config = EvolveConfig {
        turn_prefix: format!("evolve-{}-{}", std::process::id(), now_ms()),
        max_steps: opts.max_steps,
        time_budget: time_budget(opts),
    };
    let started = Instant::now();
    let report = evolve_skill(
        Arc::clone(&setup.chain) as Arc<dyn Provider>,
        &skill,
        &variants,
        &tasks,
        &factory,
        Arc::clone(&audit),
        &config,
    )
    .await?;
    for score in &report.scores {
        eprintln!("  {}: {}/{}", score.variant_id, score.passed, score.total);
    }
    eprintln!("Decision: {}", report.decision);
    let json_report = json!({
        "command": "evolve",
        "skill": report.skill,
        "tasks_file": tasks_path.display().to_string(),
        "task_ids": report.task_ids,
        "providers": setup.names,
        "profile": setup.profile_name,
        "scores": report.scores.iter().map(|score| json!({
            "variant": score.variant_id,
            "passed": score.passed,
            "total": score.total,
            "total_steps": score.total_steps,
            "total_elapsed_ms": score.total_elapsed_ms,
            "results": score.results.iter().map(|r| json!({
                "id": r.id,
                "passed": r.passed,
                "evidence": r.evidence,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "winner": report.winner,
        "eliminated": report.eliminated,
        "verdict": report.verdict,
        "pairs_completed": report.pairs_completed,
        "pairs_planned": report.pairs_planned,
        "decision": report.decision,
        "elapsed_ms": started.elapsed().as_millis() as u64,
    });
    write_report(&json_report, opts.report.as_ref())?;
    if opts.apply_winner {
        match &report.winner {
            Some(winner_id) => {
                let variant = variants
                    .iter()
                    .find(|variant| &variant.id == winner_id)
                    .expect("winner is one of the evaluated variants");
                let adopted = mimint_core::winner_def(&skill, variant);
                let version = adopted.version;
                store.save(&adopted)?;
                eprintln!(
                    "Winner {winner_id} saved as draft v{version}. Run `mimint-eval skill validate {skill_name}` to validate before promotion."
                );
            }
            None => eprintln!("No winner: base stays, nothing written."),
        }
    }
    Ok(())
}

/// `drift`: golden identity prompts against the SOUL persona.
async fn cmd_drift(opts: &Opts) -> Result<bool, MimintError> {
    let soul_file = opts
        .soul_file
        .clone()
        .unwrap_or_else(|| "SOUL.md".to_string());
    let store = SoulStore::new(data_dir(opts));
    let soul = store.load(&soul_file).map_err(|err| {
        MimintError::Config(format!(
            "{} (SOUL is loaded read-only for drift eval; run mimint-cli once to create the default SOUL, or create {} by hand)",
            err.message(),
            store.dir().join(&soul_file).display()
        ))
    })?;
    let setup = build_provider(opts)?;
    let notebook = Arc::new(Notebook::open(fresh_scratch_db("drift"))?);
    let audit = Arc::new(AuditLog::open(audit_path(opts))?);
    let system_prompt = build_system_prompt(&soul, &[]);
    let turn_prefix = format!("drift-{}-{}", std::process::id(), now_ms());
    let started = Instant::now();
    let deadline = time_budget(opts).map(|budget| started + budget);
    // Golden set selection (F5-07): v1 is the default (old
    // behavior, old results untouched); v2 adds Vietnamese
    // language assertions and is meant to run repeated.
    let golden_set = opts.golden_set.as_deref().unwrap_or("v1");
    let prompts = if golden_set == "v2" {
        GOLDEN_PROMPTS_V2
    } else {
        GOLDEN_PROMPTS
    };
    let repeat = opts.repeat.unwrap_or(1);
    // Evidence per entry (F8.2-09): the real model name (not
    // just the backend label "adhoc") and the per-request
    // timeout actually used.
    let model_name = opts
        .model
        .clone()
        .or_else(|| env::var("MIMINT_MODEL").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let timeout_secs = opts.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let mut entries = Vec::new();
    let mut total_issues = 0usize;
    let mut total_drift = 0usize;
    let mut total_timeout = 0usize;
    let mut budget_exhausted = false;
    'repeats: for rep in 1..=repeat {
        for golden in prompts {
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    budget_exhausted = true;
                    eprintln!(
                    "Time budget exhausted after {}/{} golden run(s); stopping with a partial report.",
                    entries.len(),
                    prompts.len() * repeat
                );
                    break 'repeats;
                }
            }
            let turn_id = format!("{turn_prefix}-{}-r{rep}", golden.id);
            let registry = Arc::new(mimint_core::default_registry(Arc::clone(&notebook)));
            let approver = Arc::new(ValidationApprover);
            let mut brain = Brain::new(
                Arc::clone(&setup.chain) as Arc<dyn Provider>,
                registry,
                Some(approver),
                Arc::clone(&audit),
                Arc::clone(&notebook),
                turn_id.clone(),
                "agent-001",
                opts.max_steps,
            )?;
            brain.set_system_prompt(system_prompt.clone());
            brain.set_profile(setup.profile_name.clone());
            let outcome = brain.run(golden.prompt).await;
            // Three states (F8.2-08): CLEAN / DRIFT / TIMEOUT.
            // A turn that does not complete is TIMEOUT — never
            // drift and never clean. (In practice drift turns
            // fail on the per-request timeout; any other turn
            // error is also recorded as TIMEOUT with the full
            // error text in "error", so a failure is never
            // misread as a persona drift.)
            let (status, reply, issues, findings, error) = match &outcome {
                Ok(outcome) => {
                    let issues = evaluate_golden(golden, &outcome.text);
                    let status = if issues.is_empty() { "clean" } else { "drift" };
                    (
                        status,
                        outcome.text.clone(),
                        issues,
                        drift_findings(&outcome.text),
                        None,
                    )
                }
                Err(err) => (
                    "timeout",
                    String::new(),
                    Vec::new(),
                    Vec::new(),
                    Some(format!("turn failed: {err}")),
                ),
            };
            total_issues += issues.len();
            match status {
                "drift" => total_drift += 1,
                "timeout" => total_timeout += 1,
                _ => {}
            }
            audit.log(
            &turn_id,
            "drift_eval",
            None,
            Some(status),
            json!({
                "golden_id": golden.id,
                "issues": issues,
                "findings": findings.iter().map(|f| format!("{:?}", f.kind)).collect::<Vec<_>>(),
            }),
        )?;
            eprintln!(
                "  [{}] {}: {} issue(s)",
                status.to_uppercase(),
                golden.id,
                issues.len()
            );
            for issue in &issues {
                eprintln!("      - {issue}");
            }
            if let Some(error) = &error {
                eprintln!("      - {error}");
            }
            entries.push(json!({
                "id": golden.id,
                "repeat": rep,
                "prompt": golden.prompt,
                "status": status,
                "model": model_name,
                "timeout_secs": timeout_secs,
                "assertions": {
                    "must_contain": golden.must_contain,
                    "must_not_contain": golden.must_not_contain,
                },
                "issues": issues,
                "drift_findings": findings.iter().map(|f| json!({
                    "kind": format!("{:?}", f.kind),
                    "matched": f.matched,
                })).collect::<Vec<_>>(),
                "error": error,
                "reply_excerpt": reply.chars().take(500).collect::<String>(),
            }));
        }
    }
    // Per-prompt pass/drift/timeout rates across repeats (F5-07):
    // a single run is an anecdote; the rate is the measurement.
    // TIMEOUT is its own bucket (F8.2-08): it is neither drift
    // nor clean, so `drift = runs - clean - timeout`.
    let per_prompt: Vec<Value> = prompts
        .iter()
        .map(|golden| {
            let runs: Vec<&Value> = entries
                .iter()
                .filter(|entry| entry["id"] == golden.id)
                .collect();
            let clean = runs
                .iter()
                .filter(|entry| entry["status"] == "clean")
                .count();
            let timeout = runs
                .iter()
                .filter(|entry| entry["status"] == "timeout")
                .count();
            let drift = runs.len() - clean - timeout;
            json!({
                "id": golden.id,
                "runs": runs.len(),
                "clean": clean,
                "drift": drift,
                "timeout": timeout,
                "drift_rate": if runs.is_empty() { 0.0 } else { drift as f64 / runs.len() as f64 },
            })
        })
        .collect();
    let total_clean = entries
        .iter()
        .filter(|entry| entry["status"] == "clean")
        .count();
    let report = json!({
        "command": "drift",
        "golden_set": golden_set,
        "repeat": repeat,
        "soul_file": soul_file,
        "providers": setup.names,
        "model": model_name,
        "timeout_secs": timeout_secs,
        "profile": setup.profile_name,
        "prompts": entries,
        "per_prompt": per_prompt,
        "total_issues": total_issues,
        "total_clean": total_clean,
        "total_drift": total_drift,
        "total_timeout": total_timeout,
        "verdict": if budget_exhausted {
            "budget exhausted"
        } else if total_drift > 0 {
            "drift detected"
        } else if total_timeout > 0 {
            "incomplete (timeouts)"
        } else {
            "clean"
        },
        "budget_exhausted": budget_exhausted,
        "elapsed_ms": started.elapsed().as_millis() as u64,
    });
    eprintln!(
        "Drift: {} issue(s) across {} golden run(s) (set {golden_set}, {repeat} repeat(s)): {} clean, {} drift, {} timeout",
        total_issues,
        prompts.len() * repeat,
        total_clean,
        total_drift,
        total_timeout
    );
    write_report(&report, opts.report.as_ref())?;
    Ok(total_drift == 0 && total_timeout == 0 && !budget_exhausted)
}

/// `finetune`: hardware gate status.
fn cmd_finetune() -> Result<(), MimintError> {
    let status = evaluate_finetune(&probe_finetune());
    println!("{}", status.summary());
    Ok(())
}

fn skill_store(opts: &Opts) -> SkillStore {
    SkillStore::new(data_dir(opts).join("skills"))
}

fn cmd_skill_create(opts: &Opts) -> Result<(), MimintError> {
    let name = opts
        .name
        .clone()
        .ok_or_else(|| MimintError::Config("skill create requires --name NAME".to_string()))?;
    let description = opts.description.clone().ok_or_else(|| {
        MimintError::Config("skill create requires --description TEXT".to_string())
    })?;
    let author = opts
        .author
        .clone()
        .ok_or_else(|| MimintError::Config("skill create requires --author TEXT".to_string()))?;
    let content = match (&opts.content, &opts.content_file) {
        (Some(_), Some(_)) => {
            return Err(MimintError::Config(
                "pass exactly one of --content and --content-file".to_string(),
            ));
        }
        (Some(text), None) => text.clone(),
        (None, Some(path)) => read_file(path, "content file")?,
        (None, None) => {
            return Err(MimintError::Config(
                "skill create requires --content TEXT or --content-file FILE".to_string(),
            ));
        }
    };
    let tasks_path = opts.tasks_file.clone().ok_or_else(|| {
        MimintError::Config("skill create requires --tasks-file FILE".to_string())
    })?;
    let tasks = load_eval_tasks(&tasks_path)?;
    let triggers: Vec<String> = opts
        .triggers
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|trigger| !trigger.is_empty())
        .map(str::to_string)
        .collect();
    let def = SkillDef {
        name: name.clone(),
        description,
        triggers,
        content,
        version: 1,
        author,
        validation_tasks: tasks,
        status: SkillStatus::Draft,
        validation_history: Vec::new(),
        imported: false,
        import_approved: false,
    };
    let task_count = def.validation_tasks.len();
    skill_store(opts).create_draft(def)?;
    println!("Created draft skill '{name}' (v1, {task_count} validation task(s)).");
    println!("It is a draft: run `mimint-eval skill validate {name}` to validate it.");
    Ok(())
}

async fn cmd_skill_validate(opts: &Opts) -> Result<bool, MimintError> {
    let name =
        opts.positional.first().cloned().ok_or_else(|| {
            MimintError::Config("skill validate requires a skill NAME".to_string())
        })?;
    let setup = build_provider(opts)?;
    let notebook_path = opts
        .notebook
        .clone()
        .unwrap_or_else(|| fresh_scratch_db("validate"));
    eprintln!("Scratch notebook: {}", notebook_path.display());
    let notebook = Arc::new(Notebook::open(&notebook_path)?);
    let audit = Arc::new(AuditLog::open(audit_path(opts))?);
    let store = skill_store(opts);
    let turn_prefix = format!("validate-{}-{}", std::process::id(), now_ms());
    let def = validate_skill(
        &store,
        Arc::clone(&setup.chain) as Arc<dyn Provider>,
        &name,
        notebook,
        Arc::clone(&audit),
        &turn_prefix,
        opts.max_steps,
        time_budget(opts),
    )
    .await?;
    let run = def
        .validation_history
        .last()
        .expect("validation appends a run");
    for result in &run.results {
        eprintln!(
            "  [{}] {}: {}",
            if result.passed { "PASS" } else { "FAIL" },
            result.id,
            result.evidence
        );
    }
    let promoted = def.status == SkillStatus::Promoted;
    let awaiting_approval = def.imported && !def.import_approved && run.passed == run.total;
    if def.status == SkillStatus::Quarantined {
        // F5-18: evidence produced with a quarantined skill must
        // stop influencing turns. The experience store may not
        // exist yet; opening it only to quarantine is harmless
        // (an empty store quarantines zero rows).
        let experience = ExperienceStore::open(data_dir(opts).join("experience.db"))?;
        let quarantined = experience.quarantine_trajectories_for_skill(&def.name)?;
        if quarantined > 0 {
            eprintln!(
                "Quarantined {quarantined} experience trajector(ies) that used skill '{name}'; they will no longer be retrieved."
            );
        }
    }
    eprintln!(
        "Skill '{name}': {}/{} passed -> {}",
        run.passed,
        run.total,
        if promoted {
            "PROMOTED"
        } else if awaiting_approval {
            "DRAFT — awaiting import approval (run `skill approve-import {name}` after reviewing it)"
        } else {
            "QUARANTINED"
        }
    );
    Ok(promoted)
}

fn cmd_skill_promote(opts: &Opts) -> Result<(), MimintError> {
    let name =
        opts.positional.first().cloned().ok_or_else(|| {
            MimintError::Config("skill promote requires a skill NAME".to_string())
        })?;
    let audit = AuditLog::open(audit_path(opts))?;
    let def = mimint_core::promote(&skill_store(opts), &name, &audit)?;
    println!(
        "Skill '{}' promoted (v{}, content hash {}).",
        def.name,
        def.version,
        def.content_hash()
    );
    Ok(())
}

fn cmd_skill_approve_import(opts: &Opts) -> Result<(), MimintError> {
    let name = opts.positional.first().cloned().ok_or_else(|| {
        MimintError::Config("skill approve-import requires a skill NAME".to_string())
    })?;
    let audit = AuditLog::open(audit_path(opts))?;
    let def = mimint_core::approve_import(&skill_store(opts), &name, &audit)?;
    println!(
        "Skill '{}' import approved (content hash {}). It still needs a passing validation run for its current content before promotion.",
        def.name,
        def.content_hash()
    );
    Ok(())
}

fn cmd_skill_demote(opts: &Opts) -> Result<(), MimintError> {
    let name = opts
        .positional
        .first()
        .cloned()
        .ok_or_else(|| MimintError::Config("skill demote requires a skill NAME".to_string()))?;
    let audit = AuditLog::open(audit_path(opts))?;
    mimint_core::demote(&skill_store(opts), &name, &audit)?;
    println!("Skill '{name}' demoted to draft.");
    Ok(())
}

fn cmd_skill_list(opts: &Opts) -> Result<(), MimintError> {
    let defs = skill_store(opts).load_all()?;
    if defs.is_empty() {
        println!("No skills in the store.");
        return Ok(());
    }
    for def in &defs {
        let last = def.validation_history.last().map(|run| {
            format!(
                "last run {}/{} (hash {})",
                run.passed, run.total, run.content_hash
            )
        });
        println!(
            "{:<20} {:<12} v{}  {} task(s){}",
            def.name,
            format!("{:?}", def.status).to_lowercase(),
            def.version,
            def.validation_tasks.len(),
            last.map(|text| format!("  {text}")).unwrap_or_default()
        );
    }
    let usable = defs.iter().filter(|def| def.is_usable()).count();
    println!("{usable}/{} usable (catalog: promoted + passing run for current content + import approved).", defs.len());
    Ok(())
}

fn cmd_skill_import(opts: &Opts) -> Result<(), MimintError> {
    let path = opts
        .positional
        .first()
        .cloned()
        .ok_or_else(|| MimintError::Config("skill import requires a file PATH".to_string()))?;
    let store = skill_store(opts);
    let def = store.import_file(&PathBuf::from(path))?;
    println!(
        "Imported '{}' as a draft (v{}). It is marked imported: after validation passes, run `mimint-eval skill approve-import {}` to approve it, then `skill promote`.",
        def.name, def.version, def.name
    );
    // Trigger conflicts (F5-14): warn, never override. Retrieval
    // order (score desc, name asc) decides; both skills stay
    // eligible.
    let all = store.load_all()?;
    for (trigger, names) in mimint_core::find_trigger_conflicts(&all) {
        if names.iter().any(|name| name == &def.name) {
            eprintln!(
                "warning: trigger {trigger:?} is shared by skills: {}; retrieval order is score, then name — no skill overrides another.",
                names.join(", ")
            );
        }
    }
    Ok(())
}

async fn run() -> Result<i32, MimintError> {
    let argv: Vec<String> = env::args().skip(1).collect();
    if argv.is_empty() || wants_help(&argv) {
        print!("{USAGE}");
        return Ok(0);
    }
    let (command, rest) = argv.split_first().expect("non-empty argv");
    let opts = parse_opts(rest)?;
    match command.as_str() {
        "eval" => Ok(if cmd_eval(&opts).await? { 0 } else { 1 }),
        "evolve" => {
            cmd_evolve(&opts).await?;
            Ok(0)
        }
        "drift" => Ok(if cmd_drift(&opts).await? { 0 } else { 1 }),
        "finetune" => {
            cmd_finetune()?;
            Ok(0)
        }
        "skill" => {
            let action = opts.positional.first().cloned().ok_or_else(|| {
                MimintError::Config(
                    "skill requires an action: create | validate | promote | approve-import | demote | list | import"
                        .to_string(),
                )
            })?;
            // The action word is the first positional; actions
            // that take a NAME/PATH get the remaining positionals
            // via clone_opts_for_action.
            match action.as_str() {
                "create" => {
                    cmd_skill_create(&opts)?;
                    Ok(0)
                }
                "validate" => {
                    let validate_opts = clone_opts_for_action(&opts);
                    Ok(if cmd_skill_validate(&validate_opts).await? {
                        0
                    } else {
                        1
                    })
                }
                "promote" => {
                    let promote_opts = clone_opts_for_action(&opts);
                    cmd_skill_promote(&promote_opts)?;
                    Ok(0)
                }
                "approve-import" => {
                    let approve_opts = clone_opts_for_action(&opts);
                    cmd_skill_approve_import(&approve_opts)?;
                    Ok(0)
                }
                "demote" => {
                    let demote_opts = clone_opts_for_action(&opts);
                    cmd_skill_demote(&demote_opts)?;
                    Ok(0)
                }
                "list" => {
                    cmd_skill_list(&opts)?;
                    Ok(0)
                }
                "import" => {
                    let import_opts = clone_opts_for_action(&opts);
                    cmd_skill_import(&import_opts)?;
                    Ok(0)
                }
                _ => Err(MimintError::Config(format!(
                    "unknown skill action: {action} (expected create | validate | promote | approve-import | demote | list | import)"
                ))),
            }
        }
        _ => Err(MimintError::Config(format!(
            "unknown command: {command}\n\n{USAGE}"
        ))),
    }
}

/// Clone the options with the action word stripped from the
/// positionals, for skill actions that take a NAME/PATH argument.
fn clone_opts_for_action(opts: &Opts) -> Opts {
    Opts {
        config: opts.config.clone(),
        data_dir: opts.data_dir.clone(),
        audit: opts.audit.clone(),
        backend: opts.backend.clone(),
        profile: opts.profile.clone(),
        base_url: opts.base_url.clone(),
        model: opts.model.clone(),
        privacy_mode: opts.privacy_mode,
        max_steps: opts.max_steps,
        timeout_secs: opts.timeout_secs,
        time_budget_secs: opts.time_budget_secs,
        tasks: opts.tasks.clone(),
        prompt_file: opts.prompt_file.clone(),
        report: opts.report.clone(),
        notebook: opts.notebook.clone(),
        run_dir: opts.run_dir.clone(),
        no_record: opts.no_record,
        apply_winner: opts.apply_winner,
        soul_file: opts.soul_file.clone(),
        skill: opts.skill.clone(),
        name: opts.name.clone(),
        description: opts.description.clone(),
        author: opts.author.clone(),
        content: opts.content.clone(),
        content_file: opts.content_file.clone(),
        tasks_file: opts.tasks_file.clone(),
        triggers: opts.triggers.clone(),
        golden_set: opts.golden_set.clone(),
        repeat: opts.repeat,
        positional: opts.positional[1..].to_vec(),
    }
}

#[tokio::main]
async fn main() {
    match run().await {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}
