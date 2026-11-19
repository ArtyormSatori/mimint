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
use mimint_core::friends::handoff::{shared_notebook_context, HandoffOffer};
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
    /// A handoff offer arrived and was auto-accepted after
    /// verification (the human accept/reject gate is UI-layer
    /// work; see the CROSS-CHECK deviation note).
    HandoffReceived {
        package_id: String,
        task: String,
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
            let prefs = ctx.registry.get_prefs(&target).unwrap_or_default();
            let origin = CardOrigin {
                device_id: ctx.identity.device_id().to_string(),
                device_name: format!(
                    "Thiết bị {}",
                    &ctx.identity.device_id()[..8.min(ctx.identity.device_id().len())]
                ),
                agent_id: "agent-001".to_string(),
                request_id: random_hex(16),
                action,
                detail: serde_json::json!({"source": "friends panel test card"}),
            };
            match ctx
                .relay
                .create_card(origin, &target, prefs.approve_allowed)
            {
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
                "default",
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
            let (kind, _payload) = session.recv()?;
            match kind {
                MessageKind::HandoffAccept => {
                    ctx.handoff.mark_handed_off(&package.package_id);
                    let _ = event_tx.send(FriendsEvent::HandoffDone { device_id: to });
                }
                MessageKind::HandoffReject => {
                    let _ = event_tx.send(FriendsEvent::HandoffFailed {
                        error: "peer từ chối handoff".to_string(),
                    });
                }
                _ => {
                    let _ = event_tx.send(FriendsEvent::HandoffFailed {
                        error: format!("peer trả về loại tin không mong đợi: {kind:?}"),
                    });
                }
            }
        }
    }
    Ok(())
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
    card_target: String,
    card_action: String,
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
            card_target: String::new(),
            card_action: "screenshot".to_string(),
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
                    self.status = format!("Ghép đôi thất bại: {error}");
                }
                FriendsEvent::SasShown {
                    sas,
                    fingerprint,
                    device_id,
                    decision_tx,
                } => {
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
                    self.handoff_notice = Some(format!(
                        "Đã bàn giao cho {device_id:.8} — lease đang giữ, task dừng ở phía này."
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
                        "Nhận handoff {package_id:.8}: {task} (đã xác minh chữ ký, lease đã giữ)."
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
                ui.label(format!("quyền: {}", peer.trust.as_str()));
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
                    let now = self.controller_clock_ms();
                    let _ = self.registry.revoke_peer(&peer.device_id, now);
                    self.status = format!("Đã thu hồi {}.", peer.name);
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
        ui.label(texts::PRIVACY_MODE_TEXT);
        if ui.button("Sao chép fingerprint của thiết bị này").clicked() {
            ui.ctx().copy_text(self.identity.fingerprint().to_string());
        }
    }

    fn controller_clock_ms(&self) -> u64 {
        Clock::system().now_ms()
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
            if self.pair_compare {
                ui.label("So sánh mã này với mã trên màn hình thiết bị kia. Chỉ bấm \"khớp\" khi hai mã giống hệt nhau — máy không thể phát hiện bạn xác nhận một mã sai (rủi ro T12).");
            } else {
                ui.label("Đọc mã này cho người ở thiết bị kia gõ vào. Mã không tự khớp — phía kia phải gõ đúng 6 số.");
            }
            let mut decision: Option<InitiatorDecision> = None;
            ui.horizontal(|ui| {
                if ui.button("Mã khớp — xác nhận").clicked() {
                    decision = Some(InitiatorDecision {
                        confirmed: true,
                        trust_granted: if self.pair_guest {
                            Trust::Guest
                        } else {
                            Trust::Owner
                        },
                        fingerprint_confirmed: true,
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
        let cards = self.relay.pending_cards();
        if cards.is_empty() {
            ui.label("Không có thẻ nào đang chờ.");
        }
        for card in cards {
            ui.separator();
            ui.label(format!(
                "{} — từ {} ({:.8}) — agent {} — yêu cầu {}",
                card.action, card.device_name, card.device_id, card.agent_id, card.request_id
            ));
            ui.label(format!(
                "Chi tiết: {} — tạo lúc {}, hết hạn {}",
                card.detail,
                crate::audit_view::format_time_ms(card.created_ms),
                crate::audit_view::format_time_ms(card.expires_ms)
            ));
        }
        ui.separator();
        ui.heading("Thẻ phê duyệt thử (demo)");
        ui.label("Tạo một thẻ phê duyệt cho peer đã bật phê duyệt (tab Thiết bị). Peer quyết định bằng `mimint-cli friends approve-wait --addr <hub này>`. Đường production là brain tạo thẻ trong chính tiến trình này.");
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
        if let Some(lease) = self.handoff.lease_for("default") {
            ui.separator();
            ui.label(format!(
                "Lease đang giữ: actuator {} — holder {:.8} — hết hạn {}",
                lease.actuator,
                lease.holder_device_id,
                crate::audit_view::format_time_ms(lease.expires_ms)
            ));
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
