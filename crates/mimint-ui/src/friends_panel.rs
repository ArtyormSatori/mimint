//! Phase 7 friends panel for the desktop app (`mimint-app`).
//!
//! What this is: the GUI surface for paired bodies — device
//! list, pairing (both SAS flows), conflict resolution, hub
//! status, handoff, ESP32 status, and the Privacy Mode text.
//! What it is NOT: a second implementation of the protocol.
//! Every network operation runs on one worker thread (the
//! [`FriendsWorker`]); the panel only reads local state
//! (registry, hub status, relay cards) on the frame and sends
//! commands to the worker. Sync never runs on the UI frame
//! path (§12.3).
//!
//! Pairing prompts (both directions) travel through the panel:
//! the worker's initiator UI and the hub's listener UI send
//! their SAS/prompt to the panel and block on the user's
//! decision (5-minute ceremony timeout). The typing flow shows
//! a text field for the SAS digits the user reads on the other
//! device's screen.

use mimint_core::friends::approval::{ApprovalRelay, CardOrigin, DecisionOutcome};
use mimint_core::friends::context::FriendsContext;
use mimint_core::friends::esp32::esp32_status_label;
use mimint_core::friends::esp32::Esp32Path;
use mimint_core::friends::handoff::HandoffManager;
use mimint_core::friends::handoff::{
    shared_notebook_context, HandoffOffer, HandoffReject, HANDOFF_ACTUATOR,
};
use mimint_core::friends::hub::Hub;
use mimint_core::friends::identity::Identity;
use mimint_core::friends::pairing::{
    run_ceremony_initiator, InitiatorDecision, InitiatorDecisionCtx, InitiatorUi, ListenerDecision,
    ListenerDecisionCtx, ListenerUi, PairingController, SasFlow, SasOutcome,
};
use mimint_core::friends::registry::{DeviceRegistry, Trust};
use mimint_core::friends::session::{handshake_initiator, SessionConfig};
use mimint_core::friends::sync::SyncEngine;
use mimint_core::friends::sync::{run_sync_over_session, ResolutionChoice, SyncKinds};
use mimint_core::friends::texts;
use mimint_core::friends::util::{random_hex, Clock};
use mimint_core::friends::wire::{MessageKind, PROTOCOL_VERSION};
use mimint_core::MimintError;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The friends sub-views (tabs at the top of the panel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FriendsView {
    Devices,
    Pair,
    Conflicts,
    Hub,
    Handoff,
    Esp32,
}

impl FriendsView {
    pub fn label(self) -> &'static str {
        match self {
            FriendsView::Devices => "Thiết bị",
            FriendsView::Pair => "Ghép đôi",
            FriendsView::Conflicts => "Xung đột",
            FriendsView::Hub => "Hub",
            FriendsView::Handoff => "Handoff",
            FriendsView::Esp32 => "ESP32",
        }
    }

    /// Parse `--friends-view <name>` (English or Vietnamese).
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "devices" | "thiet-bi" | "thiết-bị" => Some(FriendsView::Devices),
            "pair" | "ghep-doi" | "ghép-đôi" => Some(FriendsView::Pair),
            "conflicts" | "xung-dot" | "xung-đột" => Some(FriendsView::Conflicts),
            "hub" => Some(FriendsView::Hub),
            "handoff" => Some(FriendsView::Handoff),
            "esp32" => Some(FriendsView::Esp32),
            _ => None,
        }
    }
}

/// Commands the panel sends to the worker thread. Network
/// operations only; local registry reads/writes happen in the
/// panel directly (they are cheap SQLite calls, no network).
#[derive(Debug)]
pub enum FriendsCommand {
    Pair {
        addr: String,
        flow: SasFlow,
        guest: bool,
    },
    Sync {
        device_id: String,
        addr: String,
    },
    StartHub,
    StopHub,
    EnablePairing,
    ResolveConflict {
        id: i64,
        choice: ResolutionChoice,
    },
    HandoffOffer {
        to: String,
        addr: String,
        task: String,
        next: String,
    },
    /// Decide a pending handoff this body received (§7.3):
    /// the user pressed "Tiếp tục ở đây" (accept: lease +
    /// handoff_accept + a new turn from the next step) or
    /// "Từ chối" (reject: the lease stays with the sender).
    HandoffDecide {
        package_id: String,
        accept: bool,
    },
    /// Create a test approval card for a peer (demo hook for
    /// the G2 approval flow; the production path is the brain's
    /// approver creating the card in this same process). The
    /// worker waits up to 65s for the peer's decision.
    CreateCard {
        target: String,
        action: String,
    },
}

/// Events the worker (and the hub's listener UI) send back to
/// the panel. The SAS/prompt variants carry the decision
/// channel the panel answers when the user clicks.
pub enum FriendsEvent {
    Paired {
        peer_name: String,
        device_id: String,
    },
    PairFailed {
        error: String,
    },
    /// The initiator side shows this SAS: the user confirms it
    /// matches the other device (compare flow) or reads it to
    /// type on the other device (typing flow).
    SasShown {
        sas: String,
        fingerprint: String,
        device_id: String,
        decision_tx: Sender<InitiatorDecision>,
    },
    /// An incoming ceremony on the hub: the user decides on
    /// this device (SAS compare / typing field / trust /
    /// fingerprint).
    PairingPrompt {
        sas: Option<String>,
        fingerprint: String,
        device_id: String,
        flow: SasFlow,
        decision_tx: Sender<ListenerDecision>,
    },
    SyncDone {
        sent: usize,
        applied: usize,
        conflicts: usize,
        rejected: usize,
    },
    SyncFailed {
        error: String,
    },
    HubStarted,
    HubFailed {
        error: String,
    },
    HubStopped,
    PairingModeOn,
    ConflictResolved {
        id: i64,
        records: usize,
    },
    ConflictFailed {
        error: String,
    },
    HandoffDone {
        device_id: String,
    },
    HandoffFailed {
        error: String,
    },
    /// A verified handoff offer arrived and is waiting for
    /// this body's user to decide (§7.3): nothing runs before
    /// the user presses "Tiếp tục ở đây" / "Từ chối".
    HandoffReceived {
        package_id: String,
        task: String,
    },
    /// The user accepted a pending handoff: the lease is held
    /// and a new turn starts from the next step (§7.3).
    HandoffAccepted {
        package_id: String,
        task: String,
    },
    /// The user declined a pending handoff: the lease returns
    /// to the sender (§7.3).
    HandoffDeclined {
        package_id: String,
    },
    /// The peer decided on a card this device created.
    CardDecided {
        request_id: String,
        outcome: String,
    },
    Status(String),
}

/// Initiator UI for a pairing started from the panel: shows
/// the SAS in the panel and waits for the user's decision.
pub struct PanelInitiatorUi {
    events: Sender<FriendsEvent>,
    decision_rx: Arc<Mutex<Option<Receiver<InitiatorDecision>>>>,
}

impl PanelInitiatorUi {
    pub fn new(events: Sender<FriendsEvent>) -> Self {
        Self {
            events,
            decision_rx: Arc::new(Mutex::new(None)),
        }
    }
}

impl InitiatorUi for PanelInitiatorUi {
    fn show_sas(&self, sas: &str, peer_fingerprint: &str, peer_device_id: &str) {
        let (tx, rx) = std::sync::mpsc::channel();
        *self.decision_rx.lock().unwrap_or_else(|p| p.into_inner()) = Some(rx);
        let _ = self.events.send(FriendsEvent::SasShown {
            sas: sas.to_string(),
            fingerprint: peer_fingerprint.to_string(),
            device_id: peer_device_id.to_string(),
            decision_tx: tx,
        });
    }

