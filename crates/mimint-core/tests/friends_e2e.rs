//! Phase 7 named integration tests (§1.4/§2.6/§3.8/§4.8/§5.5/
//! §6.4/§7.4/§8.7/§9.4 of `docs/phase7-design.md`).
//!
//! Two of the 100 named tests live in the lib unit tests
//! (`friends_key_derivation_test_vector` in `crypto.rs`,
//! `device_key_file_permissions` in `identity.rs`); the other
//! 98 live here. Every test uses real keys, real TCP on
//! loopback, and two real data directories — no mocks of the
//! crypto or the wire, except the mock driver backend (which
//! stands in for the body actor) and scripted ceremony UIs
//! (which stand in for the user at the keyboard).

use mimint_core::friends::address::{
    check_resolved_addresses, is_accepted_address, resolve_and_check_all,
};
use mimint_core::friends::approval::{
    ApprovalCard, ApprovalDecision, ApprovalRelay, CardOrigin, DecisionOutcome,
};
use mimint_core::friends::context::{AutoInitiatorUi, AutoListenerUi, ConfigFileProfileSink};
use mimint_core::friends::crypto::{agree_and_derive, fresh_nonce16, generate_ephemeral};
use mimint_core::friends::esp32::{FakeV2Device, FakeXiaoZhiDevice};
use mimint_core::friends::handoff::{
    shared_notebook_context, HandoffAccept, HandoffComplete, HandoffManager, HandoffOffer,
    HandoffPackage, HandoffReject, HANDOFF_ACTUATOR,
};
use mimint_core::friends::hub::{
    check_driver_method, DriverBackend, DriverRequest, DriverResponse, HandoffTurnStatus, Hub,
    HubConfig, HubExposure,
};
use mimint_core::friends::identity::{DeviceKeyStore, Identity};
use mimint_core::friends::ota::{build_websocket_url, OtaServer};
use mimint_core::friends::pairing::{
    run_ceremony_initiator, run_ceremony_listener, InitiatorDecision, InitiatorDecisionCtx,
    InitiatorUi, ListenerDecision, ListenerDecisionCtx, ListenerUi, PairingController, SasFlow,
    SasOutcome,
};
use mimint_core::friends::records::{RecordKind, SyncRecord};
use mimint_core::friends::registry::{PeerEntry, PeerPrefs, Trust, VersionId};
use mimint_core::friends::session::{
    handshake_initiator, handshake_listener, Session, SessionConfig, SessionCrypto,
};
use mimint_core::friends::sync::{
    run_sync_over_session, ApplyOutcome, ProfileSyncSink, ResolutionChoice, SyncEngine, SyncKinds,
    SyncPush, SyncSources,
};
use mimint_core::friends::todos::{Todo, TodoPlanStore};
use mimint_core::friends::util::{hex_encode, Clock};
use mimint_core::friends::wire::{kind_of, read_frame, write_frame, MessageKind, PROTOCOL_VERSION};
use mimint_core::friends::ws::{WsConnection, WsFrame};
use mimint_core::MimintError;
use mimint_core::{Notebook, SkillDef, SkillStatus, SkillStore, SoulStore};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ----------------------------------------------------------------
// Harness
// ----------------------------------------------------------------

static NODE_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// One body under test: its own data dir, identity, registry,
/// sync engine, approval relay, and handoff manager.
struct Node {
    dir: PathBuf,
    identity: Arc<Identity>,
    registry: Arc<mimint_core::friends::registry::DeviceRegistry>,
    controller: Arc<PairingController>,
    relay: Arc<ApprovalRelay>,
    handoff: Arc<HandoffManager>,
    engine: Arc<SyncEngine>,
    notebook: Arc<Notebook>,
    config_path: PathBuf,
    clock: Clock,
    audit: Arc<mimint_core::AuditLog>,
}

impl Node {
    fn new(tag: &str) -> Self {
        let n = NODE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mimint-p7-{}-{}-{}", tag, std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let clock = Clock::system();
        let identity = Arc::new(DeviceKeyStore::in_data_dir(&dir).ensure().unwrap());
        let registry = Arc::new(
            mimint_core::friends::registry::DeviceRegistry::open(dir.join("peers.db")).unwrap(),
        );
        let controller = Arc::new(PairingController::new(clock.clone()));
        let audit = Arc::new(mimint_core::AuditLog::open(dir.join("audit.jsonl")).unwrap());
        let relay_registry = Arc::clone(&registry);
        let relay = Arc::new(ApprovalRelay::new_with_lookup(
            clock.clone(),
            Some(Arc::clone(&audit)),
            Some(Arc::new(move |id: &str| {
                relay_registry
                    .get_peer(id)
                    .ok()
                    .flatten()
                    .map(|peer| peer.name)
            })),
        ));
        let handoff = Arc::new(HandoffManager::new(clock.clone(), None));
        let notebook_path = dir.join("notebook.db");
        let notebook = Arc::new(Notebook::open(&notebook_path).unwrap());
        let config_path = dir.join("config.toml");
        let sources = SyncSources {
            soul: SoulStore::new(&dir),
            skills: SkillStore::new(dir.join("skills")),
            notebook: Arc::clone(&notebook),
            todos: TodoPlanStore::open(&notebook_path).unwrap(),
            profiles: Arc::new(ConfigFileProfileSink::new(&config_path)),
        };
        let engine = Arc::new(
            SyncEngine::new(
                Arc::clone(&registry),
                sources,
                identity.device_id().to_string(),
                // Fix pass 7: the engine's audit events
                // (sync_successor_rejected, sync_record_rejected,
                // sync_soul_pending_approval, ...) must be
                // observable in the node's audit log.
                Some(Arc::clone(&audit)),
                clock.clone(),
            )
            .unwrap(),
        );
        Self {
            dir,
            identity,
            registry,
            controller,
            relay,
            handoff,
            engine,
            notebook,
            config_path,
            clock,
            audit,
        }
    }

    fn device_id(&self) -> String {
        self.identity.device_id().to_string()
    }

    fn notebook_path(&self) -> PathBuf {
        self.dir.join("notebook.db")
    }

    fn skills(&self) -> SkillStore {
        SkillStore::new(self.dir.join("skills"))
    }

    fn todos(&self) -> TodoPlanStore {
        TodoPlanStore::open(self.notebook_path()).unwrap()
    }

    /// Build (but do not start) a hub on this node's state.
    fn hub(
        &self,
        config: HubConfig,
        backend: Arc<dyn DriverBackend>,
        ui: Arc<dyn ListenerUi>,
    ) -> Arc<Hub> {
        Arc::new(Hub::new(
            config,
            Arc::clone(&self.identity),
            Arc::clone(&self.registry),
            Arc::clone(&self.controller),
            Arc::clone(&self.relay),
            Arc::clone(&self.handoff),
            Arc::clone(&self.engine),
            Arc::new(OtaServer::new(
                Arc::clone(&self.registry),
                self.clock.clone(),
                None,
            )),
            backend,
            Some(Arc::clone(&self.audit)),
            self.clock.clone(),
            ui,
        ))
    }

    /// A free loopback port (bind :0, read the port, release).
    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// This machine's LAN address (UDP-connect trick; sends no
    /// packets). Falls back to loopback when offline.
    fn lan_addr() -> std::net::IpAddr {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let _ = sock.connect("8.8.8.8:80");
        sock.local_addr()
            .map(|a| a.ip())
            .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Mock driver backend: records every executed method instead
/// of touching a body.
struct MockBackend {
    calls: Mutex<Vec<(String, serde_json::Value)>>,
}

impl MockBackend {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
        })
    }

    fn called(&self, method: &str) -> bool {
        self.calls.lock().unwrap().iter().any(|(m, _)| m == method)
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl DriverBackend for MockBackend {
    fn execute(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, MimintError> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        Ok(serde_json::json!({"ok": true, "method": method}))
    }

    fn name(&self) -> &str {
        "mock"
    }
}

/// Scripted listener UI: always answers with this decision.
struct ScriptedListenerUi {
    decision: ListenerDecision,
}

impl ListenerUi for ScriptedListenerUi {
    fn decide(&self, _ctx: &ListenerDecisionCtx) -> ListenerDecision {
        self.decision.clone()
    }
}

/// Scripted initiator UI.
struct ScriptedInitiatorUi {
    decision: InitiatorDecision,
}

impl InitiatorUi for ScriptedInitiatorUi {
    fn show_sas(&self, _sas: &str, _fp: &str, _id: &str) {}

    fn decide(&self, _ctx: &InitiatorDecisionCtx) -> InitiatorDecision {
        self.decision.clone()
    }
}

fn auto_listener() -> AutoListenerUi {
    AutoListenerUi {
        trust: Trust::Owner,
    }
}

fn auto_initiator() -> AutoInitiatorUi {
    AutoInitiatorUi {
        trust: Trust::Owner,
    }
}

/// Pair two nodes over loopback with the auto-confirm UIs.
/// Returns (initiator's entry, listener's entry).
fn pair_nodes(a: &Node, b: &Node, flow: SasFlow) -> (PeerEntry, PeerEntry) {
    b.controller.enable_pairing_mode();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_ctl = Arc::clone(&b.controller);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (mut stream, peer_addr) = listener.accept().unwrap();
        let req = mimint_core::friends::pairing::read_pair_request(&mut stream).unwrap();
        run_ceremony_listener(
            &mut stream,
            &peer_addr.to_string(),
            req,
            &b_id,
            &b_reg,
            &b_ctl,
            &auto_listener(),
            flow,
            &b_clock,
            None,
        )
        .unwrap()
    });
    let mut stream = TcpStream::connect(addr).unwrap();
    let entry_a = run_ceremony_initiator(
        &mut stream,
        &a.identity,
        &a.registry,
        &auto_initiator(),
        flow,
        Trust::Owner,
        &a.clock,
        None,
    )
    .unwrap();
    let entry_b = handle.join().unwrap();
    (entry_a, entry_b)
}

/// Authenticated session pair between two paired nodes.
fn handshake_pair(a: &Node, b: &Node) -> (Session, Session) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(
            stream,
            &b_id,
            &b_reg,
            &SessionConfig::default(),
            &b_clock,
            None,
        )
        .unwrap()
    });
    let stream = TcpStream::connect(addr).unwrap();
    let sa = handshake_initiator(
        stream,
        &a.identity,
        &a.registry,
        &SessionConfig::default(),
        &a.clock,
        None,
    )
    .unwrap();
    let sb = handle.join().unwrap();
    (sa, sb)
}

/// Start a hub on this node's state with fresh loopback
/// ports. Returns the hub (call `stop()` at the end).
fn start_hub(node: &Node, mut config: HubConfig, backend: Arc<dyn DriverBackend>) -> Arc<Hub> {
    config.friends_port = Node::free_port();
    config.xiaozhi_port = Node::free_port();
    config.ota_port = Node::free_port();
    let hub = node.hub(config, backend, Arc::new(auto_listener()));
    hub.start().unwrap();
    hub
}

/// Open an authenticated session from `client` to a running
/// hub's friends port. The hub side is owned by the hub's
/// session loop.
fn handshake_to_hub(client: &Node, hub: &Arc<Hub>) -> Session {
    let addr = format!("127.0.0.1:{}", hub.friends_port());
    let stream = TcpStream::connect(addr).unwrap();
    handshake_initiator(
        stream,
        &client.identity,
        &client.registry,
        &SessionConfig::default(),
        &client.clock,
        None,
    )
    .unwrap()
}

/// One sync-exchange result: (sent, applied, conflicts, rejected).
type SyncCounts = (usize, usize, usize, usize);
/// Recorded (client→server, server→client) frames for proxy tests.
type FrameRecord = Arc<Mutex<Vec<(Vec<u8>, Vec<u8>)>>>;

/// Run a full two-way sync exchange over an authenticated
/// session pair. Returns ((sent, applied, conflicts,
/// rejected) for A, same for B).
fn sync_both(
    a: &Node,
    b: &Node,
    mut sa: Session,
    mut sb: Session,
    kinds: &SyncKinds,
) -> (SyncCounts, SyncCounts) {
    let a_eng = Arc::clone(&a.engine);
    let b_eng = Arc::clone(&b.engine);
    let a_id = a.device_id();
    let b_id = b.device_id();
    let kinds_b = *kinds;
    let handle = std::thread::spawn(move || {
        run_sync_over_session(&mut sb, &b_eng, &a_id, &kinds_b).unwrap()
    });
    let ra = run_sync_over_session(&mut sa, &a_eng, &b_id, kinds).unwrap();
    let rb = handle.join().unwrap();
    (ra, rb)
}

/// What a man-in-the-middle proxy does to the first
/// client-to-server frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProxyMode {
    Passthrough,
    TamperHello,
    DowngradeHello,
}

/// Start a forwarding proxy in front of `target`. Client→server
/// frames are recorded (json, payload). The first frame is
/// mutated per `mode`. Returns the proxy address.
fn start_proxy(target: SocketAddr, mode: ProxyMode, record: FrameRecord) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((client, _)) = listener.accept() else {
            return;
        };
        let Ok(server) = TcpStream::connect(target) else {
            return;
        };
        let mut c2s = client.try_clone().unwrap();
        let mut s2c = server.try_clone().unwrap();
        let mut server_w = server;
        let mut client_w = client;
        let record2 = Arc::clone(&record);
        let t1 = std::thread::spawn(move || {
            let mut first = true;
            loop {
                let frame = match read_frame(&mut c2s) {
                    Ok(f) => f,
                    Err(_) => {
                        // Client went away: tear the server side
                        // down at once, or the peer blocks on its
                        // read timeout (up to 5 minutes).
                        let _ = server_w.shutdown(std::net::Shutdown::Both);
                        break;
                    }
                };
                let mut json = frame.json;
                if first {
                    first = false;
                    match mode {
                        ProxyMode::Passthrough => {}
                        ProxyMode::TamperHello => {
                            // Flip a byte inside a hex string value:
                            // JSON stays valid, the transcript
                            // binding breaks.
                            if json.len() > 40 {
                                json[35] ^= 0x01;
                            }
                        }
                        ProxyMode::DowngradeHello => {
                            // "v":2 -> "v":1 in the hello JSON.
                            for w in json.windows(5) {
                                if w == b"\"v\":2" {
                                    break;
                                }
                            }
                            if let Some(pos) = json.windows(5).position(|w| w == b"\"v\":2") {
                                json[pos + 4] = b'1';
                            }
                        }
                    }
                }
                record2
                    .lock()
                    .unwrap()
                    .push((json.clone(), frame.payload.clone()));
                if write_frame(&mut server_w, &json, &frame.payload).is_err() {
                    break;
                }
            }
        });
        let t2 = std::thread::spawn(move || loop {
            let frame = match read_frame(&mut s2c) {
                Ok(f) => f,
                Err(_) => {
                    let _ = client_w.shutdown(std::net::Shutdown::Both);
                    break;
                }
            };
            if write_frame(&mut client_w, &frame.json, &frame.payload).is_err() {
                let _ = client_w.shutdown(std::net::Shutdown::Both);
                break;
            }
        });
        let _ = t1.join();
        let _ = t2.join();
    });
    addr
}

/// Handshake through a proxy; returns both sides' results
/// (either side may fail when the proxy tampers).
fn handshake_via_proxy(
    a: &Node,
    b: &Node,
    mode: ProxyMode,
    record: FrameRecord,
) -> (Result<Session, MimintError>, Result<Session, MimintError>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(
            stream,
            &b_id,
            &b_reg,
            &SessionConfig::default(),
            &b_clock,
            None,
        )
    });
    let proxy_addr = start_proxy(addr, mode, record);
    let stream = TcpStream::connect(proxy_addr).unwrap();
    let ra = handshake_initiator(
        stream,
        &a.identity,
        &a.registry,
        &SessionConfig::default(),
        &a.clock,
        None,
    );
    let rb = handle.join().unwrap();
    (ra, rb)
}

fn origin_for(device: &Node, request_id: &str, action: &str) -> CardOrigin {
    CardOrigin {
        device_id: device.device_id(),
        device_name: "test-device".to_string(),
        agent_id: "agent-test".to_string(),
        request_id: request_id.to_string(),
        action: action.to_string(),
        detail: serde_json::json!({"method": action}),
    }
}

// ----------------------------------------------------------------
// §1.4 — threat model
// ----------------------------------------------------------------

#[test]
fn threat_model_documented() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let doc = std::fs::read_to_string(format!("{manifest}/../../docs/friends-operations.md"))
        .expect("docs/friends-operations.md must exist");
    for label in [
        "T1", "T2", "T3", "T4", "T5", "T6", "T7", "T8", "T9", "T10", "T11", "T12", "T13",
    ] {
        assert!(doc.contains(label), "threat model must document {label}");
    }
    assert!(doc.contains("XiaoZhi"));
    assert!(doc.contains("forward secrecy"));
}

// ----------------------------------------------------------------
// §2.6 — handshake / session
// ----------------------------------------------------------------

#[test]
fn friends_handshake_happy_path() {
    let a = Node::new("hs-a");
    let b = Node::new("hs-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let (mut sa, mut sb) = handshake_pair(&a, &b);
    assert_eq!(sa.peer_device_id(), b.device_id());
    assert_eq!(sb.peer_device_id(), a.device_id());
    assert_eq!(sa.peer_trust(), Trust::Owner);
    sa.send(MessageKind::Ping, b"ping").unwrap();
    let (kind, payload) = sb.recv().unwrap();
    assert_eq!(kind, MessageKind::Ping);
    assert_eq!(payload, b"ping");
    sb.send(MessageKind::Pong, b"pong").unwrap();
    let (kind, payload) = sa.recv().unwrap();
    assert_eq!(kind, MessageKind::Pong);
    assert_eq!(payload, b"pong");
}

#[test]
fn friends_unknown_peer_rejected_before_data() {
    // Never paired: no pin exists, so the handshake must fail
    // before any application data flows.
    let a = Node::new("unk-a");
    let b = Node::new("unk-b");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(
            stream,
            &b_id,
            &b_reg,
            &SessionConfig::default(),
            &b_clock,
            None,
        )
    });
    let stream = TcpStream::connect(addr).unwrap();
    let ra = handshake_initiator(
        stream,
        &a.identity,
        &a.registry,
        &SessionConfig::default(),
        &a.clock,
        None,
    );
    assert!(ra.is_err(), "unknown peer must be rejected");
    let rb = handle.join().unwrap();
    assert!(rb.is_err(), "listener must reject an unknown peer");
}

#[test]
fn friends_wrong_pin_rejected() {
    let a = Node::new("wp-a");
    let b = Node::new("wp-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    // Corrupt the pinned key A holds for B: the pin no longer
    // matches B's real identity key.
    let mut entry = a.registry.get_peer(&b.device_id()).unwrap().unwrap();
    entry.identity_pub_hex = hex_encode(a.identity.public_key());
    a.registry.upsert_peer(&entry).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(
            stream,
            &b_id,
            &b_reg,
            &SessionConfig::default(),
            &b_clock,
            None,
        )
    });
    let stream = TcpStream::connect(addr).unwrap();
    let ra = handshake_initiator(
        stream,
        &a.identity,
        &a.registry,
        &SessionConfig::default(),
        &a.clock,
        None,
    );
    assert!(ra.is_err(), "wrong pin must be rejected");
    let _ = handle.join().unwrap();
}

