//! `mimint-app`: the Mimint desktop app (Phase 3 UI, Phase 4 body).
//!
//! Usage:
//!   mimint-app [--config PATH] [--db PATH] [--audit PATH]
//!              [--max-steps N] [--degraded] [--body]
//!              [--profile NAME] [--data-dir PATH]
//!
//! `--body` (Linux/X11 only) wires the real body through the
//! worker: the brain gets the `body_*` tools, guarded by the
//! approval card, the self-exclusion zone (this window), the
//! rate limits, and the global STOP hotkey (Ctrl+Alt+Shift+
//! Escape). Without `--body` the brain cannot touch the screen.
//!
//! `--profile` selects a named profile from the config file
//! (Phase 5): the profile's SOUL, chain, Privacy Mode, cost
//! cap, and skill selection apply to the whole run. An unknown
//! name falls closed to the default profile with a warning.
//! `--data-dir` holds SOUL.md, skills/, and experience.db
//! (default: the platform data directory).
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
//! - `MIMINT_REDUCED_MOTION=1`: OS reduced-motion signal (Linux hook).
//! - `MIMINT_APPROVAL_TIMEOUT_SECS=<n>`: approval card timeout
//!   (default 60). Values under 5 are clamped to 5.
//! - `MIMINT_MAX_OBSERVATION_AGE_SECS=<n>`: maximum age of the
//!   observation a body action may run on (default 300).
//!   Values under 5 are clamped to 5.
//! - `MIMINT_UI_PIXELS_PER_POINT=<f>`: UI scale (demo gate
//!   screenshots at 125%/150%).

use mimint_body::ScreenRect;
use mimint_control::DEFAULT_MAX_OBSERVATION_AGE;
use mimint_core::{
    build_chain, default_config_path, default_data_dir, default_vault, egress::url_is_loopback,
    evaluate_finetune, load_config_file, probe_finetune, probe_local_backends, suggestion_text,
    AuditLog, EgressPolicy, ExperienceStore, FileConfig, MimintError, Notebook, ResolvedConfig,
    SkillStore, SoulStore,
};
use mimint_ui::app::{AppOptions, BackendInfo};
use mimint_ui::approver::{approval_channel, RepaintSignal, TokenSlot, DEFAULT_APPROVAL_TIMEOUT};
use mimint_ui::worker::{BodyStack, RebuildConfig, WorkerDeps};
use mimint_ui::MimintApp;
use serde_json::json;
use std::env;
use std::path::PathBuf;
use std::process::exit;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const USAGE: &str = "mimint-app: the Mimint desktop app.\n\nUsage:\n  mimint-app [--config PATH] [--db PATH] [--audit PATH] [--max-steps N] [--degraded] [--body] [--profile NAME] [--data-dir PATH]\n\n  --config PATH     Config file (default: $XDG_CONFIG_HOME/mimint/config.toml or ~/.config/mimint/config.toml)\n  --db PATH         Notebook SQLite path (default: mimint.db)\n  --audit PATH      Audit JSONL path (default: mimint-audit.jsonl)\n  --max-steps N     Max brain steps per turn (default: 5)\n  --degraded        Static face + visible simple-mode label (weak machines)\n  --body            Wire the real body (Linux/X11 only): brain gets body_* tools + STOP hotkey\n  --profile NAME    Named profile from the config file (default: the configured default profile)\n  --data-dir PATH   User data directory for SOUL.md, skills/, experience.db (default: platform data dir)\n  --help            Show this help\n";

struct Args {
    config: Option<PathBuf>,
    db: PathBuf,
    audit: PathBuf,
    max_steps: usize,
    degraded: bool,
    body: bool,
    profile: Option<String>,
    data_dir: Option<PathBuf>,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut config = None;
    let mut db = PathBuf::from("mimint.db");
    let mut audit = PathBuf::from("mimint-audit.jsonl");
    let mut max_steps = 5usize;
    let mut degraded = false;
    let mut body = false;
    let mut profile = None;
    let mut data_dir = None;
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
            "--body" => body = true,
            "--profile" => profile = Some(value(&mut i)?),
            "--data-dir" => data_dir = Some(PathBuf::from(value(&mut i)?)),
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
        body,
        profile,
        data_dir,
    })
}

