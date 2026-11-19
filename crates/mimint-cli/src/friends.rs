//! `mimint-cli friends ...` — Phase 7 friends operations from the
//! terminal. The turn CLI never touches friends; this subcommand
//! tree is the whole friends surface of the CLI.
//!
//! The hub this CLI runs has NO body attached: driver requests
//! are refused with a clear error ([`NullDriverBackend`]) until
//! the GUI/app wires a real body through
//! [`ActorDriverBackend`]. Pairing, sync, approval relay,
//! handoff, and the ESP32 paths all work without a body.
//!
//! Pairing mode is process-scoped: `friends hub
//! --allow-pairing` enables it in the running hub process for
//! 5 minutes. There is no standalone `allow-pairing` command
//! because a short-lived CLI process cannot hold the mode for
//! another process.
//!
//! [`ActorDriverBackend`]: mimint_control::ActorDriverBackend

use mimint_core::friends::approval::ApprovalDecision;
use mimint_core::friends::context::{
    AutoInitiatorUi, AutoListenerUi, FriendsContext, TerminalInitiatorUi,
};
use mimint_core::friends::esp32::{FakeV2Device, FakeXiaoZhiDevice};
use mimint_core::friends::handoff::{shared_notebook_context, HandoffOffer};
use mimint_core::friends::hub::{DriverBackend, HubConfig};
use mimint_core::friends::identity::DeviceKeyStore;
use mimint_core::friends::pairing::{
    read_pair_request, run_ceremony_initiator, run_ceremony_listener, InitiatorDecision,
    InitiatorDecisionCtx, InitiatorUi, ListenerDecision, ListenerDecisionCtx, ListenerUi, SasFlow,
    SasOutcome,
};
use mimint_core::friends::registry::Trust;
use mimint_core::friends::session::{handshake_initiator, handshake_listener, SessionConfig};
use mimint_core::friends::sync::{run_sync_over_session, ResolutionChoice};
use mimint_core::friends::util::Clock;
use mimint_core::friends::wire::{MessageKind, PROTOCOL_VERSION};
use mimint_core::{
    default_config_path, default_data_dir, load_config_file, AuditLog, MimintError, ResolvedConfig,
};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const FRIENDS_USAGE: &str = "mimint-cli friends: Phase 7 paired-body operations.\n\nUsage:\n  mimint-cli friends [--data-dir DIR] [--config PATH] [--db PATH] [--audit PATH] <command> [args]\n\nCommands:\n  devices                              List paired devices (trust, pairing state)\n  pair <addr> [--typing] [--guest] [--auto-confirm]\n                                         Pair with the hub at <addr> (initiator side;\n                                         the peer runs `friends hub --allow-pairing`)\n  sync <device-id> --addr <addr>         Two-way sync with a paired peer\n  revoke <device-id>                     Revoke a peer (keeps the entry; access cut)\n  delete <device-id>                     Delete a peer entry entirely\n  rename <device-id> <name>             Rename a peer\n  trust <device-id> <owner|guest>        Change the trust granted to a peer\n  reset-identity                         Rotate this body's identity key (old key refused at once)\n  approve-opt-in <device-id> --on|--off  Let a peer approve actions (default off)\n  hub [--lan] [--lab] [--allow-pairing] [--auto-confirm]\n                                         Run the hub in the foreground (brain: Ollama qwen2.5:3b)\n  handoff --to <id> --addr <addr> --task <text> [--next <text>]\n                                         Offer a task handoff to a peer\n  conflicts                              List unresolved sync conflicts with diffs\n  resolve <id> --keep-local|--keep-remote|--keep-both\n                                         Resolve one conflict\n  fake-esp32 --path xiaozhi|v2 --addr <host:port> --uuid <uuid> [--text <text>] [--ota-addr <host:port>]\n                                         Run a fake ESP32 device against a hub\n  bench [--quick]                        Measure budgets B1–B8 for real (isolated temp dirs)\n  approve-wait --addr <host:port>       Wait for a peer approval card on this hub session, show its full origin, and decide\n  help                                   Show this help\n\nPairing mode is process-scoped: start the peer hub with --allow-pairing.\n--auto-confirm is demo-only (auto-answers the SAS prompts) and prints a warning.\nThe hub has no body attached: driver methods are refused until the GUI wires one.\n";

/// The CLI's driver backend: there is no body in this process,
/// so every driver method is refused with a clear error rather
/// than faked.
pub(crate) struct NullDriverBackend;

impl DriverBackend for NullDriverBackend {
    fn execute(
        &self,
        method: &str,
        _params: &serde_json::Value,
    ) -> Result<serde_json::Value, MimintError> {
        Err(MimintError::Config(format!(
            "no body attached to this CLI process: driver method '{method}' refused"
        )))
    }

    fn name(&self) -> &str {
        "none"
    }
}