/// Pin-before-sign isolation (§2.2): the pin is checked
/// BEFORE the initiator signs, so no AUTH frame (which
/// carries A's signature) may leave A toward a peer whose
/// pin does not match. The plain wrong-pin test cannot tell
/// the pin check apart from the signature check — this one
/// can, by watching the wire through the proxy.
#[test]
fn friends_pin_checked_before_signing() {
    let a = Node::new("pin-a");
    let b = Node::new("pin-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    // Corrupt the pinned key A holds for B.
    let mut entry = a.registry.get_peer(&b.device_id()).unwrap().unwrap();
    entry.identity_pub_hex = hex_encode(a.identity.public_key());
    a.registry.upsert_peer(&entry).unwrap();
    let record: FrameRecord = Arc::new(Mutex::new(Vec::new()));
    let (ra, _rb) = handshake_via_proxy(&a, &b, ProxyMode::Passthrough, Arc::clone(&record));
    assert!(ra.is_err(), "handshake with a wrong pin must fail");
    let frames = record.lock().unwrap();
    for (json, _payload) in frames.iter() {
        assert_ne!(
            kind_of(json).unwrap(),
            MessageKind::Auth,
            "no AUTH frame may be sent before the pin check passes"
        );
    }
}

#[test]
fn friends_revoked_peer_rejected() {
    let a = Node::new("rev-a");
    let b = Node::new("rev-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let now = mimint_core::audit::now_ms();
    assert!(a.registry.revoke_peer(&b.device_id(), now).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(
            stream,
            &b_id,
            &b_reg,
            &SessionConfig::default(),
            &b_clock,
            None,
        )
    });
    let stream = TcpStream::connect(addr).unwrap();
    let ra = handshake_initiator(
        stream,
        &a.identity,
        &a.registry,
        &SessionConfig::default(),
        &a.clock,
        None,
    );
    assert!(ra.is_err(), "revoked peer must be rejected");
    let _ = handle.join().unwrap();
}

#[test]
fn friends_tampered_hello_fails() {
    let a = Node::new("tam-a");
    let b = Node::new("tam-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let record = Arc::new(Mutex::new(Vec::new()));
    let (ra, rb) = handshake_via_proxy(&a, &b, ProxyMode::TamperHello, record);
    assert!(ra.is_err(), "tampered hello must fail the initiator");
    assert!(rb.is_err(), "tampered hello must fail the listener");
}

#[test]
fn friends_replay_fails() {
    let a = Node::new("rep-a");
    let b = Node::new("rep-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let record = Arc::new(Mutex::new(Vec::new()));
    let (mut sa, mut sb) =
        match handshake_via_proxy(&a, &b, ProxyMode::Passthrough, Arc::clone(&record)) {
            (Ok(sa), Ok(sb)) => (sa, sb),
            _ => panic!("handshake through proxy must succeed"),
        };
    sa.send(MessageKind::Ping, b"once").unwrap();
    let (kind, payload) = sb.recv().unwrap();
    assert_eq!(
        (kind, payload.as_slice()),
        (MessageKind::Ping, b"once".as_slice())
    );
    // Replay the exact recorded frame bytes into B.
    let (json, payload) = record.lock().unwrap().last().unwrap().clone();
    let mut injector = sa.stream_clone().unwrap();
    write_frame(&mut injector, &json, &payload).unwrap();
    let replayed = sb.recv();
    assert!(replayed.is_err(), "replayed frame must be rejected");
}

#[test]
fn friends_downgrade_fails() {
    let a = Node::new("dwn-a");
    let b = Node::new("dwn-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let record = Arc::new(Mutex::new(Vec::new()));
    let (ra, rb) = handshake_via_proxy(&a, &b, ProxyMode::DowngradeHello, record);
    assert!(ra.is_err(), "v1 hello must fail the initiator");
    assert!(rb.is_err(), "v1 hello must fail the listener");
}

#[test]
fn friends_wrong_key_cannot_read() {
    // Two crypto contexts with different keys: what A seals
    // for B, a third party C cannot open.
    let clock = Clock::system();
    let (priv_a, _pub_a) = generate_ephemeral();
    let (_priv_b, pub_b) = generate_ephemeral();
    let (priv_c, _pub_c) = generate_ephemeral();
    let na = fresh_nonce16();
    let nb = fresh_nonce16();
    let (key_ab, key_ba) = agree_and_derive(priv_a, &pub_b, &na, &nb, None).unwrap();
    let (key_ab_c, key_ba_c) = agree_and_derive(priv_c, &pub_b, &na, &nb, None).unwrap();
    let mut crypto_ab = SessionCrypto::new(
        "device-a",
        "device-b",
        key_ab,
        key_ba,
        clock.clone(),
        SessionConfig::default(),
    );
    let mut crypto_cb = SessionCrypto::new(
        "device-c",
        "device-b",
        key_ab_c,
        key_ba_c,
        clock,
        SessionConfig::default(),
    );
    let sealed = crypto_ab.seal(MessageKind::Ping, b"secret").unwrap();
    // C's keys are different: open must fail.
    assert!(crypto_cb.open(&sealed.header, &sealed.ciphertext).is_err());
}

#[test]
fn friends_rekey_works() {
    let a = Node::new("rk-a");
    let b = Node::new("rk-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let (mut sa, mut sb) = handshake_pair(&a, &b);
    // B's recv handles the rekey request transparently while
    // waiting for data.
    let handle = std::thread::spawn(move || {
        let (kind, payload) = sb.recv().unwrap();
        (kind, payload, sb.rekey_count())
    });
    sa.initiate_rekey().unwrap();
    assert_eq!(sa.rekey_count(), 1);
    sa.send(MessageKind::Ping, b"after-rekey").unwrap();
    let (kind, payload, sb_rekeys) = handle.join().unwrap();
    assert_eq!(
        (kind, payload.as_slice()),
        (MessageKind::Ping, b"after-rekey".as_slice())
    );
    assert_eq!(sb_rekeys, 1);
}

#[test]
fn friends_rekey_failure_closes_session() {
    let a = Node::new("rkf-a");
    let b = Node::new("rkf-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let (mut sa, sb) = handshake_pair(&a, &b);
    // A starts a rekey; instead of a valid rekey_ack, B's side
    // injects a garbage frame. A must fail closed.
    let mut injector = sb.stream_clone().unwrap();
    let handle = std::thread::spawn(move || {
        let result = sa.initiate_rekey();
        (result, sa.is_closed())
    });
    // Give A time to send the rekey request, then inject garbage.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let garbage_header = serde_json::json!({
        "v": PROTOCOL_VERSION,
        "kind": "rekey_ack",
        "from": b.device_id(),
        "seq": 1,
        "nonce": "AAAAAAAAAAAAAAAAAAAAAAAA",
        "payload_len": 4,
    });
    let header_json = serde_json::to_vec(&garbage_header).unwrap();
    let _ = write_frame(&mut injector, &header_json, b"junk");
    let (result, closed) = handle.join().unwrap();
    assert!(result.is_err(), "garbage rekey_ack must fail the rekey");
    assert!(closed, "session must be closed after rekey failure");
}

#[test]
fn friends_rekey_old_key_rejected_after_switch() {
    let clock = Clock::system();
    let (priv_a, pub_a) = generate_ephemeral();
    let (priv_b, pub_b) = generate_ephemeral();
    let na = fresh_nonce16();
    let nb = fresh_nonce16();
    let (key_ab, key_ba) = agree_and_derive(priv_a, &pub_b, &na, &nb, None).unwrap();
    let (key_ab2, key_ba2) = agree_and_derive(priv_b, &pub_a, &na, &nb, None).unwrap();
    let mut crypto_a = SessionCrypto::new(
        "device-a",
        "device-b",
        key_ab,
        key_ba,
        clock.clone(),
        SessionConfig::default(),
    );
    let mut crypto_b = SessionCrypto::new(
        "device-b",
        "device-a",
        key_ab2,
        key_ba2,
        clock,
        SessionConfig::default(),
    );
    // Seal one message with the OLD key and keep it aside.
    let old_sealed = crypto_a
        .seal(MessageKind::Ping, b"old-key message")
        .unwrap();
    // Rekey: A initiates, B responds, both switch.
    let request = crypto_a.begin_rekey().unwrap();
    let (ack, keys_b) = crypto_b.prepare_rekey_response(&request).unwrap();
    crypto_b.switch_after_ack_sent(keys_b);
    crypto_a.complete_rekey_as_initiator(&ack).unwrap();
    // One message under the NEW key: B opens it, which ends
    // the transition window (old receive key zeroized, R1).
    let new_sealed = crypto_a
        .seal(MessageKind::Ping, b"new-key message")
        .unwrap();
    let opened = crypto_b
        .open(&new_sealed.header, &new_sealed.ciphertext)
        .unwrap();
    assert_eq!(opened, b"new-key message");
    // The old-key message, replayed after the switch, must fail.
    assert!(crypto_b
        .open(&old_sealed.header, &old_sealed.ciphertext)
        .is_err());
}

#[test]
fn friends_no_plaintext_data() {
    let a = Node::new("npt-a");
    let b = Node::new("npt-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let (sa, mut sb) = handshake_pair(&a, &b);
    // Inject a plaintext (unsealed) sync_offer frame into B.
    let mut injector = sa.stream_clone().unwrap();
    let header = serde_json::json!({
        "v": PROTOCOL_VERSION,
        "kind": "sync_offer",
        "from": a.device_id(),
        "seq": 1,
        "nonce": "AAAAAAAAAAAAAAAAAAAAAAAA",
        "payload_len": 7,
    });
    let header_json = serde_json::to_vec(&header).unwrap();
    write_frame(&mut injector, &header_json, b"{\"v\":2}").unwrap();
    assert!(sb.recv().is_err(), "plaintext data must be rejected");
    let _ = sa;
}

#[test]
fn friends_cross_protocol_refused() {
    // An Android body-protocol hello (v1, different shape) is
    // not a friends hello: the listener must refuse it.
    let a = Node::new("xproto-a");
    let b = Node::new("xproto-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(
            stream,
            &b_id,
            &b_reg,
            &SessionConfig::default(),
            &b_clock,
            None,
        )
    });
    let mut stream = TcpStream::connect(addr).unwrap();
    let android_hello = serde_json::json!({
        "v": 1,
        "kind": "body_hello",
        "protocol": "mimint-body-v1",
        "device_id": a.device_id(),
    });
    let json = serde_json::to_vec(&android_hello).unwrap();
    write_frame(&mut stream, &json, &[]).unwrap();
    let result = handle.join().unwrap();
    assert!(result.is_err(), "Android-protocol hello must be refused");
}

// ----------------------------------------------------------------
// §3.8 helpers (manual clocks + scripted ceremonies)
// ----------------------------------------------------------------

impl Node {
    fn new_with_clock(tag: &str, clock: Clock) -> Self {
        let n = NODE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mimint-p7-{}-{}-{}", tag, std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let identity = Arc::new(DeviceKeyStore::in_data_dir(&dir).ensure().unwrap());
        let registry = Arc::new(
            mimint_core::friends::registry::DeviceRegistry::open(dir.join("peers.db")).unwrap(),
        );
        let controller = Arc::new(PairingController::new(clock.clone()));
        let audit = Arc::new(mimint_core::AuditLog::open(dir.join("audit.jsonl")).unwrap());
        let relay_registry = Arc::clone(&registry);
        let relay = Arc::new(ApprovalRelay::new_with_lookup(
            clock.clone(),
            Some(Arc::clone(&audit)),
            Some(Arc::new(move |id: &str| {
                relay_registry
                    .get_peer(id)
                    .ok()
                    .flatten()
                    .map(|peer| peer.name)
            })),
        ));
        let handoff = Arc::new(HandoffManager::new(clock.clone(), None));
        let notebook_path = dir.join("notebook.db");
        let notebook = Arc::new(Notebook::open(&notebook_path).unwrap());
        let config_path = dir.join("config.toml");
        let sources = SyncSources {
            soul: SoulStore::new(&dir),
            skills: SkillStore::new(dir.join("skills")),
            notebook: Arc::clone(&notebook),
            todos: TodoPlanStore::open(&notebook_path).unwrap(),
            profiles: Arc::new(ConfigFileProfileSink::new(&config_path)),
        };
        let engine = Arc::new(
            SyncEngine::new(
                Arc::clone(&registry),
                sources,
                identity.device_id().to_string(),
                // Fix pass 7: the engine's audit events must be
                // observable in the node's audit log (as in
                // `Node::new` above).
                Some(Arc::clone(&audit)),
                clock.clone(),
            )
            .unwrap(),
        );
        Self {
            dir,
            identity,
            registry,
            controller,
            relay,
            handoff,
            engine,
            notebook,
            config_path,
            clock,
            audit,
        }
    }

    /// A node on a manual clock; the returned cell advances it.
    fn manual(tag: &str, start_ms: u64) -> (Self, Arc<std::sync::atomic::AtomicU64>) {
        let (clock, cell) = Clock::manual(start_ms);
        (Self::new_with_clock(tag, clock), cell)
    }

    fn audit_log(&self, name: &str) -> Arc<mimint_core::AuditLog> {
        Arc::new(mimint_core::AuditLog::open(self.dir.join(name)).unwrap())
    }
}

/// Run one ceremony with explicit UIs and audit logs.
/// Returns (initiator result, listener result).
#[allow(clippy::too_many_arguments)]
fn ceremony_with(
    a: &Node,
    b: &Node,
    flow: SasFlow,
    init_ui: &dyn InitiatorUi,
    list_ui: &dyn ListenerUi,
    audit_a: Option<Arc<mimint_core::AuditLog>>,
    audit_b: Option<Arc<mimint_core::AuditLog>>,
) -> (
    Result<PeerEntry, MimintError>,
    Result<PeerEntry, MimintError>,
) {
    b.controller.enable_pairing_mode();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_ctl = Arc::clone(&b.controller);
    let b_clock = b.clock.clone();
    std::thread::scope(|s| {
        let handle = s.spawn(move || {
            let (mut stream, peer_addr) = listener.accept().unwrap();
            let req = mimint_core::friends::pairing::read_pair_request(&mut stream).unwrap();
            run_ceremony_listener(
                &mut stream,
                &peer_addr.to_string(),
                req,
                &b_id,
                &b_reg,
                &b_ctl,
                list_ui,
                flow,
                &b_clock,
                audit_b,
            )
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        let ra = run_ceremony_initiator(
            &mut stream,
            &a.identity,
            &a.registry,
            init_ui,
            flow,
            Trust::Owner,
            &a.clock,
            audit_a,
        );
        // Drop our side promptly so a failed peer does not block
        // on a dead connection.
        drop(stream);
        let rb = handle.join().unwrap();
        (ra, rb)
    })
}

/// Run one ceremony with the initiator connected through a
/// tampering proxy. Returns (initiator result, listener result).
fn ceremony_via_proxy(
    a: &Node,
    b: &Node,
    flow: SasFlow,
    mode: ProxyMode,
) -> (
    Result<PeerEntry, MimintError>,
    Result<PeerEntry, MimintError>,
) {
    b.controller.enable_pairing_mode();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_ctl = Arc::clone(&b.controller);
    let b_clock = b.clock.clone();
    let list_ui = auto_listener();
    let init_ui = auto_initiator();
    std::thread::scope(|s| {
        let handle = s.spawn(move || {
            let (mut stream, peer_addr) = listener.accept().unwrap();
            let req = mimint_core::friends::pairing::read_pair_request(&mut stream).unwrap();
            run_ceremony_listener(
                &mut stream,
                &peer_addr.to_string(),
                req,
                &b_id,
                &b_reg,
                &b_ctl,
                &list_ui,
                flow,
                &b_clock,
                None,
            )
        });
        let record = Arc::new(Mutex::new(Vec::new()));
        let proxy_addr = start_proxy(addr, mode, record);
        let mut stream = TcpStream::connect(proxy_addr).unwrap();
        let ra = run_ceremony_initiator(
            &mut stream,
            &a.identity,
            &a.registry,
            &init_ui,
            flow,
            Trust::Owner,
            &a.clock,
            None,
        );
        drop(stream);
        let rb = handle.join().unwrap();
        (ra, rb)
    })
}

/// A UI that advances the given manual-clock cells by 6
/// minutes when asked to decide (to force ceremony expiry).
struct ExpiringListenerUi {
    cells: Vec<Arc<std::sync::atomic::AtomicU64>>,
}

impl ListenerUi for ExpiringListenerUi {
    fn decide(&self, _ctx: &ListenerDecisionCtx) -> ListenerDecision {
        for cell in &self.cells {
            cell.fetch_add(6 * 60 * 1000, Ordering::SeqCst);
        }
        ListenerDecision {
            sas_outcome: SasOutcome::Matched,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        }
    }
}

// ----------------------------------------------------------------
// §3.8 — pairing
// ----------------------------------------------------------------

#[test]
fn pairing_success_two_instances() {
    let a = Node::new("ps-a");
    let b = Node::new("ps-b");
    let (entry_a, entry_b) = pair_nodes(&a, &b, SasFlow::Compare);
    assert_eq!(entry_a.device_id, b.device_id());
    assert_eq!(entry_b.device_id, a.device_id());
    assert_eq!(
        entry_a.identity_pub_hex,
        hex_encode(b.identity.public_key())
    );
    assert_eq!(
        entry_b.identity_pub_hex,
        hex_encode(a.identity.public_key())
    );
    // Right after the ceremony both sides still show
    // "Pairing chưa hoàn tất": pairing is only complete once
    // a real session works (§3.3 step 7).
    assert!(entry_a.pairing_incomplete);
    assert!(entry_b.pairing_incomplete);
    let (mut sa, mut sb) = handshake_pair(&a, &b);
    sa.send(MessageKind::Ping, b"ok").unwrap();
    let (_, payload) = sb.recv().unwrap();
    assert_eq!(payload, b"ok");
    assert!(
        !a.registry
            .get_peer(&b.device_id())
            .unwrap()
            .unwrap()
            .pairing_incomplete
    );
    assert!(
        !b.registry
            .get_peer(&a.device_id())
            .unwrap()
            .unwrap()
            .pairing_incomplete
    );
}

/// Initiator UI for the typing-flow test: publishes the SAS
/// it shows into a shared slot, standing in for the user
/// reading A's screen and typing the code into B.
struct ChannelInitiatorUi {
    sas_slot: Arc<Mutex<Option<String>>>,
    trust: Trust,
}

impl InitiatorUi for ChannelInitiatorUi {
    fn show_sas(&self, sas: &str, _fp: &str, _id: &str) {
        *self.sas_slot.lock().unwrap() = Some(sas.to_string());
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

/// Listener UI for the typing-flow test: types the SAS from
/// the shared slot (waits up to 5 s for the initiator side
/// to publish it).
struct ChannelListenerUi {
    sas_slot: Arc<Mutex<Option<String>>>,
    trust: Trust,
}

impl ListenerUi for ChannelListenerUi {
    fn decide(&self, _ctx: &ListenerDecisionCtx) -> ListenerDecision {
        let mut typed = String::new();
        for _ in 0..50 {
            if let Some(sas) = self.sas_slot.lock().unwrap().clone() {
                typed = sas;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        ListenerDecision {
            sas_outcome: SasOutcome::Typed(typed),
            trust_granted: self.trust,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        }
    }
}

#[test]
fn pairing_fallback_typing_flow_success() {
    let a = Node::new("pf-a");
    let b = Node::new("pf-b");
    let slot = Arc::new(Mutex::new(None));
    let init_ui = ChannelInitiatorUi {
        sas_slot: Arc::clone(&slot),
        trust: Trust::Owner,
    };
    let list_ui = ChannelListenerUi {
        sas_slot: Arc::clone(&slot),
        trust: Trust::Owner,
    };
    let (ra, rb) = ceremony_with(&a, &b, SasFlow::Typing, &init_ui, &list_ui, None, None);
    assert!(ra.is_ok(), "typing flow must succeed: {ra:?}");
    assert!(rb.is_ok(), "typing flow must succeed: {rb:?}");
    let entry_a = a.registry.get_peer(&b.device_id()).unwrap().unwrap();
    let entry_b = b.registry.get_peer(&a.device_id()).unwrap().unwrap();
    assert_eq!(entry_a.device_id, b.device_id());
    assert_eq!(entry_b.device_id, a.device_id());
    let (mut sa, mut sb) = handshake_pair(&a, &b);
    sa.send(MessageKind::Ping, b"typed").unwrap();
    let (_, payload) = sb.recv().unwrap();
    assert_eq!(payload, b"typed");
}

#[test]
fn pairing_wrong_sas_rejected_and_audited() {
    let a = Node::new("ws-a");
    let b = Node::new("ws-b");
    let audit_b = b.audit_log("audit.jsonl");
    let list_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Mismatched,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let (ra, rb) = ceremony_with(
        &a,
        &b,
        SasFlow::Compare,
        &auto_initiator(),
        &list_ui,
        None,
        Some(Arc::clone(&audit_b)),
    );
    assert!(ra.is_err(), "wrong SAS must fail the initiator");
    assert!(rb.is_err(), "wrong SAS must fail the listener");
    // No pin is stored after a failed ceremony.
    assert!(a.registry.get_peer(&b.device_id()).unwrap().is_none());
    assert!(b.registry.get_peer(&a.device_id()).unwrap().is_none());
    // The failure is audited on the listener side.
    let log = std::fs::read_to_string(b.dir.join("audit.jsonl")).unwrap();
    assert!(
        log.contains("pairing_failed"),
        "audit must record pairing_failed"
    );
    assert!(log.contains("sas_mismatch_reported"));
}

#[test]
fn pairing_compare_flow_mismatch_reported_fails_and_counts() {
    let a = Node::new("cm-a");
    let b = Node::new("cm-b");
    let list_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Mismatched,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let (ra, rb) = ceremony_with(
        &a,
        &b,
        SasFlow::Compare,
        &auto_initiator(),
        &list_ui,
        None,
        None,
    );
    assert!(ra.is_err() && rb.is_err());
    // The failure counts toward lockout: one failure recorded
    // for this source, not yet locked (needs 5).
    assert!(b
        .registry
        .lockout_until(&a.device_id(), b.clock.now_ms())
        .unwrap()
        .is_none());
    // A second, successful ceremony resets the count: pair
    // for real and confirm the failure is forgiven.
    let (entry_a, _) = pair_nodes(&a, &b, SasFlow::Compare);
    assert_eq!(entry_a.device_id, b.device_id());
    assert!(b
        .registry
        .lockout_until(&a.device_id(), b.clock.now_ms())
        .unwrap()
        .is_none());
}

#[test]
fn pairing_expired() {
    let (a, cell_a) = Node::manual("exp-a", 1_000_000);
    let (b, cell_b) = Node::manual("exp-b", 1_000_000);
    let cells = vec![Arc::clone(&cell_a), Arc::clone(&cell_b)];
    let list_ui = ExpiringListenerUi { cells };
    let (ra, rb) = ceremony_with(
        &a,
        &b,
        SasFlow::Compare,
        &auto_initiator(),
        &list_ui,
        None,
        None,
    );
    assert!(ra.is_err(), "expired ceremony must fail the initiator");
    assert!(rb.is_err(), "expired ceremony must fail the listener");
    assert!(a.registry.get_peer(&b.device_id()).unwrap().is_none());
}

#[test]
fn pairing_lockout_after_5_failures() {
    let a = Node::new("lo-a");
    let b = Node::new("lo-b");
    let list_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Mismatched,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    for attempt in 1..=5 {
        let (ra, rb) = ceremony_with(
            &a,
            &b,
            SasFlow::Compare,
            &auto_initiator(),
            &list_ui,
            None,
            None,
        );
        assert!(ra.is_err() && rb.is_err(), "attempt {attempt} must fail");
    }
    // The 6th attempt is refused immediately: locked.
    assert!(b
        .registry
        .lockout_until(&a.device_id(), b.clock.now_ms())
        .unwrap()
        .is_some());
    let (ra, rb) = ceremony_with(
        &a,
        &b,
        SasFlow::Compare,
        &auto_initiator(),
        &list_ui,
        None,
        None,
    );
    assert!(ra.is_err() && rb.is_err(), "locked source must be refused");
}

#[test]
fn pairing_mitm_fails() {
    let a = Node::new("mitm-a");
    let b = Node::new("mitm-b");
    // MITM flips a byte in the pair_request: the transcripts
    // diverge, so the pair_confirm signature cannot verify.
    let (ra, rb) = ceremony_via_proxy(&a, &b, SasFlow::Typing, ProxyMode::TamperHello);
    assert!(ra.is_err(), "MITM must fail the initiator");
    assert!(rb.is_err(), "MITM must fail the listener");
    assert!(a.registry.get_peer(&b.device_id()).unwrap().is_none());
    assert!(b.registry.get_peer(&a.device_id()).unwrap().is_none());
}

#[test]
fn pairing_compare_flow_mitm_fails() {
    let a = Node::new("mitmc-a");
    let b = Node::new("mitmc-b");
    let (ra, rb) = ceremony_via_proxy(&a, &b, SasFlow::Compare, ProxyMode::TamperHello);
    assert!(ra.is_err(), "MITM must fail the compare flow (initiator)");
    assert!(rb.is_err(), "MITM must fail the compare flow (listener)");
    assert!(a.registry.get_peer(&b.device_id()).unwrap().is_none());
    assert!(b.registry.get_peer(&a.device_id()).unwrap().is_none());
}

#[test]
fn pairing_fingerprint_skip_requires_warning_and_audits() {
    let a = Node::new("fps-a");
    let b = Node::new("fps-b");
    let audit_a = a.audit_log("audit.jsonl");
    let audit_b = b.audit_log("audit.jsonl");
    let list_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Matched,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: false,
            fingerprint_skipped_with_warning: true,
        },
    };
    let init_ui = ScriptedInitiatorUi {
        decision: InitiatorDecision {
            confirmed: true,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: false,
            fingerprint_skipped_with_warning: true,
        },
    };
    let (ra, rb) = ceremony_with(
        &a,
        &b,
        SasFlow::Compare,
        &init_ui,
        &list_ui,
        Some(Arc::clone(&audit_a)),
        Some(Arc::clone(&audit_b)),
    );
    assert!(ra.is_ok(), "skip-with-warning must still pair: {ra:?}");
    assert!(rb.is_ok(), "skip-with-warning must still pair: {rb:?}");
    let log_b = std::fs::read_to_string(b.dir.join("audit.jsonl")).unwrap();
    assert!(
        log_b.contains("fingerprint_check_skipped"),
        "skip must be audited on the listener side"
    );
    let log_a = std::fs::read_to_string(a.dir.join("audit.jsonl")).unwrap();
    assert!(
        log_a.contains("fingerprint_check_skipped"),
        "skip must be audited on the initiator side"
    );
}

#[test]
fn pairing_trust_both_directions_independent() {
    let a = Node::new("ti-a");
    let b = Node::new("ti-b");
    // A grants Owner to B; B grants Guest to A.
    let list_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Matched,
            trust_granted: Trust::Guest,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let init_ui = ScriptedInitiatorUi {
        decision: InitiatorDecision {
            confirmed: true,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let (ra, rb) = ceremony_with(&a, &b, SasFlow::Compare, &init_ui, &list_ui, None, None);
    assert!(ra.is_ok() && rb.is_ok());
    // Each side stores the trust IT granted.
    assert_eq!(
        a.registry.get_peer(&b.device_id()).unwrap().unwrap().trust,
        Trust::Owner
    );
    assert_eq!(
        b.registry.get_peer(&a.device_id()).unwrap().unwrap().trust,
        Trust::Guest
    );
    // And each side learns what the peer granted it
    // (`trust_received`, display only).
    assert_eq!(
        a.registry
            .get_peer(&b.device_id())
            .unwrap()
            .unwrap()
            .trust_received,
        Some(Trust::Guest)
    );
    assert_eq!(
        b.registry
            .get_peer(&a.device_id())
            .unwrap()
            .unwrap()
            .trust_received,
        Some(Trust::Owner)
    );
    // A session reports the LOCAL grant (what this side
    // enforces on the peer).
    let (sa, sb) = handshake_pair(&a, &b);
    assert_eq!(sa.peer_trust(), Trust::Owner);
    assert_eq!(sb.peer_trust(), Trust::Guest);
}

#[test]
fn pairing_requires_pairing_mode() {
    let a = Node::new("rpm-a");
    let b = Node::new("rpm-b");
    // Pairing mode is deliberately NOT enabled on B.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_ctl = Arc::clone(&b.controller);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (mut stream, peer_addr) = listener.accept().unwrap();
        let req = mimint_core::friends::pairing::read_pair_request(&mut stream).unwrap();
        run_ceremony_listener(
            &mut stream,
            &peer_addr.to_string(),
            req,
            &b_id,
            &b_reg,
            &b_ctl,
            &auto_listener(),
            SasFlow::Compare,
            &b_clock,
            None,
        )
    });
    let mut stream = TcpStream::connect(addr).unwrap();
    let ra = run_ceremony_initiator(
        &mut stream,
        &a.identity,
        &a.registry,
        &auto_initiator(),
        SasFlow::Compare,
        Trust::Owner,
        &a.clock,
        None,
    );
    drop(stream);
    assert!(ra.is_err(), "pairing without pairing mode must fail");
    assert!(handle.join().unwrap().is_err());
    assert!(a.registry.get_peer(&b.device_id()).unwrap().is_none());
}

#[test]
fn pairing_revoke_cuts_access() {
    let a = Node::new("prc-a");
    let b = Node::new("prc-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let (mut sa, mut sb) = handshake_pair(&a, &b);
    sa.send(MessageKind::Ping, b"before").unwrap();
    assert_eq!(sb.recv().unwrap().1, b"before");
    let now = mimint_core::audit::now_ms();
    assert!(a.registry.revoke_peer(&b.device_id(), now).unwrap());
    assert!(b.registry.revoke_peer(&a.device_id(), now).unwrap());
    // A new session must now fail on both sides.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(
            stream,
            &b_id,
            &b_reg,
            &SessionConfig::default(),
            &b_clock,
            None,
        )
    });
    let stream = TcpStream::connect(addr).unwrap();
    let ra = handshake_initiator(
        stream,
        &a.identity,
        &a.registry,
        &SessionConfig::default(),
        &a.clock,
        None,
    );
    assert!(ra.is_err(), "revoked peer must not start a new session");
    let _ = handle.join().unwrap();
}

#[test]
fn pairing_rotate_invalidates_old_key() {
    let a = Node::new("rot-a");
    let b = Node::new("rot-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let old_b_id = b.device_id();
    let old_b_pub = hex_encode(b.identity.public_key());
    // B rotates: new keypair, new device_id. The old identity
    // object is kept only to prove the old key is dead.
    let old_b_identity = Arc::clone(&b.identity);
    let new_b_identity = DeviceKeyStore::in_data_dir(&b.dir).rotate().unwrap();
    let new_b_id = new_b_identity.device_id().to_string();
    assert_ne!(new_b_id, old_b_id);
    // A connects using the OLD pin: B now presents a different
    // key, so the pin check refuses the handshake.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let new_b_arc = Arc::new(new_b_identity);
    let new_b_thread = Arc::clone(&new_b_arc);
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(
            stream,
            &new_b_thread,
            &b_reg,
            &SessionConfig::default(),
            &b_clock,
            None,
        )
    });
    let stream = TcpStream::connect(addr).unwrap();
    let ra = handshake_initiator(
        stream,
        &a.identity,
        &a.registry,
        &SessionConfig::default(),
        &a.clock,
        None,
    );
    assert!(ra.is_err(), "old pin must be refused after rotation");
    let _ = handle.join().unwrap();
    // The old identity can no longer authenticate as B either:
    // A pins the old key, but B's live identity is the new one.
    assert_ne!(
        hex_encode(old_b_identity.public_key()),
        hex_encode(new_b_arc.public_key())
    );
    assert_eq!(old_b_pub, hex_encode(old_b_identity.public_key()));
    // Re-pair with the new identity works.
    let _b2 = Node::new_with_clock("rot-b2", b.clock.clone());
    // Re-pair needs B's registry + new identity: rebuild a
    // node view over B's data dir with the new identity.
    let b_node = Node {
        dir: b.dir.clone(),
        identity: Arc::clone(&new_b_arc),
        registry: Arc::clone(&b.registry),
        controller: Arc::clone(&b.controller),
        relay: Arc::clone(&b.relay),
        handoff: Arc::clone(&b.handoff),
        engine: Arc::clone(&b.engine),
        notebook: Arc::clone(&b.notebook),
        config_path: b.config_path.clone(),
        clock: b.clock.clone(),
        audit: Arc::clone(&b.audit),
    };
    let (entry_a, _) = pair_nodes(&a, &b_node, SasFlow::Compare);
    assert_eq!(entry_a.device_id, new_b_id);
    assert_eq!(entry_a.identity_pub_hex, hex_encode(new_b_arc.public_key()));
    // Re-pairing the same (new) device id bumps the generation:
    // the design's key_generation distinguishes re-pair from a
    // brand-new pairing.
    let (entry_a2, _) = pair_nodes(&a, &b_node, SasFlow::Compare);
    assert_eq!(entry_a2.key_generation, entry_a.key_generation + 1);
}

#[test]
fn pairing_guest_cannot_sync_or_approve() {
    let a = Node::new("gst-a");
    let b = Node::new("gst-b");
    // A grants Guest to B.
    let list_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Matched,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let init_ui = ScriptedInitiatorUi {
        decision: InitiatorDecision {
            confirmed: true,
            trust_granted: Trust::Guest,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let (ra, rb) = ceremony_with(&a, &b, SasFlow::Compare, &init_ui, &list_ui, None, None);
    assert!(ra.is_ok() && rb.is_ok());
    // Approval is OFF by default per peer: a guest cannot get
    // a card created for it.
    let allowed = a
        .registry
        .get_prefs(&b.device_id())
        .unwrap()
        .approve_allowed;
    assert!(!allowed, "approve must default OFF per peer");
    assert!(a
        .relay
        .create_card(
            origin_for(&b, "req-guest", "screenshot"),
            &b.device_id(),
            allowed
        )
        .is_err());
    // And a guest cannot sync through the hub: the hub closes
    // the session on a guest sync_offer (§3.5, T7).
    let backend = MockBackend::new();
    let hub = start_hub(&a, HubConfig::default(), backend);
    let mut sb = handshake_to_hub(&b, &hub);
    let offer = serde_json::json!({"v": PROTOCOL_VERSION, "kind": "sync_offer", "summaries": []});
    sb.send(MessageKind::SyncOffer, &serde_json::to_vec(&offer).unwrap())
        .unwrap();
    // The hub closed the session: the next read fails (EOF).
    assert!(
        sb.recv().is_err(),
        "guest sync_offer must close the session"
    );
    hub.stop();
    // And the driver allow-list refuses input methods for a
    // guest (read methods stay allowed, §5.3).
    assert!(check_driver_method(Trust::Guest, "click").is_err());
    assert!(check_driver_method(Trust::Guest, "screenshot").is_ok());
    assert!(check_driver_method(Trust::Owner, "click").is_ok());
}

#[test]
fn pairing_trust_not_self_granted() {
    let a = Node::new("tsg-a");
    let b = Node::new("tsg-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    // Neither side has an entry for ITSELF: trust is only ever
    // granted to a peer, never self-granted.
    assert!(a.registry.get_peer(&a.device_id()).unwrap().is_none());
    assert!(b.registry.get_peer(&b.device_id()).unwrap().is_none());
    // And a device cannot grant itself trust through the
    // registry API: set_trust_granted on an unknown id errors.
    assert!(a
        .registry
        .set_trust_granted(&a.device_id(), Trust::Owner)
        .is_err());
}

#[test]
fn pairing_pin_stored_at_correct_steps() {
    let a = Node::new("pin-a");
    let b = Node::new("pin-b");
    // Failed ceremony stores no pin on either side.
    let list_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Mismatched,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let (ra, rb) = ceremony_with(
        &a,
        &b,
        SasFlow::Compare,
        &auto_initiator(),
        &list_ui,
        None,
        None,
    );
    assert!(ra.is_err() && rb.is_err());
    assert!(a.registry.get_peer(&b.device_id()).unwrap().is_none());
    assert!(b.registry.get_peer(&a.device_id()).unwrap().is_none());
    // Successful ceremony: initiator stores the pin at step 6
    // (on sending pair_accept), listener at step 7 (on
    // receiving pair_accept) — both entries exist afterwards,
    // with the exact pinned keys.
    let (entry_a, entry_b) = pair_nodes(&a, &b, SasFlow::Compare);
    assert_eq!(
        entry_a.identity_pub_hex,
        hex_encode(b.identity.public_key())
    );
    assert_eq!(
        entry_b.identity_pub_hex,
        hex_encode(a.identity.public_key())
    );
    assert_eq!(entry_a.fingerprint_hex, b.identity.fingerprint());
    assert_eq!(entry_b.fingerprint_hex, a.identity.fingerprint());
}

// ----------------------------------------------------------------
// §4.8 helpers
// ----------------------------------------------------------------

/// Run a sync exchange where each side uses its own kinds.
fn sync_both_asym(
    a: &Node,
    b: &Node,
    mut sa: Session,
    mut sb: Session,
    kinds_a: &SyncKinds,
    kinds_b: &SyncKinds,
) -> (SyncCounts, SyncCounts) {
    let a_eng = Arc::clone(&a.engine);
    let b_eng = Arc::clone(&b.engine);
    let a_id = a.device_id();
    let b_id = b.device_id();
    let kinds_b = *kinds_b;
    let handle = std::thread::spawn(move || {
        run_sync_over_session(&mut sb, &b_eng, &a_id, &kinds_b).unwrap()
    });
    let ra = run_sync_over_session(&mut sa, &a_eng, &b_id, kinds_a).unwrap();
    let rb = handle.join().unwrap();
    (ra, rb)
}

fn add_shared_note(node: &Node, title: &str, body: &str) -> (i64, String) {
    let id = node
        .notebook
        .add_for("agent-001", title, body, true)
        .unwrap();
    let sync_id = node.notebook.note_sync_id(id).unwrap().unwrap();
    (id, sync_id)
}

fn add_private_note(node: &Node, title: &str, body: &str) -> (i64, String) {
    let id = node
        .notebook
        .add_for("agent-001", title, body, false)
        .unwrap();
    let sync_id = node.notebook.note_sync_id(id).unwrap().unwrap();
    (id, sync_id)
}

fn note_body(node: &Node, sync_id: &str) -> Option<String> {
    node.notebook
        .get_note_by_sync_id(sync_id)
        .unwrap()
        .map(|n| n.body)
}

fn edit_note(node: &Node, sync_id: &str, body: &str) {
    let note = node
        .notebook
        .get_note_by_sync_id(sync_id)
        .unwrap()
        .expect("note must exist");
    node.notebook
        .update_for("agent-001", note.id, &note.title, body)
        .unwrap();
}

fn add_todo(node: &Node, title: &str) -> String {
    let sync_id = format!("{:032x}", {
        let mut bytes = [0u8; 16];
        mimint_core::friends::util::random_bytes(&mut bytes);
        u128::from_be_bytes(bytes)
    });
    node.todos()
        .upsert_todo(&Todo {
            sync_id: sync_id.clone(),
            title: title.to_string(),
            done: false,
            due_ms: None,
            notes: String::new(),
            version_lamport: 1,
            version_device: node.device_id(),
            origin_device: node.device_id(),
            updated_ms: mimint_core::audit::now_ms(),
        })
        .unwrap();
    sync_id
}

fn add_skill(node: &Node, name: &str) {
    node.skills()
        .save(&SkillDef {
            name: name.to_string(),
            description: "test skill".to_string(),
            triggers: vec!["test".to_string()],
            content: "Do the test thing.".to_string(),
            version: 1,
            author: "test".to_string(),
            validation_tasks: vec![mimint_core::ValidationTask {
                id: "v1".to_string(),
                prompt: "say hello".to_string(),
                checker: mimint_core::Checker::ResponseNonEmpty,
            }],
            status: SkillStatus::Promoted,
            validation_history: Vec::new(),
            imported: false,
            import_approved: false,
        })
        .unwrap();
}

fn paired_synced(a: &Node, b: &Node) -> (Session, Session) {
    pair_nodes(a, b, SasFlow::Compare);
    handshake_pair(a, b)
}

// ----------------------------------------------------------------
// §4.8 — sync
// ----------------------------------------------------------------

#[test]
fn sync_two_way_two_instances() {
    let a = Node::new("s2w-a");
    let b = Node::new("s2w-b");
    let (_, note_id) = add_shared_note(&a, "from A", "hello from A");
    let todo_id = add_todo(&b, "from B");
    let (sa, sb) = paired_synced(&a, &b);
    let kinds = SyncKinds::all();
    let (ra, rb) = sync_both(&a, &b, sa, sb, &kinds);
    assert!(ra.1 + rb.1 >= 2, "both records must apply: {ra:?} {rb:?}");
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("hello from A"));
    assert!(b.todos().get_todo(&todo_id).unwrap().is_some());
}

#[test]
fn sync_concurrent_detection_successor_applies() {
    let a = Node::new("suc-a");
    let b = Node::new("suc-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
    // Only A edits (successor of v1): it must apply on B.
    edit_note(&a, &note_id, "v2 by A");
    let (sa, sb) = handshake_pair(&a, &b);
    let (ra, rb) = sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(rb.1, 1, "successor must apply: {ra:?} {rb:?}");
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v2 by A"));
}

#[test]
fn sync_concurrent_detection_concurrent_conflicts() {
    let a = Node::new("cc-a");
    let b = Node::new("cc-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    edit_note(&a, &note_id, "A's version");
    edit_note(&b, &note_id, "B's version");
    let (sa, sb) = handshake_pair(&a, &b);
    let (ra, rb) = sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert!(
        ra.2 + rb.2 >= 1,
        "concurrent edits must conflict: {ra:?} {rb:?}"
    );
    // Neither side silently merged: both bodies are unchanged.
    assert_eq!(note_body(&a, &note_id).as_deref(), Some("A's version"));
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("B's version"));
    assert!(!a.registry.list_conflicts(false).unwrap().is_empty());
}

#[test]
fn sync_concurrent_detection_lamport_larger_does_not_win() {
    let a = Node::new("llw-a");
    let b = Node::new("llw-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    // A edits TWICE (larger Lamport), B edits ONCE. A larger
    // Lamport must NOT win: this is still a conflict (§4.4
    // forbids every "larger Lamport wins" rule).
    edit_note(&a, &note_id, "A edit 1");
    edit_note(&a, &note_id, "A edit 2");
    edit_note(&b, &note_id, "B edit 1");
    let (sa, sb) = handshake_pair(&a, &b);
    let (ra, rb) = sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert!(
        ra.2 + rb.2 >= 1,
        "must conflict, not Lamport-win: {ra:?} {rb:?}"
    );
    assert_eq!(note_body(&a, &note_id).as_deref(), Some("A edit 2"));
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("B edit 1"));
}

#[test]
fn sync_concurrent_detection_missing_history_is_conflict() {
    let a = Node::new("mh-a");
    let b = Node::new("mh-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    edit_note(&b, &note_id, "B's version");
    // A loses its recorded version + last_seen for this record
    // (history gap): B's record is based on a version A can no
    // longer prove. That is a conflict, never a silent apply.
    a.registry.delete_local_version(&note_id).unwrap();
    a.registry.delete_last_seen_for_sync_id(&note_id).unwrap();
    let (sa, sb) = handshake_pair(&a, &b);
    let (ra, rb) = sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert!(
        ra.2 + rb.2 >= 1,
        "missing history must conflict: {ra:?} {rb:?}"
    );
}

#[test]
fn sync_conflict_resolution_converges() {
    let a = Node::new("crc-a");
    let b = Node::new("crc-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    edit_note(&a, &note_id, "A's version");
    edit_note(&b, &note_id, "B's version");
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    let conflicts = a.registry.list_conflicts(false).unwrap();
    assert_eq!(conflicts.len(), 1);
    // The user picks the remote body on A.
    a.engine
        .resolve_conflict(conflicts[0].id, ResolutionChoice::KeepRemote)
        .unwrap();
    // Next sync converges: both sides hold B's version.
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(note_body(&a, &note_id).as_deref(), Some("B's version"));
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("B's version"));
    assert!(a.registry.list_conflicts(false).unwrap().is_empty());
}

#[test]
fn sync_conflict_keeps_both() {
    let a = Node::new("ckb-a");
    let b = Node::new("ckb-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    edit_note(&a, &note_id, "A's version");
    edit_note(&b, &note_id, "B's version");
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    let conflicts = a.registry.list_conflicts(false).unwrap();
    assert_eq!(conflicts.len(), 1);
    a.engine
        .resolve_conflict(conflicts[0].id, ResolutionChoice::KeepBoth)
        .unwrap();
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    // Both sides now hold TWO notes: the original id converged
    // to A's body, and the remote body lives under a new id.
    let notes_a = a.notebook.list_shared_notes_with_sync_ids().unwrap();
    let notes_b = b.notebook.list_shared_notes_with_sync_ids().unwrap();
    assert_eq!(notes_a.len(), 2, "A must keep both");
    assert_eq!(notes_b.len(), 2, "B must keep both");
    assert_eq!(note_body(&a, &note_id).as_deref(), Some("A's version"));
}

#[test]
fn sync_tombstone_requires_confirmation() {
    let a = Node::new("tmb-a");
    let b = Node::new("tmb-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
    // A deletes the note (tombstone). B still has it live, so
    // the tombstone must sit PENDING on B until the user
    // confirms — the note is untouched.
    a.engine
        .create_tombstone(&note_id, RecordKind::NotebookNote)
        .unwrap();
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(
        note_body(&b, &note_id).as_deref(),
        Some("v1"),
        "unconfirmed tombstone must not delete"
    );
    assert_eq!(b.engine.pending_tombstones().unwrap().len(), 1);
}

#[test]
fn sync_tombstone_no_automatic_exception() {
    let a = Node::new("tne-a");
    let b = Node::new("tne-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    a.engine
        .create_tombstone(&note_id, RecordKind::NotebookNote)
        .unwrap();
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    // The user on B rejects the tombstone: no automatic
    // deletion, no automatic exception — the note stays, and a
    // further sync does not delete it either.
    assert!(b.engine.reject_tombstone(&note_id).unwrap());
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
    assert!(b.engine.pending_tombstones().unwrap().is_empty());
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
}

#[test]
fn sync_tombstone_resurrection_after_91_days() {
    let (a, cell_a) = Node::manual("trs-a", 1_000_000);
    let (b, cell_b) = Node::manual("trs-b", 1_000_000);
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    a.engine
        .create_tombstone(&note_id, RecordKind::NotebookNote)
        .unwrap();
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert!(b.engine.confirm_tombstone(&note_id).unwrap());
    assert!(
        note_body(&b, &note_id).is_none(),
        "confirmed delete applies"
    );
    // 91 days pass: the tombstone is purged on both sides.
    for cell in [&cell_a, &cell_b] {
        cell.fetch_add(91 * 24 * 60 * 60 * 1000, Ordering::SeqCst);
    }
    let purged_a = a.engine.purge_expired_tombstones().unwrap();
    let purged_b = b.engine.purge_expired_tombstones().unwrap();
    assert_eq!(purged_a.len(), 1);
    assert_eq!(purged_b.len(), 1);
    // The same sync_id can now come back as a new record
    // (resurrection, §4.5): A re-creates it, B receives it.
    a.notebook
        .upsert_note_by_sync_id(&note_id, "note", "resurrected", 2_000_000)
        .unwrap();
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(
        note_body(&b, &note_id).as_deref(),
        Some("resurrected"),
        "resurrection after 91 days must apply"
    );
}

#[test]
fn sync_canary_secret_never_in_payload() {
    let a = Node::new("can-a");
    let b = Node::new("can-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    // Sentinels in places sync must never read: the vault
    // file, a private note, the SOUL history, the audit log.
    let vault_sentinel = "CANARY-VAULT-7f3a9c1e";
    mimint_core::FileVault::create(a.dir.join("vault-test.txt"), vault_sentinel).unwrap();
    let private_sentinel = "CANARY-PRIVATE-7f3a9c1e";
    let (_, private_id) = add_private_note(&a, "private", private_sentinel);
    let history_sentinel = "CANARY-HISTORY-7f3a9c1e";
    std::fs::write(
        a.dir.join("soul-history.jsonl"),
        format!("{{\"note\": \"{history_sentinel}\"}}\n"),
    )
    .unwrap();
    let audit_sentinel = "CANARY-AUDIT-7f3a9c1e";
    let audit = a.audit_log("audit.jsonl");
    audit
        .log(
            "test",
            "canary",
            None,
            None,
            serde_json::json!({"s": audit_sentinel}),
        )
        .unwrap();
    // A shared note that DOES sync (control: the push is
    // non-empty and carries real records).
    let (_, shared_id) = add_shared_note(&a, "shared", "shared body");
    let kinds = SyncKinds::all();
    let offer = a.engine.build_offer(&kinds).unwrap();
    let offer_json = serde_json::to_string(&offer).unwrap();
    for sentinel in [
        vault_sentinel,
        private_sentinel,
        history_sentinel,
        audit_sentinel,
    ] {
        assert!(
            !offer_json.contains(sentinel),
            "sentinel {sentinel} must not appear in the offer"
        );
    }
    // The private note's sync_id must not appear either.
    assert!(
        !offer_json.contains(&private_id),
        "private sync_id must not be offered"
    );
    let request = b
        .engine
        .handle_offer(&a.device_id(), &offer, &kinds)
        .unwrap();
    let push = a.engine.build_push(&b.device_id(), &request, &kinds).unwrap();
    assert!(!push.records.is_empty(), "push must carry the shared note");
    let push_json = serde_json::to_string(&push).unwrap();
    for sentinel in [
        vault_sentinel,
        private_sentinel,
        history_sentinel,
        audit_sentinel,
    ] {
        assert!(
            !push_json.contains(sentinel),
            "sentinel {sentinel} must not appear in the push"
        );
    }
    // And the full exchange still delivers the shared note.
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &kinds);
    assert_eq!(note_body(&b, &shared_id).as_deref(), Some("shared body"));
}

#[test]
fn sync_private_notes_never_leave() {
    let a = Node::new("pnl-a");
    let b = Node::new("pnl-b");
    let (_, private_id) = add_private_note(&a, "secret diary", "do not sync");
    let (_, shared_id) = add_shared_note(&a, "shared", "sync me");
    let kinds = SyncKinds::all();
    let offer = a.engine.build_offer(&kinds).unwrap();
    let offer_json = serde_json::to_string(&offer).unwrap();
    assert!(
        !offer_json.contains(&private_id),
        "private sync_id must not be offered"
    );
    assert!(
        offer_json.contains(&shared_id),
        "shared sync_id must be offered"
    );
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &kinds);
    assert!(note_body(&b, &private_id).is_none());
    assert_eq!(note_body(&b, &shared_id).as_deref(), Some("sync me"));
}

#[test]
fn sync_unpaired_peer_cannot_read() {
    let a = Node::new("upr-a");
    let b = Node::new("upr-b");
    let c = Node::new("upr-c");
    let (_, note_id) = add_shared_note(&a, "note", "secret sync body");
    pair_nodes(&a, &b, SasFlow::Compare);
    // A real sync push, sealed with A-B session keys.
    let kinds = SyncKinds::all();
    let offer = a.engine.build_offer(&kinds).unwrap();
    let request = b
        .engine
        .handle_offer(&a.device_id(), &offer, &kinds)
        .unwrap();
    let push = a.engine.build_push(&b.device_id(), &request, &kinds).unwrap();
    let push_bytes = serde_json::to_vec(&push).unwrap();
    assert!(!push_bytes.is_empty());
    let clock = Clock::system();
    let (priv_a, _pub_a) = generate_ephemeral();
    let (_priv_b, pub_b) = generate_ephemeral();
    let (priv_c, _pub_c) = generate_ephemeral();
    let na = fresh_nonce16();
    let nb = fresh_nonce16();
    let (key_ab, key_ba) = agree_and_derive(priv_a, &pub_b, &na, &nb, None).unwrap();
    let (key_ab_c, key_ba_c) = agree_and_derive(priv_c, &pub_b, &na, &nb, None).unwrap();
    let mut crypto_ab = SessionCrypto::new(
        "device-a",
        "device-b",
        key_ab,
        key_ba,
        clock.clone(),
        SessionConfig::default(),
    );
    let mut crypto_cb = SessionCrypto::new(
        "device-c",
        "device-b",
        key_ab_c,
        key_ba_c,
        clock,
        SessionConfig::default(),
    );
    let sealed = crypto_ab.seal(MessageKind::SyncPush, &push_bytes).unwrap();
    // C holds different keys: the captured sync payload does
    // not open for an unpaired peer.
    assert!(crypto_cb.open(&sealed.header, &sealed.ciphertext).is_err());
    let _ = c;
    let _ = note_id;
}

#[test]
fn sync_profile_missing_backend_drops() {
    let a = Node::new("pmb-a");
    let b = Node::new("pmb-b");
    std::fs::write(
        &a.config_path,
        "[[backend]]\nname = \"ollama\"\nkind = \"ollama\"\nmodel = \"qwen2.5:3b\"\n\n[[backend]]\nname = \"common\"\nkind = \"ollama\"\nmodel = \"qwen2.5:3b\"\n\n[[profile]]\nname = \"p-ollama\"\nchain = [\"ollama\"]\n\n[[profile]]\nname = \"p-common\"\nchain = [\"common\"]\n",
    )
    .unwrap();
    std::fs::write(
        &b.config_path,
        "[[backend]]\nname = \"other\"\nkind = \"ollama\"\nmodel = \"qwen2.5:3b\"\n\n[[backend]]\nname = \"common\"\nkind = \"ollama\"\nmodel = \"qwen2.5:3b\"\n",
    )
    .unwrap();
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    // F7-02 changed this: a naming_profile NEVER auto-applies
    // any more, even when its backend is known — it waits for
    // the local user's approval. The unknown-backend profile
    // is still DROPPED outright (never held, never applied).
    let b_profiles = ConfigFileProfileSink::new(&b.config_path)
        .list_profiles()
        .unwrap();
    let names: Vec<&str> = b_profiles.iter().map(|p| p.name.as_str()).collect();
    assert!(
        !names.contains(&"p-common"),
        "known-backend profile must wait for approval, not auto-apply"
    );
    assert!(
        !names.contains(&"p-ollama"),
        "unknown-backend profile must drop"
    );
    let pending = b.engine.pending_sync_approvals().unwrap();
    let held: Vec<&str> = pending.iter().map(|row| row.0.as_str()).collect();
    assert!(
        held.contains(&"profile:p-common"),
        "known-backend profile must be held: {held:?}"
    );
    assert!(
        !held.contains(&"profile:p-ollama"),
        "unknown-backend profile must not be held"
    );
    assert!(b.engine.approve_pending_sync("profile:p-common").unwrap());
    let b_profiles = ConfigFileProfileSink::new(&b.config_path)
        .list_profiles()
        .unwrap();
    let names: Vec<&str> = b_profiles.iter().map(|p| p.name.as_str()).collect();
    assert!(
        names.contains(&"p-common"),
        "approved profile must apply"
    );
}

#[test]
fn sync_selective_kind_off() {
    let a = Node::new("sko-a");
    let b = Node::new("sko-b");
    add_skill(&a, "skill-a");
    add_skill(&b, "skill-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    // B turns skills OFF: skills must not move either way,
    // while notes still sync.
    let prefs_b = PeerPrefs {
        sync_skills: false,
        ..Default::default()
    };
    let kinds_a = SyncKinds::all();
    let kinds_b = SyncKinds::from_prefs(&prefs_b);
    sync_both_asym(&a, &b, sa, sb, &kinds_a, &kinds_b);
    assert!(b.skills().get("skill-a").unwrap().is_none());
    assert!(a.skills().get("skill-b").unwrap().is_none());
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
}

#[test]
fn sync_selective_scope_shared_only() {
    let a = Node::new("sss-a");
    let b = Node::new("sss-b");
    let (_, private_id) = add_private_note(&a, "private", "mine only");
    let (_, shared_id) = add_shared_note(&a, "shared", "for both");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert!(note_body(&b, &private_id).is_none());
    assert_eq!(note_body(&b, &shared_id).as_deref(), Some("for both"));
    // The UI states this scope with one fixed sentence.
    assert!(mimint_core::friends::texts::SELECTIVE_SYNC_NOTEBOOK.contains("Chia sẻ"));
}

#[test]
fn sync_idempotent_replay() {
    let a = Node::new("idr-a");
    let b = Node::new("idr-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
    // A second exchange with no changes: nothing new applies,
    // no duplicates, no conflicts.
    let (sa, sb) = handshake_pair(&a, &b);
    let (ra, rb) = sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(ra.2, 0, "no conflicts on replay: {ra:?}");
    assert_eq!(rb.2, 0, "no conflicts on replay: {rb:?}");
    assert_eq!(
        b.notebook.list_shared_notes_with_sync_ids().unwrap().len(),
        1
    );
    let (sa, sb) = handshake_pair(&a, &b);
    let (ra, rb) = sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(ra.2 + rb.2, 0);
    assert_eq!(
        b.notebook.list_shared_notes_with_sync_ids().unwrap().len(),
        1
    );
}

#[test]
fn sync_record_size_cap() {
    let a = Node::new("rsc-a");
    let big_body = "x".repeat(300 * 1024);
    let _ = add_shared_note(&a, "big", &big_body);
    // A record over 256 KiB is refused at offer time
    // (fail-closed), never silently truncated or sent.
    let kinds = SyncKinds::all();
    assert!(a.engine.build_offer(&kinds).is_err());
}

#[test]
fn sync_tombstone_90_day_purge() {
    let (a, cell_a) = Node::manual("t90-a", 1_000_000);
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    a.engine
        .create_tombstone(&note_id, RecordKind::NotebookNote)
        .unwrap();
    assert_eq!(a.registry.list_tombstones().unwrap().len(), 1);
    // 89 days: still retained.
    cell_a.fetch_add(89 * 24 * 60 * 60 * 1000, Ordering::SeqCst);
    assert!(a.engine.purge_expired_tombstones().unwrap().is_empty());
    assert_eq!(a.registry.list_tombstones().unwrap().len(), 1);
    // 91 days: purged, with a receipt for the resurrection
    // notice.
    cell_a.fetch_add(2 * 24 * 60 * 60 * 1000, Ordering::SeqCst);
    let purged = a.engine.purge_expired_tombstones().unwrap();
    assert_eq!(purged.len(), 1);
    assert_eq!(purged[0].sync_id, note_id);
    assert!(a.registry.list_tombstones().unwrap().is_empty());
    let receipt = a.registry.take_purge_receipt(&note_id).unwrap();
    assert!(receipt.is_some(), "purge receipt must exist");
}

// ----------------------------------------------------------------
// §5.5 / §6.4 / §7.4 helpers
// ----------------------------------------------------------------

fn handshake_to_hub_at(client: &Node, addr: SocketAddr) -> Result<Session, MimintError> {
    let stream = TcpStream::connect(addr).map_err(|e| MimintError::Io(e.to_string()))?;
    handshake_initiator(
        stream,
        &client.identity,
        &client.registry,
        &SessionConfig::default(),
        &client.clock,
        None,
    )
}

fn send_driver_request(session: &mut Session, method: &str, request_id: &str) -> DriverResponse {
    let req = DriverRequest {
        v: PROTOCOL_VERSION,
        kind: "driver_request".to_string(),
        method: method.to_string(),
        params: serde_json::json!({}),
        request_id: request_id.to_string(),
    };
    session
        .send(
            MessageKind::DriverRequest,
            &serde_json::to_vec(&req).unwrap(),
        )
        .unwrap();
    let (kind, payload) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::DriverResponse);
    serde_json::from_slice(&payload).unwrap()
}

fn recv_approval_card(session: &mut Session) -> ApprovalCard {
    let (kind, payload) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::ApprovalCard);
    serde_json::from_slice(&payload).unwrap()
}

fn send_decision(session: &mut Session, card: &ApprovalCard, decision: &str) {
    // The decision's device_id binds it to the card's origin
    // device (§6.2 triple); the deciding peer is
    // authenticated by the session itself, not by this field.
    let d = ApprovalDecision {
        v: PROTOCOL_VERSION,
        kind: "approval_decision".to_string(),
        device_id: card.device_id.clone(),
        agent_id: card.agent_id.clone(),
        request_id: card.request_id.clone(),
        decision: decision.to_string(),
        decided_ms: mimint_core::audit::now_ms(),
    };
    session
        .send(
            MessageKind::ApprovalDecision,
            &serde_json::to_vec(&d).unwrap(),
        )
        .unwrap();
}

fn ping_pong(session: &mut Session) {
    session.send(MessageKind::Ping, &[]).unwrap();
    let (kind, _) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::Pong);
}

fn audit_contains(node: &Node, needle: &str) -> bool {
    let path = node.dir.join("audit.jsonl");
    std::fs::read_to_string(path)
        .map(|content| content.contains(needle))
        .unwrap_or(false)
}

fn poll_audit_contains(node: &Node, needle: &str, tries: u32) -> bool {
    for _ in 0..tries {
        if audit_contains(node, needle) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

fn make_package(a: &Node, b: &Node, task: &str) -> HandoffPackage {
    a.handoff
        .build_package(
            &a.identity,
            &b.device_id(),
            task.to_string(),
            vec!["step 1 done".to_string()],
            "next step".to_string(),
            vec!["SOUL.md".to_string()],
            vec![],
            String::new(),
            "body",
        )
        .unwrap()
}

fn offer_over_session(session: &mut Session, package: &HandoffPackage) {
    let offer = HandoffOffer {
        v: PROTOCOL_VERSION,
        kind: "handoff_offer".to_string(),
        package: package.clone(),
    };
    session
        .send(
            MessageKind::HandoffOffer,
            &serde_json::to_vec(&offer).unwrap(),
        )
        .unwrap();
}

/// True when a reply frame is already waiting on `session`
/// (peek consumes nothing, so this never disturbs the
/// session). Used to prove the hub sends no accept before the
/// receiver's user decides (§7.3).
fn reply_waiting(session: &Session, wait_ms: u64) -> bool {
    let Ok(peek) = session.stream_clone() else {
        return false;
    };
    let _ = peek.set_read_timeout(Some(std::time::Duration::from_millis(wait_ms)));
    let mut buf = [0u8; 1];
    match peek.peek(&mut buf) {
        Ok(0) => false,
        Ok(_) => true,
        Err(_) => false,
    }
}

/// A hub pair where `a` offers handoffs to `b`'s hub and `b`
/// offers back to `a`'s hub (two-hub topology: every body runs
/// a hub; a handoff and its completion travel on sessions in
/// opposite directions).
fn start_two_hubs(a: &Node, b: &Node, backend: Arc<dyn DriverBackend>) -> (Arc<Hub>, Arc<Hub>) {
    let ha = start_hub(a, HubConfig::default(), backend.clone());
    let hb = start_hub(b, HubConfig::default(), backend);
    (ha, hb)
}

// ----------------------------------------------------------------
// §5.5 — hub
// ----------------------------------------------------------------

#[test]
fn hub_loopback_by_default() {
    let a = Node::new("hlb-a");
    let b = Node::new("hlb-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend);
    assert!(!HubConfig::default().listen_lan, "LAN is opt-in");
    assert_eq!(hub.exposure_state(), HubExposure::Loopback);
    // Loopback connects.
    let mut session = handshake_to_hub(&a, &hub);
    ping_pong(&mut session);
    // A LAN address cannot even reach the loopback-only
    // listener.
    let lan = Node::lan_addr();
    assert!(!lan.is_loopback(), "test needs a real LAN address");
    let lan_addr = SocketAddr::new(lan, hub.friends_port());
    assert!(
        handshake_to_hub_at(&a, lan_addr).is_err(),
        "LAN must not reach a loopback-only hub"
    );
    hub.stop();
}

#[test]
fn hub_lan_requires_explicit_opt_in() {
    let a = Node::new("hlo-a");
    let b = Node::new("hlo-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    // Default config: LAN refused.
    let hub = start_hub(&b, HubConfig::default(), backend.clone());
    let lan = Node::lan_addr();
    assert!(!lan.is_loopback(), "test needs a real LAN address");
    let lan_addr = SocketAddr::new(lan, hub.friends_port());
    assert!(handshake_to_hub_at(&a, lan_addr).is_err());
    hub.stop();
    // Explicit opt-in: LAN works for a paired peer.
    let config = HubConfig {
        listen_lan: true,
        ..Default::default()
    };
    let hub = start_hub(&b, config, backend.clone());
    assert_eq!(hub.exposure_state(), HubExposure::LanOn);
    // NOTE: the second hub listens on fresh ports — the LAN
    // address must be recomputed for it.
    let lan_addr = SocketAddr::new(lan, hub.friends_port());
    let mut session = handshake_to_hub_at(&a, lan_addr).expect("LAN must work once opted in");
    ping_pong(&mut session);
    hub.stop();
}

#[test]
fn hub_no_auth_no_control() {
    let a = Node::new("hnc-a");
    let b = Node::new("hnc-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend.clone());
    // A driver request with NO handshake: the hub must close
    // the connection without running anything.
    let addr = format!("127.0.0.1:{}", hub.friends_port());
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let json = serde_json::to_vec(&serde_json::json!({
        "v": PROTOCOL_VERSION,
        "kind": "driver_request",
        "method": "screenshot",
        "params": {},
        "request_id": "no-auth-1",
    }))
    .unwrap();
    write_frame(&mut stream, &json, &[]).unwrap();
    assert!(
        read_frame(&mut stream).is_err(),
        "unauthenticated control must close"
    );
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(backend.call_count(), 0, "no driver call without auth");
    hub.stop();
}

#[test]
fn hub_token_path_removed() {
    // (a) No driver-token code anywhere in the friends
    // modules: the path was removed (§5.2), not renamed.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/friends");
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        if text.contains("driver_token") {
            hits.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    assert!(hits.is_empty(), "driver_token must not exist: {hits:?}");
    // (b) Behavioural: a driver request carrying a `token`
    // field is refused at parse time (`deny_unknown_fields`)
    // — the backend never runs, and the session stays usable.
    let a = Node::new("htp-a");
    let b = Node::new("htp-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend.clone());
    let mut session = handshake_to_hub(&a, &hub);
    let json = serde_json::to_vec(&serde_json::json!({
        "v": PROTOCOL_VERSION,
        "kind": "driver_request",
        "method": "screenshot",
        "params": {},
        "request_id": "token-1",
        "token": "stolen-token-value",
    }))
    .unwrap();
    session.send(MessageKind::DriverRequest, &json).unwrap();
    // No response comes back for the refused request, and the
    // session is still alive: a ping still gets a pong.
    ping_pong(&mut session);
    assert!(
        !backend.called("screenshot"),
        "token request must not drive"
    );
    hub.stop();
}

#[test]
fn hub_revoked_device_rejected() {
    let a = Node::new("hrv-a");
    let b = Node::new("hrv-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend);
    b.registry
        .revoke_peer(&a.device_id(), mimint_core::audit::now_ms())
        .unwrap();
    let lo_addr: SocketAddr = format!("127.0.0.1:{}", hub.friends_port()).parse().unwrap();
    assert!(
        handshake_to_hub_at(&a, lo_addr).is_err(),
        "a revoked peer must not handshake"
    );
    hub.stop();
}

#[test]
fn hub_replay_rejected() {
    let a = Node::new("hrp-a");
    let b = Node::new("hrp-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend);
    let hub_addr: SocketAddr = format!("127.0.0.1:{}", hub.friends_port()).parse().unwrap();
    // Record one real handshake through a proxy.
    let record = Arc::new(Mutex::new(Vec::new()));
    let proxy = start_proxy(hub_addr, ProxyMode::Passthrough, Arc::clone(&record));
    let mut session = handshake_to_hub_at(&a, proxy).unwrap();
    ping_pong(&mut session);
    drop(session);
    let frames = record.lock().unwrap().clone();
    assert!(frames.len() >= 3, "must record hello+auth+confirm");
    // Replay the recorded frames verbatim on a fresh
    // connection: the hub's listener hello carries a fresh
    // nonce, so the replayed auth signature (bound to the old
    // transcript) must fail.
    let mut stream = TcpStream::connect(hub_addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    for (json, payload) in &frames {
        // The hub closes the connection once the replayed
        // auth fails, so later writes may fail: stop there.
        if write_frame(&mut stream, json, payload).is_err() {
            break;
        }
        // Read the hub's reply to the hello before sending
        // the auth, like a real client would.
        if String::from_utf8_lossy(json).contains("\"kind\":\"hello\"") {
            let _ = read_frame(&mut stream);
        }
    }
    assert!(
        read_frame(&mut stream).is_err(),
        "replayed handshake must be refused"
    );
    hub.stop();
}

#[test]
fn hub_no_unauthenticated_routes_scan() {
    let a = Node::new("hns-a");
    let b = Node::new("hns-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend.clone());
    // Every kind except the two designed entry points
    // (hello, pair_request) must be refused pre-auth.
    let kinds = [
        "auth",
        "handshake_error",
        "session_confirm",
        "rekey",
        "rekey_ack",
        "pair_response",
        "pair_confirm",
        "pair_accept",
        "pairing_error",
        "sync_offer",
        "sync_request",
        "sync_push",
        "sync_ack",
        "approval_card",
        "approval_decision",
        "handoff_offer",
        "handoff_accept",
        "handoff_reject",
        "handoff_complete",
        "ping",
        "pong",
        "revoke_notice",
        "driver_request",
        "driver_response",
    ];
    assert_eq!(kinds.len(), 24);
    for kind in kinds {
        let addr = format!("127.0.0.1:{}", hub.friends_port());
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        let json = serde_json::to_vec(&serde_json::json!({
            "v": PROTOCOL_VERSION,
            "kind": kind,
        }))
        .unwrap();
        write_frame(&mut stream, &json, &[]).unwrap();
        assert!(
            read_frame(&mut stream).is_err(),
            "kind {kind} must be refused without auth"
        );
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(backend.call_count(), 0, "no driver call from the scan");
    hub.stop();
}

#[test]
fn hub_remote_driver_indicator_and_kill() {
    let a = Node::new("hdi-a");
    let b = Node::new("hdi-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend.clone());
    let mut session = handshake_to_hub(&a, &hub);
    let response = send_driver_request(&mut session, "screenshot", "drv-1");
    assert!(response.ok, "read method must run for an owner peer");
    assert!(backend.called("screenshot"));
    // The indicator shows the driver, and the session is
    // flagged.
    let indicator = hub.driver_active().expect("driver must be indicated");
    assert_eq!(indicator.peer_device_id, a.device_id());
    assert_eq!(hub.exposure_state(), HubExposure::DriverConnected);
    assert!(hub.sessions().iter().any(|s| s.is_driver));
    // The kill button cuts every driver session at once.
    let killed = hub.kill_all_drivers();
    assert_eq!(killed.len(), 1);
    assert!(hub.sessions().is_empty());
    assert!(hub.driver_active().is_none());
    assert!(session.recv().is_err(), "killed session must close");
    hub.stop();
}

#[test]
fn hub_close_lan_lists_sessions_and_keeps_pairing() {
    let a = Node::new("hcl-a");
    let b = Node::new("hcl-b");
    let c = Node::new("hcl-c");
    pair_nodes(&a, &b, SasFlow::Compare);
    pair_nodes(&c, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let config = HubConfig {
        listen_lan: true,
        ..Default::default()
    };
    let hub = start_hub(&b, config, backend.clone());
    let lan = Node::lan_addr();
    assert!(!lan.is_loopback(), "test needs a real LAN address");
    let lan_addr = SocketAddr::new(lan, hub.friends_port());
    // A connects over LAN, C over loopback.
    let mut session_lan = handshake_to_hub_at(&a, lan_addr).expect("LAN session");
    let mut session_lo = handshake_to_hub(&c, &hub);
    assert_eq!(hub.sessions().len(), 2);
    // Close LAN: lists the cut sessions, keeps loopback.
    let closed = hub.close_lan();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0], a.device_id());
    assert_eq!(hub.sessions().len(), 1);
    assert_eq!(hub.sessions()[0].peer_device_id, c.device_id());
    assert!(session_lan.recv().is_err(), "LAN session must be cut");
    ping_pong(&mut session_lo);
    // A new LAN handshake is refused after close, and pairing
    // still works over loopback.
    assert!(handshake_to_hub_at(&a, lan_addr).is_err());
    let mut session_again = handshake_to_hub(&a, &hub);
    ping_pong(&mut session_again);
    hub.stop();
}

// ----------------------------------------------------------------
// §6.4 — peer approval
// ----------------------------------------------------------------

#[test]
fn approve_from_peer_runs_action() {
    let h = Node::new("afp-h");
    let p = Node::new("afp-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let mut prefs = h.registry.get_prefs(&p.device_id()).unwrap();
    prefs.approve_allowed = true;
    h.registry.set_prefs(&p.device_id(), &prefs).unwrap();
    let backend = MockBackend::new();
    let hub = start_hub(&h, HubConfig::default(), backend.clone());
    let (card, rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-allow-1", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    assert!(
        !backend.called("shell.exec"),
        "no action before the decision"
    );
    let mut session = handshake_to_hub(&p, &hub);
    // The hub delivers the card when the session opens (§6.2).
    let got = recv_approval_card(&mut session);
    assert_eq!(got.request_id, card.request_id);
    send_decision(&mut session, &got, "approve");
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(outcome, DecisionOutcome::Approved);
    // The agent runs the action only now, after approval.
    backend
        .execute("shell.exec", &serde_json::json!({}))
        .unwrap();
    assert!(backend.called("shell.exec"));
    // Audit attribution (§6.3): who decided, on which channel.
    assert!(
        poll_audit_contains(
            &h,
            &format!("\"deciding_device_id\":\"{}\"", p.device_id()),
            20
        ),
        "audit must name the deciding device"
    );
    assert!(
        audit_contains(&h, "\"decision_channel\":\"peer\""),
        "audit must record the peer channel"
    );
    hub.stop();
}

#[test]
fn deny_from_peer_blocks_action() {
    let h = Node::new("dfp-h");
    let p = Node::new("dfp-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let mut prefs = h.registry.get_prefs(&p.device_id()).unwrap();
    prefs.approve_allowed = true;
    h.registry.set_prefs(&p.device_id(), &prefs).unwrap();
    let backend = MockBackend::new();
    let hub = start_hub(&h, HubConfig::default(), backend.clone());
    let (_card, rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-deny-1", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    let mut session = handshake_to_hub(&p, &hub);
    let got = recv_approval_card(&mut session);
    send_decision(&mut session, &got, "deny");
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(outcome, DecisionOutcome::Denied);
    assert!(!backend.called("shell.exec"), "denied action must not run");
    hub.stop();
}

#[test]
fn approve_replay_rejected() {
    let h = Node::new("arr-h");
    let p = Node::new("arr-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let (_card, rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-replay-1", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    let decision = ApprovalDecision {
        v: PROTOCOL_VERSION,
        kind: "approval_decision".to_string(),
        device_id: h.device_id(),
        agent_id: "agent-test".to_string(),
        request_id: "req-replay-1".to_string(),
        decision: "approve".to_string(),
        decided_ms: mimint_core::audit::now_ms(),
    };
    assert_eq!(
        h.relay.apply_decision(&decision, &p.device_id()).unwrap(),
        DecisionOutcome::Approved
    );
    // The same decision again: single use — refused.
    assert!(h.relay.apply_decision(&decision, &p.device_id()).is_err());
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        DecisionOutcome::Approved
    );
    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(300))
        .is_err());
}

#[test]
fn approve_cross_agent_rejected() {
    let h = Node::new("aca-h");
    let p = Node::new("aca-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let (_card, _rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-xagent-1", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    // A decision for another agent's card: refused.
    let decision = ApprovalDecision {
        v: PROTOCOL_VERSION,
        kind: "approval_decision".to_string(),
        device_id: h.device_id(),
        agent_id: "agent-other".to_string(),
        request_id: "req-xagent-1".to_string(),
        decision: "approve".to_string(),
        decided_ms: mimint_core::audit::now_ms(),
    };
    assert!(h.relay.apply_decision(&decision, &p.device_id()).is_err());
}

#[test]
fn approve_cross_device_replay_rejected() {
    let h = Node::new("acd-h");
    let p1 = Node::new("acd-p1");
    let p2 = Node::new("acd-p2");
    pair_nodes(&h, &p1, SasFlow::Compare);
    pair_nodes(&h, &p2, SasFlow::Compare);
    let (_card, _rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-xdev-1", "shell.exec"),
            &p1.device_id(),
            true,
        )
        .unwrap();
    // P1's decision replayed through P2's session: the
    // decision must come from the card's target peer.
    let decision = ApprovalDecision {
        v: PROTOCOL_VERSION,
        kind: "approval_decision".to_string(),
        device_id: h.device_id(),
        agent_id: "agent-test".to_string(),
        request_id: "req-xdev-1".to_string(),
        decision: "approve".to_string(),
        decided_ms: mimint_core::audit::now_ms(),
    };
    assert!(h.relay.apply_decision(&decision, &p2.device_id()).is_err());
    assert!(h.relay.apply_decision(&decision, &p1.device_id()).is_ok());
}

#[test]
fn approve_after_expiry_denied() {
    let (h, cell) = Node::manual("aex-h", 1_000_000);
    let p = Node::new("aex-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let (_card, rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-exp-1", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    // 61 seconds later: the card is expired. A late Allow is
    // denied, and the waiter is told Denied.
    cell.fetch_add(61_000, Ordering::SeqCst);
    let decision = ApprovalDecision {
        v: PROTOCOL_VERSION,
        kind: "approval_decision".to_string(),
        device_id: h.device_id(),
        agent_id: "agent-test".to_string(),
        request_id: "req-exp-1".to_string(),
        decision: "approve".to_string(),
        decided_ms: 1_061_000,
    };
    assert!(h.relay.apply_decision(&decision, &p.device_id()).is_err());
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        DecisionOutcome::Denied
    );
}

#[test]
fn approve_guest_rejected() {
    let h = Node::new("agr-h");
    let p = Node::new("agr-p");
    // Relay level: no card is ever created for a guest.
    assert!(h
        .relay
        .create_card(
            origin_for(&h, "req-guest-1", "shell.exec"),
            &p.device_id(),
            false
        )
        .is_err());
    // Pair with H granting Guest (P's session_confirm claim
    // is Guest, matching the grant — a post-pairing downgrade
    // would stale the claim and the handshake would refuse
    // for that reason instead).
    let (entry_h, _) = ceremony_with(
        &h,
        &p,
        SasFlow::Compare,
        &AutoInitiatorUi {
            trust: mimint_core::friends::registry::Trust::Guest,
        },
        &auto_listener(),
        None,
        None,
    );
    assert_eq!(
        entry_h.unwrap().trust,
        mimint_core::friends::registry::Trust::Guest
    );
    // Hub level: a guest's decision is refused and the
    // session is closed.
    let (_card, rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-guest-2", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    let backend = MockBackend::new();
    let hub = start_hub(&h, HubConfig::default(), backend.clone());
    let mut session = handshake_to_hub(&p, &hub);
    let decision = ApprovalDecision {
        v: PROTOCOL_VERSION,
        kind: "approval_decision".to_string(),
        device_id: h.device_id(),
        agent_id: "agent-test".to_string(),
        request_id: "req-guest-2".to_string(),
        decision: "approve".to_string(),
        decided_ms: mimint_core::audit::now_ms(),
    };
    session
        .send(
            MessageKind::ApprovalDecision,
            &serde_json::to_vec(&decision).unwrap(),
        )
        .unwrap();
    assert!(
        session.recv().is_err(),
        "guest decision must close the session"
    );
    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(300))
        .is_err());
    hub.stop();
}

#[test]
fn approve_peer_cannot_create_card() {
    let h = Node::new("apc-h");
    let p = Node::new("apc-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&h, HubConfig::default(), backend.clone());
    let mut session = handshake_to_hub(&p, &hub);
    // A peer sending an approval_card: refused + session
    // closed + audit. Cards only travel hub → peer (§6.2).
    let card = ApprovalCard {
        v: PROTOCOL_VERSION,
        kind: "approval_card".to_string(),
        device_id: p.device_id(),
        device_name: "attacker".to_string(),
        agent_id: "agent-test".to_string(),
        request_id: "req-fake-1".to_string(),
        action: "shell.exec".to_string(),
        detail: serde_json::json!({}),
        created_ms: mimint_core::audit::now_ms(),
        expires_ms: mimint_core::audit::now_ms() + 60_000,
    };
    session
        .send(
            MessageKind::ApprovalCard,
            &serde_json::to_vec(&card).unwrap(),
        )
        .unwrap();
    assert!(
        session.recv().is_err(),
        "forged card must close the session"
    );
    assert!(
        poll_audit_contains(&h, "\"approval_card_rejected\"", 20),
        "refusal must be audited"
    );
    hub.stop();
}

#[test]
fn approve_no_batch() {
    let h = Node::new("anb-h");
    let p = Node::new("anb-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let (card1, rx1) = h
        .relay
        .create_card(
            origin_for(&h, "req-batch-1", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    let (card2, rx2) = h
        .relay
        .create_card(
            origin_for(&h, "req-batch-2", "file.write"),
            &p.device_id(),
            true,
        )
        .unwrap();
    // One card, one action: the card carries a single
    // `action` string, never an `actions` array.
    let json = serde_json::to_value(&card1).unwrap();
    assert!(json.get("action").is_some());
    assert!(json.get("actions").is_none());
    // Deciding the first card leaves the second untouched.
    let decision = ApprovalDecision {
        v: PROTOCOL_VERSION,
        kind: "approval_decision".to_string(),
        device_id: h.device_id(),
        agent_id: "agent-test".to_string(),
        request_id: "req-batch-1".to_string(),
        decision: "approve".to_string(),
        decided_ms: mimint_core::audit::now_ms(),
    };
    assert_eq!(
        h.relay.apply_decision(&decision, &p.device_id()).unwrap(),
        DecisionOutcome::Approved
    );
    assert_eq!(
        rx1.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        DecisionOutcome::Approved
    );
    assert!(h
        .relay
        .pending_cards()
        .iter()
        .any(|c| c.request_id == card2.request_id));
    assert!(rx2
        .recv_timeout(std::time::Duration::from_millis(300))
        .is_err());
}

#[test]
fn approve_requires_explicit_peer_opt_in() {
    let h = Node::new("aro-h");
    let p = Node::new("aro-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    // Default: OFF — no card is created at all.
    let prefs = h.registry.get_prefs(&p.device_id()).unwrap();
    assert!(!prefs.approve_allowed, "peer approval defaults OFF");
    assert!(h
        .relay
        .create_card(
            origin_for(&h, "req-opt-1", "shell.exec"),
            &p.device_id(),
            false
        )
        .is_err());
    // Opt in, create the card, then opt OUT again before the
    // peer connects: the hub must not deliver it, and a
    // sneaked decision must be refused.
    let mut prefs = prefs;
    prefs.approve_allowed = true;
    h.registry.set_prefs(&p.device_id(), &prefs).unwrap();
    let (_card, rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-opt-2", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    prefs.approve_allowed = false;
    h.registry.set_prefs(&p.device_id(), &prefs).unwrap();
    let backend = MockBackend::new();
    let hub = start_hub(&h, HubConfig::default(), backend.clone());
    let mut session = handshake_to_hub(&p, &hub);
    // A ping gets a pong — and no card.
    session.send(MessageKind::Ping, &[]).unwrap();
    let (kind, _) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::Pong, "no card without opt-in");
    // Sneaked decision: refused, session closed.
    let decision = ApprovalDecision {
        v: PROTOCOL_VERSION,
        kind: "approval_decision".to_string(),
        device_id: h.device_id(),
        agent_id: "agent-test".to_string(),
        request_id: "req-opt-2".to_string(),
        decision: "approve".to_string(),
        decided_ms: mimint_core::audit::now_ms(),
    };
    session
        .send(
            MessageKind::ApprovalDecision,
            &serde_json::to_vec(&decision).unwrap(),
        )
        .unwrap();
    assert!(
        session.recv().is_err(),
        "sneaked decision must close the session"
    );
    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(300))
        .is_err());
    hub.stop();
}

#[test]
fn approve_card_shows_full_origin() {
    let h = Node::new("aco-h");
    let p = Node::new("aco-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let (card, _rx) = h
        .relay
        .create_card(
            origin_for(&h, "req-origin-1", "shell.exec"),
            &p.device_id(),
            true,
        )
        .unwrap();
    let json = serde_json::to_value(&card).unwrap();
    // Full origin (§6.2): hub, answering device, agent, time
    // left, the complete action + params — never shortened.
    for field in [
        "device_id",
        "device_name",
        "agent_id",
        "request_id",
        "action",
        "detail",
        "created_ms",
        "expires_ms",
    ] {
        assert!(json.get(field).is_some(), "card must carry {field}");
    }
    assert_eq!(json["device_id"].as_str().unwrap(), h.device_id());
    assert_eq!(json["agent_id"].as_str().unwrap(), "agent-test");
    assert_eq!(json["action"].as_str().unwrap(), "shell.exec");
    assert_eq!(json["detail"]["method"].as_str().unwrap(), "shell.exec");
    assert!(json["expires_ms"].as_u64().unwrap() > json["created_ms"].as_u64().unwrap());
    // A peer card is distinguishable from a local card by
    // content, not just colour: it names a different origin
    // device than the answering device.
    assert_ne!(json["device_id"].as_str().unwrap(), p.device_id());
}

// ----------------------------------------------------------------
// §7.4 — handoff
// ----------------------------------------------------------------

#[test]
fn handoff_two_instances_continues_next_step() {
    let a = Node::new("hti-a");
    let b = Node::new("hti-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let package = make_package(&a, &b, "write the report");
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    // B verifies and stores the package as PENDING (§7.3):
    // no accept before the receiver's user decides, no lease
    // taken yet.
    assert!(
        !reply_waiting(&session, 500),
        "no accept before the user decides"
    );
    assert!(
        hub_b
            .pending_handoffs()
            .iter()
            .any(|item| item.package.package_id == package.package_id),
        "verified offer must be listed as pending"
    );
    assert!(b.handoff.lease_for("body").is_none());
    // The user presses "Tiếp tục ở đây": accept + lease.
    hub_b.decide_handoff(&package.package_id, true).unwrap();
    assert!(
        reply_waiting(&session, 5_000),
        "accept must arrive after the user decides"
    );
    let (kind, payload) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffAccept);
    let accept: HandoffAccept = serde_json::from_slice(&payload).unwrap();
    assert_eq!(accept.package_id, package.package_id);
    a.handoff.mark_handed_off(&package.package_id);
    assert!(a.handoff.is_handed_off(&package.package_id));
    let lease = b
        .handoff
        .lease_for("body")
        .expect("B must hold the lease");
    assert_eq!(lease.package_id, package.package_id);
    assert_eq!(lease.holder_device_id, b.device_id());
    // The package content itself arrived intact: the next
    // step is what B continues.
    assert_eq!(package.next_step, "next step");
    assert_eq!(package.task_text, "write the report");
    hub_b.stop();
}

#[test]
fn handoff_lease_conflict_second_rejected() {
    let (a, _cell_a) = Node::manual("hlc-a", 1_000_000);
    let (b, cell_b) = Node::manual("hlc-b", 1_000_000);
    let (c, cell_c) = Node::manual("hlc-c", 1_000_000);
    pair_nodes(&a, &b, SasFlow::Compare);
    pair_nodes(&c, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let package_a = make_package(&a, &b, "task A");
    let mut session_a = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session_a, &package_a);
    assert!(!reply_waiting(&session_a, 500));
    hub_b.decide_handoff(&package_a.package_id, true).unwrap();
    assert!(reply_waiting(&session_a, 5_000));
    let (kind, _) = session_a.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffAccept);
    // C offers while B holds A's lease: lease_busy.
    let package_c = make_package(&c, &b, "task C");
    let mut session_c = handshake_to_hub(&c, &hub_b);
    offer_over_session(&mut session_c, &package_c);
    let (kind, payload) = session_c.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffReject);
    let reject: HandoffReject = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reject.reason, "lease_busy");
    // After the lease expires, C's offer goes through.
    cell_b.fetch_add(11 * 60 * 1000, Ordering::SeqCst);
    cell_c.fetch_add(11 * 60 * 1000, Ordering::SeqCst);
    let package_c2 = make_package(&c, &b, "task C retry");
    offer_over_session(&mut session_c, &package_c2);
    // The retry is a fresh offer: pending until C's user
    // decides (the lease is free again after expiry).
    assert!(!reply_waiting(&session_c, 500));
    hub_b.decide_handoff(&package_c2.package_id, true).unwrap();
    assert!(reply_waiting(&session_c, 5_000));
    let (kind, _) = session_c.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffAccept);
    hub_b.stop();
}

#[test]
fn handoff_forged_package_rejected() {
    let a = Node::new("hfp-a");
    let b = Node::new("hfp-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    // Manager level: a tampered package fails verification.
    let mut package = make_package(&a, &b, "task");
    package.next_step = "attacker step".to_string();
    assert!(b
        .handoff
        .verify_package(&package, a.identity.public_key(), &b.device_id())
        .is_err());
    // Hub level: the forged offer gets a reject, no lease,
    // and an audit entry (§7.3 step 4).
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    let (kind, _) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffReject);
    assert!(b.handoff.lease_for("body").is_none());
    assert!(
        poll_audit_contains(&b, "\"handoff_forged\"", 20),
        "forged package must be audited"
    );
    hub_b.stop();
}

#[test]
fn handoff_wrong_recipient_rejected() {
    let a = Node::new("hwr-a");
    let b = Node::new("hwr-b");
    let c = Node::new("hwr-c");
    pair_nodes(&a, &b, SasFlow::Compare);
    pair_nodes(&a, &c, SasFlow::Compare);
    let package = make_package(&a, &b, "task for B");
    // C verifies a package addressed to B: refused.
    assert!(c
        .handoff
        .verify_package(&package, a.identity.public_key(), &c.device_id())
        .is_err());
    // And B's hub holds the same package as pending for B —
    // accepted only after B's user decides.
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    assert!(!reply_waiting(&session, 500));
    hub_b.decide_handoff(&package.package_id, true).unwrap();
    assert!(reply_waiting(&session, 5_000));
    let (kind, _) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffAccept);
    hub_b.stop();
}

#[test]
fn handoff_expired_lease_rejected() {
    let (a, _cell_a) = Node::manual("hel-a", 1_000_000);
    let (b, cell_b) = Node::manual("hel-b", 1_000_000);
    pair_nodes(&a, &b, SasFlow::Compare);
    let package = make_package(&a, &b, "task");
    // 11 minutes pass on B before the offer arrives: the
    // lease is expired, B refuses and does not act.
    cell_b.fetch_add(11 * 60 * 1000, Ordering::SeqCst);
    assert!(b
        .handoff
        .verify_package(&package, a.identity.public_key(), &b.device_id())
        .is_err());
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    let (kind, _) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffReject);
    assert!(b.handoff.lease_for("body").is_none());
    hub_b.stop();
}

#[test]
fn handoff_no_secret_in_package() {
    let a = Node::new("hns2-a");
    let b = Node::new("hns2-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let (_, _private_id) = add_private_note(&a, "diary", "SECRET-DIARY-9q2w");
    let (_, shared_id) = add_shared_note(&a, "shared", "shared context line");
    let context = shared_notebook_context(&a.notebook).unwrap();
    assert!(context.contains("shared context line"));
    assert!(
        !context.contains("SECRET-DIARY-9q2w"),
        "private notes never enter the context"
    );
    let package = a
        .handoff
        .build_package(
            &a.identity,
            &b.device_id(),
            "task".to_string(),
            vec![],
            "next".to_string(),
            vec![],
            vec![],
            context,
            "body",
        )
        .unwrap();
    // The package carries exactly the §7.1 allow-listed
    // fields — no secret-shaped field exists to leak through.
    let json = serde_json::to_value(&package).unwrap();
    let keys: std::collections::BTreeSet<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    let expected: std::collections::BTreeSet<&str> = [
        "v",
        "kind",
        "package_id",
        "from_device_id",
        "to_device_id",
        "task_text",
        "done_steps",
        "next_step",
        "file_refs",
        "tab_refs",
        "notebook_context",
        "lease_id",
        "actuator",
        "created_ms",
        "expires_ms",
        "version",
        "sig",
    ]
    .into_iter()
    .collect();
    assert_eq!(keys, expected);
    let raw = serde_json::to_string(&package).unwrap();
    assert!(!raw.contains("SECRET-DIARY-9q2w"));
    let _ = shared_id;
}

#[test]
fn handoff_reject_returns_lease() {
    let a = Node::new("hrr-a");
    let b = Node::new("hrr-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    // A forged offer is rejected: A keeps the right to act
    // (never marked handed off) and B holds no lease.
    let mut package = make_package(&a, &b, "task");
    package.next_step = "attacker step".to_string();
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    let (kind, _) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffReject);
    assert!(!a.handoff.is_handed_off(&package.package_id));
    assert!(b.handoff.lease_for("body").is_none());
    // A valid offer the receiver declines (user presses
    // "Từ chối" — no lease is ever taken) has the same
    // outcome: A stays free to act.
    let package2 = make_package(&a, &b, "task 2");
    assert!(b
        .handoff
        .verify_package(&package2, a.identity.public_key(), &b.device_id())
        .is_ok());
    assert!(b.handoff.lease_for("body").is_none());
    assert!(!a.handoff.is_handed_off(&package2.package_id));
    hub_b.stop();
}

#[test]
fn handoff_no_accept_before_user_decision() {
    let a = Node::new("hnb-a");
    let b = Node::new("hnb-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let package = make_package(&a, &b, "task");
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    // Half a second, no reply: the hub must not answer a
    // verified offer before its user decides (§7.3).
    assert!(
        !reply_waiting(&session, 500),
        "the hub answered before the user decided"
    );
    // The verified package is listed as pending on B, and no
    // lease is held anywhere.
    assert!(
        hub_b
            .pending_handoffs()
            .iter()
            .any(|item| item.package.package_id == package.package_id),
        "verified offer must be pending on the receiver"
    );
    assert!(b.handoff.lease_for("body").is_none());
    assert!(!a.handoff.is_handed_off(&package.package_id));
    // A user reject also leaves the sender free to act.
    hub_b.decide_handoff(&package.package_id, false).unwrap();
    assert!(reply_waiting(&session, 5_000));
    let (kind, payload) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffReject);
    let reject: HandoffReject = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reject.reason, "rejected_by_user");
    assert!(b.handoff.lease_for("body").is_none());
    assert!(!a.handoff.is_handed_off(&package.package_id));
    hub_b.stop();
}

#[test]
fn handoff_accept_after_user_decision_sends_accept() {
    let a = Node::new("haa-a");
    let b = Node::new("haa-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let package = make_package(&a, &b, "task");
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    assert!(!reply_waiting(&session, 500));
    // Only the user's decision produces the accept — and the
    // lease moves to the receiver with it.
    let decided = hub_b.decide_handoff(&package.package_id, true).unwrap();
    assert_eq!(decided.task_text, "task");
    assert!(reply_waiting(&session, 5_000));
    let (kind, payload) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffAccept);
    let accept: HandoffAccept = serde_json::from_slice(&payload).unwrap();
    assert_eq!(accept.package_id, package.package_id);
    let lease = b
        .handoff
        .lease_for("body")
        .expect("B must hold the lease");
    assert_eq!(lease.package_id, package.package_id);
    // Deciding the same package twice fails: it is no longer
    // pending.
    assert!(hub_b.decide_handoff(&package.package_id, true).is_err());
    hub_b.stop();
}

#[test]
fn handoff_pending_expires_returns_lease() {
    let (a, _cell_a) = Node::manual("hpe-a", 1_000_000);
    let (b, cell_b) = Node::manual("hpe-b", 1_000_000);
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let package = make_package(&a, &b, "task");
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    // The hub stores the offer as pending asynchronously: poll
    // for it instead of asserting immediately after the send.
    let mut waited = 0;
    while !hub_b
        .pending_handoffs()
        .iter()
        .any(|item| item.package.package_id == package.package_id)
        && waited < 5_000
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
        waited += 100;
    }
    assert!(
        hub_b
            .pending_handoffs()
            .iter()
            .any(|item| item.package.package_id == package.package_id),
        "offer must be pending before expiry"
    );
    // 8 minutes + 1 second pass on B: now + 2 minutes reaches
    // the 10-minute lease expiry, so B treats the pending
    // package as expired (§7.3 step 5, conservative).
    cell_b.fetch_add(8 * 60 * 1000 + 1_000, Ordering::SeqCst);
    // The sweeper (500 ms) sends the expiry reject on the
    // live session: poll for it instead of sleeping blind.
    let mut waited = 0;
    while !reply_waiting(&session, 200) && waited < 5_000 {
        waited += 200;
    }
    assert!(reply_waiting(&session, 200), "expiry reject must arrive");
    let (kind, payload) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffReject);
    let reject: HandoffReject = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reject.reason, "expired");
    assert!(
        hub_b.pending_handoffs().is_empty(),
        "expired pending must be gone"
    );
    assert!(b.handoff.lease_for("body").is_none());
    assert!(!a.handoff.is_handed_off(&package.package_id));
    hub_b.stop();
}

#[test]
fn handoff_sender_stops_after_offer() {
    let a = Node::new("hso-a");
    let b = Node::new("hso-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    let package = make_package(&a, &b, "task");
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    // No answer before the receiver's user decides (§7.3).
    assert!(!reply_waiting(&session, 500));
    hub_b.decide_handoff(&package.package_id, true).unwrap();
    assert!(reply_waiting(&session, 5_000));
    let (kind, payload) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffAccept);
    let accept: HandoffAccept = serde_json::from_slice(&payload).unwrap();
    assert_eq!(accept.package_id, package.package_id);
    // On accept the sender stops: the app layer records the
    // handoff and refuses to act on this task again.
    a.handoff.mark_handed_off(&package.package_id);
    assert!(a.handoff.is_handed_off(&package.package_id));
    assert!(b.handoff.lease_for("body").is_some());
    hub_b.stop();
}

#[test]
fn handoff_lease_lost_on_restart() {
    let a = Node::new("hlr-a");
    let b = Node::new("hlr-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let package = make_package(&a, &b, "task");
    // A lease lives only in memory: a fresh manager (a
    // restarted process) holds nothing.
    let manager = HandoffManager::new(b.clock.clone(), None);
    manager.acquire_lease(&package).unwrap();
    assert!(manager.lease_for("body").is_some());
    drop(manager);
    let restarted = HandoffManager::new(b.clock.clone(), None);
    assert!(
        restarted.lease_for("body").is_none(),
        "restart loses the lease"
    );
}

#[test]
fn handoff_clock_skew_beyond_tolerance_rejected() {
    // A is 3 minutes ahead of B: beyond the 2-minute
    // tolerance — refused with the clock-skew reason.
    let (ahead, _) = Node::manual("hcs-a", 1_180_000);
    let (b_manual, _) = Node::manual("hcs-b2", 1_000_000);
    pair_nodes(&ahead, &b_manual, SasFlow::Compare);
    let package = make_package(&ahead, &b_manual, "task");
    assert!(b_manual
        .handoff
        .verify_package(&package, ahead.identity.public_key(), &b_manual.device_id())
        .is_err());
    // 60 seconds ahead is inside the tolerance: accepted.
    let (close, _) = Node::manual("hcs-c", 1_060_000);
    pair_nodes(&close, &b_manual, SasFlow::Compare);
    let package2 = make_package(&close, &b_manual, "task 2");
    assert!(b_manual
        .handoff
        .verify_package(
            &package2,
            close.identity.public_key(),
            &b_manual.device_id()
        )
        .is_ok());
}

#[test]
fn handoff_complete_ack_returns_lease() {
    let a = Node::new("hca-a");
    let b = Node::new("hca-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    // Two-hub topology: A offers to B's hub, B completes back
    // to A's hub.
    let (hub_a, hub_b) = start_two_hubs(&a, &b, backend);
    let package = make_package(&a, &b, "task");
    let mut session_ab = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session_ab, &package);
    assert!(!reply_waiting(&session_ab, 500));
    hub_b.decide_handoff(&package.package_id, true).unwrap();
    assert!(reply_waiting(&session_ab, 5_000));
    let (kind, payload) = session_ab.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffAccept);
    let accept: HandoffAccept = serde_json::from_slice(&payload).unwrap();
    a.handoff.mark_handed_off(&package.package_id);
    assert!(a.handoff.is_handed_off(&package.package_id));
    assert!(b.handoff.lease_for("body").is_some());
    // B finishes the task: it releases its lease locally and
    // sends the complete ack to A's hub.
    assert!(b.handoff.release_lease_by_id(&accept.lease_id));
    let complete = HandoffComplete {
        v: PROTOCOL_VERSION,
        kind: "handoff_complete".to_string(),
        package_id: package.package_id.clone(),
        lease_id: accept.lease_id.clone(),
    };
    let mut session_ba = handshake_to_hub(&b, &hub_a);
    session_ba
        .send(
            MessageKind::HandoffComplete,
            &serde_json::to_vec(&complete).unwrap(),
        )
        .unwrap();
    // A sees the lease return immediately — no waiting for
    // the TTL — and may act on the task again.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!a.handoff.is_handed_off(&package.package_id));
    assert!(b.handoff.lease_for("body").is_none());
    hub_a.stop();
    hub_b.stop();
}

// ----------------------------------------------------------------
// §8.7 / §9.4 helpers
// ----------------------------------------------------------------

fn ota_server(node: &Node) -> OtaServer {
    OtaServer::new(Arc::clone(&node.registry), node.clock.clone(), None)
}

fn register_uuid(node: &Node, uuid: &str) {
    node.registry
        .register_xiaozhi_uuid(uuid, "test-device", mimint_core::audit::now_ms())
        .unwrap();
}

/// Write a Python script into `dir` and return its path.
fn write_py_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn wait_for_tcp(addr: SocketAddr, tries: u32) -> bool {
    for _ in 0..tries {
        if TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

// ----------------------------------------------------------------
// §8.7 — ESP32
// ----------------------------------------------------------------

#[test]
fn ota_only_safe_urls() {
    // Default (lab off): wss:// only, for every address.
    assert!(build_websocket_url("127.0.0.1:19121", false)
        .unwrap()
        .starts_with("wss://"));
    assert!(build_websocket_url("192.168.1.10:19121", false)
        .unwrap()
        .starts_with("wss://"));
    assert!(build_websocket_url("100.64.0.5:19121", false)
        .unwrap()
        .starts_with("wss://"));
    // Lab mode: ws:// ONLY for loopback/tailnet.
    assert!(build_websocket_url("127.0.0.1:19121", true)
        .unwrap()
        .starts_with("ws://"));
    assert!(build_websocket_url("100.64.0.5:19121", true)
        .unwrap()
        .starts_with("ws://"));
    // A private LAN address still gets wss:// in lab mode.
    assert!(build_websocket_url("192.168.1.10:19121", true)
        .unwrap()
        .starts_with("wss://"));
    // A public address is refused in both modes.
    assert!(build_websocket_url("8.8.8.8:19121", false).is_err());
    assert!(build_websocket_url("8.8.8.8:19121", true).is_err());
    // A non-IP host is refused (no DNS tricks).
    assert!(build_websocket_url("example.com:19121", true).is_err());
}

#[test]
fn ota_forces_websocket() {
    let node = Node::new("ofw-a");
    let uuid = "test-uuid-0001";
    register_uuid(&node, uuid);
    let ota = ota_server(&node);
    let response = ota.check(uuid, "127.0.0.1:19121", false).unwrap();
    let json = serde_json::to_value(&response).unwrap();
    // The response carries exactly one object: websocket.
    // No mqtt, no firmware — the firmware can only choose
    // WebSocket, and can never fetch firmware through OTA.
    assert!(json.get("websocket").is_some());
    assert!(json.get("mqtt").is_none(), "OTA must not offer MQTT");
    assert!(
        json.get("firmware").is_none(),
        "OTA must not offer firmware"
    );
    assert!(json
        .get("websocket")
        .and_then(|w| w.get("url"))
        .and_then(|u| u.as_str())
        .unwrap()
        .starts_with("wss://"));
    assert!(!json
        .get("websocket")
        .and_then(|w| w.get("token"))
        .and_then(|t| t.as_str())
        .unwrap()
        .is_empty());
    // The token is single-use: consuming it twice fails.
    let token = response.websocket.token.clone();
    assert!(ota.consume_token(&token).is_some());
    assert!(ota.consume_token(&token).is_none());
}

#[test]
fn ota_unknown_uuid_rejected() {
    let node = Node::new("ouu-a");
    let ota = ota_server(&node);
    // Unknown UUID: rejected, no URL, no token.
    assert!(ota.check("no-such-uuid", "127.0.0.1:19121", false).is_err());
    assert!(ota.issue_token("no-such-uuid").is_err());
    // Registered UUID: a response with URL + token.
    let uuid = "test-uuid-0002";
    register_uuid(&node, uuid);
    let response = ota.check(uuid, "127.0.0.1:19121", false).unwrap();
    assert!(!response.websocket.token.is_empty());
    // Rate limit: a second check within 60s is refused.
    assert!(ota.check(uuid, "127.0.0.1:19121", false).is_err());
}

#[test]
fn ws_interop_with_standard_implementation() {
    let node = Node::new("wsi-a");
    // --- Direction 1: our Rust client vs a Python
    // `websockets` echo server (the independent standard
    // implementation). ---
    let py_server = write_py_script(
        &node.dir,
        "ws_echo_server.py",
        r#"import asyncio, sys, websockets
async def echo(ws):
    async for msg in ws:
        await ws.send(msg)
async def main():
    async with websockets.serve(echo, "127.0.0.1", int(sys.argv[1])):
        await asyncio.Future()
asyncio.run(main())
"#,
    );
    let port = Node::free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut server_child = std::process::Command::new("python3")
        .arg(&py_server)
        .arg(port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("python3 must run the echo server");
    assert!(wait_for_tcp(addr, 50), "python echo server did not listen");
    let stream = TcpStream::connect(addr).unwrap();
    let mut conn = WsConnection::connect_upgrade(stream, &format!("127.0.0.1:{port}"), "/echo")
        .expect("our client must complete the standard handshake");
    conn.write_frame(&WsFrame::Text(b"interop-hello".to_vec()))
        .unwrap();
    assert_eq!(
        conn.read_frame().unwrap(),
        WsFrame::Text(b"interop-hello".to_vec())
    );
    conn.write_frame(&WsFrame::Binary(vec![1, 2, 3])).unwrap();
    assert_eq!(conn.read_frame().unwrap(), WsFrame::Binary(vec![1, 2, 3]));
    conn.write_frame(&WsFrame::Ping(b"p".to_vec())).unwrap();
    assert_eq!(conn.read_frame().unwrap(), WsFrame::Pong(b"p".to_vec()));
    conn.close(1000, "done");
    let _ = server_child.kill();
    let _ = server_child.wait();

    // --- Direction 2: a Python `websockets` client vs our
    // Rust server. ---
    let py_client = write_py_script(
        &node.dir,
        "ws_client.py",
        r#"import asyncio, sys, websockets
async def main():
    uri = f"ws://127.0.0.1:{sys.argv[1]}/echo"
    async with websockets.connect(uri, compression=None) as ws:
        await ws.send("from-python")
        assert await ws.recv() == "from-python", "text echo"
        await ws.send(b"\x01\x02")
        assert await ws.recv() == b"\x01\x02", "binary echo"
        await ws.ping()
        await ws.close()
asyncio.run(main())
"#,
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let rust_addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let (mut conn, _path) = WsConnection::accept_upgrade(stream, None).unwrap();
        loop {
            match conn.read_frame() {
                Ok(WsFrame::Text(bytes)) => {
                    if conn.write_frame(&WsFrame::Text(bytes)).is_err() {
                        return;
                    }
                }
                Ok(WsFrame::Binary(bytes)) => {
                    if conn.write_frame(&WsFrame::Binary(bytes)).is_err() {
                        return;
                    }
                }
                Ok(WsFrame::Ping(payload)) => {
                    if conn.write_frame(&WsFrame::Pong(payload)).is_err() {
                        return;
                    }
                }
                Ok(WsFrame::Close(_)) => return,
                Ok(WsFrame::Pong(_)) => {}
                Err(_) => return,
            }
        }
    });
    let status = std::process::Command::new("python3")
        .arg(&py_client)
        .arg(rust_addr.port().to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .expect("python3 must run the client");
    assert!(status.success(), "python client interop failed");
    server.join().unwrap();
}

#[test]
fn ws_frame_vectors() {
    // Loopback pair: our client vs our server, exchanging the
    // RFC 6455 vectors (masked text, 16-bit length, 64-bit
    // length, ping/pong, close 1000).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let (mut conn, _path) = WsConnection::accept_upgrade(stream, None).unwrap();
        let mut got = Vec::new();
        loop {
            match conn.read_frame() {
                Ok(WsFrame::Text(bytes)) => {
                    got.push(bytes.clone());
                    conn.write_frame(&WsFrame::Text(bytes)).unwrap();
                }
                Ok(WsFrame::Binary(bytes)) => {
                    got.push(bytes.clone());
                    conn.write_frame(&WsFrame::Binary(bytes)).unwrap();
                }
                Ok(WsFrame::Ping(payload)) => {
                    conn.write_frame(&WsFrame::Pong(payload)).unwrap();
                }
                Ok(WsFrame::Close(_)) => break,
                Ok(WsFrame::Pong(_)) => {}
                Err(_) => break,
            }
        }
        got
    });
    let stream = TcpStream::connect(addr).unwrap();
    let mut conn = WsConnection::connect_upgrade(stream, "127.0.0.1", "/vectors").unwrap();
    // Masked "Hello" (client frames are always masked).
    conn.write_frame(&WsFrame::Text(b"Hello".to_vec())).unwrap();
    assert_eq!(conn.read_frame().unwrap(), WsFrame::Text(b"Hello".to_vec()));
    // 16-bit length: 200 bytes.
    let medium = vec![b'x'; 200];
    conn.write_frame(&WsFrame::Text(medium.clone())).unwrap();
    assert_eq!(conn.read_frame().unwrap(), WsFrame::Text(medium));
    // 64-bit length: 70_000 bytes.
    let large = vec![b'y'; 70_000];
    conn.write_frame(&WsFrame::Binary(large.clone())).unwrap();
    assert_eq!(conn.read_frame().unwrap(), WsFrame::Binary(large));
    // Ping/pong + close 1000.
    conn.write_frame(&WsFrame::Ping(b"ping".to_vec())).unwrap();
    assert_eq!(conn.read_frame().unwrap(), WsFrame::Pong(b"ping".to_vec()));
    conn.close(1000, "vectors done");
    let got = server.join().unwrap();
    assert_eq!(got.len(), 3, "server must see the 3 data frames");
    // A fragmented frame is refused: raw FIN=0 text frame.
    let listener2 = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr2 = listener2.local_addr().unwrap();
    let server2 = std::thread::spawn(move || {
        let (stream, _) = listener2.accept().unwrap();
        let (mut conn, _) = WsConnection::accept_upgrade(stream, None).unwrap();
        // Backstop: the test must terminate even if the
        // refusal ever regressed into waiting for more data.
        let _ = conn
            .stream()
            .set_read_timeout(Some(std::time::Duration::from_secs(3)));
        conn.read_frame()
    });
    let stream2 = TcpStream::connect(addr2).unwrap();
    let conn2 = WsConnection::connect_upgrade(stream2, "127.0.0.1", "/frag").unwrap();
    // Raw bytes: FIN=0, opcode=1 (text), masked, len 5, mask
    // + "Hello" — a first fragment, which we refuse.
    let mut raw = conn2.stream().try_clone().unwrap();
    use std::io::Write as _;
    raw.write_all(&[
        0x01, 0x85, 0x00, 0x00, 0x00, 0x00, b'H', b'e', b'l', b'l', b'o',
    ])
    .unwrap();
    let _ = raw.flush();
    assert!(
        server2.join().unwrap().is_err(),
        "fragmented frame must be refused"
    );
    drop(conn2);
}

#[test]
fn ws_fuzzing_no_crash_no_bypass() {
    // 200 random frame-shaped inputs at the server: no panic,
    // no crash, nothing delivered past the 256 KiB cap, and
    // the read loop terminates.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
        let (mut conn, _) = match WsConnection::accept_upgrade(stream, None) {
            Ok(pair) => pair,
            Err(_) => return (0, 0),
        };
        let mut delivered = 0usize;
        let mut errors = 0usize;
        loop {
            match conn.read_frame() {
                Ok(WsFrame::Text(bytes)) | Ok(WsFrame::Binary(bytes)) => {
                    // Nothing past the cap may ever be
                    // delivered as application data.
                    assert!(bytes.len() <= 256 * 1024);
                    delivered += 1;
                }
                Ok(WsFrame::Ping(payload)) => {
                    let _ = conn.write_frame(&WsFrame::Pong(payload));
                }
                Ok(_) => {}
                Err(_) => {
                    errors += 1;
                    break;
                }
            }
        }
        (delivered, errors)
    });
    let stream = TcpStream::connect(addr).unwrap();
    let conn = WsConnection::connect_upgrade(stream, "127.0.0.1", "/fuzz").unwrap();
    let mut rng_state: u64 = 0x9e3779b97f4a7c15;
    let mut next_byte = || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        (rng_state & 0xff) as u8
    };
    use std::io::Write as _;
    let mut raw = conn.stream().try_clone().unwrap();
    for _ in 0..200 {
        let len = 1 + (next_byte() as usize % 300);
        let mut chunk = vec![0u8; len];
        for b in &mut chunk {
            *b = next_byte();
        }
        let _ = raw.write_all(&chunk);
    }
    let _ = raw.flush();
    drop(conn);
    let (delivered, errors) = server.join().expect("fuzz server must not panic");
    assert!(
        errors >= 1,
        "garbage must end in a protocol error, got delivered={delivered}"
    );
}

#[test]
fn xiaozhi_fake_device_end_to_end() {
    let hub_node = Node::new("xfe-hub");
    let uuid = "xiao-zhi-test-0001";
    register_uuid(&hub_node, uuid);
    let backend = MockBackend::new();
    // Lab hub: LAN on + lab mode (ws:// loopback), with the
    // shared brain installed (the same closure the v2 path
    // uses — one brain, §8.5).
    let config = HubConfig {
        listen_lan: true,
        lab_mode: true,
        ..Default::default()
    };
    let hub = start_hub(&hub_node, config, backend.clone());
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = Arc::clone(&seen);
    let brain: Arc<dyn Fn(&str) -> String + Send + Sync> = Arc::new(move |prompt: &str| {
        seen2.lock().unwrap().push(prompt.to_string());
        format!("brain: {prompt}")
    });
    // Install the exact production WS handler (shared
    // constructor) plus the same brain for the v2 path.
    hub.set_ws_handler(mimint_core::friends::context::make_ws_turn_handler(
        Arc::clone(&brain),
    ));
    hub.install_v2_brain_turn(Arc::clone(&brain));
    // The device checks in via OTA first (UUID allow-list +
    // rate limit), then connects with the issued token.
    // Tokens MUST come from the hub's own OTA server —
    // the instance the WS listener consumes tokens from.
    let ota = hub.ota_server();
    let lan = Node::lan_addr();
    let ws_addr = format!("{}:{}", lan, hub.xiaozhi_port());
    let response = ota.check(uuid, &ws_addr, true).unwrap();
    let mut device = FakeXiaoZhiDevice::connect(&ws_addr, uuid, &response.websocket.token)
        .expect("fake device must connect in lab mode");
    // Hello is answered with the audio-config ack (§8.5).
    let hello_ack = device.read_text().unwrap();
    assert!(
        hello_ack.contains("\"hello\"") && hello_ack.contains("opus"),
        "hello ack expected, got {hello_ack}"
    );
    // A text turn goes through the shared brain.
    device.send_text("turn from xiaozhi").unwrap();
    let reply = device.read_text().unwrap();
    assert!(reply.contains("brain: turn from xiaozhi"), "got {reply}");
    // The turn's emotion follows (§8.5).
    let emotion = device.read_text().unwrap();
    assert!(emotion.contains("llm.emotion"), "got {emotion}");
    // An MCP servo call goes through the hub allow-list
    // (§8.4): angle 200 is clamped to 180 and executed.
    device
        .send_json(&serde_json::json!({
            "type": "mcp",
            "method": "servo.move",
            "params": {"angle": 200.0, "speed": 50.0},
        }))
        .unwrap();
    let mcp_reply = device.read_text().unwrap();
    assert!(mcp_reply.contains("mcp_result"), "got {mcp_reply}");
    assert!(mcp_reply.contains("\"ok\":true"), "got {mcp_reply}");
    assert!(backend.called("servo.move"), "servo call must execute");
    let calls = backend.calls.lock().unwrap();
    assert_eq!(
        calls[0].1["angle"].as_f64().unwrap(),
        180.0,
        "angle clamped"
    );
    drop(calls);
    // An audio frame is acknowledged, never transcribed:
    // voice complete is out of Phase 7.
    device.send_binary(vec![0xAA; 64]).unwrap();
    let audio_reply = device.read_text().unwrap();
    assert!(
        audio_reply.contains("Voice hoàn chỉnh"),
        "audio must get the voice-unsupported label, got {audio_reply}"
    );
    // One brain: the same closure also answers a desktop
    // turn (the body path) — the log holds both prompts.
    let desktop_reply = mimint_core::friends::esp32::handle_esp32_turn(
        |prompt| brain(prompt),
        &mimint_core::friends::esp32::Esp32Input {
            source: mimint_core::friends::esp32::Esp32Path::V2,
            device_uuid: "desktop".to_string(),
            text: "turn from desktop".to_string(),
            audio: Vec::new(),
        },
    );
    assert_eq!(desktop_reply.text, "brain: turn from desktop");
    let log = seen.lock().unwrap();
    assert!(log.iter().any(|p| p == "turn from xiaozhi"));
    assert!(log.iter().any(|p| p == "turn from desktop"));
    hub.stop();
}

/// The XiaoZhi path on a LOOPBACK hub (listen_lan = false,
/// the default): the WS listener must serve loopback
/// connections. An earlier accept-loop gate refused ALL WS
/// service unless LAN was on, so the lab path was dead on
/// the default hub while the LAN-on test above still passed.
#[test]
fn xiaozhi_fake_device_loopback_hub() {
    let hub_node = Node::new("xfe-loop");
    let uuid = "xiao-zhi-test-loop-0001";
    register_uuid(&hub_node, uuid);
    let backend = MockBackend::new();
    let config = HubConfig {
        listen_lan: false,
        lab_mode: true,
        ..Default::default()
    };
    let hub = start_hub(&hub_node, config, backend.clone());
    let brain: Arc<dyn Fn(&str) -> String + Send + Sync> =
        Arc::new(|prompt: &str| format!("brain: {prompt}"));
    hub.set_ws_handler(mimint_core::friends::context::make_ws_turn_handler(
        Arc::clone(&brain),
    ));
    hub.install_v2_brain_turn(Arc::clone(&brain));
    let ota = hub.ota_server();
    let ws_addr = format!("127.0.0.1:{}", hub.xiaozhi_port());
    let response = ota.check(uuid, &ws_addr, true).unwrap();
    let mut device = FakeXiaoZhiDevice::connect(&ws_addr, uuid, &response.websocket.token)
        .expect("fake device must connect to a loopback hub");
    let hello_ack = device.read_text().unwrap();
    assert!(
        hello_ack.contains("\"hello\"") && hello_ack.contains("opus"),
        "hello ack expected, got {hello_ack}"
    );
    device.send_text("turn from xiaozhi on loopback").unwrap();
    let reply = device.read_text().unwrap();
    assert!(
        reply.contains("brain: turn from xiaozhi on loopback"),
        "got {reply}"
    );
}

#[test]
fn v2_reference_client_handshake() {
    let a = Node::new("v2r-a");
    let b = Node::new("v2r-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend);
    // The reference client connects with the SAME brain the
    // XiaoZhi path uses (one brain, §8.5).
    let brain = Arc::new(|prompt: &str| format!("brain: {prompt}"));
    hub.install_v2_brain_turn(brain);
    // Handshake + one encrypted text turn.
    let addr = format!("127.0.0.1:{}", hub.friends_port());
    let mut device = FakeV2Device::connect(&addr, &a.identity, &a.registry, &a.clock).unwrap();
    device.send_text("v2 turn").unwrap();
    let reply = device.read_text().unwrap();
    assert_eq!(reply, "brain: v2 turn");
    assert_eq!(
        mimint_core::friends::esp32::esp32_status_label(
            mimint_core::friends::esp32::Esp32Path::V2,
            false,
            true
        ),
        "Firmware v2 — khóa đã pair"
    );
    // A stranger (unpaired) is refused like §2.6.
    let c = Node::new("v2r-c");
    assert!(FakeV2Device::connect(&addr, &c.identity, &c.registry, &c.clock).is_err());
    hub.stop();
}

#[test]
fn mcp_servo_allowlist_enforced() {
    use mimint_core::friends::servo::{
        validate_servo_command, ApprovedSequences, McpCaller, ServoCommand,
    };
    // Allowed: servo.move runs (angle clamped into range).
    let ok = validate_servo_command(
        McpCaller::Device,
        &ServoCommand {
            method: "servo.move".to_string(),
            params: serde_json::json!({"angle": 200.0, "speed": 50.0}),
        },
    )
    .unwrap();
    assert_eq!(ok.method, "servo.move");
    assert!(ok.clamped, "angle 200 must clamp to 180");
    assert_eq!(ok.params["angle"].as_f64().unwrap(), 180.0);
    // Never callable: reboot / upgrade_firmware are refused
    // for every caller, with an explicit reason.
    for method in ["reboot", "upgrade_firmware"] {
        for caller in [McpCaller::User, McpCaller::Peer, McpCaller::Device] {
            let err = validate_servo_command(
                caller,
                &ServoCommand {
                    method: method.to_string(),
                    params: serde_json::json!({}),
                },
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("never callable"),
                "{method} must be refused: {err}"
            );
        }
    }
    // Unknown methods are refused too.
    assert!(validate_servo_command(
        McpCaller::User,
        &ServoCommand {
            method: "servo.explode".to_string(),
            params: serde_json::json!({}),
        },
    )
    .is_err());
    // Sequences: an unapproved sequence is blocked, and a
    // sequence containing a forbidden step cannot be approved.
    let sequences = ApprovedSequences::new();
    assert!(
        sequences.get("seq-1").is_err(),
        "unapproved sequence blocked"
    );
    assert!(sequences
        .approve(
            "seq-bad",
            &[ServoCommand {
                method: "reboot".to_string(),
                params: serde_json::json!({}),
            }]
        )
        .is_err());
    sequences
        .approve(
            "seq-ok",
            &[ServoCommand {
                method: "servo.move".to_string(),
                params: serde_json::json!({"angle": 90.0}),
            }],
        )
        .unwrap();
    assert_eq!(sequences.get("seq-ok").unwrap().len(), 1);
    // And the hub never calls them either: a driver request
    // for reboot over an authenticated session is refused.
    let a = Node::new("msa-a");
    let b = Node::new("msa-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub = start_hub(&b, HubConfig::default(), backend.clone());
    let mut session = handshake_to_hub(&a, &hub);
    let response = send_driver_request(&mut session, "reboot", "drv-reboot-1");
    assert!(!response.ok, "reboot must be refused at the hub");
    assert!(!backend.called("reboot"));
    hub.stop();
}

#[test]
fn mcp_user_only_not_trusted() {
    use mimint_core::friends::servo::{validate_servo_command, McpCaller, ServoCommand};
    // No caller bypasses validation: User, Peer and Device
    // get the identical verdict for the same command.
    for method in ["reboot", "upgrade_firmware", "servo.explode"] {
        let verdicts: Vec<bool> = [McpCaller::User, McpCaller::Peer, McpCaller::Device]
            .iter()
            .map(|caller| {
                validate_servo_command(
                    *caller,
                    &ServoCommand {
                        method: method.to_string(),
                        params: serde_json::json!({}),
                    },
                )
                .is_ok()
            })
            .collect();
        assert_eq!(
            verdicts,
            vec![false, false, false],
            "{method} refused for all"
        );
    }
    // And the identical clamping for a valid command.
    let mut clamped = Vec::new();
    for caller in [McpCaller::User, McpCaller::Peer, McpCaller::Device] {
        let ok = validate_servo_command(
            caller,
            &ServoCommand {
                method: "servo.move".to_string(),
                params: serde_json::json!({"angle": 200.0}),
            },
        )
        .unwrap();
        clamped.push((ok.clamped, ok.params["angle"].as_f64().unwrap()));
    }
    assert_eq!(clamped, vec![(true, 180.0); 3]);
}

/// Files under `dir` with one of `exts` whose text contains
/// `needle` (recursive).
/// Repo root (the test binary lives in `crates/mimint-core`).
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate lives two levels under the repo root")
        .to_path_buf()
}

fn files_containing(dir: &std::path::Path, exts: &[&str], needle: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_containing(&path, exts, needle, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if exts.contains(&ext) {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                if text.contains(needle) {
                    out.push(path.to_string_lossy().into_owned());
                }
            }
        }
    }
}

#[test]
fn esp32_no_8080_in_fork() {
    // The firmware skeleton (the fork board) must contain no
    // WS control server on :8080 — code and config only (the
    // README documents the removal in prose).
    let root = repo_root().join("firmware/mimint-esp32-v2");
    assert!(root.is_dir(), "firmware skeleton must exist");
    let mut hits = Vec::new();
    files_containing(&root, &["c", "h", "txt", "defaults"], "8080", &mut hits);
    assert!(
        hits.is_empty(),
        "no :8080 control server may remain: {hits:?}"
    );
}

#[test]
fn esp32_firmware_build_blocked_honest() {
    // No toolchain on vega → the skeleton README must say
    // BLOCKED with exact install steps, and no binary may
    // exist to pretend otherwise.
    let readme = std::fs::read_to_string(repo_root().join("firmware/mimint-esp32-v2/README.md"))
        .expect("firmware README must exist");
    assert!(readme.contains("BLOCKED"), "README must state BLOCKED");
    assert!(
        readme.contains("idf.py"),
        "README must name the missing tool"
    );
    assert!(readme.contains("esp-idf"), "README must name ESP-IDF");
    assert!(
        readme.contains("install.sh") || readme.contains("install steps"),
        "README must give install steps"
    );
    assert!(
        readme.contains("NOT been compiled"),
        "README must not claim a build"
    );
    let root = repo_root().join("firmware/mimint-esp32-v2");
    let mut binaries = Vec::new();
    files_containing(&root, &["bin", "elf", "a"], "", &mut binaries);
    // `files_containing` with an empty needle matches every
    // file of those extensions; the list must still be empty.
    assert!(
        binaries.is_empty(),
        "no firmware binary may exist: {binaries:?}"
    );
}

// ----------------------------------------------------------------
// §9.4 — Privacy Mode
// ----------------------------------------------------------------

#[test]
fn privacy_on_sync_still_runs() {
    // Privacy Mode is ON by default, and sync to a paired
    // peer still runs on private/tailnet addresses.
    assert!(mimint_core::friends::texts::PRIVACY_MODE_TEXT.contains("peer"));
    let a = Node::new("psr-a");
    let b = Node::new("psr-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    let kinds = SyncKinds::all();
    sync_both(&a, &b, sa, sb, &kinds);
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
    // CGNAT (tailnet) is an accepted private address.
    assert!(is_accepted_address("100.64.0.5".parse().unwrap()));
    assert!(is_accepted_address("127.0.0.1".parse().unwrap()));
    assert!(is_accepted_address("192.168.1.10".parse().unwrap()));
}

#[test]
fn privacy_on_sync_refuses_public_address() {
    // A public address is refused absolutely.
    assert!(!is_accepted_address("8.8.8.8".parse().unwrap()));
    assert!(!is_accepted_address("1.1.1.1".parse().unwrap()));
    // A resolved name with any public address is refused as
    // a whole.
    assert!(
        check_resolved_addresses(&["127.0.0.1".parse().unwrap(), "8.8.8.8".parse().unwrap(),])
            .is_err()
    );
    // All-private resolves pass.
    assert!(check_resolved_addresses(&[
        "127.0.0.1".parse().unwrap(),
        "192.168.1.10".parse().unwrap(),
    ])
    .is_ok());
    // The OTA builder (same address law) refuses public too.
    assert!(build_websocket_url("8.8.8.8:19121", false).is_err());
}

#[test]
fn privacy_on_sync_refuses_dns_rebinding() {
    // A hostname that resolves to a private address AND a
    // public one is refused entirely — one public address
    // poisons the whole name (DNS rebinding).
    assert!(check_resolved_addresses(&[
        "192.168.1.10".parse().unwrap(),
        "93.184.216.34".parse().unwrap(),
    ])
    .is_err());
    // Localhost resolves loopback-only and passes.
    assert!(resolve_and_check_all("localhost", 19120).is_ok());
}

#[test]
fn privacy_on_no_relay_code() {
    // No relay / cloud hostname may appear in the Phase 7
    // code: sync, approval and handoff only ever run
    // peer-to-peer over private addresses.
    // (No bare "relay." needle: the approval relay is a
    // local struct, not a network relay — its name would
    // false-positive on `self.relay.apply_decision`.)
    let forbidden = [
        "cloudflare",
        "amazonaws",
        "googleapis",
        "ngrok",
        "trycloudflare",
        "public-relay",
        "api.mimint",
        "wss://relay",
        "mqtt://",
        "broker.",
    ];
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/friends");
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        for needle in forbidden {
            if text.contains(needle) {
                hits.push(format!("{}: {needle}", entry.file_name().to_string_lossy()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "relay/cloud references must not exist: {hits:?}"
    );
}

#[test]
fn privacy_on_backend_unchanged() {
    // Provider egress is unchanged by Privacy Mode: the
    // configured loopback backend is still reachable.
    assert!(mimint_core::url_is_loopback("http://127.0.0.1:11434").unwrap());
    assert!(!mimint_core::url_is_loopback("https://api.example.com").unwrap());
    // A real request to the local Ollama backend answers
    // (proves the egress path still works under Privacy ON).
    let mut stream =
        TcpStream::connect("127.0.0.1:11434").expect("ollama must be reachable on vega");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    use std::io::Write as _;
    stream
        .write_all(b"GET /api/tags HTTP/1.1\r\nHost: 127.0.0.1:11434\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut buf = [0u8; 512];
    use std::io::Read as _;
    let n = stream.read(&mut buf).unwrap();
    let head = String::from_utf8_lossy(&buf[..n]);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "ollama must answer: {head}"
    );
}

// ----------------------------------------------------------------
// Fix pass 7 (F7-01..F7-28): regression tests for the review
// findings. Each test names the finding it pins.
// ----------------------------------------------------------------

/// F7-01: a successor claim anchored at the receiver's local
/// version, from a peer that never received that version, is
/// a forgery: Rejected with a distinct audit event, no apply,
/// no conflict row.
#[test]
fn sync_forged_successor_without_provenance_rejected() {
    let a = Node::new("fsr-a");
    let b = Node::new("fsr-b");
    let c = Node::new("fsr-c");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
    // C never synced with B and never received v1, yet claims
    // its edit is built on B's local version.
    let local = b
        .registry
        .get_local_version(&note_id)
        .unwrap()
        .expect("B must hold v1");
    let forged = SyncRecord::build(
        note_id.clone(),
        RecordKind::NotebookNote,
        VersionId::new(local.version.lamport + 10, c.device_id()),
        Some(local.version.clone()),
        None,
        c.device_id(),
        b.clock.now_ms(),
        Some(serde_json::json!({
            "title": "note",
            "text": "forged by C",
            "created_ms": b.clock.now_ms(),
            "shared": true,
        })),
    );
    let push = SyncPush {
        v: PROTOCOL_VERSION,
        kind: "sync_push".to_string(),
        records: vec![forged],
    };
    let outcomes = b
        .engine
        .apply_push(&c.device_id(), &push, &SyncKinds::all())
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].1, ApplyOutcome::Rejected);
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v1"));
    assert!(
        b.registry.list_conflicts(false).unwrap().is_empty(),
        "a forgery must not open a conflict"
    );
    assert!(
        audit_contains(&b, "sync_successor_rejected"),
        "distinct audit event required"
    );
    assert!(audit_contains(&b, "forged_based_on"));
}

/// F7-01 (narrowed gate): a peer claiming THIS body authored
/// the record it sends is impersonating us — Rejected. (The
/// broader version_device==peer rule was dropped: sync is
/// transitive, B forwards A's records to C verbatim, so the
/// sender is routinely not the author. Authorship of
/// successor claims is gated by provenance instead.)
#[test]
fn sync_self_impersonating_version_rejected() {
    let a = Node::new("sir-a");
    let b = Node::new("sir-b");
    let c = Node::new("sir-c");
    pair_nodes(&a, &b, SasFlow::Compare);
    pair_nodes(&b, &c, SasFlow::Compare);
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let forged = SyncRecord::build(
        note_id.clone(),
        RecordKind::NotebookNote,
        VersionId::new(7, b.device_id()),
        None,
        None,
        c.device_id(),
        b.clock.now_ms(),
        Some(serde_json::json!({
            "title": "note",
            "text": "I am B, trust me",
            "created_ms": b.clock.now_ms(),
            "shared": true,
        })),
    );
    let push = SyncPush {
        v: PROTOCOL_VERSION,
        kind: "sync_push".to_string(),
        records: vec![forged],
    };
    let outcomes = b
        .engine
        .apply_push(&c.device_id(), &push, &SyncKinds::all())
        .unwrap();
    assert_eq!(outcomes[0].1, ApplyOutcome::Rejected);
    assert!(note_body(&b, &note_id).is_none());
    assert!(audit_contains(&b, "sync_record_rejected_version_device"));
}

/// F7-27: multi-hop A→B→C→B. B forwards A's record to C
/// verbatim (version_device stays A); C's successor, built
/// on a version B really sent it, applies on B. A concurrent
/// multi-hop edit still conflicts instead of silently
/// overwriting.
#[test]
fn sync_multihop_forward_and_return_applies() {
    let a = Node::new("mhf-a");
    let b = Node::new("mhf-b");
    let c = Node::new("mhf-c");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    let (sb2, sc) = paired_synced(&b, &c);
    sync_both(&b, &c, sb2, sc, &SyncKinds::all());
    assert_eq!(
        note_body(&c, &note_id).as_deref(),
        Some("v1"),
        "forwarded record must arrive at C"
    );
    // C edits on top of the forwarded version; B accepts it.
    edit_note(&c, &note_id, "v2 by C");
    let (sc2, sb3) = handshake_pair(&c, &b);
    let (rc, rb) = sync_both(&c, &b, sc2, sb3, &SyncKinds::all());
    assert_eq!(rb.1, 1, "C's successor must apply on B: {rc:?} {rb:?}");
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v2 by C"));
    // A concurrently edits from v1 (never saw v2): conflict,
    // never a silent overwrite.
    edit_note(&a, &note_id, "A's concurrent edit");
    let (sa2, sb4) = handshake_pair(&a, &b);
    let (ra, rb2) = sync_both(&a, &b, sa2, sb4, &SyncKinds::all());
    assert!(
        ra.2 + rb2.2 >= 1,
        "concurrent multi-hop edit must conflict: {ra:?} {rb2:?}"
    );
    assert_eq!(note_body(&b, &note_id).as_deref(), Some("v2 by C"));
}

/// F7-02: a SoulFile never auto-applies — not even as a
/// brand-new record. It waits for the local user's explicit
/// approval, then applies through `approve_pending_sync`.
#[test]
fn sync_soul_never_auto_applies_awaits_approval() {
    let a = Node::new("sna-a");
    let b = Node::new("sna-b");
    SoulStore::new(&a.dir)
        .save("agent-001", "SOUL.md", "You are a careful helper.")
        .unwrap();
    let (sa, sb) = paired_synced(&a, &b);
    let (_ra, rb) = sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert!(
        SoulStore::new(&b.dir).load("SOUL.md").is_err(),
        "SOUL must not arrive unapproved"
    );
    let pending = b.engine.pending_sync_approvals().unwrap();
    assert_eq!(pending.len(), 1, "one record held: {pending:?}");
    assert_eq!(pending[0].0, "soul:SOUL.md");
    assert!(audit_contains(&b, "sync_soul_pending_approval"));
    assert!(
        b.engine.approve_pending_sync("soul:SOUL.md").unwrap(),
        "approval must apply the held record"
    );
    let doc = SoulStore::new(&b.dir).load("SOUL.md").unwrap();
    assert_eq!(doc.body, "You are a careful helper.");
    assert!(b.engine.pending_sync_approvals().unwrap().is_empty());
}

/// F7-05: rotating device_id from the same IP still hits the
/// IP lockout bucket — five failures from five fresh
/// identities on loopback lock a sixth fresh identity out.
#[test]
fn pairing_lockout_by_ip_survives_device_rotation() {
    let b = Node::new("lbi-b");
    let list_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Mismatched,
            trust_granted: Trust::Owner,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let mut fresh_ids = Vec::new();
    for i in 0..5 {
        let a = Node::new(&format!("lbi-a{i}"));
        fresh_ids.push(a.device_id());
        let (ra, rb) = ceremony_with(
            &a,
            &b,
            SasFlow::Compare,
            &auto_initiator(),
            &list_ui,
            None,
            // The listener's audit must be wired: the
            // `pairing_locked` event is what proves the IP
            // bucket fired.
            Some(Arc::clone(&b.audit)),
        );
        assert!(ra.is_err() && rb.is_err(), "attempt {i} must fail");
    }
    // No single device failed twice, so no device bucket is
    // locked — only the shared IP bucket can be.
    for id in &fresh_ids {
        assert!(
            b.registry
                .lockout_until(id, b.clock.now_ms())
                .unwrap()
                .is_none(),
            "device bucket for {id} must be clean"
        );
    }
    assert!(
        b.registry
            .lockout_until("ip:127.0.0.1", b.clock.now_ms())
            .unwrap()
            .is_some(),
        "IP bucket must be locked after 5 failures"
    );
    let sixth = Node::new("lbi-a6");
    let (ra, rb) = ceremony_with(
        &sixth,
        &b,
        SasFlow::Compare,
        &auto_initiator(),
        &list_ui,
        None,
        Some(Arc::clone(&b.audit)),
    );
    assert!(ra.is_err() && rb.is_err(), "6th fresh identity refused");
    assert!(audit_contains(&b, "pairing_locked"));
}

/// F7-06: after `close_lan`, the OTA loop serves nothing (a
/// new connection is dropped at once) and every token issued
/// before the close is dead. `open_lan` brings OTA back.
#[test]
fn hub_close_lan_stops_ota_and_revokes_tokens() {
    let b = Node::new("clo-b");
    register_uuid(&b, "test-uuid-0002");
    let mut config = HubConfig::default();
    config.listen_lan = true;
    let hub = start_hub(&b, config, MockBackend::new());
    assert!(hub.ota_open());
    let addr = format!("127.0.0.1:{}", hub.ota_port());
    assert!(wait_for_tcp(addr.parse().unwrap(), 20), "OTA must listen");
    // One token issued before the close (a second issue
    // within 60 s would hit the OTA per-device rate limit).
    let token2 = hub.ota_server().issue_token("test-uuid-0002").unwrap();
    hub.close_lan();
    assert!(!hub.ota_open());
    assert!(
        hub.ota_server().consume_token(&token2).is_none(),
        "token issued before close_lan must be dead"
    );
    // A fresh OTA connection is dropped at once (EOF).
    let mut stream = TcpStream::connect(&addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let mut buf = [0u8; 1];
    use std::io::Read as _;
    let n = stream.read(&mut buf).unwrap();
    assert_eq!(n, 0, "OTA must drop connections while closed");
    assert!(audit_contains(&b, "hub_lan_closed"));
    hub.open_lan();
    assert!(hub.ota_open());
    // Re-enabled: a fresh OTA connection is NOT dropped at
    // once (the loop now waits for the request instead of
    // closing — a read times out rather than returning EOF).
    // (No new token is issued here: the OTA per-device rate
    // limit allows one check per 60 s.)
    let mut stream = TcpStream::connect(&addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut buf = [0u8; 1];
    use std::io::Read as _;
    let read = stream.read(&mut buf);
    assert!(
        read.is_err(),
        "OTA must serve again after open_lan (read must time out, not EOF): {read:?}"
    );
    hub.stop();
}

/// F7-07: revoking a peer cuts its live session at once —
/// the client sees the close, not just a refusal next time.
#[test]
fn hub_revoke_cuts_live_session() {
    let a = Node::new("rcl-a");
    let b = Node::new("rcl-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let hub = start_hub(&b, HubConfig::default(), MockBackend::new());
    let mut session = handshake_to_hub(&a, &hub);
    ping_pong(&mut session);
    assert!(hub.revoke_peer(&a.device_id()));
    assert!(audit_contains(&b, "hub_peer_revoked"));
    // The hub's session loop re-checks revocation each poll
    // and closes; the client must see the close promptly.
    let _ = session
        .stream_clone()
        .map(|s| s.set_read_timeout(Some(std::time::Duration::from_secs(5))));
    assert!(
        session.recv().is_err(),
        "revoked session must close on the client"
    );
    hub.stop();
}

/// F7-08: one bad record never fails the batch — the good
/// records apply, the bad one is Rejected + audited, and the
/// next sync is not stalled (the good version still arrives).
#[test]
fn sync_one_bad_record_does_not_stall_batch() {
    let a = Node::new("obr-a");
    let b = Node::new("obr-b");
    let (_, bad_id) = add_shared_note(&a, "bad", "good body");
    let (_, good1_id) = add_shared_note(&a, "good1", "one");
    let (_, good2_id) = add_shared_note(&a, "good2", "two");
    pair_nodes(&a, &b, SasFlow::Compare);
    let kinds = SyncKinds::all();
    let offer = a.engine.build_offer(&kinds).unwrap();
    let request = b
        .engine
        .handle_offer(&a.device_id(), &offer, &kinds)
        .unwrap();
    let mut push = a
        .engine
        .build_push(&b.device_id(), &request, &kinds)
        .unwrap();
    assert_eq!(push.records.len(), 3);
    // Corrupt the first record's body in transit: its hash no
    // longer matches, so validation fails.
    let bad = push
        .records
        .iter_mut()
        .find(|r| r.sync_id == bad_id)
        .unwrap();
    bad.body = Some(serde_json::json!({
        "title": "bad",
        "text": "tampered in transit",
        "created_ms": 1,
        "shared": true,
    }));
    let outcomes = b
        .engine
        .apply_push(&a.device_id(), &push, &kinds)
        .unwrap();
    assert_eq!(outcomes.len(), 3);
    let by_id: std::collections::HashMap<_, _> = outcomes.into_iter().collect();
    assert_eq!(by_id[&bad_id], ApplyOutcome::Rejected);
    assert_eq!(by_id[&good1_id], ApplyOutcome::Applied);
    assert_eq!(by_id[&good2_id], ApplyOutcome::Applied);
    assert_eq!(note_body(&b, &good1_id).as_deref(), Some("one"));
    assert_eq!(note_body(&b, &good2_id).as_deref(), Some("two"));
    assert!(note_body(&b, &bad_id).is_none());
    assert!(audit_contains(&b, "sync_record_rejected"));
    // The next real sync still delivers the (untampered) bad
    // note: no permanent stall.
    let (sa, sb) = handshake_pair(&a, &b);
    let (_ra, rb) = sync_both(&a, &b, sa, sb, &kinds);
    assert!(rb.1 >= 1, "next sync must still apply: {rb:?}");
    assert_eq!(note_body(&b, &bad_id).as_deref(), Some("good body"));
}

/// F7-09: a tombstone past 90 days is purged by a real sync
/// (the production caller in `run_sync_exchange`), not only
/// by a direct engine call.
#[test]
fn sync_purge_runs_on_real_sync() {
    let (a, cell_a) = Node::manual("prs-a", 1_000_000);
    let b = Node::new("prs-b");
    let (_, note_id) = add_shared_note(&a, "note", "v1");
    a.engine
        .create_tombstone(&note_id, RecordKind::NotebookNote)
        .unwrap();
    assert_eq!(a.registry.list_tombstones().unwrap().len(), 1);
    pair_nodes(&a, &b, SasFlow::Compare);
    cell_a.fetch_add(91 * 24 * 60 * 60 * 1000, Ordering::SeqCst);
    let (sa, sb) = handshake_pair(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert!(
        a.registry.list_tombstones().unwrap().is_empty(),
        "91-day tombstone must be purged by the sync path"
    );
}

/// F7-10: a flooding peer's rekeys are dropped + audited; the
/// receiver's session stays open and its rekey count stays
/// bounded (1: only the first rekey completed).
#[test]
fn session_rekey_flood_rate_limited_receiver_stays_open() {
    let a = Node::new("rkf-a");
    let b = Node::new("rkf-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    // Short rekey timeout so the flooding side fails fast
    // instead of waiting the production 10 s per attempt.
    let config = SessionConfig {
        rekey_timeout_ms: 800,
        ..SessionConfig::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let b_id = Arc::clone(&b.identity);
    let b_reg = Arc::clone(&b.registry);
    let b_clock = b.clock.clone();
    let cfg_b = config.clone();
    let b_audit = Arc::clone(&b.audit);
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handshake_listener(stream, &b_id, &b_reg, &cfg_b, &b_clock, Some(b_audit)).unwrap()
    });
    let stream = TcpStream::connect(addr).unwrap();
    let mut sa = handshake_initiator(
        stream,
        &a.identity,
        &a.registry,
        &config,
        &a.clock,
        None,
    )
    .unwrap();
    let sb = handle.join().unwrap();
    // The receiver only processes rekey requests while
    // waiting in `recv` (same pump pattern as
    // `friends_rekey_works`): run it on a thread.
    let pump = std::thread::spawn(move || {
        let mut sb = sb;
        let _ = sb.recv();
        (sb.rekey_count(), sb.is_closed())
    });
    sa.initiate_rekey().unwrap();
    // Second rekey 0 s later: under the 5 s cooldown, the
    // receiver drops it; the sender times out on its side.
    assert!(sa.initiate_rekey().is_err());
    assert!(
        poll_audit_contains(&b, "rekey_rate_limited", 20),
        "excess rekey must be audited"
    );
    // Dropping the sender closes the socket, so the pump's
    // recv returns and the thread can be joined.
    drop(sa);
    let (sb_rekeys, sb_closed) = pump.join().unwrap();
    assert_eq!(sb_rekeys, 1, "receiver rekey count bounded");
    assert!(!sb_closed, "receiver session must stay open");
}

/// F7-11: a handoff turn is visible while Running, its
/// `handoff_turn_started` audit precedes the brain's return,
/// and cancelling releases the lease at once — a second
/// offer for the same actuator is accepted immediately,
/// not held to the 10-minute TTL.
#[test]
fn handoff_turn_cancel_releases_lease_early() {
    let a = Node::new("htc-a");
    let b = Node::new("htc-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let backend = MockBackend::new();
    let hub_b = start_hub(&b, HubConfig::default(), backend);
    hub_b.install_v2_brain_turn(Arc::new(|_prompt| {
        std::thread::sleep(std::time::Duration::from_secs(5));
        "turn reply".to_string()
    }));
    let package = make_package(&a, &b, "slow task");
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &package);
    // The hub verifies the offer on its session loop: poll
    // until it is listed as pending before deciding.
    let mut pending = false;
    for _ in 0..50 {
        if hub_b
            .pending_handoffs()
            .iter()
            .any(|item| item.package.package_id == package.package_id)
        {
            pending = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(pending, "offer must become pending");
    hub_b.decide_handoff(&package.package_id, true).unwrap();
    // Turn is Running and the started audit is already there
    // while the brain is still sleeping.
    let mut running = false;
    for _ in 0..20 {
        if let Some(state) = hub_b.handoff_turn_state(&package.package_id) {
            if state.status == HandoffTurnStatus::Running {
                running = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(running, "turn must be Running");
    assert!(
        audit_contains(&b, "handoff_turn_started"),
        "started audit precedes the brain's return"
    );
    assert!(b.handoff.lease_for(HANDOFF_ACTUATOR).is_some());
    assert!(hub_b.cancel_handoff_turn(&package.package_id));
    // The brain still sleeps ~5 s; when it returns, the turn
    // is Cancelled and the lease is already gone.
    let mut cancelled = false;
    for _ in 0..80 {
        if let Some(state) = hub_b.handoff_turn_state(&package.package_id) {
            if state.status == HandoffTurnStatus::Cancelled {
                cancelled = true;
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(cancelled, "turn must end Cancelled");
    assert!(
        b.handoff.lease_for(HANDOFF_ACTUATOR).is_none(),
        "lease must be released on cancel, not at TTL"
    );
    // A second offer for the same actuator is accepted at
    // once — no lease_busy to the TTL.
    let package2 = make_package(&a, &b, "second task");
    offer_over_session(&mut session, &package2);
    hub_b
        .decide_handoff(&package2.package_id, true)
        .expect("second offer must be accepted: lease was freed");
    assert!(b.handoff.lease_for(HANDOFF_ACTUATOR).is_some());
    hub_b.stop();
}

/// F7-12 (hub half): while an approval card awaits a
/// decision, the session loop keeps serving — a ping still
/// gets its pong. The panel half (a separate worker thread
/// for the card wait) lives in mimint-ui and is verified by
/// inspection + screenshot, not by this core test.
#[test]
fn hub_session_serves_ping_while_card_awaits_decision() {
    let h = Node::new("spc-h");
    let p = Node::new("spc-p");
    pair_nodes(&h, &p, SasFlow::Compare);
    let mut prefs = h.registry.get_prefs(&p.device_id()).unwrap();
    prefs.approve_allowed = true;
    h.registry.set_prefs(&p.device_id(), &prefs).unwrap();
    let hub = start_hub(&h, HubConfig::default(), MockBackend::new());
    let (_card, _rx) = h
        .relay
        .create_card(origin_for(&h, "req-pending-1", "shell.exec"), &p.device_id(), true)
        .unwrap();
    let mut session = handshake_to_hub(&p, &hub);
    let got = recv_approval_card(&mut session);
    assert_eq!(got.request_id, "req-pending-1");
    // No decision is ever sent — the card waits. A ping on
    // the same session must still be answered promptly.
    let start = std::time::Instant::now();
    ping_pong(&mut session);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(5),
        "pong must arrive while the card waits"
    );
    hub.stop();
}

/// F7-22: a guest's INPUT driver_request over a real session
/// is refused (ok:false + forbidden_role, backend never
/// called, session alive); a read method (screenshot) is
/// still allowed for the guest (§5.3).
#[test]
fn guest_driver_input_refused_over_real_session() {
    let h = Node::new("gdi-h");
    let p = Node::new("gdi-p");
    // Pair with the HUB granting Guest to the peer (a
    // downgrade after pairing would break the handshake's
    // trust claim by design — trust changes need re-pairing).
    let guest_ui = ScriptedListenerUi {
        decision: ListenerDecision {
            sas_outcome: SasOutcome::Matched,
            trust_granted: Trust::Guest,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        },
    };
    let (ra, rb) = ceremony_with(
        &p,
        &h,
        SasFlow::Compare,
        &auto_initiator(),
        &guest_ui,
        None,
        None,
    );
    assert!(ra.is_ok() && rb.is_ok(), "guest pairing must succeed");
    assert_eq!(
        h.registry.get_peer(&p.device_id()).unwrap().unwrap().trust,
        Trust::Guest
    );
    let backend = MockBackend::new();
    let hub = start_hub(&h, HubConfig::default(), backend.clone());
    let mut session = handshake_to_hub(&p, &hub);
    let denied = send_driver_request(&mut session, "click", "req-guest-1");
    assert!(!denied.ok, "guest click must be refused");
    assert_eq!(denied.code.as_deref(), Some("forbidden_role"));
    assert!(!backend.called("click"), "backend must not run a refused input");
    let allowed = send_driver_request(&mut session, "screenshot", "req-guest-2");
    assert!(allowed.ok, "guest screenshot must stay allowed: {allowed:?}");
    ping_pong(&mut session);
    hub.stop();
}

/// F7-03: a handoff package naming a strange actuator is
/// refused by the hub — it never becomes pending and no
/// lease exists for it. (The good path with "body" is
/// covered by the existing handoff tests.)
#[test]
fn handoff_strange_actuator_rejected_over_session() {
    let a = Node::new("sar-a");
    let b = Node::new("sar-b");
    pair_nodes(&a, &b, SasFlow::Compare);
    let hub_b = start_hub(&b, HubConfig::default(), MockBackend::new());
    let mut evil = make_package(&a, &b, "evil task");
    evil.actuator = "evil-actuator".to_string();
    let mut session = handshake_to_hub(&a, &hub_b);
    offer_over_session(&mut session, &evil);
    // The hub answers with a HandoffReject naming the bad
    // actuator — never an accept, never a pending package.
    let (kind, payload) = session.recv().unwrap();
    assert_eq!(kind, MessageKind::HandoffReject);
    let reject: HandoffReject = serde_json::from_slice(&payload).unwrap();
    assert_eq!(reject.package_id, evil.package_id);
    assert!(
        reject.reason.contains("evil-actuator"),
        "reject must name the strange actuator: {}",
        reject.reason
    );
    assert!(
        !hub_b
            .pending_handoffs()
            .iter()
            .any(|item| item.package.package_id == evil.package_id),
        "strange-actuator package must never be pending"
    );
    assert!(b.handoff.lease_for("evil-actuator").is_none());
    assert!(b.handoff.lease_for(HANDOFF_ACTUATOR).is_none());
    hub_b.stop();
}

/// F7-18: a symlinked SOUL file is never synced — the walker
/// skips it, and `SoulStore::load` refuses to follow it.
#[test]
fn soul_symlink_never_syncs_and_load_refuses() {
    let a = Node::new("ssy-a");
    let b = Node::new("ssy-b");
    let target = a.dir.join("outside.md");
    std::fs::write(
        &target,
        "# SOUL\n\nYou are an impostor soul.\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&target, a.dir.join("EVIL.md")).unwrap();
    assert!(
        SoulStore::new(&a.dir).load("EVIL.md").is_err(),
        "load must refuse symlinks"
    );
    SoulStore::new(&a.dir)
        .save("agent-001", "SOUL.md", "You are the real soul.")
        .unwrap();
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert!(
        SoulStore::new(&b.dir).load("EVIL.md").is_err(),
        "symlink must not arrive"
    );
    // The real soul is held for approval (F7-02), never
    // auto-applied — and the symlink is not held either.
    let pending = b.engine.pending_sync_approvals().unwrap();
    assert_eq!(pending.len(), 1, "only the real soul is held: {pending:?}");
    assert_eq!(pending[0].0, "soul:SOUL.md");
    assert!(b.engine.approve_pending_sync("soul:SOUL.md").unwrap());
    assert!(
        SoulStore::new(&b.dir).load("SOUL.md").is_ok(),
        "real soul applies after approval"
    );
}

/// F7-18: cross-agent notebook isolation — a private note
/// owned by another agent never leaves, through sync or
/// through the handoff notebook context.
#[test]
fn notebook_cross_agent_private_never_leaves() {
    let a = Node::new("xan-a");
    let b = Node::new("xan-b");
    let _ = a
        .notebook
        .add_for("agent-002", "other agent private", "do not sync this line", false)
        .unwrap();
    let (_, shared_id) = add_shared_note(&a, "shared", "sync me");
    let (sa, sb) = paired_synced(&a, &b);
    sync_both(&a, &b, sa, sb, &SyncKinds::all());
    assert_eq!(note_body(&b, &shared_id).as_deref(), Some("sync me"));
    let ctx = shared_notebook_context(&a.notebook).unwrap();
    assert!(
        !ctx.contains("do not sync this line"),
        "handoff context must not carry another agent's private line"
    );
    assert!(ctx.contains("sync me"));
}
