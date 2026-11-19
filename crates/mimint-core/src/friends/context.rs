//! Friends composition root (Phase 7): one place that builds
//! every friends component from a data directory + config
//! file, and the app-facing wiring (terminal ceremony UIs,
//! the XiaoZhi WS turn loop, the profile config sink).
//!
//! Layout (same conventions as the rest of the app):
//! * identity key: `<data-dir>/device-identity.key` (0600)
//! * registry: `<data-dir>/peers.db`
//! * SOUL: `<data-dir>/SOUL.md` (+ other SOUL files)
//! * skills: `<data-dir>/skills/`
//! * todos/plans: the notebook SQLite file (same file)
//! * profiles: the config file's `[[profile]]` tables

use crate::audit::AuditLog;
use crate::config::FileConfig;
use crate::error::MimintError;
use crate::friends::approval::ApprovalRelay;
use crate::friends::esp32::{handle_esp32_turn, Esp32Input, Esp32Path};
use crate::friends::handoff::HandoffManager;
use crate::friends::hub::{BrainTurn, DriverBackend, Hub, HubConfig};
use crate::friends::identity::{DeviceKeyStore, Identity};
use crate::friends::ota::OtaServer;
use crate::friends::pairing::{
    InitiatorDecision, InitiatorDecisionCtx, InitiatorUi, ListenerDecision, ListenerDecisionCtx,
    ListenerUi, PairingController, SasFlow, SasOutcome,
};
use crate::friends::registry::{DeviceRegistry, Trust};
use crate::friends::servo::McpCaller;
use crate::friends::sync::{ProfileSyncSink, SyncEngine, SyncSources};
use crate::friends::todos::TodoPlanStore;
use crate::friends::util::Clock;
use crate::friends::ws::{WsConnection, WsFrame};
use crate::notebook::Notebook;
use crate::profile::ProfileEntry;
use crate::skills::SkillStore;
use crate::soul::SoulStore;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The naming-profile sink backed by the config file: only
/// the `[[profile]]` tables are rewritten (via a TOML value
/// round-trip); every other section is preserved as parsed.
/// The rewritten file is re-parsed as a `FileConfig` before
/// it is written, so a bad profile can never corrupt the
/// config.
pub struct ConfigFileProfileSink {
    path: PathBuf,
}

impl ConfigFileProfileSink {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn read_config(&self) -> Result<FileConfig, MimintError> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(FileConfig::default());
        }
        FileConfig::parse(&text)
    }

    fn write_profiles(&self, profiles: &[ProfileEntry]) -> Result<(), MimintError> {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut value: toml::Value = if text.trim().is_empty() {
            toml::Value::Table(toml::value::Table::new())
        } else {
            toml::from_str(&text)
                .map_err(|err| MimintError::Config(format!("invalid config TOML: {err}")))?
        };
        let table = value
            .as_table_mut()
            .ok_or_else(|| MimintError::Config("config is not a table".to_string()))?;
        let array: Vec<toml::Value> = profiles
            .iter()
            .map(|entry| toml::Value::Table(profile_entry_to_table(entry)))
            .collect();
        table.insert("profile".to_string(), toml::Value::Array(array));
        let new_text = toml::to_string(&value)
            .map_err(|err| MimintError::Config(format!("cannot serialize config: {err}")))?;
        // Validate before writing: a bad profile must never
        // corrupt the config file.
        FileConfig::parse(&new_text)?;
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, new_text)
            .map_err(|err| MimintError::Io(format!("cannot write config temp file: {err}")))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|err| MimintError::Io(format!("cannot replace config file: {err}")))?;
        Ok(())
    }
}