    fn decide(&self, _ctx: &InitiatorDecisionCtx) -> InitiatorDecision {
        let rx = self
            .decision_rx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        match rx {
            Some(rx) => rx
                .recv_timeout(Duration::from_secs(5 * 60))
                .unwrap_or(InitiatorDecision {
                    confirmed: false,
                    trust_granted: Trust::Owner,
                    fingerprint_confirmed: false,
                    fingerprint_skipped_with_warning: false,
                }),
            None => InitiatorDecision {
                confirmed: false,
                trust_granted: Trust::Owner,
                fingerprint_confirmed: false,
                fingerprint_skipped_with_warning: false,
            },
        }
    }
}

/// Listener UI for ceremonies arriving at the hub: shows the
/// prompt in the panel and waits for the user's decision.
pub struct PanelListenerUi {
    events: Sender<FriendsEvent>,
}

impl PanelListenerUi {
    pub fn new(events: Sender<FriendsEvent>) -> Self {
        Self { events }
    }
}

impl ListenerUi for PanelListenerUi {
    fn decide(&self, ctx: &ListenerDecisionCtx) -> ListenerDecision {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = self.events.send(FriendsEvent::PairingPrompt {
            sas: ctx.sas.clone(),
            fingerprint: ctx.peer_fingerprint.clone(),
            device_id: ctx.peer_device_id.clone(),
            flow: ctx.flow,
            decision_tx: tx,
        });
        rx.recv_timeout(Duration::from_secs(5 * 60))
            .unwrap_or(ListenerDecision {
                sas_outcome: SasOutcome::Mismatched,
                trust_granted: Trust::Guest,
                fingerprint_confirmed: false,
                fingerprint_skipped_with_warning: false,
            })
    }
}

/// One brain turn through the local Ollama endpoint
/// (qwen2.5:3b), std-only HTTP like the CLI. This is the ONE
/// brain for both ESP32 paths (§8.5); the app's configured
/// backends may be remote, so the ESP32 brain never goes
/// through them.
pub fn ollama_turn(prompt: &str) -> String {
    match ollama_generate(prompt) {
        Ok(text) => text,
        Err(err) => format!("(brain unavailable: {err})"),
    }
}

fn ollama_generate(prompt: &str) -> Result<String, MimintError> {
    let body = serde_json::json!({
        "model": "qwen2.5:3b",
        "prompt": prompt,
        "stream": false,
    })
    .to_string();
    let mut stream = TcpStream::connect("127.0.0.1:11434")
        .map_err(|err| MimintError::Io(format!("cannot reach Ollama: {err}")))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|err| MimintError::Io(err.to_string()))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| MimintError::Io(err.to_string()))?;
    let request = format!(
        "POST /api/generate HTTP/1.1\r\nHost: 127.0.0.1:11434\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|err| MimintError::Io(err.to_string()))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|err| MimintError::Io(err.to_string()))?;
    let text = String::from_utf8_lossy(&raw);
    let Some(start) = text.find("\r\n\r\n") else {
        return Err(MimintError::Config("bad Ollama response".to_string()));
    };
    let value: serde_json::Value = serde_json::from_str(&text[start + 4..])
        .map_err(|err| MimintError::Config(err.to_string()))?;
    value
        .get("response")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| MimintError::Config("Ollama response has no text".to_string()))
}

/// The UI's driver backend when no body is attached
/// (no `--body`): every driver method is refused with a clear
/// error rather than faked.
pub struct NullDriverBackend;

impl mimint_core::friends::hub::DriverBackend for NullDriverBackend {
    fn name(&self) -> &str {
        "none"
    }

    fn execute(
        &self,
        method: &str,
        _params: &serde_json::Value,
    ) -> Result<serde_json::Value, MimintError> {
        Err(MimintError::Config(format!(
            "no body attached to this app process: driver method '{method}' refused"
        )))
    }
}

/// The worker thread: owns the [`FriendsContext`] and runs
/// every blocking network operation the panel asks for. The
/// panel never blocks on it — results come back as events.
pub fn spawn_friends_worker(
    ctx: FriendsContext,
    cmd_rx: Receiver<FriendsCommand>,
    event_tx: Sender<FriendsEvent>,
) {
    std::thread::spawn(move || {
        for cmd in cmd_rx {
            let result = handle_command(&ctx, cmd, &event_tx);
            if let Err(err) = result {
                let _ = event_tx.send(FriendsEvent::Status(format!("lỗi: {err}")));
            }
        }
    });
}