/// Global friends flags + the remaining argv.
pub(crate) struct FriendsArgs {
    data_dir: PathBuf,
    config_path: PathBuf,
    db_path: PathBuf,
    audit_path: PathBuf,
    rest: Vec<String>,
}

fn parse_friends_args(argv: &[String]) -> Result<FriendsArgs, MimintError> {
    let mut data_dir = None;
    let mut config_path = None;
    let mut db_path = None;
    let mut audit_path = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (arg.as_str(), None),
        };
        let take = |i: &mut usize| -> Result<String, MimintError> {
            if let Some(v) = inline.clone() {
                return Ok(v);
            }
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| MimintError::Config(format!("flag {flag} needs a value")))
        };
        match flag {
            "--data-dir" => data_dir = Some(PathBuf::from(take(&mut i)?)),
            "--config" => config_path = Some(PathBuf::from(take(&mut i)?)),
            "--db" => db_path = Some(PathBuf::from(take(&mut i)?)),
            "--audit" => audit_path = Some(PathBuf::from(take(&mut i)?)),
            "--help" | "-h" => {
                print!("{FRIENDS_USAGE}");
                std::process::exit(0);
            }
            _ => rest.push(arg.clone()),
        }
        i += 1;
    }
    let data_dir = data_dir.or_else(default_data_dir).ok_or_else(|| {
        MimintError::Config(
            "cannot determine the data dir: pass --data-dir or set HOME".to_string(),
        )
    })?;
    let config_path = config_path.or_else(default_config_path).ok_or_else(|| {
        MimintError::Config(
            "cannot determine the config path: pass --config or set HOME".to_string(),
        )
    })?;
    Ok(FriendsArgs {
        data_dir,
        config_path,
        db_path: db_path.unwrap_or_else(|| PathBuf::from("mimint.db")),
        audit_path: audit_path.unwrap_or_else(|| PathBuf::from("mimint-audit.jsonl")),
        rest,
    })
}