fn profile_entry_to_table(entry: &ProfileEntry) -> toml::value::Table {
    use toml::Value;
    let mut table = toml::value::Table::new();
    table.insert("name".to_string(), Value::String(entry.name.clone()));
    if let Some(value) = &entry.soul {
        table.insert("soul".to_string(), Value::String(value.clone()));
    }
    if let Some(value) = &entry.chain {
        table.insert(
            "chain".to_string(),
            Value::Array(value.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if let Some(value) = &entry.voice {
        table.insert("voice".to_string(), Value::String(value.clone()));
    }
    if let Some(value) = &entry.skills {
        table.insert(
            "skills".to_string(),
            Value::Array(value.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if let Some(value) = entry.proactive {
        table.insert("proactive".to_string(), Value::Boolean(value));
    }
    if let Some(value) = entry.budget_usd {
        table.insert("budget_usd".to_string(), Value::Float(value));
    }
    if let Some(value) = entry.privacy {
        table.insert("privacy".to_string(), Value::Boolean(value));
    }
    if let Some(value) = entry.cost_cap_usd {
        table.insert("cost_cap_usd".to_string(), Value::Float(value));
    }
    if let Some(value) = &entry.avatar {
        table.insert("avatar".to_string(), Value::String(value.clone()));
    }
    table
}

impl ProfileSyncSink for ConfigFileProfileSink {
    fn list_profiles(&self) -> Result<Vec<ProfileEntry>, MimintError> {
        Ok(self.read_config()?.profile)
    }

    fn apply_profile(&self, entry: &ProfileEntry) -> Result<(), MimintError> {
        let mut profiles = self.list_profiles()?;
        if let Some(existing) = profiles.iter_mut().find(|p| p.name == entry.name) {
            *existing = entry.clone();
        } else {
            profiles.push(entry.clone());
        }
        self.write_profiles(&profiles)
    }

    fn delete_profile(&self, name: &str) -> Result<bool, MimintError> {
        let mut profiles = self.list_profiles()?;
        let before = profiles.len();
        profiles.retain(|p| p.name != name);
        if profiles.len() == before {
            return Ok(false);
        }
        self.write_profiles(&profiles)?;
        Ok(true)
    }

    fn known_backend_names(&self) -> Vec<String> {
        self.read_config()
            .map(|config| {
                config
                    .backend
                    .iter()
                    .map(|entry| entry.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A terminal ceremony UI: prints the SAS + fingerprint and
/// reads the user's answers from stdin. Used by the CLI.
pub struct TerminalListenerUi;

impl TerminalListenerUi {
    fn read_line(prompt: &str) -> String {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_string()
    }

    fn ask_trust(default: Trust) -> Trust {
        let answer = Self::read_line(&format!(
            "Cấp quyền cho thiết bị này? [owner/guest, mặc định {}]: ",
            default.as_str()
        ));
        Trust::parse(&answer).unwrap_or(default)
    }

    fn ask_fingerprint(peer_fingerprint: &str) -> (bool, bool) {
        println!("Dấu vân tay của thiết bị bên kia:");
        println!(
            "  {}",
            crate::friends::crypto::fingerprint_short(peer_fingerprint)
        );
        println!("  Đầy đủ: {peer_fingerprint}");
        let answer = Self::read_line(
            "Dấu vân tay có khớp không? [y = khớp / skip = bỏ qua có cảnh báo / n = không]: ",
        );
        match answer.to_ascii_lowercase().as_str() {
            "y" | "yes" | "khớp" => (true, false),
            "skip" | "bỏ qua" => {
                println!(
                    "CẢNH BÁO: {}",
                    crate::friends::texts::FINGERPRINT_SKIP_WARNING
                );
                (false, true)
            }
            _ => (false, false),
        }
    }
}

impl ListenerUi for TerminalListenerUi {
    fn decide(&self, ctx: &ListenerDecisionCtx) -> ListenerDecision {
        println!();
        println!(
            "=== Ghép đôi: thiết bị {} xin kết nối ===",
            ctx.peer_device_id
        );
        println!("Quyền xin: {}", ctx.wanted_trust.as_str());
        let sas_outcome = match ctx.flow {
            SasFlow::Compare => {
                let sas = ctx.sas.clone().unwrap_or_default();
                println!("Mã SAS trên thiết bị này: {sas}");
                println!("So sánh với mã SAS hiển thị trên thiết bị bên kia.");
                let answer = Self::read_line("Hai mã có giống nhau không? [y/n]: ");
                if answer.eq_ignore_ascii_case("y") {
                    SasOutcome::Matched
                } else {
                    SasOutcome::Mismatched
                }
            }
            SasFlow::Typing => {
                let typed = Self::read_line("Nhập mã SAS hiển thị trên thiết bị bên kia: ");
                SasOutcome::Typed(typed)
            }
        };
        let trust_granted = Self::ask_trust(ctx.wanted_trust);
        let (fingerprint_confirmed, fingerprint_skipped_with_warning) =
            Self::ask_fingerprint(&ctx.peer_fingerprint);
        ListenerDecision {
            sas_outcome,
            trust_granted,
            fingerprint_confirmed,
            fingerprint_skipped_with_warning,
        }
    }
}

/// Terminal UI for the initiator side.
pub struct TerminalInitiatorUi;

impl InitiatorUi for TerminalInitiatorUi {
    fn show_sas(&self, sas: &str, peer_fingerprint: &str, peer_device_id: &str) {
        println!();
        println!("=== Ghép đôi: mã SAS của bạn ===");
        println!("Mã SAS: {sas}");
        println!("Thiết bị bên kia: {peer_device_id}");
        println!(
            "Dấu vân tay: {}",
            crate::friends::crypto::fingerprint_short(peer_fingerprint)
        );
    }

    fn decide(&self, ctx: &InitiatorDecisionCtx) -> InitiatorDecision {
        println!();
        println!(
            "Thiết bị bên kia đã xác nhận. Quyền được cấp cho bạn: {}",
            ctx.trust_received.as_str()
        );
        let answer = TerminalListenerUi::read_line("Xác nhận ghép đôi với thiết bị này? [y/n]: ");
        let confirmed = answer.eq_ignore_ascii_case("y");
        let trust_granted = if confirmed {
            TerminalListenerUi::ask_trust(Trust::Owner)
        } else {
            Trust::Guest
        };
        let (fingerprint_confirmed, fingerprint_skipped_with_warning) =
            TerminalListenerUi::ask_fingerprint(&ctx.peer_fingerprint);
        InitiatorDecision {
            confirmed,
            trust_granted,
            fingerprint_confirmed,
            fingerprint_skipped_with_warning,
        }
    }
}

/// Demo-only auto-confirm UI: accepts everything without
/// asking. NEVER used outside demos/tests — the CLI prints a
/// warning whenever it is active. In the Typing flow it types
/// an empty string — it cannot know the SAS the user read on
/// the initiator side — so the Typing flow needs a UI that
/// carries the SAS (for example the channel UI used in the
/// integration tests), never this one.
pub struct AutoListenerUi {
    pub trust: Trust,
}

impl ListenerUi for AutoListenerUi {
    fn decide(&self, ctx: &ListenerDecisionCtx) -> ListenerDecision {
        ListenerDecision {
            sas_outcome: match ctx.flow {
                SasFlow::Compare => SasOutcome::Matched,
                SasFlow::Typing => SasOutcome::Typed(ctx.sas.clone().unwrap_or_default()),
            },
            trust_granted: self.trust,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        }
    }
}

/// Demo-only auto-confirm UI for the initiator side.
pub struct AutoInitiatorUi {
    pub trust: Trust,
}

impl InitiatorUi for AutoInitiatorUi {
    fn show_sas(&self, _sas: &str, _peer_fingerprint: &str, _peer_device_id: &str) {}

    fn decide(&self, _ctx: &InitiatorDecisionCtx) -> InitiatorDecision {
        InitiatorDecision {
            confirmed: true,
            trust_granted: self.trust,
            fingerprint_confirmed: true,
            fingerprint_skipped_with_warning: false,
        }
    }
}

/// The friends composition root for one body.
pub struct FriendsContext {
    pub data_dir: PathBuf,
    pub identity: Arc<Identity>,
    pub registry: Arc<DeviceRegistry>,
    pub controller: Arc<PairingController>,
    pub relay: Arc<ApprovalRelay>,
    pub handoff: Arc<HandoffManager>,
    pub sync_engine: Arc<SyncEngine>,
    pub ota: Arc<OtaServer>,
    pub hub: Arc<Hub>,
    pub audit: Option<Arc<AuditLog>>,
    pub clock: Clock,
    pub config_path: PathBuf,
    pub notebook: Arc<Notebook>,
}

impl FriendsContext {
    /// Open (or create) all friends state for one body.
    /// `backend` is the driver backend (the production one
    /// drives the `BodyActor`; tests pass a mock).
    /// Open with the terminal listener UI (the default: a
    /// human answers the ceremony prompts on stdin).
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        data_dir: impl AsRef<Path>,
        config_path: impl AsRef<Path>,
        notebook_path: impl AsRef<Path>,
        hub_config: HubConfig,
        backend: Arc<dyn DriverBackend>,
        audit: Option<Arc<AuditLog>>,
        clock: Clock,
    ) -> Result<Self, MimintError> {
        Self::open_with_ui(
            data_dir,
            config_path,
            notebook_path,
            hub_config,
            backend,
            audit,
            clock,
            Arc::new(TerminalListenerUi),
        )
    }

    /// Open with an explicit ceremony listener UI (headless
    /// demos pass `AutoListenerUi`; the GUI passes its own).
    #[allow(clippy::too_many_arguments)]
    pub fn open_with_ui(
        data_dir: impl AsRef<Path>,
        config_path: impl AsRef<Path>,
        notebook_path: impl AsRef<Path>,
        hub_config: HubConfig,
        backend: Arc<dyn DriverBackend>,
        audit: Option<Arc<AuditLog>>,
        clock: Clock,
        ceremony_ui: Arc<dyn ListenerUi>,
    ) -> Result<Self, MimintError> {
        let data_dir = data_dir.as_ref().to_path_buf();
        let config_path = config_path.as_ref().to_path_buf();
        std::fs::create_dir_all(&data_dir)
            .map_err(|err| MimintError::Io(format!("cannot create data dir: {err}")))?;
        let identity = Arc::new(DeviceKeyStore::in_data_dir(&data_dir).ensure()?);
        let registry = Arc::new(DeviceRegistry::open(data_dir.join("peers.db"))?);
        let controller = Arc::new(PairingController::new(clock.clone()));
        let relay_registry = Arc::clone(&registry);
        let relay = Arc::new(ApprovalRelay::new_with_lookup(
            clock.clone(),
            audit.clone(),
            Some(Arc::new(move |id: &str| {
                relay_registry
                    .get_peer(id)
                    .ok()
                    .flatten()
                    .map(|peer| peer.name)
            })),
        ));
        let handoff = Arc::new(HandoffManager::new(clock.clone(), audit.clone()));
        let notebook = Arc::new(Notebook::open(notebook_path.as_ref())?);
        let sources = SyncSources {
            soul: SoulStore::new(&data_dir),
            skills: SkillStore::new(data_dir.join("skills")),
            notebook: Arc::clone(&notebook),
            todos: TodoPlanStore::open(notebook_path.as_ref())?,
            profiles: Arc::new(ConfigFileProfileSink::new(&config_path)),
        };
        let sync_engine = Arc::new(SyncEngine::new(
            Arc::clone(&registry),
            sources,
            identity.device_id().to_string(),
            audit.clone(),
            clock.clone(),
        )?);
        let ota = Arc::new(OtaServer::new(
            Arc::clone(&registry),
            clock.clone(),
            audit.clone(),
        ));
        let hub = Arc::new(Hub::new(
            hub_config,
            Arc::clone(&identity),
            Arc::clone(&registry),
            Arc::clone(&controller),
            Arc::clone(&relay),
            Arc::clone(&handoff),
            Arc::clone(&sync_engine),
            Arc::clone(&ota),
            backend,
            audit.clone(),
            clock.clone(),
            ceremony_ui,
        ));
        Ok(Self {
            data_dir,
            identity,
            registry,
            controller,
            relay,
            handoff,
            sync_engine,
            ota,
            hub,
            audit,
            clock,
            config_path,
            notebook,
        })
    }

    /// Install the XiaoZhi WS turn handler on the hub: every
    /// WS turn goes through the SAME brain adapter as the v2
    /// path (`handle_esp32_turn`). `brain_turn` is one turn
    /// of the body brain on the received text.
    pub fn install_ws_handler(&self, brain_turn: BrainTurn) {
        // The v2 path shares this exact brain closure (one
        // brain, §8.5): the hub answers v2 text turns with it.
        self.hub.install_v2_brain_turn(Arc::clone(&brain_turn));
        self.hub.set_ws_handler(make_ws_turn_handler(brain_turn));
    }
}

/// The XiaoZhi WS turn loop (§8.5 message table), built from
/// one brain closure. Shared by the app context and the
/// integration test so the fake device exercises the exact
/// production handler — never a test-only copy.
pub fn make_ws_turn_handler(brain_turn: BrainTurn) -> crate::friends::hub::WsTurnHandler {
    Arc::new(
        move |hub: &Arc<Hub>, mut conn: WsConnection, uuid: String| loop {
            match conn.read_frame() {
                Ok(WsFrame::Text(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    let value: serde_json::Value =
                        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
                    let msg_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match msg_type {
                        // Hello: answer with the audio
                        // config ack (§8.5), no brain call.
                        "hello" => {
                            let ack = serde_json::json!({
                                "type": "hello",
                                "audio": {
                                    "codec": "opus",
                                    "sample_rate": 16000,
                                    "channels": 1
                                },
                            });
                            let _ = conn.write_frame(&WsFrame::Text(
                                serde_json::to_string(&ack).unwrap().into_bytes(),
                            ));
                        }
                        // MCP tools/call (§8.4/§8.5): the hub
                        // is the MCP client — allow-list +
                        // clamp first, never reboot/upgrade.
                        "mcp" => {
                            let method = value
                                .get("method")
                                .and_then(|m| m.as_str())
                                .unwrap_or("")
                                .to_string();
                            let params = value
                                .get("params")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null);
                            // Servo/MCP methods go through the
                            // DEVICE allow-list (§8.4), not the
                            // desktop driver list (§5.3).
                            let result =
                                hub.execute_servo_method(McpCaller::Device, &method, &params);
                            let reply = match result {
                                Ok(result) => serde_json::json!({
                                    "type": "mcp_result",
                                    "ok": true,
                                    "result": result,
                                }),
                                Err(err) => serde_json::json!({
                                    "type": "mcp_result",
                                    "ok": false,
                                    "error": err.to_string(),
                                }),
                            };
                            let _ = conn.write_frame(&WsFrame::Text(
                                serde_json::to_string(&reply).unwrap().into_bytes(),
                            ));
                        }
                        // System messages (reboot/upgrade):
                        // never executed by the hub (§8.4).
                        "system" => {}
                        // Text turn (or plain text without a
                        // JSON envelope): one brain turn, then
                        // the turn's emotion (§8.5).
                        "text" | "" => {
                            let input_text = if msg_type == "text" {
                                value
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string()
                            } else {
                                text.clone()
                            };
                            let output = handle_esp32_turn(
                                |prompt| brain_turn(prompt),
                                &Esp32Input {
                                    source: Esp32Path::XiaoZhi,
                                    device_uuid: uuid.clone(),
                                    text: input_text,
                                    audio: Vec::new(),
                                },
                            );
                            let reply = serde_json::json!({"type": "text", "text": output.text});
                            let _ = conn.write_frame(&WsFrame::Text(
                                serde_json::to_string(&reply).unwrap().into_bytes(),
                            ));
                            let emotion = serde_json::json!({
                                "type": "llm.emotion",
                                "emotion": "neutral",
                            });
                            let _ = conn.write_frame(&WsFrame::Text(
                                serde_json::to_string(&emotion).unwrap().into_bytes(),
                            ));
                        }
                        // Unknown message: ignore, stay open
                        // (§8.5) — never crash on it.
                        _ => {}
                    }
                }
                Ok(WsFrame::Binary(audio)) => {
                    let output = handle_esp32_turn(
                        |prompt| brain_turn(prompt),
                        &Esp32Input {
                            source: Esp32Path::XiaoZhi,
                            device_uuid: uuid.clone(),
                            text: String::new(),
                            audio,
                        },
                    );
                    let reply = serde_json::json!({"type": "text", "text": output.text});
                    let _ = conn.write_frame(&WsFrame::Text(
                        serde_json::to_string(&reply).unwrap().into_bytes(),
                    ));
                }
                Ok(WsFrame::Ping(payload)) => {
                    let _ = conn.write_frame(&WsFrame::Pong(payload));
                }
                Ok(WsFrame::Pong(_)) => {}
                Ok(WsFrame::Close(_)) => {
                    conn.close(1000, "bye");
                    return;
                }
                Err(_) => return,
            }
        },
    )
}