fn handle_command(
    ctx: &FriendsContext,
    cmd: FriendsCommand,
    event_tx: &Sender<FriendsEvent>,
) -> Result<(), MimintError> {
    match cmd {
        FriendsCommand::Pair { addr, flow, guest } => {
            let mut stream = TcpStream::connect(&addr)
                .map_err(|err| MimintError::Io(format!("cannot connect to {addr}: {err}")))?;
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .map_err(|err| MimintError::Io(err.to_string()))?;
            let ui = PanelInitiatorUi::new(event_tx.clone());
            let wanted = if guest { Trust::Guest } else { Trust::Owner };
            match run_ceremony_initiator(
                &mut stream,
                &ctx.identity,
                &ctx.registry,
                &ui,
                flow,
                wanted,
                &ctx.clock,
                ctx.audit.clone(),
            ) {
                Ok(peer) => {
                    let _ = event_tx.send(FriendsEvent::Paired {
                        peer_name: peer.name,
                        device_id: peer.device_id,
                    });
                }
                Err(err) => {
                    let _ = event_tx.send(FriendsEvent::PairFailed {
                        error: err.to_string(),
                    });
                }
            }
        }
        FriendsCommand::Sync { device_id, addr } => {
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
            let prefs = ctx.registry.get_prefs(&device_id).unwrap_or_default();
            let kinds = SyncKinds::from_prefs(&prefs);
            let (sent, applied, conflicts, rejected) =
                run_sync_over_session(&mut session, &ctx.sync_engine, &device_id, &kinds)?;
            let _ = event_tx.send(FriendsEvent::SyncDone {
                sent,
                applied,
                conflicts,
                rejected,
            });
        }
        FriendsCommand::StartHub => match ctx.hub.start() {
            Ok(()) => {
                let _ = event_tx.send(FriendsEvent::HubStarted);
            }
            Err(err) => {
                let _ = event_tx.send(FriendsEvent::HubFailed {
                    error: err.to_string(),
                });
            }
        },
        FriendsCommand::StopHub => {
            ctx.hub.stop();
            let _ = event_tx.send(FriendsEvent::HubStopped);
        }
        FriendsCommand::EnablePairing => {
            ctx.controller.enable_pairing_mode();
            let _ = event_tx.send(FriendsEvent::PairingModeOn);
        }
        FriendsCommand::ResolveConflict { id, choice } => {
            match ctx.sync_engine.resolve_conflict(id, choice) {
                Ok(records) => {
                    let _ = event_tx.send(FriendsEvent::ConflictResolved {
                        id,
                        records: records.len(),
                    });
                }
                Err(err) => {
                    let _ = event_tx.send(FriendsEvent::ConflictFailed {
                        error: err.to_string(),
                    });
                }
            }
        }
        FriendsCommand::CreateCard { target, action } => {
            // F7-12: the card wait (up to 65 s) runs on its OWN
            // thread. It used to block this worker thread, so a
            // card awaiting a decision queued every other
            // friends operation behind it.
            let relay = Arc::clone(&ctx.relay);
            let registry = Arc::clone(&ctx.registry);
            let identity = Arc::clone(&ctx.identity);
            let event_tx = event_tx.clone();
            std::thread::spawn(move || {
                let prefs = registry.get_prefs(&target).unwrap_or_default();
                let origin = CardOrigin {
                    device_id: identity.device_id().to_string(),
                    device_name: format!(
                        "Thiết bị {}",
                        &identity.device_id()[..8.min(identity.device_id().len())]
                    ),
                    agent_id: "agent-001".to_string(),
                    request_id: random_hex(16),
                    action,
                    detail: serde_json::json!({"source": "friends panel test card"}),
                };
                match relay.create_card(origin, &target, prefs.approve_allowed) {
                    Ok((card, rx)) => {
                        let _ = event_tx.send(FriendsEvent::Status(format!(
                            "Đã tạo thẻ {} — đang chờ peer quyết định (tối đa 60 giây).",
                            card.request_id
                        )));
                        match rx.recv_timeout(Duration::from_secs(65)) {
                            Ok(outcome) => {
                                let outcome = match outcome {
                                    DecisionOutcome::Approved => "approved",
                                    DecisionOutcome::Denied => "denied",
                                };
                                let _ = event_tx.send(FriendsEvent::CardDecided {
                                    request_id: card.request_id,
                                    outcome: outcome.to_string(),
                                });
                            }
                            Err(_) => {
                                let _ = event_tx.send(FriendsEvent::Status(
                                    "Thẻ hết hạn mà peer chưa quyết định.".to_string(),
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        let _ = event_tx.send(FriendsEvent::Status(format!(
                            "Không tạo được thẻ phê duyệt: {err}"
                        )));
                    }
                }
            });
        }
        FriendsCommand::HandoffOffer {
            to,
            addr,
            task,
            next,
        } => {
            let notebook_context = shared_notebook_context(&ctx.notebook)?;
            let package = ctx.handoff.build_package(
                &ctx.identity,
                &to,
                task,
                Vec::new(),
                next,
                Vec::new(),
                Vec::new(),
                notebook_context,
                HANDOFF_ACTUATOR,
            )?;
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
            let offer = HandoffOffer {
                v: PROTOCOL_VERSION,
                kind: MessageKind::HandoffOffer.as_str().to_string(),
                package: package.clone(),
            };
            session.send(
                MessageKind::HandoffOffer,
                &serde_json::to_vec(&offer).map_err(|err| MimintError::Config(err.to_string()))?,
            )?;
            // The receiver's user decides (§7.3): the answer
            // may take up to the lease TTL (10 minutes). Wait
            // on a thread so the wait is bounded by the lease,
            // and a missing answer is reported as an expiry,
            // not a bare error.
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut session = session;
                let _ = reply_tx.send(session.recv());
            });
            let wait = Duration::from_secs(11 * 60);
            match reply_rx.recv_timeout(wait) {
                Ok(Ok((MessageKind::HandoffAccept, _))) => {
                    ctx.handoff.mark_handed_off(&package.package_id);
                    let _ = event_tx.send(FriendsEvent::HandoffDone { device_id: to });
                }
                Ok(Ok((MessageKind::HandoffReject, payload))) => {
                    // The lease is back with the sender: it may
                    // act on the task again (§7.2). Clear any
                    // handed-off mark defensively (none is set
                    // before an accept, but a stale mark must
                    // never block the sender).
                    ctx.handoff.clear_handed_off_package(&package.package_id);
                    let reason = serde_json::from_slice::<HandoffReject>(&payload)
                        .map(|r| r.reason)
                        .unwrap_or_default();
                    let _ = event_tx.send(FriendsEvent::HandoffFailed {
                        error: handoff_reject_text(&reason),
                    });
                }
                Ok(Ok((kind, _))) => {
                    ctx.handoff.clear_handed_off_package(&package.package_id);
                    let _ = event_tx.send(FriendsEvent::HandoffFailed {
                        error: format!("peer trả về loại tin không mong đợi: {kind:?}"),
                    });
                }
                Ok(Err(_)) => {
                    // The session died without an answer: the
                    // lease returns on TTL expiry (§7.2).
                    ctx.handoff.clear_handed_off_package(&package.package_id);
                    let _ = event_tx.send(FriendsEvent::HandoffFailed {
                        error: "Không nhận được trả lời từ peer (kết nối đã đóng) — lease hết hạn theo TTL rồi bạn tiếp tục.".to_string(),
                    });
                }
                Err(_) => {
                    // No answer before the lease TTL ran out.
                    ctx.handoff.clear_handed_off_package(&package.package_id);
                    let _ = event_tx.send(FriendsEvent::HandoffFailed {
                        error: "Lease đã hết hạn mà peer chưa trả lời — bạn có thể tiếp tục hoặc handoff lại.".to_string(),
                    });
                }
            }
        }
        FriendsCommand::HandoffDecide { package_id, accept } => {
            match ctx.hub.decide_handoff(&package_id, accept) {
                Ok(package) => {
                    if accept {
                        let _ = event_tx.send(FriendsEvent::HandoffAccepted {
                            package_id: package.package_id,
                            task: package.task_text,
                        });
                    } else {
                        let _ = event_tx.send(FriendsEvent::HandoffDeclined {
                            package_id: package.package_id,
                        });
                    }
                }
                Err(err) => {
                    let _ = event_tx.send(FriendsEvent::Status(format!(
                        "Không quyết định được handoff {package_id}: {err}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Sender-side text for a handoff rejection (§7.2): the lease
/// is back, the sender continues.
fn handoff_reject_text(reason: &str) -> String {
    match reason {
        "rejected_by_user" => "B đã từ chối, bạn tiếp tục.".to_string(),
        "expired" => {
            "Lease đã hết hạn mà peer chưa trả lời — bạn có thể tiếp tục hoặc handoff lại."
                .to_string()
        }
        "lease_busy" => {
            "B đang giữ một lease khác (lease_busy) — bạn tiếp tục hoặc handoff lại sau."
                .to_string()
        }
        "" => "B đã từ chối, bạn tiếp tục.".to_string(),
        other => format!("B đã từ chối ({other}), bạn tiếp tục."),
    }
}

/// One pending SAS confirmation (initiator side, started
/// from this panel).
struct PendingSas {
    sas: String,
    fingerprint: String,
    device_id: String,
    decision_tx: Sender<InitiatorDecision>,
}

/// One pending incoming ceremony (listener side, arrived at
/// the hub): the user decides in the panel.
struct PendingPrompt {
    sas: Option<String>,
    fingerprint: String,
    device_id: String,
    flow: SasFlow,
    decision_tx: Sender<ListenerDecision>,
    typed: String,
    guest: bool,
    fingerprint_confirmed: bool,
    fingerprint_skip: bool,
}

/// The friends panel state (UI thread only). Network work
/// lives in the worker thread; this struct only reads local
/// state and sends commands.
pub struct FriendsPanel {
    view: FriendsView,
    identity: Arc<Identity>,
    registry: Arc<DeviceRegistry>,
    controller: Arc<PairingController>,
    relay: Arc<ApprovalRelay>,
    handoff: Arc<HandoffManager>,
    sync_engine: Arc<SyncEngine>,
    hub: Arc<Hub>,
    cmd_tx: Sender<FriendsCommand>,
    event_rx: Receiver<FriendsEvent>,
    status: String,
    pair_addr: String,
    pair_compare: bool,
    pair_guest: bool,
    pair_busy: bool,
    pending_sas: Option<PendingSas>,
    pending_prompt: Option<PendingPrompt>,
    handoff_to: String,
    handoff_addr: String,
    handoff_task: String,
    handoff_next: String,
    handoff_notice: Option<String>,
    /// A handoff this body accepted: (package id, task). The
    /// panel polls the hub for the new turn's brain reply.
    accepted_handoff: Option<(String, String)>,
    /// The accepted handoff's brain-turn reply, once it lands.
    handoff_turn_reply: Option<String>,
    card_target: String,
    card_action: String,
    /// F7-23: the registry was recreated after peers.db was
    /// deleted — shown as a warning above the tabs.
    peers_db_restored: bool,
    /// F7-16: the initiator's separate fingerprint check —
    /// ticked only after comparing the peer fingerprint with
    /// the other device's screen. The confirm button stays
    /// disabled until then.
    sas_fingerprint_checked: bool,
    /// F7-15: when the current pairing ceremony started, for
    /// the 5-minute expiry countdown.
    pair_started_ms: Option<u64>,
    rename_target: Option<String>,
    rename_text: String,
    diff_cache: HashMap<i64, Vec<String>>,
    hidden_conflicts: std::collections::HashSet<i64>,
}

impl FriendsPanel {
    pub fn new(
        ctx: &FriendsContext,
        cmd_tx: Sender<FriendsCommand>,
        event_rx: Receiver<FriendsEvent>,
        view: FriendsView,
    ) -> Self {
        Self {
            view,
            identity: Arc::clone(&ctx.identity),
            registry: Arc::clone(&ctx.registry),
            controller: Arc::clone(&ctx.controller),
            relay: Arc::clone(&ctx.relay),
            handoff: Arc::clone(&ctx.handoff),
            sync_engine: Arc::clone(&ctx.sync_engine),
            hub: Arc::clone(&ctx.hub),
            cmd_tx,
            event_rx,
            status: String::new(),
            pair_addr: "127.0.0.1:19120".to_string(),
            pair_compare: true,
            pair_guest: false,
            pair_busy: false,
            pending_sas: None,
            pending_prompt: None,
            handoff_to: String::new(),
            handoff_addr: "127.0.0.1:19120".to_string(),
            handoff_task: String::new(),
            handoff_next: String::new(),
            handoff_notice: None,
            accepted_handoff: None,
            handoff_turn_reply: None,
            card_target: String::new(),
            card_action: "screenshot".to_string(),
            peers_db_restored: ctx.peers_db_restored,
            sas_fingerprint_checked: false,
            pair_started_ms: None,
            rename_target: None,
            rename_text: String::new(),
            diff_cache: HashMap::new(),
            hidden_conflicts: std::collections::HashSet::new(),
        }
    }

    fn send(&self, cmd: FriendsCommand) {
        if self.cmd_tx.send(cmd).is_err() {
            // The worker thread is gone; the status line says
            // so on the next frame (no panic on the UI thread).
        }
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                FriendsEvent::Paired {
                    peer_name,
                    device_id,
                } => {
                    self.pair_busy = false;
                    self.status = format!("Đã ghép đôi với {peer_name} ({device_id:.8}).");
                }
                FriendsEvent::PairFailed { error } => {
                    self.pair_busy = false;
                    // F7-15: pairing errors name their cause and
                    // say what to do next, instead of one generic
                    // line.
                    let lower = error.to_lowercase();
                    if lower.contains("locked") {
                        self.status = format!(
                            "Ghép đôi bị khóa sau nhiều lần sai (lockout 15 phút): {error} — đợi hết 15 phút rồi thử lại, kiểm tra mã SAS kỹ trước khi bấm."
                        );
                    } else if lower.contains("sas")
                        || lower.contains("mismatch")
                        || lower.contains("mismatched")
                    {
                        self.status = format!(
                            "Mã SAS không khớp: {error} — so sánh lại mã trên cả hai màn hình rồi thử lại từ đầu."
                        );
                    } else if lower.contains("expired")
                        || lower.contains("timeout")
                        || lower.contains("timed out")
                    {
                        self.status = format!(
                            "Ceremony đã hết hạn (5 phút): {error} — bật lại chế độ ghép đôi ở phía hub rồi thử lại."
                        );
                    } else {
                        self.status = format!("Ghép đôi thất bại: {error}");
                    }
                }
                FriendsEvent::SasShown {
                    sas,
                    fingerprint,
                    device_id,
                    decision_tx,
                } => {
                    // A fresh SAS resets the initiator's
                    // separate fingerprint check (F7-16) and
                    // starts the ceremony countdown (F7-15).
                    self.sas_fingerprint_checked = false;
                    self.pair_started_ms = Some(Clock::system().now_ms());
                    self.pending_sas = Some(PendingSas {
                        sas,
                        fingerprint,
                        device_id,
                        decision_tx,
                    });
                }
                FriendsEvent::PairingPrompt {
                    sas,
                    fingerprint,
                    device_id,
                    flow,
                    decision_tx,
                } => {
                    self.pending_prompt = Some(PendingPrompt {
                        sas,
                        fingerprint,
                        device_id,
                        flow,
                        decision_tx,
                        typed: String::new(),
                        guest: false,
                        fingerprint_confirmed: false,
                        fingerprint_skip: false,
                    });
                }
                FriendsEvent::SyncDone {
                    sent,
                    applied,
                    conflicts,
                    rejected,
                } => {
                    self.status = format!(
                        "Đồng bộ xong: gửi {sent}, áp dụng {applied}, xung đột {conflicts}, từ chối {rejected}."
                    );
                }
                FriendsEvent::SyncFailed { error } => {
                    self.status = format!("Đồng bộ thất bại: {error}");
                }
                FriendsEvent::HubStarted => {
                    self.status = "Hub đã khởi động.".to_string();
                }
                FriendsEvent::HubFailed { error } => {
                    self.status = format!("Không khởi động được hub: {error}");
                }
                FriendsEvent::HubStopped => {
                    self.status = "Hub đã dừng.".to_string();
                }
                FriendsEvent::PairingModeOn => {
                    self.status =
                        "Chế độ ghép đôi đang bật 5 phút (một ceremony, rồi tự tắt).".to_string();
                }
                FriendsEvent::ConflictResolved { id, records } => {
                    self.diff_cache.remove(&id);
                    self.status = format!("Đã giải quyết xung đột {id} ({records} record ghi).");
                }
                FriendsEvent::ConflictFailed { error } => {
                    self.status = format!("Giải quyết xung đột thất bại: {error}");
                }
                FriendsEvent::HandoffDone { device_id } => {
                    // F7-04: the RECEIVER holds the lease now —
                    // the sender's notice must not claim it
                    // holds the lease. Also clears the
                    // "Đang gửi…" status set at click time.
                    if self.status.starts_with("Đang gửi handoff") {
                        self.status.clear();
                    }
                    self.handoff_notice = Some(format!(
                        "Đã bàn giao cho {device_id:.8} — máy nhận đang giữ lease và tiếp tục task; phía này đã dừng task."
                    ));
                }
                FriendsEvent::HandoffFailed { error } => {
                    self.status = format!("Handoff thất bại: {error}");
                }
                FriendsEvent::CardDecided {
                    request_id,
                    outcome,
                } => {
                    self.status = format!("Peer đã quyết định thẻ {request_id}: {outcome}.");
                }
                FriendsEvent::HandoffReceived { package_id, task } => {
                    self.handoff_notice = Some(format!(
                        "Nhận handoff {package_id:.8}: {task} (đã xác minh chữ ký, đang chờ bạn quyết định ở tab Handoff)."
                    ));
                }
                FriendsEvent::HandoffAccepted { package_id, task } => {
                    self.accepted_handoff = Some((package_id, task.clone()));
                    self.handoff_notice = Some(format!(
                        "Đã nhận handoff: {task} — lease đang giữ, đang tiếp tục từ bước tiếp theo."
                    ));
                }
                FriendsEvent::HandoffDeclined { package_id } => {
                    self.handoff_notice = Some(format!(
                        "Đã từ chối handoff {package_id:.8} — lease trả về peer."
                    ));
                }
                FriendsEvent::Status(msg) => {
                    self.status = msg;
                }
            }
        }
    }

    /// Render the whole panel into the central area.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.drain_events();
        if let Some((package_id, _)) = &self.accepted_handoff {
            if let Some(reply) = self.hub.take_handoff_turn_reply(package_id) {
                self.handoff_turn_reply = Some(reply);
            }
        }
        // F7-23: the registry was recreated after peers.db
        // was deleted — Lamport is back to 0 and last_seen is
        // gone, so the next sync may conflict more.
        if self.peers_db_restored {
            ui.colored_label(
                egui::Color32::from_rgb(0xDC, 0x26, 0x26),
                texts::PEERS_DB_RESTORE_WARNING,
            );
        }
        ui.heading("Bạn bè — thiết bị đã ghép đôi");
        ui.label(format!(
            "Thiết bị này: {} (fingerprint {})",
            &self.identity.device_id()[..8.min(self.identity.device_id().len())],
            mimint_core::friends::crypto::fingerprint_short(self.identity.fingerprint())
        ));
        if let Some(notice) = &self.handoff_notice {
            ui.colored_label(
                egui::Color32::from_rgb(0x1D, 0x4E, 0xD8),
                format!("Handoff: {notice}"),
            );
            if ui.button("Đóng thanh handoff").clicked() {
                self.handoff_notice = None;
            }
        }
        self.show_pending_prompt(ui);
        ui.horizontal(|ui| {
            for view in [
                FriendsView::Devices,
                FriendsView::Pair,
                FriendsView::Conflicts,
                FriendsView::Hub,
                FriendsView::Handoff,
                FriendsView::Esp32,
            ] {
                if ui
                    .selectable_label(self.view == view, view.label())
                    .clicked()
                {
                    self.view = view;
                }
            }
        });
        ui.separator();
        match self.view {
            FriendsView::Devices => self.show_devices(ui),
            FriendsView::Pair => self.show_pair(ui),
            FriendsView::Conflicts => self.show_conflicts(ui),
            FriendsView::Hub => self.show_hub(ui),
            FriendsView::Handoff => self.show_handoff(ui),
            FriendsView::Esp32 => self.show_esp32(ui),
        }
        if !self.status.is_empty() {
            ui.separator();
            ui.label(&self.status);
        }
    }

    /// The incoming-ceremony prompt (rendered above the tabs
    /// whenever one is pending, whatever tab is open).
    fn show_pending_prompt(&mut self, ui: &mut egui::Ui) {
        let Some(prompt) = &mut self.pending_prompt else {
            return;
        };
        ui.separator();
        ui.colored_label(
            egui::Color32::from_rgb(0xB4, 0x53, 0x09),
            format!(
                "Thiết bị {} muốn ghép đôi với bạn.",
                &prompt.device_id[..8.min(prompt.device_id.len())]
            ),
        );
        ui.label(format!(
            "Fingerprint: {} (đầy đủ: {})",
            mimint_core::friends::crypto::fingerprint_short(&prompt.fingerprint),
            prompt.fingerprint
        ));
        match prompt.flow {
            SasFlow::Compare => {
                let sas = prompt.sas.clone().unwrap_or_default();
                ui.label(format!("Mã SAS bên kia hiện: {sas}"));
                ui.label("So sánh với mã trên màn hình thiết bị kia. Chỉ bấm \"khớp\" khi hai mã giống hệt nhau.");
            }
            SasFlow::Typing => {
                ui.label("Nhập mã SAS 6 số bạn đọc được trên màn hình thiết bị kia:");
                ui.text_edit_singleline(&mut prompt.typed);
            }
        }
        ui.horizontal(|ui| {
            ui.checkbox(
                &mut prompt.guest,
                "Chỉ cho quyền Guest (không sync/không phê duyệt)",
            );
            ui.checkbox(&mut prompt.fingerprint_confirmed, "Đã kiểm tra fingerprint");
            ui.checkbox(
                &mut prompt.fingerprint_skip,
                "Bỏ qua fingerprint (có cảnh báo)",
            );
        });
        if prompt.fingerprint_skip {
            ui.label(texts::FINGERPRINT_SKIP_WARNING);
        }
        let can_decide = prompt.fingerprint_confirmed || prompt.fingerprint_skip;
        if !can_decide {
            ui.label("Hãy xác nhận fingerprint hoặc chọn bỏ qua có cảnh báo trước khi quyết định.");
        }
        let mut decision: Option<ListenerDecision> = None;
        ui.horizontal(|ui| {
            let accept_label = match prompt.flow {
                SasFlow::Compare => "SAS khớp — đồng ý ghép đôi",
                SasFlow::Typing => "Xác nhận mã đã nhập",
            };
            if ui
                .add_enabled(can_decide, egui::Button::new(accept_label))
                .clicked()
            {
                decision = Some(ListenerDecision {
                    sas_outcome: match prompt.flow {
                        SasFlow::Compare => SasOutcome::Matched,
                        SasFlow::Typing => SasOutcome::Typed(prompt.typed.clone()),
                    },
                    trust_granted: if prompt.guest {
                        Trust::Guest
                    } else {
                        Trust::Owner
                    },
                    fingerprint_confirmed: prompt.fingerprint_confirmed,
                    fingerprint_skipped_with_warning: prompt.fingerprint_skip,
                });
            }
            if ui.button("Từ chối").clicked() {
                decision = Some(ListenerDecision {
                    sas_outcome: SasOutcome::Mismatched,
                    trust_granted: Trust::Guest,
                    fingerprint_confirmed: false,
                    fingerprint_skipped_with_warning: false,
                });
            }
        });
        if let Some(decision) = decision {
            let prompt = self.pending_prompt.take().unwrap();
            let _ = prompt.decision_tx.send(decision);
        }
    }

    fn show_devices(&mut self, ui: &mut egui::Ui) {
        let peers = self.registry.list_peers().unwrap_or_default();
        if peers.is_empty() {
            ui.label("Chưa có thiết bị nào được ghép đôi. Sang tab \"Ghép đôi\" để thêm.");
        }
        for peer in peers {
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(format!("{} ({:.8})", peer.name, peer.device_id));
                // F7-21: the trust scale is the LOCAL grant —
                // label it as such, and show what the peer
                // claims to have granted only as display-only.
                ui.label(format!(
                    "quyền máy này cấp cho peer: {}",
                    peer.trust.as_str()
                ));
                if let Some(received) = peer.trust_received {
                    ui.label(format!(
                        "peer nói đã cấp cho bạn: {} (chỉ để xem)",
                        received.as_str()
                    ));
                }
                if peer.pairing_incomplete {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xB4, 0x53, 0x09),
                        "chưa hoàn tất (xong sau handshake đầu tiên)",
                    );
                }
                if peer.revoked {
                    ui.colored_label(egui::Color32::from_rgb(0xDC, 0x26, 0x26), "đã thu hồi");
                }
                ui.label(format!("key generation {}", peer.key_generation));
            });
            ui.label(format!(
                "Fingerprint: {}",
                mimint_core::friends::crypto::fingerprint_short(&peer.fingerprint_hex)
            ));
            let mut prefs = self.registry.get_prefs(&peer.device_id).unwrap_or_default();
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut prefs.approve_allowed, "Cho phép phê duyệt từ peer này")
                    .changed()
                {
                    let _ = self.registry.set_prefs(&peer.device_id, &prefs);
                }
                if ui.button("Đồng bộ ngay").clicked() {
                    self.send(FriendsCommand::Sync {
                        device_id: peer.device_id.clone(),
                        addr: "127.0.0.1:19120".to_string(),
                    });
                    self.status = "Đang đồng bộ (hub của peer phải chạy ở 127.0.0.1:19120; đổi địa chỉ ở tab Ghép đôi nếu khác).".to_string();
                }
            });
            ui.horizontal(|ui| {
                if !peer.revoked && ui.button("Thu hồi").clicked() {
                    // F7-07: revoke through the hub, not the
                    // registry alone — the hub also cuts the
                    // peer's live session at once.
                    if self.hub.revoke_peer(&peer.device_id) {
                        self.status = format!(
                            "Đã thu hồi {} (phiên đang chạy đã bị ngắt).",
                            peer.name
                        );
                    } else {
                        self.status = format!("Không thu hồi được {}.", peer.name);
                    }
                }
                if ui.button("Xóa").clicked() {
                    let _ = self.registry.delete_peer(&peer.device_id);
                    self.status = format!("Đã xóa {}.", peer.name);
                }
                if ui.button("Đổi tên").clicked() {
                    self.rename_target = Some(peer.device_id.clone());
                    self.rename_text = peer.name.clone();
                }
                let trust_label = match peer.trust {
                    Trust::Owner => "Đổi quyền thành Guest",
                    Trust::Guest => "Đổi quyền thành Owner",
                };
                if ui.button(trust_label).clicked() {
                    let next = match peer.trust {
                        Trust::Owner => Trust::Guest,
                        Trust::Guest => Trust::Owner,
                    };
                    let _ = self.registry.set_trust_granted(&peer.device_id, next);
                }
            });
            if self.rename_target.as_deref() == Some(peer.device_id.as_str()) {
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut self.rename_text);
                    if ui.button("Lưu tên").clicked() {
                        let _ = self
                            .registry
                            .rename_peer(&peer.device_id, &self.rename_text);
                        self.rename_target = None;
                    }
                    if ui.button("Hủy").clicked() {
                        self.rename_target = None;
                    }
                });
            }
        }
        ui.separator();
        ui.label(texts::SELECTIVE_SYNC_NOTEBOOK);
        ui.separator();
        // F7-25: the "[Xem danh sách thiết bị đã ghép đôi]"
        // inside PRIVACY_MODE_TEXT was plain text — render it
        // as a real button that switches to this tab.
        let privacy = texts::PRIVACY_MODE_TEXT
            .replace(" [Xem danh sách thiết bị đã ghép đôi]", "");
        ui.label(privacy);
        if ui.button("Xem danh sách thiết bị đã ghép đôi").clicked() {
            self.view = FriendsView::Devices;
        }
        if ui.button("Sao chép fingerprint của thiết bị này").clicked() {
            ui.ctx().copy_text(self.identity.fingerprint().to_string());
        }
        // F7-25: the demo approval card lives here now, not in
        // the Hub tab — it is a per-peer approval test, and the
        // Hub tab must stay about the hub.
        ui.separator();
        ui.heading("Thẻ phê duyệt thử (demo)");
        ui.label("Tạo một thẻ phê duyệt cho peer đã bật phê duyệt (checkbox ở trên). Peer quyết định bằng `mimint-cli friends approve-wait --addr <hub này>`. Đường production là brain tạo thẻ trong chính tiến trình này.");
        ui.horizontal(|ui| {
            ui.label("Peer (device id):");
            ui.text_edit_singleline(&mut self.card_target);
        });
        ui.horizontal(|ui| {
            ui.label("Hành động:");
            ui.text_edit_singleline(&mut self.card_action);
        });
        if ui.button("Tạo thẻ phê duyệt thử").clicked() {
            self.send(FriendsCommand::CreateCard {
                target: self.card_target.clone(),
                action: self.card_action.clone(),
            });
        }
    }

    fn show_pair(&mut self, ui: &mut egui::Ui) {
        ui.label("Ghép đôi với một hub đang bật chế độ ghép đôi (5 phút, một ceremony).");
        ui.horizontal(|ui| {
            ui.label("Địa chỉ hub:");
            ui.text_edit_singleline(&mut self.pair_addr);
        });
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.pair_compare, true, "So sánh mã SAS (mặc định)");
            ui.radio_value(&mut self.pair_compare, false, "Gõ mã SAS bằng tay");
        });
        ui.checkbox(
            &mut self.pair_guest,
            "Chỉ xin quyền Guest (không sync, không phê duyệt)",
        );
        if ui
            .add_enabled(!self.pair_busy, egui::Button::new("Ghép đôi"))
            .clicked()
        {
            self.pair_busy = true;
            self.pending_sas = None;
            self.send(FriendsCommand::Pair {
                addr: self.pair_addr.clone(),
                flow: if self.pair_compare {
                    SasFlow::Compare
                } else {
                    SasFlow::Typing
                },
                guest: self.pair_guest,
            });
            self.status = "Đang ghép đôi — chờ mã SAS hiện ra rồi xác nhận.".to_string();
        }
        if let Some(pending) = &self.pending_sas {
            ui.separator();
            // F7-15: the ceremony expires 5 minutes after the
            // SAS appears — show the countdown, not just the
            // pairing-mode timer in the Hub tab.
            if let Some(started) = self.pair_started_ms {
                let elapsed = Clock::system().now_ms().saturating_sub(started);
                let remaining = (5 * 60 * 1000u64).saturating_sub(elapsed);
                if remaining > 0 {
                    ui.label(format!(
                        "Ceremony còn {} giây (hết hạn sau 5 phút — phải xác nhận trước đó).",
                        remaining / 1000
                    ));
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xDC, 0x26, 0x26),
                        "Ceremony đã hết hạn 5 phút — bật lại chế độ ghép đôi rồi thử lại.",
                    );
                }
            }
            ui.colored_label(
                egui::Color32::from_rgb(0x1D, 0x4E, 0xD8),
                format!(
                    "Mã SAS của bạn: {} (cho {})",
                    pending.sas,
                    &pending.device_id[..8.min(pending.device_id.len())]
                ),
            );
            ui.label(format!(
                "Fingerprint của peer: {} (đầy đủ: {})",
                mimint_core::friends::crypto::fingerprint_short(&pending.fingerprint),
                pending.fingerprint
            ));
            // F7-16: copy the PEER's fingerprint (the Devices
            // tab button copies this device's instead).
            if ui.button("Sao chép fingerprint của peer").clicked() {
                ui.ctx().copy_text(pending.fingerprint.clone());
            }
            if self.pair_compare {
                ui.label("So sánh mã này với mã trên màn hình thiết bị kia. Chỉ bấm \"khớp\" khi hai mã giống hệt nhau — máy không thể phát hiện bạn xác nhận một mã sai (rủi ro T12).");
            } else {
                ui.label("Đọc mã này cho người ở thiết bị kia gõ vào. Mã không tự khớp — phía kia phải gõ đúng 6 số.");
            }
            // F7-16: confirming the SAS and confirming the
            // fingerprint are two separate steps. The confirm
            // button stays disabled until the user ticks that
            // they compared the peer fingerprint with the other
            // device's screen — it no longer sets
            // fingerprint_confirmed by itself.
            ui.checkbox(
                &mut self.sas_fingerprint_checked,
                "Đã đối chiếu fingerprint của peer với màn hình thiết bị kia",
            );
            let mut decision: Option<InitiatorDecision> = None;
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        self.sas_fingerprint_checked,
                        egui::Button::new("Mã khớp — xác nhận"),
                    )
                    .clicked()
                {
                    decision = Some(InitiatorDecision {
                        confirmed: true,
                        trust_granted: if self.pair_guest {
                            Trust::Guest
                        } else {
                            Trust::Owner
                        },
                        fingerprint_confirmed: self.sas_fingerprint_checked,
                        fingerprint_skipped_with_warning: false,
                    });
                }
                if ui.button("Hủy ghép đôi").clicked() {
                    decision = Some(InitiatorDecision {
                        confirmed: false,
                        trust_granted: Trust::Owner,
                        fingerprint_confirmed: false,
                        fingerprint_skipped_with_warning: false,
                    });
                }
            });
            if let Some(decision) = decision {
                let pending = self.pending_sas.take().unwrap();
                let _ = pending.decision_tx.send(decision);
            }
        }
        ui.separator();
        ui.label("Ghép đôi xong vẫn \"chưa hoàn tất\" cho tới handshake đầu tiên thành công — đó là thiết kế, không phải lỗi.");
    }

    fn show_conflicts(&mut self, ui: &mut egui::Ui) {
        // F7-25: the 90-day tombstone rule, where the user
        // resolves deletions.
        ui.label(texts::TOMBSTONE_90_DAY_LIMIT);
        ui.separator();
        // F7-02: SoulFile / naming_profile records held for
        // this user's approval are listed here next to the
        // conflicts, with approve / reject buttons.
        let pending = self.sync_engine.pending_sync_approvals().unwrap_or_default();
        if !pending.is_empty() {
            ui.heading("Chờ bạn duyệt (SOUL / profile từ peer)");
            ui.label("Các record này KHÔNG tự áp dụng — chỉ bạn mới được duyệt. Từ chối thì record bị bỏ, không áp dụng.");
            for (sync_id, kind, peer_device_id, _record_json, _received_ms) in pending {
                ui.separator();
                ui.label(format!(
                    "{kind} {sync_id} — từ {peer_device_id:.8}"
                ));
                ui.horizontal(|ui| {
                    if ui.button("Duyệt và áp dụng").clicked() {
                        match self.sync_engine.approve_pending_sync(&sync_id) {
                            Ok(true) => {
                                self.status = format!("Đã duyệt và áp dụng {sync_id}.");
                            }
                            Ok(false) => {
                                self.status = format!("Không còn gì chờ duyệt cho {sync_id}.");
                            }
                            Err(err) => {
                                self.status = format!("Không duyệt được {sync_id}: {err}");
                            }
                        }
                    }
                    if ui.button("Từ chối (bỏ record)").clicked() {
                        match self.sync_engine.reject_pending_sync(&sync_id) {
                            Ok(true) => {
                                self.status = format!("Đã từ chối {sync_id}.");
                            }
                            Ok(false) => {
                                self.status = format!("Không còn gì chờ duyệt cho {sync_id}.");
                            }
                            Err(err) => {
                                self.status = format!("Không từ chối được {sync_id}: {err}");
                            }
                        }
                    }
                });
            }
            ui.separator();
        }
        let conflicts = self.registry.list_conflicts(false).unwrap_or_default();
        if conflicts.is_empty() {
            ui.label("Không có xung đột chưa giải quyết.");
            return;
        }
        for row in conflicts {
            if self.hidden_conflicts.contains(&row.id) {
                continue;
            }
            ui.separator();
            ui.label(format!(
                "Xung đột {}: {} {} (peer {:.8}, phát hiện {})",
                row.id,
                row.kind,
                row.sync_id,
                row.peer_device_id,
                crate::audit_view::format_time_ms(row.detected_ms)
            ));
            ui.label(format!(
                "Phiên bản: local v{}@{} — remote v{}@{}",
                row.local_version.lamport,
                &row.local_version.device[..8.min(row.local_version.device.len())],
                row.remote_version.lamport,
                &row.remote_version.device[..8.min(row.remote_version.device.len())]
            ));
            let diff = self
                .diff_cache
                .entry(row.id)
                .or_insert_with(|| {
                    self.sync_engine
                        .conflict_diff(row.id)
                        .unwrap_or_else(|_| vec!["không đọc được diff".to_string()])
                })
                .clone();
            egui::ScrollArea::vertical()
                .max_height(120.0)
                .show(ui, |ui| {
                    for line in &diff {
                        ui.label(line);
                    }
                });
            let keep_both_disabled = row.kind == "soul_file" || row.kind == "naming_profile";
            ui.label(texts::CONFLICT_KEEP_MINE);
            if ui.button("Giữ bản của tôi").clicked() {
                self.send(FriendsCommand::ResolveConflict {
                    id: row.id,
                    choice: ResolutionChoice::KeepLocal,
                });
            }
            ui.label(texts::CONFLICT_TAKE_PEER);
            if ui.button("Giữ bản của peer").clicked() {
                self.send(FriendsCommand::ResolveConflict {
                    id: row.id,
                    choice: ResolutionChoice::KeepRemote,
                });
            }
            ui.label(texts::CONFLICT_KEEP_BOTH);
            if keep_both_disabled {
                ui.label(texts::CONFLICT_KEEP_BOTH_DISABLED);
                ui.add_enabled(false, egui::Button::new("Giữ cả hai (không khả dụng)"));
            } else if ui.button("Giữ cả hai").clicked() {
                self.send(FriendsCommand::ResolveConflict {
                    id: row.id,
                    choice: ResolutionChoice::KeepBoth,
                });
            }
            ui.label(texts::CONFLICT_EDIT_MYSELF);
            if ui.button("Để sau").clicked() {
                self.hidden_conflicts.insert(row.id);
            }
        }
    }

    fn show_hub(&mut self, ui: &mut egui::Ui) {
        let exposure = self.hub.exposure_state();
        ui.label(format!("Trạng thái phơi nhiễm: {exposure:?}"));
        ui.label(format!(
            "Cổng: friends {} · xiaozhi {} · ota {}",
            self.hub.friends_port(),
            self.hub.xiaozhi_port(),
            self.hub.ota_port()
        ));
        // F7-06: OTA is an exposure state of its own — show
        // whether it is serving right now.
        if self.hub.ota_open() {
            ui.label(texts::HUB_EXPOSURE_OTA_OPEN);
        } else {
            ui.label("OTA đã đóng (đóng LAN sẽ đóng luôn OTA).");
        }
        ui.label(texts::CLOSE_LAN_KEEPS_PAIRING);
        let remaining = self.controller.pairing_mode_remaining_ms();
        if remaining > 0 {
            ui.label(format!("Chế độ ghép đôi còn {} giây.", remaining / 1000));
        }
        ui.horizontal(|ui| {
            if ui.button("Khởi động hub").clicked() {
                self.send(FriendsCommand::StartHub);
            }
            if ui.button("Dừng hub").clicked() {
                self.send(FriendsCommand::StopHub);
            }
            if ui.button("Bật chế độ ghép đôi (5 phút)").clicked() {
                self.send(FriendsCommand::EnablePairing);
            }
            if ui.button("Tắt LAN (giữ loopback)").clicked() {
                let cut = self.hub.close_lan();
                self.status = format!("Đã tắt LAN, cắt {} session.", cut.len());
            }
            if ui.button("Bật lại LAN").clicked() {
                self.hub.open_lan();
                self.status = "Đã bật lại LAN (OTA cũng mở lại).".to_string();
            }
            if ui.button("Ngắt tất cả driver ngay").clicked() {
                let killed = self.hub.kill_all_drivers();
                self.status = format!("Đã ngắt {} driver.", killed.len());
            }
        });
        ui.separator();
        ui.heading("Phiên đang kết nối");
        let sessions = self.hub.sessions();
        if sessions.is_empty() {
            ui.label("Không có phiên nào.");
        }
        for session in sessions {
            ui.label(format!(
                "{} ({:.8}) — quyền {} — {} — {}",
                session.peer_name,
                session.peer_device_id,
                session.trust.as_str(),
                session.peer_addr,
                if session.is_driver {
                    "đang điều khiển"
                } else {
                    "không điều khiển"
                }
            ));
        }
        if let Some(indicator) = self.hub.driver_active() {
            ui.colored_label(
                egui::Color32::from_rgb(0xDC, 0x26, 0x26),
                format!(
                    "Driver đang hoạt động: {} ({:.8}) — bấm \"Ngắt tất cả driver ngay\" để dừng.",
                    indicator.peer_name, indicator.peer_device_id
                ),
            );
        }
        ui.separator();
        ui.heading("Thẻ phê duyệt đang chờ peer quyết định");
        // F7-13: each card names BOTH sides — the hub that
        // created it (origin) and the device that must answer
        // — plus a live countdown instead of clock times only.
        let cards = self.relay.pending_cards_detailed();
        if cards.is_empty() {
            ui.label("Không có thẻ nào đang chờ.");
        }
        let now = Clock::system().now_ms();
        for (card, target) in cards {
            let remaining = card.expires_ms.saturating_sub(now) / 1000;
            let target_name = self
                .registry
                .get_peer(&target)
                .ok()
                .flatten()
                .map(|p| p.name)
                .unwrap_or_else(|| target.clone());
            ui.separator();
            ui.label(format!(
                "{} — hub gửi: {} ({:.8}) — agent {} — yêu cầu {}",
                card.action, card.device_name, card.device_id, card.agent_id, card.request_id
            ));
            ui.label(format!(
                "Thiết bị trả lời: {target_name} ({:.8}) — còn lại {remaining} giây",
                target
            ));
            ui.label(format!(
                "Chi tiết: {} — tạo lúc {}, hết hạn {}",
                card.detail,
                crate::audit_view::format_time_ms(card.created_ms),
                crate::audit_view::format_time_ms(card.expires_ms)
            ));
        }
    }

    fn show_handoff(&mut self, ui: &mut egui::Ui) {
        ui.label("Bàn giao một task cho peer: peer giữ lease trên 1 actuator và tiếp tục từ bước tiếp theo. Lease KHÔNG sống qua restart.");
        ui.horizontal(|ui| {
            ui.label("Thiết bị nhận (device id):");
            ui.text_edit_singleline(&mut self.handoff_to);
        });
        ui.horizontal(|ui| {
            ui.label("Địa chỉ hub của peer:");
            ui.text_edit_singleline(&mut self.handoff_addr);
        });
        ui.horizontal(|ui| {
            ui.label("Task:");
            ui.text_edit_singleline(&mut self.handoff_task);
        });
        ui.horizontal(|ui| {
            ui.label("Bước tiếp theo:");
            ui.text_edit_singleline(&mut self.handoff_next);
        });
        if ui.button("Gửi handoff").clicked() {
            self.send(FriendsCommand::HandoffOffer {
                to: self.handoff_to.clone(),
                addr: self.handoff_addr.clone(),
                task: self.handoff_task.clone(),
                next: self.handoff_next.clone(),
            });
            self.status = "Đang gửi handoff — chờ peer chấp nhận.".to_string();
        }
        if let Some(lease) = self.handoff.lease_for(HANDOFF_ACTUATOR) {
            ui.separator();
            // F7-04: the holder is the RECEIVING device (this
            // body, when it accepted) — and the panel shows a
            // live countdown, not just the expiry clock time.
            let remaining = lease
                .expires_ms
                .saturating_sub(self.hub.now_ms())
                / 1000;
            ui.label(format!(
                "Lease đang giữ: actuator {} — holder là máy nhận ({:.8}) — còn lại {remaining} giây (hết hạn {})",
                lease.actuator,
                lease.holder_device_id,
                crate::audit_view::format_time_ms(lease.expires_ms)
            ));
        } else if self.status.starts_with("Đang gửi handoff") {
            // F7-24: no answer yet and no lease held by either
            // side — the state is genuinely unknown, say so.
            ui.label(format!(
                "Trạng thái handoff: {} — chưa có trả lời từ peer.",
                texts::HANDOFF_STATE_UNKNOWN
            ));
        }
        // Pending handoffs waiting for THIS user's decision
        // (§7.3): task, done steps, next step, references, and
        // the two buttons. Nothing runs before the user
        // decides.
        let pending = self.hub.pending_handoffs();
        if !pending.is_empty() {
            ui.separator();
            ui.heading("Handoff đang chờ bạn quyết định");
            for item in &pending {
                let package = &item.package;
                ui.label(format!("Task: {}", package.task_text));
                ui.label(format!(
                    "Từ: {:.8} — còn lại {} giây",
                    package.from_device_id,
                    item.remaining_ms(self.hub.now_ms()) / 1000
                ));
                if !package.done_steps.is_empty() {
                    ui.label(format!("Đã xong: {}", package.done_steps.join(" · ")));
                }
                ui.label(format!("Bước tiếp theo: {}", package.next_step));
                if !package.file_refs.is_empty() {
                    ui.label(format!("File: {}", package.file_refs.join(", ")));
                }
                if !package.tab_refs.is_empty() {
                    ui.label(format!("Tab: {}", package.tab_refs.join(", ")));
                }
                if !package.notebook_context.is_empty() {
                    let excerpt: String = package.notebook_context.chars().take(200).collect();
                    ui.label(format!("Notebook (shared): {excerpt}"));
                }
                ui.horizontal(|ui| {
                    if ui.button("Tiếp tục ở đây").clicked() {
                        self.send(FriendsCommand::HandoffDecide {
                            package_id: package.package_id.clone(),
                            accept: true,
                        });
                        self.status =
                            "Đang nhận handoff — tạo lượt mới từ bước tiếp theo.".to_string();
                    }
                    if ui.button("Từ chối").clicked() {
                        self.send(FriendsCommand::HandoffDecide {
                            package_id: package.package_id.clone(),
                            accept: false,
                        });
                        self.status = "Đang từ chối handoff — lease trả về peer.".to_string();
                    }
                });
                ui.separator();
            }
        }
        if let Some((_, task)) = &self.accepted_handoff {
            ui.separator();
            ui.label(format!("Đang tiếp tục handoff: {task}"));
            if let Some(reply) = &self.handoff_turn_reply {
                ui.label(format!("Lượt mới (não): {reply}"));
            } else {
                ui.label("Lượt mới đang chạy qua não…");
            }
        }
    }

    fn show_esp32(&mut self, ui: &mut egui::Ui) {
        let any_peer = !self.registry.list_peers().unwrap_or_default().is_empty();
        let lab = self.hub.lab_mode();
        ui.heading("ESP32 — hai đường, một não duy nhất");
        ui.label(format!(
            "XiaoZhi: {}",
            esp32_status_label(Esp32Path::XiaoZhi, lab, any_peer)
        ));
        ui.label(format!(
            "Firmware v2: {}",
            esp32_status_label(Esp32Path::V2, lab, any_peer)
        ));
        ui.label(format!(
            "Cổng: xiaozhi {} · ota {} (OTA chỉ trả URL an toàn wss/8883/https).",
            self.hub.xiaozhi_port(),
            self.hub.ota_port()
        ));
        ui.label(texts::ESP32_LABEL_VOICE_UNSUPPORTED);
        ui.label(texts::VOICE_LIMIT);
        ui.separator();
        ui.label("Fake device trên host (kiểm thử): chạy `mimint-cli friends fake-esp32 --path xiaozhi|v2 --addr 127.0.0.1:<cổng> --uuid <uuid> --text <câu>` — cùng một não Ollama qwen2.5:3b trả lời cả hai đường.");
    }
}
