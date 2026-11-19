//! The friends hub (Phase 7, §5): the authenticated
//! listener for peers, the driver allow-list, the remote
//! driver indicator + kill button, and the exposure state.
//!
//! Rules enforced here (server-side, deny-by-default):
//! * Loopback by default; LAN only with an explicit opt-in
//!   (`[hub] listen_lan = true`). No env var, no hidden CLI
//!   flag enables LAN.
//! * No unauthenticated control: the ONLY pre-auth message is
//!   `pair_request` (pairing mode only, rate-limited) — and
//!   the OTA XiaoZhi endpoint (§8.2). A raw TCP connection
//!   sending a driver method gets nothing.
//! * The driver-token path is REMOVED (§5.2): no token is
//!   read, no token config exists. A request carrying a
//!   token-style credential is refused like any unauthenticated
//!   request.
//! * Max 4 friends sessions + max 8 pending handshakes.
//! * Driver methods go through the [`DriverBackend`] — the
//!   production implementation drives the existing
//!   `BodyActor`; there is no second path to the body.
//! * A peer can never create an approval card: an inbound
//!   `approval_card` closes the session + audits.
//!
//! The hub runs on its own threads, never on the UI thread.

use crate::audit::AuditLog;
use crate::error::MimintError;
use crate::friends::approval::{ApprovalDecision, ApprovalRelay};
use crate::friends::handoff::{
    HandoffAccept, HandoffComplete, HandoffManager, HandoffOffer, HandoffReject,
};
use crate::friends::identity::Identity;
use crate::friends::ota::{handle_ota_connection, OtaServer};
use crate::friends::pairing::{run_ceremony_listener, PairingController};
use crate::friends::registry::{DeviceRegistry, Trust};
use crate::friends::session::{Session, SessionConfig};
use crate::friends::sync::{run_sync_exchange, SyncEngine, SyncKinds};
use crate::friends::texts;
use crate::friends::util::Clock;
use crate::friends::wire::{kind_of, read_frame, version_of, MessageKind, PROTOCOL_VERSION};
use crate::friends::ws::WsConnection;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Friends port (§2.1).
pub const FRIENDS_PORT: u16 = 19120;
/// XiaoZhi WebSocket port (§8.2).
pub const XIAOZHI_WS_PORT: u16 = 19121;
/// OTA HTTP port (§8.2).
pub const OTA_PORT: u16 = 19122;
/// Max simultaneous friends sessions (§5.4).
pub const HUB_MAX_SESSIONS: usize = 4;
/// Max TCP connections waiting for a handshake (§5.4).
pub const HUB_MAX_PENDING_HANDSHAKES: usize = 8;

/// Read-only screen methods: owner AND guest (§5.3).
pub const DRIVER_READ_METHODS: &[&str] = &[
    "screenshot",
    "display_size",
    "focused_window",
    "permissions",
];
/// Input methods: owner only (§5.3).
pub const DRIVER_INPUT_METHODS: &[&str] = &[
    "click",
    "move",
    "type_text",
    "key",
    "scroll",
    "drag",
    "double_click",
    "right_click",
    "hold_key",
    "stop",
];

/// Hub configuration (from `[hub]`, §5.1).
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub listen_lan: bool,
    /// Lab mode for the XiaoZhi OTA path (`[hub.xiaozhi]
    /// lab_mode`, user-enabled only). When false, OTA only
    /// ever returns `wss://` URLs.
    pub lab_mode: bool,
    pub friends_port: u16,
    pub xiaozhi_port: u16,
    pub ota_port: u16,
    pub max_sessions: usize,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            listen_lan: false,
            lab_mode: false,
            friends_port: FRIENDS_PORT,
            xiaozhi_port: XIAOZHI_WS_PORT,
            ota_port: OTA_PORT,
            max_sessions: HUB_MAX_SESSIONS,
        }
    }
}

/// The 5 exposure states (§5.1), highest first. The UI shows
/// the highest that applies plus the others as secondary
/// states — never a vague "Hub đang mở".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubExposure {
    Loopback,
    LanOn,
    PairingMode,
    DriverConnected,
    LanUnpaired,
}

impl HubExposure {
    pub fn as_text(self) -> &'static str {
        match self {
            HubExposure::Loopback => texts::HUB_EXPOSURE_LOOPBACK,
            HubExposure::LanOn => texts::HUB_EXPOSURE_LAN_ON,
            HubExposure::PairingMode => texts::HUB_EXPOSURE_PAIRING_MODE,
            HubExposure::DriverConnected => texts::HUB_EXPOSURE_DRIVER_CONNECTED,
            HubExposure::LanUnpaired => texts::HUB_EXPOSURE_LAN_UNPAIRED,
        }
    }
}

/// A driver request from a peer (kind `driver_request`,
/// D-BUILD-1: §2.5 names no driver kind; this is the one the
/// design's §5.3/§5.5 require).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverRequest {
    pub v: u32,
    pub kind: String,
    pub method: String,
    pub params: serde_json::Value,
    pub request_id: String,
}