/// What `--body` mode builds: the worker's body stack, the live
/// exclusion list the app publishes to, and the approval-pending
/// flag the approver sets and the guard consults.
type BodyShares = (BodyStack, Arc<RwLock<Vec<ScreenRect>>>, Arc<AtomicBool>);

/// Build the body stack for `--body` mode (Linux/X11 only): the
/// X11 body behind the guards, sharing the stop flag, the
/// approval-pending flag, and the live exclusion list with the
/// UI. Returns the stack plus the two shares the app/worker need.
#[cfg(target_os = "linux")]
fn build_body_stack(privacy_on: bool) -> Result<BodyShares, MimintError> {
    use mimint_body::{GuardedBody, RateLimits, StopFlag, X11Body};
    use mimint_control::{BodyActor, FrameStore, PrivacyFlag};
    let stop_flag = StopFlag::new();
    let pending = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(RwLock::new(Vec::new()));
    let body = X11Body::connect()
        .map_err(|err| MimintError::Config(format!("cannot connect the X11 body: {err}")))?;
    let guarded = GuardedBody::new(body, RateLimits::default())
        .with_stop_flag(Arc::clone(&stop_flag))
        .with_approval_pending(Arc::clone(&pending))
        .with_shared_exclusion(Arc::clone(&shared));
    let actor = BodyActor::spawn(Box::new(guarded), Arc::clone(&stop_flag));
    let stack = BodyStack {
        actor,
        frames: Arc::new(FrameStore::new()),
        privacy: PrivacyFlag::new(privacy_on),
        stop_flag,
    };
    Ok((stack, shared, pending))
}

