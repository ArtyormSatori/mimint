//! `mimint-app`: the Mimint desktop app (Phase 3).
//!
//! Usage:
//!   mimint-app [--config PATH] [--db PATH] [--audit PATH]
//!              [--max-steps N] [--degraded]
//!
//! Config semantics match the CLI: an explicit `--config` file is
//! required, the default path is optional (absent means an empty
//! configuration: Privacy ON, paid locked, no backends). When no
//! backend can be built, the window still opens and shows an
//! onboarding suggestion; nothing is changed on disk.
//!
//! Env hooks (demo gate / weak machines):
//! - `MIMINT_UI_EVENT_LOG=<path>`: append UI test events.
//! - `MIMINT_UI_BENCH_SECS=<n>`: run N seconds, print FPS, exit.
//! - `MIMINT_UI_DEGRADED=1`: static face + visible simple-mode label.

use mimint_core::{
    build_chain, default_config_path, default_vault, load_config_file, probe_local_backends,
    suggestion_text, AuditLog, EgressPolicy, FileConfig, MimintError, Notebook, ResolvedConfig,
};
use mimint_ui::approver::approval_channel;
use mimint_ui::worker::{RebuildConfig, WorkerDeps};
use mimint_ui::{app::AppOptions, MimintApp};
use serde_json::json;
use std::env;
use std::path::PathBuf;
use std::process::exit;
use std::sync::Arc;

const USAGE: &str = "mimint-app: the Mimint desktop app (Phase 3).\n\nUsage:\n  mimint-app [--config PATH] [--db PATH] [--audit PATH] [--max-steps N] [--degraded]\n\n  --config PATH     Config file (default: $XDG_CONFIG_HOME/mimint/config.toml or ~/.config/mimint/config.toml)\n  --db PATH         Notebook SQLite path (default: mimint.db)\n  --audit PATH      Audit JSONL path (default: mimint-audit.jsonl)\n  --max-steps N     Max brain steps per turn (default: 5)\n  --degraded        Static face + visible simple-mode label (weak machines)\n  --help            Show this help\n";

struct Args {
    config: Option<PathBuf>,
    db: PathBuf,
    audit: PathBuf,
    max_steps: usize,
    degraded: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut config = None;
    let mut db = PathBuf::from("mimint.db");
    let mut audit = PathBuf::from("mimint-audit.jsonl");
    let mut max_steps = 5usize;
    let mut degraded = false;
    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];
        let value = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| format!("missing value for {}", argv[*i - 1]))
        };
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{USAGE}");
                exit(0);
            }
            "--config" => config = Some(PathBuf::from(value(&mut i)?)),
            "--db" => db = PathBuf::from(value(&mut i)?),
            "--audit" => audit = PathBuf::from(value(&mut i)?),
            "--max-steps" => {
                max_steps = value(&mut i)?
                    .parse()
                    .map_err(|_| "--max-steps must be a positive integer".to_string())?;
                if max_steps == 0 {
                    return Err("--max-steps must be at least 1".to_string());
                }
            }
            "--degraded" => degraded = true,
            other => return Err(format!("unknown argument: {other}\n{USAGE}")),
        }
        i += 1;
    }
    Ok(Args {
        config,
        db,
        audit,
        max_steps,
        degraded,
    })
}

fn main() {
    if let Err(err) = run() {
        eprintln!("mimint-app: {err}");
        exit(1);
    }
}

fn run() -> Result<(), MimintError> {
    let argv: Vec<String> = env::args().collect();
    let args = parse_args(&argv).map_err(MimintError::Config)?;

    let config_path = args.config.clone().or_else(default_config_path);
    let config_required = args.config.is_some();
    let resolved: ResolvedConfig = match &config_path {
        Some(path) => load_config_file(path, config_required)?.validate()?,
        None => FileConfig::empty().validate()?,
    };
    let config_display = config_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "(no config file: home directory not found)".to_string());

    let privacy_on = resolved.privacy_enabled;
    let policy = EgressPolicy::new(privacy_on);

    let notebook = Arc::new(Notebook::open(&args.db)?);
    let audit = Arc::new(AuditLog::open(&args.audit)?);
    // Record the effective Privacy Mode at startup, like the CLI.
    // A failed audit write fails the start: nothing runs unrecorded.
    audit.log(
        "ui-startup",
        "privacy_mode",
        None,
        Some(if privacy_on { "on" } else { "off" }),
        json!({
            "enabled": privacy_on,
            "source": "config file",
            "config_path": config_display,
        }),
    )?;

    // Initial chain. A failure does not stop the window: the worker
    // reports it, and the UI shows an onboarding suggestion.
    let vault = default_vault();
    let mut onboarding_text = None;
    let initial_providers = match build_chain(
        &resolved.backends,
        &resolved.chain,
        &vault,
        policy,
        resolved.cost_cap,
    ) {
        Ok(providers) => providers,
        Err(err) => {
            eprintln!("mimint-app: cannot build the initial backend chain: {err}");
            onboarding_text = Some(probe_suggestion(&config_display, policy));
            Vec::new()
        }
    };
    let backend_names: Vec<String> = resolved
        .backends
        .iter()
        .map(|spec| spec.name.clone())
        .collect();

    let (approval_tx, approval_rx) = approval_channel();
    let deps = WorkerDeps {
        initial_providers,
        retry: resolved.retry,
        rebuild: Some(RebuildConfig {
            specs: resolved.backends,
            retry: resolved.retry,
            cost_cap: resolved.cost_cap,
        }),
        notebook,
        audit,
        max_steps: args.max_steps,
        approval_tx,
    };
    let worker = mimint_ui::worker::spawn_worker(deps);

    let degraded = args.degraded || env::var("MIMINT_UI_DEGRADED").is_ok_and(|v| v == "1");
    let options = AppOptions {
        privacy_on,
        backend_names,
        audit_path: args.audit.clone(),
        degraded,
        bench_secs: env::var("MIMINT_UI_BENCH_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok()),
        event_log: env::var("MIMINT_UI_EVENT_LOG").ok().map(PathBuf::from),
        onboarding_text,
    };
    let app = MimintApp::new(worker, approval_rx, options);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1024.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native("Mimint", native_options, Box::new(|_cc| Ok(Box::new(app))))
        .map_err(|err| MimintError::Config(format!("cannot open the window: {err}")))
}

/// Probe loopback backends and render the onboarding suggestion.
/// Never fails: any error means "nothing detected".
fn probe_suggestion(config_display: &str, policy: EgressPolicy) -> String {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return "No backend is configured, and no configuration was changed.".to_string();
    };
    let probe = runtime.block_on(probe_local_backends(policy));
    suggestion_text(&probe, config_display)
}