/// A driver response (kind `driver_response`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverResponse {
    pub v: u32,
    pub kind: String,
    pub request_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `forbidden_role` / `unknown_method` / `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// The driver backend: the one path from a remote method to
/// the body. The production implementation drives the
/// existing `BodyActor` (mimint-control); tests use a mock.
pub trait DriverBackend: Send + Sync {
    /// Execute one validated method. Read methods return a
    /// JSON value; input methods return an outcome object.
    fn execute(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, MimintError>;
    fn name(&self) -> &str;
}

/// One live session, for the indicator + kill button.
#[derive(Debug, Clone)]
pub struct HubSessionInfo {
    pub peer_device_id: String,
    pub peer_name: String,
    pub trust: Trust,
    pub connected_ms: u64,
    pub last_action_ms: u64,
    pub is_driver: bool,
    /// The peer's socket address (close-LAN cuts LAN
    /// sessions and keeps loopback ones).
    pub peer_addr: std::net::SocketAddr,
}

/// The remote-driver indicator (§5.3): a peer driving this
/// body right now.
#[derive(Debug, Clone)]
pub struct DriverIndicator {
    pub peer_device_id: String,
    pub peer_name: String,
    pub trust: Trust,
    pub last_action_ms: u64,
}

/// Check one driver method against the role allow-list.
/// Returns Ok when allowed, Err(code) otherwise.
pub fn check_driver_method(trust: Trust, method: &str) -> Result<(), &'static str> {
    if DRIVER_READ_METHODS.contains(&method) {
        return Ok(());
    }
    if DRIVER_INPUT_METHODS.contains(&method) {
        return match trust {
            Trust::Owner => Ok(()),
            Trust::Guest => Err("forbidden_role"),
        };
    }
    // Hub management methods (revoke, rotate, config) are
    // local-UI only; unknown methods are unknown.
    Err("unknown_method")
}

/// The app-installed WebSocket turn handler: receives the
/// hub, the accepted connection, and the device's UUID.
pub type WsTurnHandler = Arc<dyn Fn(&Arc<Hub>, WsConnection, String) + Send + Sync>;

/// The hub: listeners + session table + shared friends state.
pub struct Hub {
    config: HubConfig,
    identity: Arc<Identity>,
    registry: Arc<DeviceRegistry>,
    controller: Arc<PairingController>,
    relay: Arc<ApprovalRelay>,
    handoff: Arc<HandoffManager>,
    sync_engine: Arc<SyncEngine>,
    ota: Arc<OtaServer>,
    backend: Arc<dyn DriverBackend>,
    audit: Option<Arc<AuditLog>>,
    clock: Clock,
    session_config: SessionConfig,
    /// Live sessions: peer device id → info + a stream clone
    /// for the kill button.
    sessions: Mutex<HashMap<String, (HubSessionInfo, TcpStream)>>,
    pending_handshakes: AtomicUsize,
    stop_flag: AtomicBool,
    lan_listener_on: AtomicBool,
    /// Was a driver action seen in the last 60s?
    driver_active: Mutex<Option<DriverIndicator>>,
    /// The WS turn handler, installed by the app context (it
    /// owns the brain closure).
    ws_handler_slot: Mutex<Option<WsTurnHandler>>,
    /// The pairing-ceremony UI (GUI prompts / CLI prompts),
    /// supplied by the app: the hub never invents ceremony
    /// decisions.
    ceremony_ui: Arc<dyn crate::friends::pairing::ListenerUi>,
    /// The brain closure for v2 text turns (the same brain
    /// as the XiaoZhi WS path — one brain, §8.5). Installed
    /// by the app context alongside the WS handler.
    v2_brain_turn: Mutex<Option<BrainTurn>>,
    /// Approval cards already delivered to a peer (by
    /// request id): a card is sent once, never re-sent on
    /// every ping.
    cards_sent: Mutex<std::collections::HashSet<String>>,
    /// The address OTA responses advertise (the hub's LAN IP
    /// when LAN is on, else loopback), set at `start()`.
    advertise_host: Mutex<String>,
}

impl Hub {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: HubConfig,
        identity: Arc<Identity>,
        registry: Arc<DeviceRegistry>,
        controller: Arc<PairingController>,
        relay: Arc<ApprovalRelay>,
        handoff: Arc<HandoffManager>,
        sync_engine: Arc<SyncEngine>,
        ota: Arc<OtaServer>,
        backend: Arc<dyn DriverBackend>,
        audit: Option<Arc<AuditLog>>,
        clock: Clock,
        ceremony_ui: Arc<dyn crate::friends::pairing::ListenerUi>,
    ) -> Self {
        Self {
            config,
            identity,
            registry,
            controller,
            relay,
            handoff,
            sync_engine,
            ota,
            backend,
            audit,
            clock,
            session_config: SessionConfig::default(),
            sessions: Mutex::new(HashMap::new()),
            pending_handshakes: AtomicUsize::new(0),
            stop_flag: AtomicBool::new(false),
            lan_listener_on: AtomicBool::new(false),
            driver_active: Mutex::new(None),
            ws_handler_slot: Mutex::new(None),
            ceremony_ui,
            v2_brain_turn: Mutex::new(None),
            cards_sent: Mutex::new(std::collections::HashSet::new()),
            advertise_host: Mutex::new("127.0.0.1".to_string()),
        }
    }

    fn audit(&self, kind: &str, detail: serde_json::Value) {
        if let Some(log) = &self.audit {
            let _ = log.log("friends-hub", kind, Some("friends"), None, detail);
        }
    }

    /// The current exposure state (highest applies).
    /// The friends port this hub listens on.
    pub fn friends_port(&self) -> u16 {
        self.config.friends_port
    }

    /// The XiaoZhi WS port this hub listens on.
    pub fn xiaozhi_port(&self) -> u16 {
        self.config.xiaozhi_port
    }

    /// The hub's OTA server (same instance the WS listener
    /// consumes tokens from — tests must issue tokens here).
    pub fn ota_server(&self) -> &Arc<OtaServer> {
        &self.ota
    }

    /// The OTA port this hub listens on.
    pub fn ota_port(&self) -> u16 {
        self.config.ota_port
    }

    /// Whether the hub runs in lab mode (plain `ws://` on
    /// loopback/tailnet only, with a continuous warning).
    pub fn lab_mode(&self) -> bool {
        self.config.lab_mode
    }

    pub fn exposure_state(&self) -> HubExposure {
        if self.controller.is_pairing_mode() {
            return HubExposure::PairingMode;
        }
        if self.driver_active().is_some() {
            return HubExposure::DriverConnected;
        }
        if self.config.listen_lan && self.lan_listener_on.load(Ordering::SeqCst) {
            let paired = self
                .registry
                .list_peers()
                .map(|peers| peers.iter().any(|p| !p.revoked))
                .unwrap_or(false);
            return if paired {
                HubExposure::LanOn
            } else {
                HubExposure::LanUnpaired
            };
        }
        HubExposure::Loopback
    }

    /// Live sessions, for the UI + close-LAN confirmation.
    pub fn sessions(&self) -> Vec<HubSessionInfo> {
        let sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        sessions.values().map(|(info, _)| info.clone()).collect()
    }

    /// The remote-driver indicator, when a peer drove this
    /// body in the last 60 seconds.
    pub fn driver_active(&self) -> Option<DriverIndicator> {
        let now = self.clock.now_ms();
        let mut slot = self.driver_active.lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_ref() {
            Some(indicator) if now.saturating_sub(indicator.last_action_ms) <= 60_000 => {
                Some(indicator.clone())
            }
            _ => {
                *slot = None;
                None
            }
        }
    }

    /// Kill all remote driver sessions NOW (the kill button,
    /// §5.3). Pairing is untouched; peers reconnect by
    /// opening a new session. Returns the closed peer ids.
    pub fn kill_all_drivers(&self) -> Vec<String> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let ids: Vec<String> = sessions.keys().cloned().collect();
        for (_info, stream) in sessions.values() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        sessions.clear();
        let mut slot = self.driver_active.lock().unwrap_or_else(|p| p.into_inner());
        *slot = None;
        self.audit("hub_drivers_killed", serde_json::json!({"peers": ids}));
        ids
    }

    /// Close the LAN listener (the close-LAN flow, §5.5):
    /// the LAN listener stops, LAN sessions are cut,
    /// loopback sessions stay, pairing in the registry is
    /// untouched, and the hub falls back to loopback.
    /// Returns the closed session peer ids.
    pub fn close_lan(&self) -> Vec<String> {
        self.lan_listener_on.store(false, Ordering::SeqCst);
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let mut ids = Vec::new();
        let keys: Vec<String> = sessions.keys().cloned().collect();
        for key in keys {
            let is_lan = sessions
                .get(&key)
                .is_some_and(|(info, _)| !info.peer_addr.ip().is_loopback());
            if is_lan {
                if let Some((_info, stream)) = sessions.remove(&key) {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    ids.push(key);
                }
            }
        }
        drop(sessions);
        let mut slot = self.driver_active.lock().unwrap_or_else(|p| p.into_inner());
        if ids.iter().any(|id| {
            slot.as_ref()
                .is_some_and(|indicator| &indicator.peer_device_id == id)
        }) {
            *slot = None;
        }
        self.audit(
            "hub_lan_closed",
            serde_json::json!({"closed_sessions": ids}),
        );
        ids
    }

    /// Start the listeners (friends + XiaoZhi WS + OTA) on
    /// background threads. Returns immediately; the hub runs
    /// until [`Hub::stop`] is called.
    pub fn start(self: &Arc<Self>) -> Result<(), MimintError> {
        let friends_addr: IpAddr = if self.config.listen_lan {
            "0.0.0.0".parse().unwrap()
        } else {
            "127.0.0.1".parse().unwrap()
        };
        let friends_listener = TcpListener::bind((friends_addr, self.config.friends_port))
            .map_err(|err| {
                MimintError::Io(format!(
                    "cannot bind friends port {}: {err}",
                    self.config.friends_port
                ))
            })?;
        friends_listener
            .set_nonblocking(true)
            .map_err(|err| MimintError::Io(err.to_string()))?;
        if self.config.listen_lan {
            self.lan_listener_on.store(true, Ordering::SeqCst);
            if let Some(ip) = lan_ip() {
                let mut host = self
                    .advertise_host
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                *host = ip.to_string();
            }
        }
        let hub = Arc::clone(self);
        std::thread::spawn(move || hub.accept_loop(friends_listener, false));
        let ws_listener =
            TcpListener::bind((friends_addr, self.config.xiaozhi_port)).map_err(|err| {
                MimintError::Io(format!(
                    "cannot bind xiaozhi port {}: {err}",
                    self.config.xiaozhi_port
                ))
            })?;
        ws_listener
            .set_nonblocking(true)
            .map_err(|err| MimintError::Io(err.to_string()))?;
        let hub = Arc::clone(self);
        std::thread::spawn(move || hub.accept_loop(ws_listener, true));
        let ota_listener =
            TcpListener::bind((friends_addr, self.config.ota_port)).map_err(|err| {
                MimintError::Io(format!(
                    "cannot bind ota port {}: {err}",
                    self.config.ota_port
                ))
            })?;
        ota_listener
            .set_nonblocking(true)
            .map_err(|err| MimintError::Io(err.to_string()))?;
        let hub = Arc::clone(self);
        std::thread::spawn(move || hub.ota_accept_loop(ota_listener));
        Ok(())
    }

    /// Stop the hub: listeners stop accepting, live sessions
    /// are cut. Pairing and state are untouched.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.kill_all_drivers();
    }

    fn accept_loop(self: &Arc<Self>, listener: TcpListener, is_ws: bool) {
        loop {
            if self.stop_flag.load(Ordering::SeqCst) {
                return;
            }
            if is_ws && !self.config.listen_lan {
                // The XiaoZhi WS listener only serves while LAN
                // is on (loopback-only hubs do not expose it).
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            match listener.accept() {
                Ok((stream, addr)) => {
                    // After `close_lan` the friends listener
                    // (bound to 0.0.0.0 while LAN was on) must
                    // refuse LAN connections, matching the
                    // exposure state — otherwise the UI would
                    // report loopback-only while LAN still
                    // connects (found while writing
                    // `hub_close_lan_lists_sessions_and_keeps_pairing`).
                    // Loopback is always accepted.
                    if !is_ws
                        && !self.lan_listener_on.load(Ordering::SeqCst)
                        && !addr.ip().is_loopback()
                    {
                        drop(stream);
                        continue;
                    }
                    if self.pending_handshakes.load(Ordering::SeqCst) >= HUB_MAX_PENDING_HANDSHAKES
                    {
                        drop(stream);
                        continue;
                    }
                    self.pending_handshakes.fetch_add(1, Ordering::SeqCst);
                    let hub = Arc::clone(self);
                    if is_ws {
                        std::thread::spawn(move || {
                            hub.serve_ws_connection(stream, addr);
                            hub.pending_handshakes.fetch_sub(1, Ordering::SeqCst);
                        });
                    } else {
                        std::thread::spawn(move || {
                            hub.serve_friends_connection(stream, addr);
                            hub.pending_handshakes.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    fn ota_accept_loop(self: &Arc<Self>, listener: TcpListener) {
        loop {
            if self.stop_flag.load(Ordering::SeqCst) {
                return;
            }
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let hub = Arc::clone(self);
                    std::thread::spawn(move || {
                        let host = hub
                            .advertise_host
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .clone();
                        let hub_addr = format!("{host}:{}", hub.config.xiaozhi_port);
                        handle_ota_connection(stream, &hub.ota, &hub_addr, hub.config.lab_mode);
                    });
                }
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// Serve one friends TCP connection: first frame decides
    /// the path (pairing ceremony vs session handshake).
    fn serve_friends_connection(
        self: &Arc<Self>,
        mut stream: TcpStream,
        addr: std::net::SocketAddr,
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
        let frame = match read_frame(&mut stream) {
            Ok(frame) => frame,
            Err(_) => return,
        };
        if version_of(&frame.json).unwrap_or(0) != PROTOCOL_VERSION {
            return; // Downgrade attempt: silent close.
        }
        let kind = match kind_of(&frame.json) {
            Ok(kind) => kind,
            Err(_) => return,
        };
        if kind == MessageKind::PairRequest {
            if !frame.payload.is_empty() {
                return;
            }
            let ui = Arc::clone(&self.ceremony_ui);
            let result = run_ceremony_listener(
                &mut stream,
                &addr.to_string(),
                frame.json,
                &self.identity,
                &self.registry,
                &self.controller,
                ui.as_ref(),
                crate::friends::pairing::SasFlow::Compare,
                &self.clock,
                self.audit.clone(),
            );
            let _ = result;
            return;
        }
        if kind != MessageKind::Hello {
            // No unauthenticated control: anything else before
            // a HELLO is refused, silently.
            self.audit(
                "hub_unauthenticated_refused",
                serde_json::json!({"kind": kind.as_str(), "peer_addr": addr.to_string()}),
            );
            return;
        }
        // The HELLO was consumed by the dispatcher; the
        // handshake replays it from the frame bytes.
        let _ = self.serve_session_from_hello(stream, frame.json, addr);
    }

    /// Continue a connection whose HELLO frame was already
    /// read: replay the hello into the handshake.
    fn serve_session_from_hello(
        self: &Arc<Self>,
        stream: TcpStream,
        hello_json: Vec<u8>,
        addr: std::net::SocketAddr,
    ) -> Result<(), MimintError> {
        let session = crate::friends::session::handshake_listener_with_hello(
            stream,
            hello_json,
            &self.identity,
            &self.registry,
            &self.session_config,
            &self.clock,
            self.audit.clone(),
        )?;
        let peer_device_id = session.peer_device_id().to_string();
        let peer = self
            .registry
            .get_peer(&peer_device_id)?
            .ok_or_else(|| MimintError::Config("peer vanished after handshake".to_string()))?;
        if peer.revoked {
            return Err(MimintError::Config("revoked peer".to_string()));
        }
        {
            let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if sessions.len() >= self.config.max_sessions {
                self.audit(
                    "hub_session_limit",
                    serde_json::json!({"peer_device_id": peer_device_id}),
                );
                return Err(MimintError::Config("hub session limit reached".to_string()));
            }
            let info = HubSessionInfo {
                peer_device_id: peer_device_id.clone(),
                peer_name: peer.name.clone(),
                trust: peer.trust,
                connected_ms: self.clock.now_ms(),
                last_action_ms: 0,
                is_driver: false,
                peer_addr: addr,
            };
            let stream_clone = session
                .stream_clone()
                .map_err(|err| MimintError::Io(err.to_string()))?;
            sessions.insert(peer_device_id.clone(), (info, stream_clone));
        }
        let result = self.session_loop(session, &peer);
        {
            let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            sessions.remove(&peer_device_id);
        }
        result
    }

    /// Send pending approval cards to this peer (§6.2): the
    /// hub pushes each card once, only to an owner peer that
    /// explicitly opted in to peer approval. Called when the
    /// session opens and whenever the peer pings — the session
    /// loop blocks on `recv`, so peer traffic is the wake-up
    /// for cards created after connect (the peer app pings on
    /// connect and periodically; delivery is well inside the
    /// 60s decision deadline either way).
    fn deliver_pending_cards(
        &self,
        session: &mut Session,
        peer: &crate::friends::registry::PeerEntry,
    ) {
        if peer.trust != crate::friends::registry::Trust::Owner {
            return;
        }
        let allowed = self
            .registry
            .get_prefs(&peer.device_id)
            .map(|prefs| prefs.approve_allowed)
            .unwrap_or(false);
        if !allowed {
            return;
        }
        let cards = self.relay.pending_cards_for(&peer.device_id);
        if cards.is_empty() {
            return;
        }
        let mut sent = self.cards_sent.lock().unwrap_or_else(|p| p.into_inner());
        for card in cards {
            if sent.contains(&card.request_id) {
                continue;
            }
            let Ok(bytes) = serde_json::to_vec(&card) else {
                continue;
            };
            if session.send(MessageKind::ApprovalCard, &bytes).is_ok() {
                sent.insert(card.request_id.clone());
                self.audit(
                    "approval_card_sent",
                    serde_json::json!({
                        "peer_device_id": peer.device_id,
                        "request_id": card.request_id,
                    }),
                );
            }
        }
    }

    /// The post-handshake message loop for one session.
    fn session_loop(
        self: &Arc<Self>,
        mut session: Session,
        peer: &crate::friends::registry::PeerEntry,
    ) -> Result<(), MimintError> {
        let peer_device_id = peer.device_id.clone();
        // Cards created before this session opened go out
        // immediately (§6.2).
        self.deliver_pending_cards(&mut session, peer);
        loop {
            if self.stop_flag.load(Ordering::SeqCst) {
                return Ok(());
            }
            let (kind, payload) = match session.recv() {
                Ok(pair) => pair,
                Err(_) => return Ok(()), // Closed / idle timeout.
            };
            match kind {
                MessageKind::Ping => {
                    // A ping is the wake-up for cards created
                    // after this session opened (§6.2).
                    self.deliver_pending_cards(&mut session, peer);
                    let _ = session.send(MessageKind::Pong, &[]);
                }
                MessageKind::SyncOffer => {
                    // Guests never sync (§3.5, T7): trust
                    // boundary, independent of the address
                    // boundary. Deny + audit, close the session.
                    if peer.trust == Trust::Guest {
                        self.audit(
                            "sync_refused_guest",
                            serde_json::json!({"peer_device_id": peer_device_id}),
                        );
                        return Ok(());
                    }
                    let offer: crate::friends::sync::SyncOffer = serde_json::from_slice(&payload)
                        .unwrap_or(crate::friends::sync::SyncOffer {
                            v: PROTOCOL_VERSION,
                            kind: "sync_offer".to_string(),
                            summaries: Vec::new(),
                        });
                    let kinds = SyncKinds::from_prefs(&self.registry.get_prefs(&peer_device_id)?);
                    let _ = run_sync_exchange(
                        &mut session,
                        &self.sync_engine,
                        &peer_device_id,
                        &kinds,
                        Some(offer),
                    );
                }
                MessageKind::DriverRequest => {
                    self.handle_driver_request(&mut session, peer, &payload);
                }
                MessageKind::ApprovalDecision => {
                    // Guests never approve (§3.5, T7).
                    if peer.trust == Trust::Guest {
                        self.audit(
                            "approval_refused_guest",
                            serde_json::json!({"peer_device_id": peer_device_id}),
                        );
                        return Ok(());
                    }
                    // Opt-in (§6.2, check 1): a decision from a
                    // peer that never opted in to peer approval
                    // is refused — the relay alone cannot know
                    // this, because the card was created while
                    // the peer WAS opted in (found while writing
                    // `approve_requires_explicit_peer_opt_in`).
                    let opted_in = self
                        .registry
                        .get_prefs(&peer_device_id)
                        .map(|prefs| prefs.approve_allowed)
                        .unwrap_or(false);
                    if !opted_in {
                        self.audit(
                            "approval_refused_not_opted_in",
                            serde_json::json!({"peer_device_id": peer_device_id}),
                        );
                        return Ok(());
                    }
                    let decision: ApprovalDecision = match serde_json::from_slice(&payload) {
                        Ok(decision) => decision,
                        Err(_) => continue,
                    };
                    let _ = self.relay.apply_decision(&decision, &peer_device_id);
                }
                MessageKind::ApprovalCard => {
                    // A peer can never create a card: close the
                    // session + audit.
                    self.audit(
                        "approval_card_rejected",
                        serde_json::json!({"peer_device_id": peer_device_id}),
                    );
                    return Ok(());
                }
                MessageKind::HandoffOffer => {
                    // Guests never send or receive handoffs
                    // (§3.5, §7).
                    if peer.trust == Trust::Guest {
                        self.audit(
                            "handoff_refused_guest",
                            serde_json::json!({"peer_device_id": peer_device_id}),
                        );
                        return Ok(());
                    }
                    self.handle_handoff_offer(&mut session, peer, &payload);
                }
                MessageKind::HandoffAccept => {
                    if peer.trust == Trust::Guest {
                        self.audit(
                            "handoff_refused_guest",
                            serde_json::json!({"peer_device_id": peer_device_id}),
                        );
                        return Ok(());
                    }
                    if let Ok(accept) = serde_json::from_slice::<HandoffAccept>(&payload) {
                        self.handoff.mark_handed_off(&accept.package_id);
                    }
                }
                MessageKind::HandoffReject => {
                    if let Ok(reject) = serde_json::from_slice::<HandoffReject>(&payload) {
                        self.audit(
                            "handoff_rejected_by_peer",
                            serde_json::json!({
                                "package_id": reject.package_id,
                                "reason": reject.reason,
                            }),
                        );
                    }
                }
                MessageKind::HandoffComplete => {
                    if let Ok(complete) = serde_json::from_slice::<HandoffComplete>(&payload) {
                        // The complete message carries no
                        // actuator: release by lease id, and
                        // return the task to the sender — without
                        // clearing the handed-off mark the sender
                        // could never act on the task again
                        // (found while writing
                        // `handoff_complete_ack_returns_lease`).
                        self.handoff.release_lease_by_id(&complete.lease_id);
                        self.handoff.clear_handed_off_package(&complete.package_id);
                    }
                }
                MessageKind::SyncPush => {
                    // Guests never sync (§3.5, T7).
                    if peer.trust == Trust::Guest {
                        self.audit(
                            "sync_refused_guest",
                            serde_json::json!({"peer_device_id": peer_device_id}),
                        );
                        return Ok(());
                    }
                    // A bare sync_push outside an exchange is
                    // the v2 text turn (the fake/reference v2
                    // client reuses this kind as its text
                    // carrier — see esp32.rs). A real sync push
                    // only travels inside run_sync_exchange.
                    if !self.handle_v2_text_turn(&mut session, peer, &payload) {
                        return Ok(());
                    }
                }
                MessageKind::RevokeNotice => {
                    self.audit(
                        "revoke_notice_received",
                        serde_json::json!({"peer_device_id": peer_device_id}),
                    );
                    return Ok(());
                }
                _ => {
                    // Unknown kinds never reach here (the wire
                    // allow-list rejects them); anything else is
                    // a protocol error: close the session.
                    return Ok(());
                }
            }
        }
    }

    /// Answer one v2 text turn through the shared brain.
    /// Returns false when the session must close (not a text
    /// turn, or no brain installed — fail-closed).
    fn handle_v2_text_turn(
        &self,
        session: &mut Session,
        peer: &crate::friends::registry::PeerEntry,
        payload: &[u8],
    ) -> bool {
        let value: serde_json::Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(_) => {
                self.audit(
                    "unexpected_sync_push",
                    serde_json::json!({"peer_device_id": peer.device_id}),
                );
                return false;
            }
        };
        let text = match value.get("type").and_then(|t| t.as_str()) {
            Some("text") => value
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
            _ => {
                self.audit(
                    "unexpected_sync_push",
                    serde_json::json!({"peer_device_id": peer.device_id}),
                );
                return false;
            }
        };
        let brain = self
            .v2_brain_turn
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(brain) = brain else {
            self.audit(
                "v2_text_no_brain",
                serde_json::json!({"peer_device_id": peer.device_id}),
            );
            return false;
        };
        let output = crate::friends::esp32::handle_esp32_turn(
            |prompt| brain(prompt),
            &crate::friends::esp32::Esp32Input {
                source: crate::friends::esp32::Esp32Path::V2,
                device_uuid: peer.device_id.clone(),
                text,
                audio: Vec::new(),
            },
        );
        let reply = serde_json::json!({"type": "text", "text": output.text});
        session
            .send(MessageKind::SyncPush, &serde_json::to_vec(&reply).unwrap())
            .is_ok()
    }

    fn handle_driver_request(
        &self,
        session: &mut Session,
        peer: &crate::friends::registry::PeerEntry,
        payload: &[u8],
    ) {
        let request: DriverRequest = match serde_json::from_slice(payload) {
            Ok(request) => request,
            Err(_) => return,
        };
        let response = match check_driver_method(peer.trust, &request.method) {
            Err(code) => {
                self.audit(
                    "hub_driver_forbidden",
                    serde_json::json!({
                        "peer_device_id": peer.device_id,
                        "method": request.method,
                        "code": code,
                    }),
                );
                DriverResponse {
                    v: PROTOCOL_VERSION,
                    kind: MessageKind::DriverResponse.as_str().to_string(),
                    request_id: request.request_id.clone(),
                    ok: false,
                    result: None,
                    error: Some(format!(
                        "method '{}' is not allowed for this peer",
                        request.method
                    )),
                    code: Some(code.to_string()),
                }
            }
            Ok(()) => match self.backend.execute(&request.method, &request.params) {
                Ok(result) => {
                    let now = self.clock.now_ms();
                    {
                        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some((info, _)) = sessions.get_mut(&peer.device_id) {
                            info.last_action_ms = now;
                            info.is_driver = true;
                        }
                    }
                    let mut slot = self.driver_active.lock().unwrap_or_else(|p| p.into_inner());
                    *slot = Some(DriverIndicator {
                        peer_device_id: peer.device_id.clone(),
                        peer_name: peer.name.clone(),
                        trust: peer.trust,
                        last_action_ms: now,
                    });
                    DriverResponse {
                        v: PROTOCOL_VERSION,
                        kind: MessageKind::DriverResponse.as_str().to_string(),
                        request_id: request.request_id.clone(),
                        ok: true,
                        result: Some(result),
                        error: None,
                        code: None,
                    }
                }
                Err(err) => DriverResponse {
                    v: PROTOCOL_VERSION,
                    kind: MessageKind::DriverResponse.as_str().to_string(),
                    request_id: request.request_id.clone(),
                    ok: false,
                    result: None,
                    error: Some(err.to_string()),
                    code: Some("error".to_string()),
                },
            },
        };
        let _ = session.send(
            MessageKind::DriverResponse,
            &serde_json::to_vec(&response).unwrap(),
        );
    }

    fn handle_handoff_offer(
        &self,
        session: &mut Session,
        peer: &crate::friends::registry::PeerEntry,
        payload: &[u8],
    ) {
        let offer: HandoffOffer = match serde_json::from_slice(payload) {
            Ok(offer) => offer,
            Err(_) => return,
        };
        let package = &offer.package;
        let sender_pub = match crate::friends::util::hex_decode(&peer.identity_pub_hex) {
            Some(bytes) => bytes,
            None => return,
        };
        let verified = self
            .handoff
            .verify_package(package, &sender_pub, self.identity.device_id());
        if let Err(err) = &verified {
            // §7.3 step 4: a forged package is audited, not
            // just rejected.
            if err.to_string().contains("signature invalid") {
                self.audit(
                    "handoff_forged",
                    serde_json::json!({
                        "peer_device_id": peer.device_id,
                        "package_id": package.package_id,
                    }),
                );
            }
        }
        let accept = match verified {
            Ok(()) => match self.handoff.acquire_lease(package) {
                Ok(lease) => Some(HandoffAccept {
                    v: PROTOCOL_VERSION,
                    kind: MessageKind::HandoffAccept.as_str().to_string(),
                    package_id: package.package_id.clone(),
                    lease_id: lease.lease_id,
                }),
                Err(err) => {
                    let reason = if err.to_string().contains("lease_busy") {
                        "lease_busy"
                    } else {
                        "error"
                    };
                    let _ = session.send(
                        MessageKind::HandoffReject,
                        &serde_json::to_vec(&HandoffReject {
                            v: PROTOCOL_VERSION,
                            kind: MessageKind::HandoffReject.as_str().to_string(),
                            package_id: package.package_id.clone(),
                            reason: reason.to_string(),
                        })
                        .unwrap(),
                    );
                    return;
                }
            },
            Err(err) => {
                let _ = session.send(
                    MessageKind::HandoffReject,
                    &serde_json::to_vec(&HandoffReject {
                        v: PROTOCOL_VERSION,
                        kind: MessageKind::HandoffReject.as_str().to_string(),
                        package_id: package.package_id.clone(),
                        reason: err.to_string(),
                    })
                    .unwrap(),
                );
                return;
            }
        };
        if let Some(accept) = accept {
            let _ = session.send(
                MessageKind::HandoffAccept,
                &serde_json::to_vec(&accept).unwrap(),
            );
        }
    }

    /// Serve one XiaoZhi WS connection: token check, then the
    /// hello + text-turn loop through the single brain adapter
    /// (the brain closure is supplied by the app; here the
    /// loop is driven by the context's brain hook).
    fn serve_ws_connection(self: &Arc<Self>, stream: TcpStream, _addr: std::net::SocketAddr) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let (mut conn, path) = match WsConnection::accept_upgrade(stream, self.audit.clone()) {
            Ok(pair) => pair,
            Err(_) => return,
        };
        let token = crate::friends::ota::extract_token_from_path(&path);
        let uuid = match token.and_then(|t| self.ota.consume_token(&t)) {
            Some(uuid) => uuid,
            None => {
                conn.close(1008, "invalid token");
                return;
            }
        };
        // The WS loop itself lives in the context layer (it
        // owns the brain closure). The hub hands the
        // connection over via the registered WS handler.
        if let Some(handler) = self.ws_handler() {
            handler(self, conn, uuid);
        } else {
            conn.close(1011, "no handler");
        }
    }

    /// The WS turn handler, installed by the app context
    /// (it owns the brain). `None` until the app installs it.
    fn ws_handler(&self) -> Option<WsTurnHandler> {
        self.ws_handler_slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Install the WS turn handler (app context only).
    /// Install the brain closure for v2 text turns. The app
    /// context installs the same closure it gives the XiaoZhi
    /// WS handler, so both ESP32 paths share one brain (§8.5).
    pub fn install_v2_brain_turn(&self, brain_turn: BrainTurn) {
        let mut slot = self.v2_brain_turn.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Some(brain_turn);
    }

    /// Run one driver method through the same gate the
    /// friends session uses (role allow-list + backend). Used
    /// by the XiaoZhi WS path for MCP servo calls (§8.4).
    /// The driver indicator is a friends-session concern and
    /// is not updated here (the device drives its own body).
    /// Execute a device (servo/MCP) method: validated
    /// against the device allow-list (§8.4 — clamping +
    /// never-callable), then run on the backend with the
    /// CLAMPED params. This is a different allow-list from
    /// the desktop driver methods (§5.3); the two must never
    /// be conflated (found while running
    /// `xiaozhi_fake_device_end_to_end`: `servo.move` was
    /// refused as `unknown_method` by the driver list).
    pub fn execute_servo_method(
        &self,
        caller: crate::friends::servo::McpCaller,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, MimintError> {
        let validated = crate::friends::servo::validate_servo_command(
            caller,
            &crate::friends::servo::ServoCommand {
                method: method.to_string(),
                params: params.clone(),
            },
        )?;
        self.backend.execute(&validated.method, &validated.params)
    }

    pub fn execute_driver_method(
        &self,
        trust: crate::friends::registry::Trust,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, MimintError> {
        crate::friends::hub::check_driver_method(trust, method).map_err(|code| {
            MimintError::Config(format!("method '{method}' is not allowed: {code}"))
        })?;
        self.backend.execute(method, params)
    }

    pub fn set_ws_handler(&self, handler: WsTurnHandler) {
        let mut slot = self
            .ws_handler_slot
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *slot = Some(handler);
    }
}

/// One brain closure: text in, reply text out. Shared
/// by the XiaoZhi WS handler and the v2 text path (§8.5).
pub type BrainTurn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// The machine's LAN IP, via the standard UDP-connect trick
/// (no packet is sent). Used for the address OTA responses
/// advertise when LAN is on. `None` when it cannot be
/// determined (loopback fallback).
fn lan_ip() -> Option<IpAddr> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let addr = socket.local_addr().ok()?;
    let ip = addr.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
}