#[cfg(not(target_os = "linux"))]
fn build_body_stack(_privacy_on: bool) -> Result<BodyShares, MimintError> {
    Err(MimintError::Config(
        "--body is only available on Linux (X11) in this build: no real body exists for this platform yet".to_string(),
    ))
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

    // Active profile (Phase 5): the profile's Privacy Mode,
    // chain, and cost cap drive the whole run. An unknown
    // --profile name falls closed to the default with a warning
    // on stderr; the default profile inherits the top-level
    // settings, so a config without [[profile]] tables behaves
    // exactly as before.
    let (profile, profile_warning) = match &args.profile {
        Some(name) => resolved.profiles.resolve_or_default(name),
        None => (resolved.profiles.default_profile(), None),
    };
    let profile = profile.clone();
    if let Some(warning) = &profile_warning {
        eprintln!("mimint-app: {warning}");
    }
    let privacy_on = profile.privacy;
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
            "source": if args.profile.is_some() { "command line --profile" } else { "config file" },
            "config_path": config_display,
            "profile": profile.name,
        }),
    )?;

    // Initial chain, from the active profile. A failure does not
    // stop the window: the worker reports it, and the UI shows an
    // onboarding suggestion.
    let vault = default_vault();
    let mut onboarding_text = None;
    let initial_providers = match build_chain(
        &resolved.backends,
        &profile.chain,
        &vault,
        policy,
        profile.cost_cap,
    ) {
        Ok(providers) => providers,
        Err(err) => {
            eprintln!("mimint-app: cannot build the initial backend chain: {err}");
            onboarding_text = Some(probe_suggestion(&config_display, policy));
            Vec::new()
        }
    };
    // The backend picker lists the active profile's chain, in
    // chain order; switching profiles replaces this list.
    let backend_names: Vec<String> = profile.chain.clone();
    // Info about the providers that actually built, for the UI's
    // backend-change warning and "backend in use" line. A backend
    // that failed to build has no entry (unknown, not assumed).
    let backend_infos: Vec<BackendInfo> = initial_providers
        .iter()
        .map(|provider| BackendInfo {
            name: provider.name().to_string(),
            endpoint_url: provider.endpoint_url().to_string(),
            is_paid: provider.is_paid(),
            loopback: url_is_loopback(provider.endpoint_url()).unwrap_or(false),
        })
        .collect();

    // `--body`: build the real body stack before the worker
    // starts, so the worker registers the body tools and the app
    // shares the exclusion list + pending flag with the guard.
    let (body_stack, shared_exclusion, approval_pending) = if args.body {
        let (stack, shared, pending) = build_body_stack(privacy_on)?;
        (Some(stack), Some(shared), Some(pending))
    } else {
        (None, None, None)
    };
    // Global STOP hotkey (Linux/X11 only): kept alive for the
    // whole run; dropping it ungrabs the key and joins the
    // listener thread.
    #[cfg(target_os = "linux")]
    let _hotkey_listener = match &body_stack {
        Some(stack) => Some(
            mimint_body::HotkeyListener::start(Arc::clone(&stack.stop_flag)).map_err(|err| {
                MimintError::Config(format!("cannot start the STOP hotkey listener: {err}"))
            })?,
        ),
        None => None,
    };

    // User data (Phase 5): SOUL, skills, experience. Built-in
    // skills are installed as drafts when absent; they must pass
    // validation before use (see `mimint-eval skill`). The SOUL
    // greeting is loaded here so the first frame already greets
    // with the active profile's persona; the worker reloads
    // SOUL on every rebuild/switch.
    let data_dir = args
        .data_dir
        .clone()
        .or_else(default_data_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let soul_store = SoulStore::new(&data_dir);
    let soul_doc = soul_store.load_or_create(&profile.soul_file)?;
    let greeting = soul_doc.greeting().to_string();
    // The SOUL file path shown in Settings (F5-11): the real
    // path the document was loaded from, when known.
    let soul_path = soul_doc
        .path
        .clone()
        .unwrap_or_else(|| data_dir.join(&profile.soul_file));
    let skill_store = SkillStore::new(data_dir.join("skills"));
    skill_store.install_builtins()?;
    let experience = Arc::new(ExperienceStore::open(data_dir.join("experience.db"))?);
    let finetune = evaluate_finetune(&probe_finetune());

    let (approval_tx, approval_rx) = approval_channel();
    let approval_timeout = env::var("MIMINT_APPROVAL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.max(5)))
        .unwrap_or(DEFAULT_APPROVAL_TIMEOUT);
    let max_observation_age = env::var("MIMINT_MAX_OBSERVATION_AGE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.max(5)))
        .unwrap_or(DEFAULT_MAX_OBSERVATION_AGE);
    // The card's crop preview shares the worker's frame store:
    // clone the Arc before the stack moves into the worker.
    let body_frames = body_stack.as_ref().map(|stack| Arc::clone(&stack.frames));
    let repaint = RepaintSignal::new();
    // Capture the warnings before `resolved.profiles` moves into
    // the app options below.
    let profile_warnings = resolved.profiles.warnings.clone();
    let fallback_active = resolved.profiles.fallback_active;
    let deps = WorkerDeps {
        initial_providers,
        retry: resolved.retry,
        rebuild: Some(RebuildConfig {
            specs: resolved.backends,
            retry: resolved.retry,
        }),
        notebook,
        audit,
        max_steps: args.max_steps,
        approval_tx,
        stop_slot: TokenSlot::new(),
        turn_slot: TokenSlot::new(),
        repaint: Arc::clone(&repaint),
        approval_timeout,
        max_observation_age,
        body: body_stack,
        approval_pending,
        privacy_on,
        profiles: resolved.profiles.clone(),
        active_profile: profile.name.clone(),
        soul_store: Some(soul_store),
        skill_store: Some(skill_store),
        experience: Some(experience),
    };
    let worker = mimint_ui::worker::spawn_worker(deps);

    let degraded = args.degraded || env::var("MIMINT_UI_DEGRADED").is_ok_and(|v| v == "1");
    let options = AppOptions {
        privacy_on,
        backend_names,
        backend_infos,
        audit_path: args.audit.clone(),
        degraded,
        bench_secs: env::var("MIMINT_UI_BENCH_SECS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok()),
        event_log: env::var("MIMINT_UI_EVENT_LOG").ok().map(PathBuf::from),
        onboarding_text,
        repaint,
        shared_exclusion,
        frame_store: body_frames,
        greeting,
        profiles: resolved.profiles,
        active_profile: profile.name,
        profile_warnings,
        finetune,
        fallback_active,
        soul_path,
        data_dir: data_dir.clone(),
    };
    let app = MimintApp::new(worker, approval_rx, options);
    let pixels_per_point = env::var("MIMINT_UI_PIXELS_PER_POINT")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| *v > 0.0);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1024.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Mimint",
        native_options,
        Box::new(move |cc| {
            if let Some(ppp) = pixels_per_point {
                cc.egui_ctx.set_pixels_per_point(ppp);
            }
            Ok(Box::new(app))
        }),
    )
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