fn value_of(args: &[String], flag: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args.get(i + 1).cloned();
        }
        if let Some((f, v)) = args[i].split_once('=') {
            if f == flag {
                return Some(v.to_string());
            }
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            if !args[i].contains('=') {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

fn need_positional(args: &[String], index: usize, what: &str) -> Result<String, MimintError> {
    positionals(args)
        .into_iter()
        .nth(index)
        .ok_or_else(|| MimintError::Config(format!("missing {what}")))
}

/// Load the config (absent file = defaults) and open the
/// friends context. Refuses when `[friends] enabled = false`.
fn open_context(args: &FriendsArgs) -> Result<(FriendsContext, ResolvedConfig), MimintError> {
    open_context_with_ui(args, None)
}

/// Open a context, optionally with an explicit ceremony
/// listener UI (headless demos pass `AutoListenerUi`).
fn open_context_with_ui(
    args: &FriendsArgs,
    ceremony_ui: Option<Arc<dyn mimint_core::friends::pairing::ListenerUi>>,
) -> Result<(FriendsContext, ResolvedConfig), MimintError> {
    let resolved = load_config_file(&args.config_path, false)?.validate()?;
    if !resolved.friends_enabled {
        return Err(MimintError::Config(
            "friends is disabled in the config file ([friends] enabled = false)".to_string(),
        ));
    }
    let audit = AuditLog::open(&args.audit_path).ok().map(Arc::new);
    let ctx = match ceremony_ui {
        Some(ui) => FriendsContext::open_with_ui(
            &args.data_dir,
            &args.config_path,
            &args.db_path,
            resolved.hub.clone(),
            Arc::new(NullDriverBackend),
            audit,
            Clock::system(),
            ui,
        )?,
        None => FriendsContext::open(
            &args.data_dir,
            &args.config_path,
            &args.db_path,
            resolved.hub.clone(),
            Arc::new(NullDriverBackend),
            audit,
            Clock::system(),
        )?,
    };
    Ok((ctx, resolved))
}

fn peer_name(ctx: &FriendsContext, device_id: &str) -> String {
    ctx.registry
        .get_peer(device_id)
        .ok()
        .flatten()
        .map(|p| p.name)
        .unwrap_or_else(|| device_id.to_string())
}

pub fn run_friends(argv: &[String]) -> Result<(), MimintError> {
    let args = parse_friends_args(argv)?;
    if args.rest.is_empty() {
        print!("{FRIENDS_USAGE}");
        return Ok(());
    }
    let command = args.rest[0].clone();
    let rest = &args.rest[1..];
    match command.as_str() {
        "help" => {
            print!("{FRIENDS_USAGE}");
            Ok(())
        }
        "devices" => cmd_devices(&args),
        "pair" => cmd_pair(&args, rest),
        "sync" => cmd_sync(&args, rest),
        "revoke" => cmd_revoke(&args, rest),
        "delete" => cmd_delete(&args, rest),
        "rename" => cmd_rename(&args, rest),
        "trust" => cmd_trust(&args, rest),
        "reset-identity" => cmd_reset_identity(&args),
        "approve-opt-in" => cmd_approve_opt_in(&args, rest),
        "hub" => cmd_hub(&args, rest),
        "handoff" => cmd_handoff(&args, rest),
        "conflicts" => cmd_conflicts(&args),
        "resolve" => cmd_resolve(&args, rest),
        "fake-esp32" => cmd_fake_esp32(&args, rest),
        "bench" => crate::friends_bench::cmd_bench(&args, rest),
        "approve-wait" => cmd_approve_wait(&args, rest),
        other => Err(MimintError::Config(format!(
            "unknown friends command '{other}'\n{FRIENDS_USAGE}"
        ))),
    }
}

fn cmd_devices(args: &FriendsArgs) -> Result<(), MimintError> {
    let (ctx, _) = open_context(args)?;
    let peers = ctx.registry.list_peers()?;
    println!("This body: {}", ctx.identity.device_id());
    println!("Fingerprint: {}", ctx.identity.fingerprint());
    if peers.is_empty() {
        println!("No paired devices.");
        return Ok(());
    }
    for peer in peers {
        let prefs = ctx.registry.get_prefs(&peer.device_id)?;
        println!(
            "- {} ({}) trust={:?} pairing_incomplete={} approve={} key_generation={}",
            peer.name,
            peer.device_id,
            peer.trust,
            peer.pairing_incomplete,
            prefs.approve_allowed,
            peer.key_generation
        );
    }
    Ok(())
}

fn cmd_pair(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let addr = need_positional(rest, 0, "peer address (host:port)")?;
    let (ctx, _) = open_context(args)?;
    let flow = if has_flag(rest, "--typing") {
        SasFlow::Typing
    } else {
        SasFlow::Compare
    };
    let wanted = if has_flag(rest, "--guest") {
        Trust::Guest
    } else {
        Trust::Owner
    };
    let auto = has_flag(rest, "--auto-confirm");
    if auto {
        println!("WARNING: --auto-confirm is demo-only: it auto-answers the SAS prompts.");
    }
    let mut stream = TcpStream::connect(&addr)
        .map_err(|err| MimintError::Io(format!("cannot connect to {addr}: {err}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(300)))?;
    let entry = if auto {
        let ui = AutoInitiatorUi { trust: wanted };
        run_ceremony_initiator(
            &mut stream,
            &ctx.identity,
            &ctx.registry,
            &ui,
            flow,
            wanted,
            &ctx.clock,
            ctx.audit.clone(),
        )?
    } else {
        let ui = TerminalInitiatorUi;
        run_ceremony_initiator(
            &mut stream,
            &ctx.identity,
            &ctx.registry,
            &ui,
            flow,
            wanted,
            &ctx.clock,
            ctx.audit.clone(),
        )?
    };
    println!(
        "Paired with {} ({}) trust={:?}. Pairing completes on the first successful session.",
        entry.name, entry.device_id, entry.trust
    );
    Ok(())
}

fn cmd_sync(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let device_id = need_positional(rest, 0, "device id")?;
    let addr = value_of(rest, "--addr")
        .ok_or_else(|| MimintError::Config("sync needs --addr <host:port>".to_string()))?;
    let (ctx, resolved) = open_context(args)?;
    let peer = ctx
        .registry
        .get_peer(&device_id)?
        .ok_or_else(|| MimintError::Config(format!("unknown peer '{device_id}'")))?;
    let stream = TcpStream::connect(&addr)
        .map_err(|err| MimintError::Io(format!("cannot connect to {addr}: {err}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut session = handshake_initiator(
        stream,
        &ctx.identity,
        &ctx.registry,
        &SessionConfig::default(),
        &ctx.clock,
        ctx.audit.clone(),
    )?;
    let (sent, applied, conflicts, rejected) = run_sync_over_session(
        &mut session,
        &ctx.sync_engine,
        &peer.device_id,
        &resolved.sync_kinds,
    )?;
    println!(
        "Sync with {} ({}): sent={sent} applied={applied} conflicts={conflicts} rejected={rejected}",
        peer.name, peer.device_id
    );
    if conflicts > 0 {
        println!("Run `mimint-cli friends conflicts` to review them.");
    }
    Ok(())
}

fn cmd_revoke(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let device_id = need_positional(rest, 0, "device id")?;
    let (ctx, _) = open_context(args)?;
    let name = peer_name(&ctx, &device_id);
    ctx.registry.revoke_peer(&device_id, ctx.clock.now_ms())?;
    println!("Revoked {name} ({device_id}). The entry is kept; access is cut at once.");
    println!("Use `delete` to remove the entry entirely.");
    Ok(())
}

fn cmd_delete(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let device_id = need_positional(rest, 0, "device id")?;
    let (ctx, _) = open_context(args)?;
    let name = peer_name(&ctx, &device_id);
    ctx.registry.delete_peer(&device_id)?;
    println!("Deleted {name} ({device_id}).");
    Ok(())
}

fn cmd_rename(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let device_id = need_positional(rest, 0, "device id")?;
    let name = need_positional(rest, 1, "new name")?;
    let (ctx, _) = open_context(args)?;
    ctx.registry.rename_peer(&device_id, &name)?;
    println!("Renamed {device_id} to {name}.");
    Ok(())
}

fn cmd_trust(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let device_id = need_positional(rest, 0, "device id")?;
    let level = need_positional(rest, 1, "trust level (owner|guest)")?;
    let trust = match level.as_str() {
        "owner" => Trust::Owner,
        "guest" => Trust::Guest,
        other => {
            return Err(MimintError::Config(format!(
                "trust level must be owner or guest, got '{other}'"
            )));
        }
    };
    let (ctx, _) = open_context(args)?;
    ctx.registry.set_trust_granted(&device_id, trust)?;
    println!("Trust for {device_id} is now {trust:?}.");
    Ok(())
}

fn cmd_reset_identity(args: &FriendsArgs) -> Result<(), MimintError> {
    let store = DeviceKeyStore::in_data_dir(&args.data_dir);
    let old = store.ensure()?;
    let new = store.rotate()?;
    println!(
        "Old identity {} is refused at once (no grace period).",
        old.device_id()
    );
    println!("New device id: {}", new.device_id());
    println!("New fingerprint: {}", new.fingerprint());
    println!("WARNING: {}", mimint_core::friends::texts::KEY_LOSS_WARNING);
    println!("Re-pair every peer: the old key no longer authenticates this body.");
    Ok(())
}

fn cmd_approve_opt_in(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let device_id = need_positional(rest, 0, "device id")?;
    let on = has_flag(rest, "--on");
    let off = has_flag(rest, "--off");
    if on == off {
        return Err(MimintError::Config(
            "approve-opt-in needs exactly one of --on / --off".to_string(),
        ));
    }
    let (ctx, _) = open_context(args)?;
    let mut prefs = ctx.registry.get_prefs(&device_id)?;
    prefs.approve_allowed = on;
    ctx.registry.set_prefs(&device_id, &prefs)?;
    println!(
        "Peer {} may {} approve actions from this body.",
        peer_name(&ctx, &device_id),
        if on { "now" } else { "no longer" }
    );
    Ok(())
}

/// Minimal std-only Ollama client for the demo brain: one
/// POST to 127.0.0.1:11434, model qwen2.5:3b, no streaming.
fn ollama_generate(prompt: &str) -> Result<String, MimintError> {
    let body = serde_json::to_string(&serde_json::json!({
        "model": "qwen2.5:3b",
        "prompt": prompt,
        "stream": false,
    }))
    .map_err(|err| MimintError::Config(err.to_string()))?;
    let mut stream = TcpStream::connect("127.0.0.1:11434")
        .map_err(|err| MimintError::Io(format!("cannot reach Ollama at 127.0.0.1:11434: {err}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.write_all(
        format!(
            "POST /api/generate HTTP/1.1\r\nHost: 127.0.0.1:11434\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .as_bytes(),
    )?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let json_body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let value: serde_json::Value = serde_json::from_str(json_body)
        .map_err(|err| MimintError::Config(format!("bad Ollama reply: {err}")))?;
    value
        .get("response")
        .and_then(|r| r.as_str())
        .map(|r| r.to_string())
        .ok_or_else(|| MimintError::Config("Ollama reply has no response field".to_string()))
}

fn cmd_hub(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    // --auto-confirm is demo-only: the hub answers ceremony
    // prompts itself (no human at the terminal).
    let auto = has_flag(rest, "--auto-confirm");
    if auto {
        println!("WARNING: --auto-confirm is demo-only: the hub auto-answers the SAS prompts.");
    }
    let ui: Option<Arc<dyn mimint_core::friends::pairing::ListenerUi>> = if auto {
        Some(Arc::new(AutoListenerUi {
            trust: mimint_core::friends::registry::Trust::Owner,
        }))
    } else {
        None
    };
    let (ctx, resolved) = open_context_with_ui(args, ui.clone())?;
    let mut hub_config = resolved.hub.clone();
    if has_flag(rest, "--lan") {
        hub_config.listen_lan = true;
    }
    if has_flag(rest, "--lab") {
        hub_config.lab_mode = true;
    }
    // The context was opened with the resolved hub config;
    // rebuild it when flags override the file.
    let ctx = if hub_config.listen_lan != resolved.hub.listen_lan
        || hub_config.lab_mode != resolved.hub.lab_mode
    {
        match ui {
            Some(ui) => FriendsContext::open_with_ui(
                &args.data_dir,
                &args.config_path,
                &args.db_path,
                hub_config.clone(),
                Arc::new(NullDriverBackend),
                ctx.audit.clone(),
                Clock::system(),
                ui,
            )?,
            None => FriendsContext::open(
                &args.data_dir,
                &args.config_path,
                &args.db_path,
                hub_config.clone(),
                Arc::new(NullDriverBackend),
                ctx.audit.clone(),
                Clock::system(),
            )?,
        }
    } else {
        ctx
    };
    let brain: Arc<dyn Fn(&str) -> String + Send + Sync> =
        Arc::new(|prompt: &str| match ollama_generate(prompt) {
            Ok(text) => text,
            Err(err) => format!("(brain unavailable: {err})"),
        });
    ctx.install_ws_handler(brain);
    if has_flag(rest, "--allow-pairing") {
        ctx.controller.enable_pairing_mode();
        println!("Pairing mode ON for 5 minutes (one ceremony, then off).");
    }
    ctx.hub.start()?;
    println!("Hub running: this body {}", ctx.identity.device_id());
    println!(
        "  friends  127.0.0.1:{}{}  exposure={:?}",
        ctx.hub.friends_port(),
        if hub_config.listen_lan {
            " (LAN on)"
        } else {
            ""
        },
        ctx.hub.exposure_state()
    );
    println!("  xiaozhi  port {}", ctx.hub.xiaozhi_port());
    println!("  ota      port {}", ctx.hub.ota_port());
    println!("  brain    Ollama qwen2.5:3b @ 127.0.0.1:11434 (one brain for both ESP32 paths)");
    println!("  driver   none (no body attached to this CLI process)");
    println!("Press Ctrl-C to stop. Pairing: peer runs `mimint-cli friends pair <this-host>:<friends-port>`.");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn cmd_handoff(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let to = value_of(rest, "--to")
        .ok_or_else(|| MimintError::Config("handoff needs --to <device-id>".to_string()))?;
    let addr = value_of(rest, "--addr")
        .ok_or_else(|| MimintError::Config("handoff needs --addr <host:port>".to_string()))?;
    let task = value_of(rest, "--task")
        .ok_or_else(|| MimintError::Config("handoff needs --task <text>".to_string()))?;
    let next = value_of(rest, "--next").unwrap_or_default();
    let (ctx, _) = open_context(args)?;
    let peer = ctx
        .registry
        .get_peer(&to)?
        .ok_or_else(|| MimintError::Config(format!("unknown peer '{to}'")))?;
    let notebook_context = shared_notebook_context(&ctx.notebook)?;
    let package = ctx.handoff.build_package(
        &ctx.identity,
        &peer.device_id,
        task,
        Vec::new(),
        next,
        Vec::new(),
        Vec::new(),
        notebook_context,
        "default",
    )?;
    let stream = TcpStream::connect(&addr)
        .map_err(|err| MimintError::Io(format!("cannot connect to {addr}: {err}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut session = handshake_initiator(
        stream,
        &ctx.identity,
        &ctx.registry,
        &SessionConfig::default(),
        &ctx.clock,
        ctx.audit.clone(),
    )?;
    let offer = HandoffOffer {
        v: PROTOCOL_VERSION,
        kind: "handoff_offer".to_string(),
        package: package.clone(),
    };
    session.send(
        MessageKind::HandoffOffer,
        &serde_json::to_vec(&offer).unwrap(),
    )?;
    println!(
        "Handoff offered to {} ({}): package {} lease {}",
        peer.name, peer.device_id, package.package_id, package.lease_id
    );
    println!("Waiting for the peer's answer (the hub auto-accepts verified packages)...");
    let (kind, payload) = session.recv()?;
    match kind {
        MessageKind::HandoffAccept => {
            let value: serde_json::Value = serde_json::from_slice(&payload)
                .map_err(|err| MimintError::Config(err.to_string()))?;
            println!(
                "ACCEPTED: lease {} held by {}. This body stops acting on the task.",
                value
                    .get("lease_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"),
                peer.name
            );
        }
        MessageKind::HandoffReject => {
            println!("REJECTED by the peer (lease returned).");
        }
        other => {
            println!("Unexpected reply: {}", other.as_str());
        }
    }
    Ok(())
}

fn cmd_conflicts(args: &FriendsArgs) -> Result<(), MimintError> {
    let (ctx, _) = open_context(args)?;
    let rows = ctx.registry.list_conflicts(false)?;
    if rows.is_empty() {
        println!("No unresolved conflicts.");
        return Ok(());
    }
    for row in rows {
        println!(
            "Conflict {}: {} {} (local v{}@{:.8} vs remote v{}@{:.8})",
            row.id,
            row.kind,
            row.sync_id,
            row.local_version.lamport,
            row.local_version.device,
            row.remote_version.lamport,
            row.remote_version.device
        );
        for line in ctx.sync_engine.conflict_diff(row.id)? {
            println!("    {line}");
        }
        println!(
            "    Resolve: `friends resolve {} --keep-local|--keep-remote|--keep-both`",
            row.id
        );
        println!("    (SOUL/profile: --keep-both is disabled, with an explanation.)");
    }
    Ok(())
}

fn cmd_resolve(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let id: i64 = need_positional(rest, 0, "conflict id")?
        .parse()
        .map_err(|_| MimintError::Config("conflict id must be a number".to_string()))?;
    let choice = if has_flag(rest, "--keep-local") {
        ResolutionChoice::KeepLocal
    } else if has_flag(rest, "--keep-remote") {
        ResolutionChoice::KeepRemote
    } else if has_flag(rest, "--keep-both") {
        ResolutionChoice::KeepBoth
    } else {
        return Err(MimintError::Config(
            "resolve needs one of --keep-local / --keep-remote / --keep-both".to_string(),
        ));
    };
    let (ctx, _) = open_context(args)?;
    let records = ctx.sync_engine.resolve_conflict(id, choice)?;
    println!(
        "Resolved conflict {id}: {} record(s) produced for the next sync.",
        records.len()
    );
    Ok(())
}

/// Raw HTTP GET to an OTA server: returns the OtaResponse JSON.
fn ota_check_http(ota_addr: &str, uuid: &str) -> Result<serde_json::Value, MimintError> {
    let mut stream = TcpStream::connect(ota_addr)
        .map_err(|err| MimintError::Io(format!("cannot connect to OTA {ota_addr}: {err}")))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(
        format!(
            "GET /ota/check HTTP/1.1\r\nHost: {ota_addr}\r\nDevice-Id: {uuid}\r\nClient-Id: {uuid}\r\nConnection: close\r\n\r\n"
        )
        .as_bytes(),
    )?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => ("", ""),
    };
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Err(MimintError::Config(format!(
            "OTA check refused (HTTP {status}): unknown UUID or rate limited (1 check/60s)"
        )));
    }
    serde_json::from_str(body).map_err(|err| MimintError::Config(format!("bad OTA reply: {err}")))
}

fn cmd_fake_esp32(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let path = value_of(rest, "--path")
        .ok_or_else(|| MimintError::Config("fake-esp32 needs --path xiaozhi|v2".to_string()))?;
    let addr = value_of(rest, "--addr")
        .ok_or_else(|| MimintError::Config("fake-esp32 needs --addr <host:port>".to_string()))?;
    let uuid = value_of(rest, "--uuid")
        .ok_or_else(|| MimintError::Config("fake-esp32 needs --uuid <device uuid>".to_string()))?;
    let text = value_of(rest, "--text").unwrap_or_else(|| "xin chào".to_string());
    let (ctx, _) = open_context(args)?;
    match path.as_str() {
        "xiaozhi" => {
            let ota_addr = value_of(rest, "--ota-addr").unwrap_or_else(|| {
                let host = addr.split(':').next().unwrap_or("127.0.0.1");
                format!("{host}:19122")
            });
            let response = ota_check_http(&ota_addr, &uuid)?;
            let token = response
                .get("websocket")
                .and_then(|w| w.get("token"))
                .and_then(|t| t.as_str())
                .ok_or_else(|| MimintError::Config("OTA reply has no token".to_string()))?;
            let url = response
                .get("websocket")
                .and_then(|w| w.get("url"))
                .and_then(|u| u.as_str())
                .unwrap_or("");
            // The OTA URL is scheme://host:port/path?token=...:
            // the fake device takes host:port only (it builds
            // its own /xiaozhi path with the real token).
            let ws_addr = url
                .trim_start_matches("wss://")
                .trim_start_matches("ws://")
                .split(['?', '/'])
                .next()
                .unwrap_or(&addr)
                .to_string();
            println!("OTA ok: websocket {url}");
            let mut device = FakeXiaoZhiDevice::connect(&ws_addr, &uuid, token)?;
            let ack = device.read_text()?;
            println!("hello ack: {ack}");
            device.send_text(&text)?;
            println!("sent text: {text}");
            println!("reply: {}", device.read_text()?);
            println!("(emotion message follows; reading one more frame)");
            println!("frame: {}", device.read_text()?);
        }
        "v2" => {
            let mut device =
                FakeV2Device::connect(&addr, &ctx.identity, &ctx.registry, &ctx.clock)?;
            device.send_text(&text)?;
            println!("sent text (v2): {text}");
            println!("reply: {}", device.read_text()?);
        }
        other => {
            return Err(MimintError::Config(format!(
                "--path must be xiaozhi or v2, got '{other}'"
            )));
        }
    }
    Ok(())
}

/// Open a context for bench measurements (isolated temp dir,
/// loopback hub config, no audit, no body).
pub(crate) fn bench_context(dir: &Path) -> Result<FriendsContext, MimintError> {
    FriendsContext::open(
        dir,
        dir.join("config.toml"),
        dir.join("notebook.db"),
        HubConfig::default(),
        Arc::new(NullDriverBackend),
        None,
        Clock::system(),
    )
}

/// One sync-exchange result: (sent, applied, conflicts, rejected).
pub type SyncCounts = (usize, usize, usize, usize);

/// Two-way sync exchange between two contexts over loopback
/// (both sides run the symmetric exchange concurrently).
/// Returns (counts for A, counts for B).
pub fn sync_both_in_process(
    a: &FriendsContext,
    b: &FriendsContext,
    kinds: &mimint_core::friends::sync::SyncKinds,
) -> Result<(SyncCounts, SyncCounts), MimintError> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let a_id = a.identity.device_id().to_string();
    let b_id = b.identity.device_id().to_string();
    std::thread::scope(|scope| {
        let listener_handle = scope.spawn(move || {
            let (stream, _) = listener
                .accept()
                .map_err(|err| MimintError::Io(err.to_string()))?;
            let mut session_b = handshake_listener(
                stream,
                &b.identity,
                &b.registry,
                &SessionConfig::default(),
                &b.clock,
                b.audit.clone(),
            )?;
            run_sync_over_session(&mut session_b, &b.sync_engine, &a_id, kinds)
        });
        let stream = TcpStream::connect(addr)
            .map_err(|err| MimintError::Io(format!("cannot connect to {addr}: {err}")))?;
        let mut session_a = handshake_initiator(
            stream,
            &a.identity,
            &a.registry,
            &SessionConfig::default(),
            &a.clock,
            a.audit.clone(),
        )?;
        let result_a = run_sync_over_session(&mut session_a, &a.sync_engine, &b_id, kinds);
        let result_b = listener_handle
            .join()
            .map_err(|_| MimintError::Config("sync listener thread panicked".to_string()))?;
        Ok((result_a?, result_b?))
    })
}
/// One-shot SAS slot for the bench typing flow: it stands in
/// for the user reading A's screen and typing the digits on B.
/// `AutoListenerUi` cannot do the typing flow by design — in
/// that flow B must NOT display its SAS (`ctx.sas` is `None`),
/// so an auto UI would type an empty string and the SAS check
/// would fail. The channel carries the real SAS A showed.
struct SasChannel {
    slot: Mutex<Option<String>>,
}

impl SasChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(None),
        })
    }

    fn publish(&self, sas: &str) {
        *self.slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(sas.to_string());
    }

    fn take(&self, timeout: Duration) -> Option<String> {
        let start = std::time::Instant::now();
        loop {
            if let Some(sas) = self.slot.lock().unwrap_or_else(|p| p.into_inner()).take() {
                return Some(sas);
            }
            if start.elapsed() >= timeout {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Initiator UI for the bench typing flow: publishes the SAS A
/// shows so the listener side can type it (standing in for
/// the user's eyes).
struct ChannelSasInitiatorUi {
    channel: Arc<SasChannel>,
    trust: Trust,
}

impl InitiatorUi for ChannelSasInitiatorUi {
    fn show_sas(&self, sas: &str, _peer_fingerprint: &str, _peer_device_id: &str) {
        self.channel.publish(sas);
    }

    fn decide(&self, _ctx: &InitiatorDecisionCtx) -> InitiatorDecision {
        InitiatorDecision {
            confirmed: true,
            trust_granted: self.trust,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        }
    }
}

/// Listener UI for the bench typing flow: types the SAS the
/// initiator published (compare flow: confirms the match).
struct ChannelSasListenerUi {
    channel: Arc<SasChannel>,
    trust: Trust,
}

impl ListenerUi for ChannelSasListenerUi {
    fn decide(&self, ctx: &ListenerDecisionCtx) -> ListenerDecision {
        ListenerDecision {
            sas_outcome: match ctx.flow {
                SasFlow::Compare => SasOutcome::Matched,
                SasFlow::Typing => SasOutcome::Typed(
                    self.channel
                        .take(Duration::from_secs(5))
                        .unwrap_or_default(),
                ),
            },
            trust_granted: self.trust,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        }
    }
}

/// Wait for a peer approval card: connect to the peer hub,
/// ping (which triggers card delivery), show the first card
/// with its full origin fields, and ask the user to approve
/// or deny it. The decision goes back over the same E2E
/// session. This is the peer-side decision surface for cards
/// the local brain (or the panel's test-card button) created.
fn cmd_approve_wait(args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let addr = value_of(rest, "--addr")
        .ok_or_else(|| MimintError::Config("approve-wait needs --addr <host:port>".to_string()))?;
    let (ctx, _resolved) = open_context(args)?;
    let stream = TcpStream::connect(&addr)
        .map_err(|err| MimintError::Io(format!("cannot connect to {addr}: {err}")))?;
    let mut session = handshake_initiator(
        stream,
        &ctx.identity,
        &ctx.registry,
        &SessionConfig::default(),
        &ctx.clock,
        ctx.audit.clone(),
    )?;
    println!("Đã kết nối tới {addr}. Đang chờ thẻ phê duyệt (tối đa 70 giây)...");
    let deadline = Instant::now() + Duration::from_secs(70);
    loop {
        if Instant::now() > deadline {
            println!("Hết thời gian chờ: không có thẻ phê duyệt nào.");
            return Ok(());
        }
        session.send(MessageKind::Ping, b"")?;
        let (kind, payload) = session.recv()?;
        match kind {
            MessageKind::ApprovalCard => {
                let card: mimint_core::friends::approval::ApprovalCard =
                    serde_json::from_slice(&payload)
                        .map_err(|err| MimintError::Config(err.to_string()))?;
                println!("Thẻ phê duyệt từ peer:");
                println!("  thiết bị: {} ({:.8})", card.device_name, card.device_id);
                println!("  agent: {}", card.agent_id);
                println!("  yêu cầu: {}", card.request_id);
                println!("  hành động: {}", card.action);
                println!("  chi tiết: {}", card.detail);
                print!("Phê duyệt? [approve/deny]: ");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                let decision = if line.trim().eq_ignore_ascii_case("approve") {
                    "approve"
                } else {
                    "deny"
                };
                let d = ApprovalDecision {
                    v: PROTOCOL_VERSION,
                    kind: "approval_decision".to_string(),
                    device_id: card.device_id.clone(),
                    agent_id: card.agent_id.clone(),
                    request_id: card.request_id.clone(),
                    decision: decision.to_string(),
                    decided_ms: mimint_core::audit::now_ms(),
                };
                session.send(
                    MessageKind::ApprovalDecision,
                    &serde_json::to_vec(&d).map_err(|err| MimintError::Config(err.to_string()))?,
                )?;
                println!("Đã gửi quyết định: {decision}.");
                return Ok(());
            }
            MessageKind::Pong => {
                std::thread::sleep(Duration::from_secs(2));
            }
            _ => {}
        }
    }
}

/// In-process ceremony between two contexts (bench only):
/// B listens on a loopback port, A connects as initiator.
pub fn ceremony_in_process(
    a: &FriendsContext,
    b: &FriendsContext,
    flow: SasFlow,
) -> Result<(), MimintError> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    b.controller.enable_pairing_mode();
    // The typing flow needs a channel: the initiator publishes
    // the SAS it shows, the listener types it (standing in for
    // the user's eyes). The compare flow uses the plain auto UIs.
    let channel = SasChannel::new();
    let list_ui: Arc<dyn ListenerUi> = match flow {
        SasFlow::Typing => Arc::new(ChannelSasListenerUi {
            channel: Arc::clone(&channel),
            trust: Trust::Owner,
        }),
        SasFlow::Compare => Arc::new(AutoListenerUi {
            trust: Trust::Owner,
        }),
    };
    let init_ui: Arc<dyn InitiatorUi> = match flow {
        SasFlow::Typing => Arc::new(ChannelSasInitiatorUi {
            channel: Arc::clone(&channel),
            trust: Trust::Owner,
        }),
        SasFlow::Compare => Arc::new(AutoInitiatorUi {
            trust: Trust::Owner,
        }),
    };
    std::thread::scope(|scope| {
        let listener_handle = scope.spawn(move || {
            let (mut stream, peer_addr) = listener
                .accept()
                .map_err(|err| MimintError::Io(err.to_string()))?;
            stream.set_read_timeout(Some(Duration::from_secs(30)))?;
            let request = read_pair_request(&mut stream)?;
            run_ceremony_listener(
                &mut stream,
                &peer_addr.to_string(),
                request,
                &b.identity,
                &b.registry,
                &b.controller,
                list_ui.as_ref(),
                flow,
                &b.clock,
                b.audit.clone(),
            )
        });
        let mut stream = TcpStream::connect(addr)
            .map_err(|err| MimintError::Io(format!("cannot connect to {addr}: {err}")))?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let initiator = run_ceremony_initiator(
            &mut stream,
            &a.identity,
            &a.registry,
            init_ui.as_ref(),
            flow,
            Trust::Owner,
            &a.clock,
            a.audit.clone(),
        );
        let listener_result = listener_handle
            .join()
            .map_err(|_| MimintError::Config("ceremony listener thread panicked".to_string()))?;
        initiator?;
        listener_result?;
        Ok(())
    })
}

/// One handshake between two paired contexts over loopback.
pub fn handshake_in_process(a: &FriendsContext, b: &FriendsContext) -> Result<(), MimintError> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    std::thread::scope(|scope| {
        let listener_handle = scope.spawn(move || {
            let (stream, _) = listener
                .accept()
                .map_err(|err| MimintError::Io(err.to_string()))?;
            handshake_listener(
                stream,
                &b.identity,
                &b.registry,
                &SessionConfig::default(),
                &b.clock,
                b.audit.clone(),
            )
        });
        let stream = TcpStream::connect(addr)
            .map_err(|err| MimintError::Io(format!("cannot connect to {addr}: {err}")))?;
        let _session = handshake_initiator(
            stream,
            &a.identity,
            &a.registry,
            &SessionConfig::default(),
            &a.clock,
            a.audit.clone(),
        )?;
        let _session_b = listener_handle
            .join()
            .map_err(|_| MimintError::Config("handshake listener thread panicked".to_string()))??;
        Ok(())
    })
}
