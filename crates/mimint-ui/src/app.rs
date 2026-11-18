//! The eframe application: top bar, sidebar, ball avatar, chat,
//! approval card, control lamp + STOP, and the audit viewer.
//!
//! Layout follows `design/DESIGN-BALL-UI.md` §3. Everything the user
//! sees is state from [`ChatState`]/[`FaceMachine`]; this module only
//! renders it and forwards input to the worker.
//!
//! Header responsiveness (F3-15): the top bar measures its own width
//! and drops to compact/minimal layouts instead of clipping labels:
//! full (>= 900pt), compact (760..900pt, short profile label, lamp
//! kept), minimal (< 760pt, lamp dot only, the rest behind a "⋯"
//! menu). Both edges keep padding in every mode. The 760pt floor
//! is measured: the compact header overflows at 683pt (1024px at
//! 150% UI scale).
//!
//! Test hooks (for the demo gate, not for users):
//! - `MIMINT_UI_EVENT_LOG=<path>`: appends `window_ready`,
//!   `input_rect x y w h`, `prompt_submitted`, `reply_displayed`,
//!   `approval_shown`, `approval_decided`, `approval_expired` lines.
//! - `MIMINT_UI_BENCH_SECS=<n>`: run N seconds, print frame stats,
//!   exit (the Phase 0 lane-3 method).
//! - `MIMINT_UI_DEGRADED=1`: static face + visible "simple mode"
//!   label (weak machines).
//! - `MIMINT_UI_PIXELS_PER_POINT=<f>`: UI scale (demo gate
//!   screenshots at 125%/150%; applied by `main`, not here).

use crate::agent_state::{
    display_state, next_stop_phase, AgentDisplayState, AgentView, BodyDisplayState,
    BodyNotReadyReason, FailAllRole, StopEvent, StopPhase,
};
use crate::approval_text::{
    approval_detail_with_context, crop_preview, execute_code_hash, impact_labels, is_point_tool,
    target_row,
};
use crate::approver::{approval_point, ApprovalJob, RepaintSignal};
use crate::audit_view::{distinct_agents, read_audit_tail, stop_banner, AuditRow};
use crate::chat::{friendly_tool_error, ChatRole, ChatState, ControlState, TurnStatus};
use crate::error_messages::{error_message, situation_for_code, BudgetScope, ErrorSituation};
use crate::face::{breath_scale, is_blinking, EyeShape, FaceMachine, FaceState, MouthShape};
use crate::motion::{MotionController, MotionTier};
use crate::worker::{FailureKind, FailureScope, McpServerStatus, WorkerEvent, WorkerHandle};
use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use mimint_body::ScreenRect;
use mimint_control::FrameStore;
use mimint_core::{
    evaluate_finetune, execute_code_input_hashes, probe_finetune, ApprovalDecision, BrainProgress,
    CostDisplay, FineTuneStatus, Profile, ProfileSet, VerdictTracker, VerificationKind,
    VerificationStatus, AGENT_DEFAULT,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Mint green body.
const MINT_BODY: Color32 = Color32::from_rgb(0x7F, 0xD8, 0xB0);
/// Off-white face patch.
const FACE_PATCH: Color32 = Color32::from_rgb(0xFF, 0xFD, 0xF5);
/// Glossy black eyes.
const EYE_BLACK: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);
/// Pink cheeks (translucent; not a const: the unmultiplied
/// constructor is not const-evaluable).
fn blush_color() -> Color32 {
    Color32::from_rgba_unmultiplied(0xF5, 0x9B, 0xB8, 120)
}
/// Dark mint for the sprout and tuft.
const LEAF_DARK: Color32 = Color32::from_rgb(0x3D, 0xA8, 0x7D);

/// What the UI knows about one configured backend (from the built
/// providers, so the endpoint is the real one, not a guess). Used
/// for the backend-change warning and the "backend in use" line.
/// Backends that failed to build have no entry: unknown, not assumed.
#[derive(Debug, Clone)]
pub struct BackendInfo {
    pub name: String,
    pub endpoint_url: String,
    pub is_paid: bool,
    pub loopback: bool,
}

/// Options for one app instance (from flags/env, decided by `main`).
pub struct AppOptions {
    pub privacy_on: bool,
    /// Backend names in configured chain order.
    pub backend_names: Vec<String>,
    /// Built-provider info, for warnings and status (may be empty).
    pub backend_infos: Vec<BackendInfo>,
    pub audit_path: PathBuf,
    pub degraded: bool,
    pub bench_secs: Option<f64>,
    pub event_log: Option<PathBuf>,
    /// Onboarding suggestion shown when no backend is configured.
    pub onboarding_text: Option<String>,
    /// Wake-up flag the worker pokes when an approval job arrives.
    pub repaint: Arc<RepaintSignal>,
    /// Shared self-exclusion list (`--body` mode only): the app
    /// writes its window rectangle here every frame, and the body
    /// guard refuses clicks inside it. `None` without `--body`.
    pub shared_exclusion: Option<Arc<RwLock<Vec<ScreenRect>>>>,
    /// Shared frame store (`--body` mode only): the approval
    /// card crops its preview from the newest stored frame.
    /// `None` without `--body`.
    pub frame_store: Option<Arc<FrameStore>>,
    /// Greeting shown at startup: the active profile's SOUL
    /// first line, loaded by `main` before the worker starts.
    pub greeting: String,
    /// All resolved profiles, for the sidebar switcher.
    pub profiles: ProfileSet,
    /// Active profile name at startup.
    pub active_profile: String,
    /// Profile-resolution warnings to surface in the sidebar.
    pub profile_warnings: Vec<String>,
    /// Fine-tune hardware status, probed once at startup; the
    /// sidebar re-probes on demand.
    pub finetune: FineTuneStatus,
    /// True when every `[[profile]]` table was dropped and the
    /// app runs on the forced-safe fallback profile (F5-03).
    pub fallback_active: bool,
    /// Path of the active profile's SOUL file, for the Settings
    /// section (F5-11).
    pub soul_path: PathBuf,
    /// User data directory, so the SOUL path can follow profile
    /// switches (the switch event carries the bare file name).
    pub data_dir: PathBuf,
    /// Phase 6: parallelism enabled in the config. Shows the
    /// agents panel and lets submits start new agents.
    pub parallelism_enabled: bool,
    /// Phase 6: configured agent cap, for the panel note.
    pub max_agents: usize,
    /// Phase 6: hierarchy enabled in the config, for the
    /// agents panel line explaining the default (F6-18).
    pub hierarchy_enabled: bool,
    /// Phase 6: a body factory exists, so a body rebuild is
    /// possible after a stop/fault (the header offers it).
    pub can_rebuild_body: bool,
}

/// One row of the agents panel (§15.1): the agent's identity,
/// its display-model facts, its turn phase, its stop-transition
/// phase, and its fail-all role when one applies.
struct AgentRow {
    label: String,
    parent: Option<String>,
    view: AgentView,
    /// Human phase line from the latest progress event.
    phase: String,
    stop_phase: StopPhase,
    fail_role: Option<FailAllRole>,
}

impl AgentRow {
    fn new(label: String, parent: Option<String>) -> Self {
        Self {
            label,
            parent,
            view: AgentView::default(),
            phase: String::new(),
            stop_phase: StopPhase::Idle,
            fail_role: None,
        }
    }
}

/// Display name for an agent id: "Agent 1" for `agent-001`,
/// "Agent 12" for `agent-012`; the raw id when it does not
/// parse (never invent a number).
fn agent_display_name(agent_id: &str) -> String {
    agent_id
        .strip_prefix("agent-")
        .and_then(|num| num.parse::<u32>().ok())
        .map(|num| format!("Agent {num}"))
        .unwrap_or_else(|| agent_id.to_string())
}

/// The verification badge label for a turn outcome (§15.5), or
/// `None` when the turn had no verification-relevant step (no
/// badge at all — not even "unverified").
/// The §15.5 badge. A mixed turn (some steps GUI-verified,
/// some artifact-verified) gets the combined kind label —
/// never the bare GUI label the aggregate alone would give
/// (F6-05). Every other case uses the aggregate's label.
fn verification_badge(
    verified: Option<VerificationStatus>,
    kind: VerificationKind,
) -> Option<&'static str> {
    match (verified, kind) {
        (Some(_), VerificationKind::Mixed) => Some(kind.label()),
        (Some(status), _) => Some(status.badge_label()),
        (None, _) => None,
    }
}

/// True when a verification outcome counts as "all verified"
/// for the display model: verified outcomes and the no-badge
/// case both count; unverified/failed/unavailable do not.
fn outcome_all_verified(verified: Option<VerificationStatus>) -> bool {
    !matches!(
        verified,
        Some(
            VerificationStatus::Unverified
                | VerificationStatus::Failed
                | VerificationStatus::Unavailable
        )
    )
}

pub struct MimintApp {
    chat: ChatState,
    face: FaceMachine,
    worker: WorkerHandle,
    approvals: std::sync::mpsc::Receiver<ApprovalJob>,
    pending_job: Option<ApprovalJob>,
    privacy_on: bool,
    backend_names: Vec<String>,
    backend_infos: Vec<BackendInfo>,
    /// "" means the full configured chain order.
    selected_backend: String,
    /// A backend switch waiting on the user's confirmation (the
    /// warning bar): switching to it would move data off this
    /// machine.
    pending_backend_change: Option<String>,
    chain_names: Vec<String>,
    chain_error: Option<String>,
    /// Backend answering the current/last turn, when known.
    backend_in_use: Option<String>,
    show_audit: bool,
    audit_technical: bool,
    audit_rows: Vec<AuditRow>,
    audit_path: PathBuf,
    motion: MotionController,
    sidebar_collapsed: bool,
    fuse_notice_shown: bool,
    onboarding_text: Option<String>,
    onboarding_shown: bool,
    started: Instant,
    last_frame: Option<Instant>,
    state_since: Instant,
    first_frame: bool,
    bench_secs: Option<f64>,
    frames: u64,
    event_log: Option<PathBuf>,
    last_input_rect: Option<Rect>,
    repaint: Arc<RepaintSignal>,
    shared_exclusion: Option<Arc<RwLock<Vec<ScreenRect>>>>,
    /// Last exclusion rect written, so the shared list is only
    /// rewritten when the window actually moved or resized.
    last_exclusion: Option<ScreenRect>,
    /// Shared frame store for the approval-card crop preview.
    frame_store: Option<Arc<FrameStore>>,
    /// Card crop texture, keyed by (agent_id, approval job
    /// id): `Some((key, Some(texture)))` when the job has a
    /// crop, `Some((key, None))` when the crop was computed
    /// and there is none (no frame, no point), `None` when no
    /// card is showing. Keyed so the preview freezes at card
    /// time instead of tracking newer frames while the user
    /// decides. The agent id is part of the key because
    /// request ids restart at 1 per turn and collide across
    /// agents.
    approval_crop: Option<((String, u64), Option<egui::TextureHandle>)>,
    /// Phase 6: per-agent panel rows, keyed by agent id.
    agents: HashMap<String, AgentRow>,
    /// Phase 6: agents with a turn currently running. Drives
    /// `chat.busy` (busy while any agent runs).
    running_agents: HashSet<String>,
    /// Phase 6: the global body display state (§15.1).
    body_state: BodyDisplayState,
    /// Phase 6: coordinator degraded (a task end could not be
    /// verified) — no new agents until restart.
    degraded: bool,
    degraded_reason: Option<String>,
    /// Phase 6: the DỪNG KHẨN CẤP scope warning is on screen.
    pending_stop_all: bool,
    /// Phase 6: last MCP server status from the worker.
    mcp_servers: Vec<McpServerStatus>,
    /// Phase 6: audit panel agent filter (None = all agents).
    audit_agent_filter: Option<String>,
    /// Phase 6: parallelism enabled (from AppOptions).
    multi_agent: bool,
    max_agents: usize,
    /// Phase 6: hierarchy enabled (from AppOptions).
    hierarchy_enabled: bool,
    /// Phase 6: a body factory exists, so a body rebuild is
    /// possible after a stop/fault (the header offers it).
    can_rebuild_body: bool,
    /// All resolved profiles (sidebar switcher).
    profiles: ProfileSet,
    /// Active profile name, updated only on
    /// [`WorkerEvent::ProfileSwitched`] — never optimistically.
    active_profile: String,
    /// Profile-resolution warnings, shown in the sidebar.
    profile_warnings: Vec<String>,
    /// Fine-tune hardware status (sidebar, re-probed on demand).
    finetune: FineTuneStatus,
    /// Hysteresis for the displayed fine-tune verdict (F5-09):
    /// the raw verdict must repeat twice before the displayed
    /// one flips, so borderline hardware does not flicker.
    finetune_tracker: VerdictTracker,
    /// Forced-safe fallback mode (F5-03): shown persistently
    /// in the sidebar until restart.
    fallback_active: bool,
    /// Path of the active profile's SOUL file (Settings, F5-11).
    soul_path: PathBuf,
    /// User data directory (SOUL path resolution on switch).
    data_dir: PathBuf,
    /// Set when the worker reports the SOUL file changed
    /// outside the code path; the Settings warning stays up
    /// for the rest of the session (F5-11).
    soul_changed_warning: bool,
    show_journal: bool,
    journal_rows: Vec<AuditRow>,
    /// Last emotion code the model sent in the current turn
    /// (F5-10): shown separately from the turn progress, so
    /// the user can tell a model signal from system state.
    /// Reset on every new turn.
    last_model_emotion: Option<FaceState>,
}

impl MimintApp {
    pub fn new(
        worker: WorkerHandle,
        approvals: std::sync::mpsc::Receiver<ApprovalJob>,
        options: AppOptions,
    ) -> Self {
        let now = Instant::now();
        let mut chat = ChatState::new();
        // The greeting comes from the active profile's SOUL
        // (first line), loaded by `main`; the default SOUL's
        // first line is the settled greeting from the design spec.
        chat.add_system(options.greeting.clone());
        if options.fallback_active {
            // Forced-safe fallback (F5-03): one startup message
            // plus the persistent sidebar banner below. The
            // profile/field/reason detail lives in the warnings.
            chat.add_system(
                "Đang chạy ở chế độ dự phòng: mọi hồ sơ trong file cấu hình đều bị loại, nên Mimint dùng hồ sơ an toàn ép buộc (Chế độ riêng tư BẬT, backend trả phí bị khóa). Xem lý do trong thanh bên, mục Hồ sơ."
                    .to_string(),
            );
        }
        let finetune_tracker = VerdictTracker::new(options.finetune.available);
        chat.multi_agent = options.parallelism_enabled;
        Self {
            chat,
            face: FaceMachine::new(now),
            worker,
            approvals,
            pending_job: None,
            privacy_on: options.privacy_on,
            backend_names: options.backend_names,
            backend_infos: options.backend_infos,
            selected_backend: String::new(),
            pending_backend_change: None,
            chain_names: Vec::new(),
            chain_error: None,
            backend_in_use: None,
            show_audit: false,
            audit_technical: false,
            audit_rows: Vec::new(),
            audit_path: options.audit_path,
            motion: MotionController::new(
                options.degraded,
                MotionController::os_reduced_motion_from_env(),
            ),
            sidebar_collapsed: false,
            fuse_notice_shown: false,
            onboarding_text: options.onboarding_text,
            onboarding_shown: false,
            started: now,
            last_frame: None,
            state_since: now,
            first_frame: true,
            bench_secs: options.bench_secs,
            frames: 0,
            event_log: options.event_log,
            last_input_rect: None,
            repaint: options.repaint,
            shared_exclusion: options.shared_exclusion,
            last_exclusion: None,
            frame_store: options.frame_store,
            approval_crop: None,
            agents: HashMap::new(),
            running_agents: HashSet::new(),
            body_state: BodyDisplayState::Ready,
            degraded: false,
            degraded_reason: None,
            pending_stop_all: false,
            mcp_servers: Vec::new(),
            audit_agent_filter: None,
            multi_agent: options.parallelism_enabled,
            max_agents: options.max_agents,
            hierarchy_enabled: options.hierarchy_enabled,
            can_rebuild_body: options.can_rebuild_body,
            profiles: options.profiles,
            active_profile: options.active_profile,
            profile_warnings: options.profile_warnings,
            finetune: options.finetune,
            finetune_tracker,
            fallback_active: options.fallback_active,
            soul_path: options.soul_path,
            data_dir: options.data_dir,
            soul_changed_warning: false,
            show_journal: false,
            journal_rows: Vec::new(),
            last_model_emotion: None,
        }
    }

    /// Publish the app window rectangle (in framebuffer pixels)
    /// to the shared self-exclusion list, so the body guard
    /// refuses clicks into this window. Runs every frame; writes
    /// only when the rect actually changed. The OUTER rect
    /// (including title bar and decorations) is the exclusion:
    /// input into the decorations is input into this app.
    fn publish_exclusion(&mut self, ctx: &egui::Context) {
        let Some(shared) = &self.shared_exclusion else {
            return;
        };
        let points = ctx
            .input(|i| i.viewport().outer_rect)
            .unwrap_or_else(|| ctx.viewport_rect());
        let ppp = ctx.pixels_per_point();
        // Round outward: a partially covered pixel is covered.
        let x = (points.min.x * ppp).floor() as i32;
        let y = (points.min.y * ppp).floor() as i32;
        let max_x = (points.max.x * ppp).ceil() as i32;
        let max_y = (points.max.y * ppp).ceil() as i32;
        let rect = ScreenRect::new(x, y, (max_x - x).max(0) as u32, (max_y - y).max(0) as u32);
        if self.last_exclusion == Some(rect) {
            return;
        }
        if let Ok(mut list) = shared.write() {
            *list = vec![rect];
            self.last_exclusion = Some(rect);
        }
    }

    fn log_event(&self, line: &str) {
        if let Some(path) = &self.event_log {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
    }

    fn backend_info(&self, name: &str) -> Option<&BackendInfo> {
        self.backend_infos.iter().find(|info| info.name == name)
    }

    fn drain_worker_events(&mut self, now: Instant) {
        while let Some(event) = self.worker.try_recv_event() {
            match event {
                WorkerEvent::TurnDone {
                    agent_id,
                    text,
                    tools_used,
                    partial,
                    backend,
                    verified,
                    verification_kind,
                    sandbox_denied_paths,
                    delegate_failures,
                } => {
                    let is_default = agent_id == AGENT_DEFAULT;
                    self.running_agents.remove(&agent_id);
                    if let Some(row) = self.agents.get_mut(&agent_id) {
                        row.view.turn_running = false;
                        row.view.turn_ended = true;
                        row.view.all_verified = outcome_all_verified(verified);
                        row.view.partial = !delegate_failures.is_empty();
                        row.view.pending_approval = false;
                        row.phase.clear();
                    }
                    let mut text = text;
                    if partial {
                        text.push_str("\n(câu trả lời chưa xong: hết ngân sách bước)");
                    }
                    // The §15.5 verification badge rides with
                    // the reply; no badge when the turn had no
                    // verification-relevant step.
                    if let Some(badge) = verification_badge(verified, verification_kind) {
                        text.push_str(&format!("\n({badge})"));
                    }
                    let had_tools = !tools_used.is_empty();
                    self.backend_in_use = backend;
                    if is_default {
                        self.chat.on_reply(text, tools_used);
                        match verified {
                            // Unverified/failed/unavailable
                            // badges never ride with the
                            // generic "tools ran" notice (§15.5):
                            // say plainly what was not checked.
                            Some(VerificationStatus::Unverified) => {
                                self.chat.add_system(
                                    "Lượt này có bước chưa được xác minh — hãy kiểm tra màn hình xem việc đã xong chưa trước khi tin kết quả."
                                        .to_string(),
                                );
                            }
                            Some(VerificationStatus::Failed) => {
                                self.chat.add_system(
                                    "Xác minh thất bại: có bước không đạt kiểm tra trên màn hình — hãy kiểm tra lại trước khi tin kết quả."
                                        .to_string(),
                                );
                            }
                            Some(VerificationStatus::Unavailable) => {
                                self.chat.add_system(
                                    "Xác minh không khả dụng cho lượt này — kết quả chưa được kiểm tra trên màn hình hay artifact."
                                        .to_string(),
                                );
                            }
                            _ => {
                                if had_tools {
                                    // Action is not completion:
                                    // the tools ran, but only
                                    // the user can tell whether
                                    // the job is actually done.
                                    self.chat.add_system(
                                        "Mimint đã chạy công cụ trong lượt này. Hãy kiểm tra màn hình xem việc đã xong chưa — Mimint không tự kết luận việc đã hoàn thành."
                                            .to_string(),
                                    );
                                }
                            }
                        }
                        self.face.on_turn_finished(now, had_tools);
                        self.state_since = now;
                        self.log_event("reply_displayed");
                    } else {
                        // Only the default agent talks to the
                        // user (§8): other agents report as
                        // system messages naming the agent.
                        let name = agent_display_name(&agent_id);
                        let mut msg = format!("{name}: {text}");
                        if had_tools {
                            msg.push_str(&format!("\nCông cụ đã dùng: {}", tools_used.join(", ")));
                        }
                        self.chat.add_system(msg);
                    }
                    self.clear_card_for_agent(&agent_id, now);
                    // Partial results (§15.3): name what is
                    // missing and that the parent went on
                    // without it — never a bare "done".
                    if !delegate_failures.is_empty() {
                        self.chat.add_system(format!(
                            "Kết quả một phần: {} sub-agent lỗi nên một phần việc còn thiếu ({}). Agent cha đã tiếp tục với phần còn lại — kiểm tra lại kết quả trước khi tin.",
                            delegate_failures.len(),
                            delegate_failures.join("; ")
                        ));
                    }
                    // F6-17: sandbox denials, detected from the
                    // TOOL RESULT (never from the model's
                    // words). Two labels, kept apart: what
                    // actually ran, and what it means. At most
                    // 3 paths; never a suggestion to disable
                    // the sandbox.
                    if !sandbox_denied_paths.is_empty() {
                        let shown: Vec<&str> = sandbox_denied_paths
                            .iter()
                            .take(3)
                            .map(String::as_str)
                            .collect();
                        let note = format!(
                            "Trạng thái thực thi: code đã chạy trong sandbox nhưng không truy cập được file ngoài vùng làm việc: {}. Kết quả: nội dung file chưa được đọc; \"không tồn tại\" chỉ nghĩa là file nằm ngoài vùng làm việc của lượt này. Hãy đặt file vào vùng làm việc của lượt rồi thử lại.",
                            shown.join(", ")
                        );
                        if is_default {
                            self.chat.add_system(note);
                        } else {
                            let name = agent_display_name(&agent_id);
                            self.chat.add_system(format!("{name}: {note}"));
                        }
                    }
                    self.sync_busy();
                }
                WorkerEvent::TurnFailed {
                    agent_id,
                    error,
                    kind,
                } => {
                    let is_default = agent_id == AGENT_DEFAULT;
                    self.running_agents.remove(&agent_id);
                    let code = error.split(':').next().unwrap_or("");
                    if let Some(row) = self.agents.get_mut(&agent_id) {
                        row.view.turn_running = false;
                        row.view.turn_ended = true;
                        row.view.all_verified = false;
                        row.view.pending_approval = false;
                        row.view.budget_exhausted = code == "budget_exhausted";
                        row.view.unknown_outcome = error.contains("unknown_outcome");
                        row.phase.clear();
                    }
                    if is_default {
                        self.chat.on_error(error.clone(), kind);
                        self.face.on_error(now);
                        self.state_since = now;
                    } else {
                        let name = agent_display_name(&agent_id);
                        self.chat.add_system(format!(
                            "{name} thất bại: {}",
                            friendly_tool_error(&error)
                        ));
                    }
                    // The §15.4 three-part message when the
                    // error code is one of the five.
                    self.report_error_message(&agent_id, &error);
                    self.clear_card_for_agent(&agent_id, now);
                    self.sync_busy();
                }
                WorkerEvent::TurnCancelled { agent_id } => {
                    let is_default = agent_id == AGENT_DEFAULT;
                    self.running_agents.remove(&agent_id);
                    if let Some(row) = self.agents.get_mut(&agent_id) {
                        row.view.turn_running = false;
                        row.view.cancel_requested = false;
                        // TurnCancelled only arrives after the
                        // end was verified (§10): this is a
                        // confirmed cancel, never "unknown".
                        row.view.cancelled = true;
                        row.view.end_verified = true;
                        row.view.pending_approval = false;
                        row.phase.clear();
                        row.stop_phase = next_stop_phase(row.stop_phase, StopEvent::EndVerified);
                    }
                    if is_default {
                        self.chat.on_cancelled();
                        self.chat.add_system(
                            "Đã hủy lượt. Câu hỏi của bạn vẫn còn — bấm Thử lại để hỏi tiếp."
                                .to_string(),
                        );
                        self.face.on_error(now);
                        self.state_since = now;
                    } else {
                        let name = agent_display_name(&agent_id);
                        self.chat
                            .add_system(format!("{name} đã hủy (đã xác nhận dừng hẳn)."));
                    }
                    self.clear_card_for_agent(&agent_id, now);
                    self.sync_busy();
                }
                WorkerEvent::TryingBackend { agent_id, name } => {
                    // The first progress of a turn registers
                    // the agent as running: the worker decides
                    // which agent runs, so the UI learns it
                    // here, not at submit time.
                    self.running_agents.insert(agent_id.clone());
                    {
                        let row = self.agent_row_mut(&agent_id);
                        row.view.turn_running = true;
                        row.phase = format!("Đang thử backend {name}");
                    }
                    if agent_id == AGENT_DEFAULT {
                        self.backend_in_use = Some(name.clone());
                        self.chat.status = TurnStatus::TryingBackend(name);
                    }
                    self.sync_busy();
                }
                WorkerEvent::TurnProgress { progress } => {
                    let agent_id = progress.agent_id().to_string();
                    self.running_agents.insert(agent_id.clone());
                    let is_default = agent_id == AGENT_DEFAULT;
                    // Per-agent phase line for the panel.
                    let phase = match &progress {
                        BrainProgress::Emotion { .. } => None,
                        BrainProgress::Analyzing { .. } => Some("Mimint đang nghĩ…".to_string()),
                        BrainProgress::Acting { tool, .. } => Some(format!("Đang chạy {tool}…")),
                        BrainProgress::Verifying { .. } => {
                            Some("Đang kiểm tra kết quả trên màn hình…".to_string())
                        }
                        BrainProgress::ToolFailed { tool, error, .. } => Some(format!(
                            "Công cụ {tool} thất bại: {}",
                            friendly_tool_error(error)
                        )),
                    };
                    {
                        let row = self.agent_row_mut(&agent_id);
                        row.view.turn_running = true;
                        if let Some(phase) = phase {
                            row.phase = phase;
                        }
                    }
                    // The face and the main status line follow
                    // the default agent only; other agents
                    // report their phase in the panel.
                    if is_default {
                        // An emotion signal changes the face,
                        // not the turn status; the other
                        // progress events change the status,
                        // not the face.
                        match progress {
                            BrainProgress::Emotion { code, .. } => {
                                let state = FaceState::from_emotion(code);
                                self.last_model_emotion = Some(state);
                                self.face.on_emotion(state, now);
                                self.state_since = now;
                            }
                            BrainProgress::Analyzing { .. } => {
                                self.chat.status = TurnStatus::Analyzing;
                            }
                            BrainProgress::Acting { tool, .. } => {
                                self.chat.status = TurnStatus::Acting(tool);
                            }
                            BrainProgress::Verifying { .. } => {
                                self.chat.status = TurnStatus::Verifying;
                            }
                            BrainProgress::ToolFailed { tool, error, .. } => {
                                self.chat.status = TurnStatus::ToolFailed { tool, error };
                            }
                        }
                    }
                    self.sync_busy();
                }
                WorkerEvent::WorkerFault { error } => {
                    self.chat.control_error = true;
                    self.chat.on_error(error, FailureKind::Other);
                    self.face.on_error(now);
                    self.state_since = now;
                    self.clear_stale_card(now);
                }
                WorkerEvent::ChainRebuilt { names, error } => {
                    self.chain_names = names;
                    self.chain_error = error;
                    if self.chain_error.is_some() && !self.onboarding_shown {
                        if let Some(text) = self.onboarding_text.take() {
                            self.chat.add_system(text);
                            self.onboarding_shown = true;
                        }
                    }
                }
                WorkerEvent::ProfileSwitched {
                    name,
                    greeting,
                    soul_sent,
                    privacy_on,
                    order,
                    warning,
                    soul_file,
                } => {
                    self.active_profile = name.clone();
                    self.privacy_on = privacy_on;
                    self.backend_names = order;
                    self.selected_backend = String::new();
                    self.pending_backend_change = None;
                    // The Settings SOUL path follows the switch
                    // (demo catch): the file name is a validated
                    // bare name inside the data directory.
                    self.soul_path = self.data_dir.join(soul_file);
                    self.soul_changed_warning = false;
                    self.chat
                        .add_system(format!("Đã chuyển sang hồ sơ \"{name}\". {greeting}"));
                    self.chat.add_system(
                        "Phạm vi chuyển hồ sơ: chỉ lượt trò chuyện tiếp theo dùng SOUL, thứ tự backend và chế độ riêng tư của hồ sơ mới; lịch sử trò chuyện không đổi."
                            .to_string(),
                    );
                    if !soul_sent {
                        self.chat.add_system(
                            "SOUL của hồ sơ này đang bị giữ lại (Chế độ riêng tư + backend ở xa): Mimint trả lời bằng câu nhắc mặc định."
                                .to_string(),
                        );
                    }
                    if let Some(warning) = warning {
                        self.chat.add_system(format!("Cảnh báo hồ sơ: {warning}"));
                    }
                    self.log_event("profile_switched");
                }
                WorkerEvent::StopTripped { reason: _ } => {
                    // The body's emergency stop latched: the lamp
                    // shows Stopped, turns block, and only a body
                    // rebuild (when possible) or a restart clears
                    // it. The running turn reports its own
                    // cancellation separately. The raw reason
                    // stays in the audit record, never in the
                    // chat line (F6-15).
                    self.pending_stop_all = false;
                    self.chat.request_body_stop();
                    self.chat.on_cancelled();
                    if self.can_rebuild_body {
                        self.chat.add_system(
                            "Đã DỪNG KHẨN CẤP — phiên điều khiển đã dừng và sẽ không chạy tiếp; trò chuyện cũng đang dừng. Bấm \"Khởi tạo lại bộ điều khiển\" để điều khiển và trò chuyện tiếp."
                                .to_string(),
                        );
                    } else {
                        self.chat.add_system(
                            "Đã DỪNG KHẨN CẤP — phiên điều khiển đã dừng và sẽ không chạy tiếp; trò chuyện cũng đang dừng. Khởi động lại ứng dụng để điều khiển tiếp."
                                .to_string(),
                        );
                    }
                    self.face.on_error(now);
                    self.state_since = now;
                    self.clear_stale_card(now);
                }
                WorkerEvent::SoulChangedExternally { profile, soul_file } => {
                    // The SOUL file changed outside the code path
                    // (F5-11): the new text is already in use for
                    // the next turn. Warn persistently; the
                    // Settings section keeps the warning up.
                    self.soul_changed_warning = true;
                    self.chat.add_system(format!(
                        "Cảnh báo: file SOUL ({soul_file}) của hồ sơ \"{profile}\" đã đổi ngoài ứng dụng. Nội dung mới sẽ dùng cho lượt tiếp theo — kiểm tra lại file nếu bạn không tự sửa."
                    ));
                    self.log_event("soul_changed_externally");
                }
                WorkerEvent::AgentSpawned {
                    agent_id,
                    label,
                    parent,
                } => {
                    // A spawn is always followed by a turn,
                    // except the default agent's startup row.
                    let mut row = AgentRow::new(label, parent);
                    if agent_id != AGENT_DEFAULT {
                        row.view.turn_running = true;
                        self.running_agents.insert(agent_id.clone());
                    }
                    self.agents.insert(agent_id, row);
                    self.sync_busy();
                }
                WorkerEvent::AgentEnded { agent_id } => {
                    // The terminal event already updated the
                    // row and reported the outcome; the row
                    // retires with the id.
                    self.running_agents.remove(&agent_id);
                    self.agents.remove(&agent_id);
                    self.sync_busy();
                }
                WorkerEvent::AgentFault {
                    agent_id,
                    scope,
                    cause,
                } => {
                    // F6-15: the raw cause stays in the audit
                    // record; the chat line names only what the
                    // user must know. Unknown outcomes get their
                    // own plain wording; anything else points at
                    // the audit log.
                    let cause_text = if cause.contains("unknown_outcome") {
                        "không rõ hành động cuối có chạy hay không"
                    } else {
                        "lỗi nội bộ (chi tiết trong nhật ký kiểm tra)"
                    };
                    if matches!(scope, FailureScope::All) {
                        self.body_state = BodyDisplayState::NotReady(BodyNotReadyReason::Broken);
                        for (id, row) in self.agents.iter_mut() {
                            if *id == agent_id {
                                row.fail_role = Some(FailAllRole::Causer);
                                row.view.unknown_outcome = cause.contains("unknown_outcome");
                            } else if row.view.turn_running {
                                row.fail_role = Some(FailAllRole::StoppedBySystem);
                            }
                        }
                        let name = agent_display_name(&agent_id);
                        self.chat.add_system(format!(
                            "Lỗi chạm tới body từ {name} ({cause_text}): mọi agent đều dừng, cần khởi tạo lại bộ điều khiển mới chạy lại. Agent gây lỗi: {name}; các agent khác bị dừng theo hệ thống; agent nào kết thúc chưa xác minh sẽ hiện \"Kết quả chưa xác định\"."
                        ));
                    } else {
                        let name = agent_display_name(&agent_id);
                        self.chat.add_system(format!(
                            "{name} gặp lỗi nội bộ và dừng một mình: {cause_text}. Các agent khác không bị ảnh hưởng."
                        ));
                    }
                    self.log_event("agent_fault");
                }
                WorkerEvent::Degraded { agent_id, reason } => {
                    self.degraded = true;
                    self.degraded_reason = Some(reason);
                    // The end is unverified: the UI cannot wait
                    // forever for a terminal event that may
                    // never arrive. The row keeps showing
                    // "Kết quả chưa xác định".
                    self.running_agents.remove(&agent_id);
                    if let Some(row) = self.agents.get_mut(&agent_id) {
                        row.view.turn_running = false;
                        row.view.unknown_outcome = true;
                        row.view.degraded = true;
                        row.view.end_verified = false;
                        row.phase.clear();
                        row.stop_phase = next_stop_phase(row.stop_phase, StopEvent::JoinTimeout);
                    }
                    // The §15.4 #5 three-part message, verbatim.
                    let message = error_message(ErrorSituation::TaskEndUnverified);
                    let name = agent_display_name(&agent_id);
                    self.chat.add_system(format!(
                        "{name}: {}\n{}\n{}\n{} (mã kỹ thuật: {})",
                        message.title,
                        message.what,
                        message.impact,
                        message.next,
                        message.tech_code
                    ));
                    self.sync_busy();
                }
                WorkerEvent::BodyRebuilt => {
                    self.body_state = BodyDisplayState::Ready;
                    for row in self.agents.values_mut() {
                        row.fail_role = None;
                    }
                    // A rebuild replaces the body stack and its
                    // stop flag: the stop latches from the
                    // fault no longer apply.
                    self.chat.body_stopped = false;
                    self.chat.stopped = false;
                    self.chat.add_system(
                        "Bộ điều khiển đã khởi tạo lại xong. Lượt tiếp theo sẽ chụp màn hình mới trước khi hành động."
                            .to_string(),
                    );
                    self.log_event("body_rebuilt");
                }
                WorkerEvent::BodyRebuildFailed { error: _ } => {
                    // F6-15: the raw error is in the
                    // `body_rebuild_failed` audit record (the
                    // worker logs it); the chat line stays
                    // friendly and names the recovery path.
                    self.body_state = BodyDisplayState::NotReady(BodyNotReadyReason::Broken);
                    self.chat.add_system(
                        "Khởi tạo lại bộ điều khiển thất bại. Bộ điều khiển chưa sẵn sàng — thử khởi tạo lại hoặc khởi động lại ứng dụng. Chi tiết lỗi đã ghi vào nhật ký kiểm tra."
                            .to_string(),
                    );
                }
                WorkerEvent::McpStatus { servers } => {
                    self.mcp_servers = servers;
                }
            }
        }
    }

    /// The panel row for an agent, creating it with a default
    /// label when the worker never announced the spawn
    /// (defensive: spawns always precede progress in practice).
    fn agent_row_mut(&mut self, agent_id: &str) -> &mut AgentRow {
        self.agents
            .entry(agent_id.to_string())
            .or_insert_with(|| AgentRow::new(agent_display_name(agent_id), None))
    }

    /// Recompute `chat.busy` from the running set: busy while
    /// any agent runs, idle when none does. The worker's first
    /// progress event registers the agent before any terminal
    /// event can arrive, so busy never drops mid-turn.
    fn sync_busy(&mut self) {
        self.chat.busy = !self.running_agents.is_empty();
        if self.chat.busy {
            if self.chat.turn_started.is_none() {
                self.chat.turn_started = Some(Instant::now());
            }
        } else {
            self.chat.status = TurnStatus::Idle;
            self.chat.turn_started = None;
        }
    }

    /// Clear the on-screen approval card, but only when it
    /// belongs to the agent whose turn just ended: another
    /// agent's live card must never be cleared by this agent's
    /// completion.
    fn clear_card_for_agent(&mut self, agent_id: &str, now: Instant) {
        let belongs = self
            .pending_job
            .as_ref()
            .is_some_and(|job| job.agent_id == agent_id)
            || self
                .chat
                .pending_approval
                .as_ref()
                .is_some_and(|req| req.agent_id == agent_id);
        if belongs {
            self.clear_stale_card(now);
        }
    }

    /// Post the §15.4 three-part message for an error whose
    /// code is one of the five fixed situations; silently
    /// nothing for other errors (they keep their own text).
    fn report_error_message(&mut self, agent_id: &str, error: &str) {
        let code = error.split(':').next().unwrap_or("");
        let mut situation = match situation_for_code(code) {
            Some(situation) => situation,
            None => return,
        };
        // The budget message has three scopes: the ledger's
        // message names the global cap when it is the global
        // one ("global cap has ... tokens left").
        if code == "budget_exhausted" && error.contains("global cap") {
            situation = ErrorSituation::BudgetExhausted(BudgetScope::Global);
        }
        let message = error_message(situation);
        let prefix = if agent_id == AGENT_DEFAULT {
            String::new()
        } else {
            format!("{}: ", agent_display_name(agent_id))
        };
        self.chat.add_system(format!(
            "{prefix}{}\n{}\n{}\n{} (mã kỹ thuật: {})",
            message.title, message.what, message.impact, message.next, message.tech_code
        ));
    }

    /// A turn ended: any card still on screen is stale (the worker
    /// already resolved it). Clear it silently — the turn's own
    /// outcome is the news, not the card. The system face
    /// override (F5-10) clears with the card.
    fn clear_stale_card(&mut self, now: Instant) {
        self.pending_job = None;
        self.approval_crop = None;
        self.face.clear_system_override(now);
        self.chat.expire_approval();
    }

    fn drain_approval_jobs(&mut self, now: Instant) {
        if self.pending_job.is_some() {
            return;
        }
        // Drain until a live job shows: a withdrawn job (its
        // agent was cancelled) must never flash on screen.
        while let Ok(job) = self.approvals.try_recv() {
            if job.withdrawn.load(Ordering::SeqCst) {
                continue;
            }
            let agent_id = job.agent_id.clone();
            self.chat.request_approval_job(&job);
            self.pending_job = Some(job);
            if let Some(row) = self.agents.get_mut(&agent_id) {
                row.view.pending_approval = true;
            }
            // System state overrides model codes (F5-10): while
            // a card waits, the face shows Neutral, whatever
            // the model signaled. Turn-phase events and card
            // resolution clear the override.
            self.face.set_system_override(FaceState::Neutral, now);
            self.log_event("approval_shown");
            return;
        }
    }

    /// A card whose deadline passed without a decision: the worker
    /// already timed it out and denied. Clear the card and say so —
    /// never answer it late.
    fn expire_stale_card(&mut self, now: Instant) {
        let expired = self
            .pending_job
            .as_ref()
            .is_some_and(|job| now >= job.deadline);
        if expired {
            let timeout_secs = self
                .pending_job
                .as_ref()
                .map(|job| job.timeout.as_secs())
                .unwrap_or(0);
            self.pending_job = None;
            self.approval_crop = None;
            self.face.clear_system_override(now);
            if self.chat.expire_approval() {
                self.chat.add_system(format!(
                    "Hết thời gian chờ phê duyệt ({timeout_secs} giây): công cụ đã bị từ chối."
                ));
                self.log_event("approval_expired");
            }
        }
    }

    fn submit(&mut self, now: Instant) {
        if let Some(prompt) = self.chat.submit_input() {
            self.last_model_emotion = None;
            self.worker.run_turn(prompt);
            self.face.on_turn_started(now);
            self.state_since = now;
            self.log_event("prompt_submitted");
        }
    }

    /// Retry / "Reply in Vietnamese": re-run a prompt that is
    /// already in the history (no duplicate user message).
    fn resubmit(&mut self, prompt: String, notice: &str, now: Instant) {
        if self.chat.resubmit(prompt.clone()).is_some() {
            self.last_model_emotion = None;
            self.chat.add_system(notice.to_string());
            self.worker.run_turn(prompt);
            self.face.on_turn_started(now);
            self.state_since = now;
            self.log_event("prompt_submitted");
        }
    }

    fn current_order(&self) -> Vec<String> {
        if self.selected_backend.is_empty() {
            self.backend_names.clone()
        } else {
            vec![self.selected_backend.clone()]
        }
    }

    fn rebuild(&self) {
        self.worker.rebuild(self.privacy_on, self.current_order());
    }

    /// Would switching to `name` move data off this machine? True
    /// when the target is a known-remote backend and the current
    /// selection is the chain or a known-local backend. Unknown
    /// backends never trigger the warning (unknown is not remote).
    fn switch_needs_warning(&self, name: &str) -> bool {
        let Some(target) = self.backend_info(name) else {
            return false;
        };
        if target.loopback {
            return false;
        }
        if self.selected_backend.is_empty() {
            return true;
        }
        match self.backend_info(&self.selected_backend) {
            Some(current) => current.loopback,
            None => true,
        }
    }

    fn lamp_color(state: ControlState) -> Color32 {
        match state {
            ControlState::Idle => Color32::from_rgb(0x9C, 0xA3, 0xAF),
            ControlState::AwaitingApproval => Color32::from_rgb(0xF5, 0x9E, 0x0B),
            ControlState::Controlling => Color32::from_rgb(0x22, 0xC5, 0x5E),
            ControlState::Stopping => Color32::from_rgb(0xF9, 0x73, 0x16),
            ControlState::Stopped => Color32::from_rgb(0xDC, 0x26, 0x26),
            ControlState::Error => Color32::from_rgb(0x7F, 0x1D, 0x1D),
        }
    }

    fn status_line(&self) -> Option<String> {
        // F6-16: a latched body stop is NOT a plain stop —
        // control is locked AND chat is blocked until the
        // body stack is rebuilt (or the app restarts).
        // Offering "Tiếp tục" here would lie.
        if self.chat.body_stopped {
            if self.can_rebuild_body {
                return Some(
                    "Đã DỪNG KHẨN CẤP: điều khiển bị khóa và trò chuyện đang dừng. Bấm \"Khởi tạo lại bộ điều khiển\" để tiếp tục."
                        .to_string(),
                );
            }
            return Some(
                "Đã DỪNG KHẨN CẤP: điều khiển bị khóa và trò chuyện đang dừng. Khởi động lại ứng dụng để tiếp tục."
                    .to_string(),
            );
        }
        if self.chat.stopped {
            return Some("Đã dừng. Bấm Tiếp tục để trò chuyện tiếp.".to_string());
        }
        if self.chat.pending_approval.is_some() {
            return Some("Chờ bạn xác nhận trên thẻ phê duyệt…".to_string());
        }
        self.turn_phase_line()
    }

    /// What the running turn is doing, with real elapsed time
    /// (F4-17). `None` when no turn is running. The header and
    /// the message area share this, so the phase never
    /// disagrees with itself on screen.
    fn turn_phase_line(&self) -> Option<String> {
        let elapsed = self
            .chat
            .turn_elapsed_secs()
            .map(|secs| format!(" ({secs} giây)"))
            .unwrap_or_default();
        match &self.chat.status {
            TurnStatus::Connecting => Some(format!("Đang kết nối…{elapsed}")),
            TurnStatus::TryingBackend(name) => Some(format!("Đang thử backend {name}…{elapsed}")),
            TurnStatus::Analyzing => Some(format!("Mimint đang nghĩ…{elapsed}")),
            TurnStatus::Acting(tool) => Some(format!("Đang chạy {tool}…{elapsed}")),
            TurnStatus::Verifying => Some(format!("Đang kiểm tra kết quả trên màn hình…{elapsed}")),
            TurnStatus::ToolFailed { tool, error } => Some(format!(
                "Công cụ {tool} thất bại: {}. Đang xem lại bước tiếp theo…{elapsed}",
                friendly_tool_error(error)
            )),
            TurnStatus::Idle => None,
        }
    }

    fn error_line(&self) -> Option<String> {
        let error = self.chat.last_error.as_ref()?;
        let (heading, guidance) = match self.chat.last_error_kind {
            Some(FailureKind::Auth) => ("Lỗi xác thực", " Kiểm tra API key trong file cấu hình."),
            Some(FailureKind::PolicyBlocked) => (
                "Bị chặn bởi Chế độ riêng tư",
                " Tắt Chế độ riêng tư hoặc dùng backend cục bộ.",
            ),
            Some(FailureKind::OutOfBackends) => (
                "Hết backend",
                " Kiểm tra Ollama/LM Studio hoặc file cấu hình backend.",
            ),
            Some(FailureKind::Other) | None => ("Lỗi", ""),
        };
        Some(format!("{heading}: {error}{guidance}"))
    }
}

impl eframe::App for MimintApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.frames += 1;
        self.publish_exclusion(ui.ctx());
        if self.first_frame {
            self.first_frame = false;
            self.log_event("window_ready");
        }
        // FPS fuse: feed real frame times (gaps over 1s are sleep /
        // unfocus, not slow frames).
        if let Some(last) = self.last_frame {
            let dt = now.duration_since(last);
            if dt < Duration::from_secs(1)
                && self.motion.record_frame(dt)
                && !self.fuse_notice_shown
            {
                self.fuse_notice_shown = true;
                self.chat.add_system(
                    "Hoạt ảnh đã tạm tắt để máy chạy mượt hơn (giữ nguyên cho tới khi khởi động lại)."
                        .to_string(),
                );
            }
        }
        self.last_frame = Some(now);
        // Repaint scheduling: bench forces every frame; the worker's
        // signal and the approval countdown force prompt repaints;
        // otherwise the motion tier decides (66ms / 250ms / none).
        let signalled = self.repaint.take();
        if let Some(secs) = self.bench_secs {
            let elapsed = now.duration_since(self.started).as_secs_f64();
            if elapsed >= secs {
                eprintln!(
                    "MIMINT_UI frames={} elapsed_s={:.3} fps={:.2}",
                    self.frames,
                    elapsed,
                    self.frames as f64 / elapsed
                );
                std::process::exit(0);
            }
            ui.ctx().request_repaint();
        } else if signalled {
            ui.ctx().request_repaint();
        } else if self.chat.pending_approval.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        } else if let Some(interval) = self.motion.repaint_interval() {
            ui.ctx().request_repaint_after(interval);
        }

        self.drain_worker_events(now);
        self.drain_approval_jobs(now);
        self.expire_stale_card(now);
        self.face.tick(now);

        // ---- Top bar (responsive) ----
        let width = ui.available_width();
        let compact = width < 900.0;
        // Minimal below 760pt, not 600pt: measured on 2026-11-18,
        // the compact header (lamp label + STOP + privacy +
        // backend picker) overflows at 683pt (a 1024px window at
        // 150% UI scale) and the right-aligned items overlap the
        // STOP button. 760pt leaves margin above that measured
        // overflow point; the compact range 760..900pt was
        // eye-checked at 800pt with no overlap.
        let minimal = width < 760.0;
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(6.0);
                // Sidebar toggle.
                if ui.button("☰").on_hover_text("Thu gọn / mở thanh bên").clicked() {
                    self.sidebar_collapsed = !self.sidebar_collapsed;
                }
                // Mini avatar.
                let (rect, _) = ui.allocate_exact_size(Vec2::new(28.0, 28.0), egui::Sense::hover());
                draw_face(
                    ui.painter(),
                    rect.center(),
                    13.0,
                    self.face.params(),
                    false,
                    0.0,
                );
                if !minimal {
                    ui.heading("Mimint");
                }
                ui.separator();
                // Control lamp (6 states) + STOP / resume.
                let state = self.chat.control_state();
                let (lamp_rect, lamp_resp) =
                    ui.allocate_exact_size(Vec2::new(12.0, 12.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(lamp_rect.center(), 5.0, Self::lamp_color(state));
                lamp_resp.on_hover_text(state.label());
                if !minimal {
                    ui.label(state.label());
                }
                if self.chat.body_stopped {
                    // A body stop clears on a body rebuild when
                    // one is possible (§4.1); otherwise only a
                    // restart clears it. Offering "Tiếp tục"
                    // without either would lie (the body flag
                    // stays tripped).
                    if self.can_rebuild_body {
                        // F6-14: the name says what it does, and
                        // the hover text states the consequence:
                        // stopped actions never re-run, and
                        // unknown outcomes are not undone.
                        let rebuild = ui.button("Khởi tạo lại bộ điều khiển").on_hover_text(
                            "Khởi tạo lại bộ điều khiển: hành động đã dừng không tự chạy lại; không hoàn tác kết quả chưa xác định.",
                        );
                        if rebuild.clicked() {
                            self.body_state =
                                BodyDisplayState::NotReady(BodyNotReadyReason::Rebuilding);
                            self.worker.rebuild_body();
                        }
                    } else if !minimal {
                        ui.label("Khởi động lại ứng dụng để điều khiển tiếp.");
                    }
                } else if self.chat.stopped {
                    if ui.button("Tiếp tục").clicked() {
                        self.worker.resume();
                        self.chat.resume();
                    }
                } else {
                    let can_stop = self.chat.busy
                        || self.chat.pending_approval.is_some()
                        || self.chat.control_active;
                    // F6-16: one latch, one name. This button
                    // and the panel's stop button send the same
                    // StopAll: both are "DỪNG KHẨN CẤP".
                    let stop =
                        ui.add_enabled(can_stop, egui::Button::new("DỪNG KHẨN CẤP"));
                    if stop.clicked() {
                        self.worker.stop_now();
                        self.chat.request_stop_now();
                    }
                    if can_stop {
                        stop.on_hover_text(
                            "Dừng mọi agent và khóa điều khiển cho tới khi khởi tạo lại bộ điều khiển hoặc khởi động lại ứng dụng.",
                        );
                    } else {
                        stop.on_hover_text(
                            "DỪNG KHẨN CẤP chỉ dùng khi đang có lượt trò chuyện, thẻ phê duyệt, hoặc phiên điều khiển.",
                        );
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    if minimal {
                        // Everything else hides behind one menu.
                        ui.menu_button("⋯", |ui| {
                            ui.label(format!("Hồ sơ: {}", self.active_profile));
                            self.backend_picker_ui(ui);
                            self.privacy_ui(ui);
                        });
                    } else {
                        if compact {
                            ui.label("👤")
                                .on_hover_text(format!("Hồ sơ: {}", self.active_profile));
                        } else {
                            ui.label(format!("Hồ sơ: {}", self.active_profile));
                        }
                        ui.separator();
                        self.backend_picker_ui(ui);
                        ui.separator();
                        self.privacy_ui(ui);
                    }
                });
            });
        });

        // ---- Backend-change warning bar ----
        if let Some(name) = self.pending_backend_change.clone() {
            egui::Panel::top("backend_warning").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.colored_label(
                        Color32::from_rgb(0xB4, 0x53, 0x09),
                        format!(
                            "Backend {name} chạy ở xa: câu hỏi sẽ rời khỏi máy này. Chuyển sang {name}?"
                        ),
                    );
                    if ui.button("Chuyển").clicked() {
                        self.selected_backend = name.clone();
                        self.pending_backend_change = None;
                        self.rebuild();
                    }
                    if ui.button("Giữ nguyên").clicked() {
                        self.pending_backend_change = None;
                    }
                    ui.add_space(6.0);
                });
            });
        }

        // ---- Sidebar ----
        if !self.sidebar_collapsed {
            egui::Panel::left("sidebar")
                .default_size(200.0)
                .show(ui, |ui| {
                    // Scrollable (fix pass): the profile
                    // explanations + SOUL section overflow a
                    // short window; without the scroll area
                    // the lower sections are unreachable.
                    egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Cuộc trò chuyện");
                    ui.label("• Phiên hiện tại");
                    ui.separator();
                    self.agents_panel_ui(ui);
                    ui.heading("Hồ sơ");
                    // Forced-safe fallback (F5-03): persistent
                    // banner while the mode is active. The
                    // profile/field/reason detail is in the
                    // warnings listed below it.
                    if self.fallback_active {
                        ui.colored_label(
                            Color32::from_rgb(0xDC, 0x26, 0x26),
                            "Đang chạy ở chế độ dự phòng: mọi hồ sơ trong file cấu hình đều bị loại. Mimint dùng hồ sơ an toàn ép buộc — Chế độ riêng tư BẬT, backend trả phí bị khóa, không chủ động. Sửa file cấu hình rồi khởi động lại để thoát chế độ này.",
                        );
                    }
                    // One row per profile; clicking sends the
                    // switch request. The active row only moves
                    // when the worker confirms the switch, so a
                    // failed rebuild never shows the wrong
                    // profile as active.
                    let mut switch_to: Option<String> = None;
                    for profile in self.profiles.profiles.clone() {
                        let active = profile.name == self.active_profile;
                        let label = if active {
                            format!("• {} (đang dùng)", profile.name)
                        } else {
                            format!("• {}", profile.name)
                        };
                        if ui.selectable_label(active, label).clicked() && !active {
                            switch_to = Some(profile.name.clone());
                        }
                        // Plain-language attribute explanations
                        // (F5-13). Voice, proactive mode, budget,
                        // and avatar have no execution engine
                        // yet: those lines say so, plainly.
                        for line in profile_detail_lines(&profile) {
                            ui.label(format!("    {line}"));
                        }
                    }
                    if let Some(name) = switch_to {
                        // Switch-during-approval (F5-13): the
                        // worker processes requests in order, so
                        // a switch would only queue behind the
                        // running turn — but deciding on a card
                        // for one profile and then switching
                        // mid-decision is confusing, so the UI
                        // refuses the click and says why. The
                        // pending card is never cancelled by a
                        // switch: it must be decided or time out.
                        if self.chat.pending_approval.is_some() {
                            self.chat.add_system(
                                "Không chuyển hồ sơ lúc này: đang có thẻ phê duyệt chờ bạn quyết định. Hãy quyết định trên thẻ trước, rồi chuyển hồ sơ sau."
                                    .to_string(),
                            );
                        } else {
                            self.worker.switch_profile(name);
                        }
                    }
                    for warning in &self.profile_warnings {
                        ui.colored_label(
                            Color32::from_rgb(0xB4, 0x53, 0x09),
                            format!("Cảnh báo: {warning}"),
                        );
                    }
                    ui.separator();
                    ui.heading("Fine-tune");
                    // The displayed verdict goes through the
                    // hysteresis tracker (F5-09); the gates and
                    // reasons below are always the latest probe.
                    if self.finetune_tracker.displayed() {
                        ui.label("Trạng thái: khả dụng trên phần cứng này.");
                    } else {
                        ui.label("Trạng thái: không khả dụng trên phần cứng này.");
                    }
                    for gate in &self.finetune.gates {
                        ui.label(format!(
                            "• {}: {} (đo được: {})",
                            gate.gate.label(),
                            if gate.passed { "ĐẠT" } else { "KHÔNG ĐẠT" },
                            gate.measured
                        ));
                    }
                    for reason in &self.finetune.reasons {
                        ui.label(format!("Lý do: {reason}"));
                    }
                    if let Some(warning) = &self.finetune.busy_warning {
                        ui.label(format!("Cảnh báo: {warning}"));
                    }
                    ui.label("Huấn luyện thật bị chặn trong bản này.");
                    if ui.button("Kiểm tra lại").clicked() {
                        // Bounded probe (the GPU query has a 5s
                        // cap; on machines without NVIDIA tooling
                        // it returns immediately).
                        self.finetune = evaluate_finetune(&probe_finetune());
                        self.finetune_tracker.update(self.finetune.available);
                    }
                    ui.separator();
                    ui.heading("Cài đặt");
                    ui.checkbox(&mut self.show_audit, "Nhật ký kiểm tra");
                    ui.checkbox(&mut self.show_journal, "Nhật ký học tập");
                    ui.separator();
                    ui.heading("Persona / SOUL");
                    ui.label(format!("File: {}", self.soul_path.display()));
                    if let Ok(modified) = std::fs::metadata(&self.soul_path)
                        .and_then(|meta| meta.modified())
                    {
                        ui.label(format!("Lần sửa gần nhất: {}", ago_text(modified)));
                    } else {
                        ui.label("Lần sửa gần nhất: không đọc được file.");
                    }
                    ui.label("Trạng thái tải: đã tải (file được tạo mới với persona mặc định nếu chưa có).");
                    if ui.button("Mở file SOUL.md").clicked() {
                        open_file_in_default_app(&self.soul_path);
                    }
                    ui.label("Sửa file bằng trình soạn thảo của bạn, giữ nguyên dòng đánh dấu phiên bản ở đầu file. Mimint không tự sửa SOUL.");
                    ui.label("Lưu ý: lịch sử thay đổi SOUL chỉ ghi việc tạo file và các lần lưu qua code — ứng dụng không bao giờ lưu, nên sửa tay KHÔNG được ghi vào lịch sử.");
                    if self.soul_changed_warning {
                        ui.colored_label(
                            Color32::from_rgb(0xDC, 0x26, 0x26),
                            "Cảnh báo: file SOUL đã đổi ngoài ứng dụng trong phiên này. Nội dung mới đang được dùng cho lượt tiếp theo.",
                        );
                    }
                    match self.motion.tier() {
                        MotionTier::Static => {
                            ui.label("Chế độ đơn giản: BẬT");
                        }
                        MotionTier::Reduced => {
                            ui.label("Hoạt ảnh tạm tắt");
                        }
                        MotionTier::Full => {}
                    }
                    ui.separator();
                    ui.heading("Phím dừng khẩn cấp");
                    #[cfg(target_os = "linux")]
                    ui.label(
                        "Ctrl+Alt+Shift+Escape — dừng ngay mọi điều khiển, kể cả khi cửa sổ Mimint không được chọn. Đã dừng thì phải khởi tạo lại bộ điều khiển hoặc khởi động lại ứng dụng mới điều khiển tiếp.",
                    );
                    #[cfg(not(target_os = "linux"))]
                    ui.label(
                        "Phím dừng khẩn cấp toàn cục chưa có trên nền tảng này. Dùng nút DỪNG KHẨN CẤP trong ứng dụng.",
                    );
                    ui.separator();
                    ui.heading("Chó robot (ESP32)");
                    ui.label("Trạng thái: chưa kết nối");
                    ui.label("Kết nối robot là việc Phase 7.");
                    });
                });
        }

        // ---- Audit panel (bottom, collapsible, two layers) ----
        if self.show_audit {
            egui::Panel::bottom("audit_panel")
                .default_size(160.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Nhật ký kiểm tra");
                        if ui.button("Làm mới").clicked() {
                            self.audit_rows = read_audit_tail(&self.audit_path, 100);
                        }
                        ui.checkbox(&mut self.audit_technical, "Kỹ thuật");
                        // Phase 6: the agent filter appears only
                        // when the loaded rows name more than
                        // one agent (§1.4: hide the column when
                        // the log holds a single agent).
                        let agents = distinct_agents(&self.audit_rows);
                        if agents.len() > 1 {
                            let current = self
                                .audit_agent_filter
                                .clone()
                                .unwrap_or_else(|| "Tất cả agent".to_string());
                            egui::ComboBox::from_id_salt("audit_agent_filter")
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    if ui
                                        .selectable_label(
                                            self.audit_agent_filter.is_none(),
                                            "Tất cả agent",
                                        )
                                        .clicked()
                                    {
                                        self.audit_agent_filter = None;
                                    }
                                    for agent in &agents {
                                        if ui
                                            .selectable_label(
                                                self.audit_agent_filter.as_deref()
                                                    == Some(agent.as_str()),
                                                agent_display_name(agent),
                                            )
                                            .clicked()
                                        {
                                            self.audit_agent_filter = Some(agent.clone());
                                        }
                                    }
                                });
                        }
                        ui.label(format!(
                            "{} bản ghi mới nhất (giờ UTC)",
                            self.audit_rows.len()
                        ));
                    });
                    // Latch banner: a live emergency stop, with
                    // its source and time, above the records.
                    if let Some(banner) = stop_banner(&self.audit_rows) {
                        ui.colored_label(Color32::from_rgb(0xDC, 0x26, 0x26), banner);
                    }
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let show_agent = distinct_agents(&self.audit_rows).len() > 1;
                        let filter = self.audit_agent_filter.clone();
                        let mut shown = 0usize;
                        for row in &self.audit_rows {
                            if let Some(filter) = &filter {
                                if &row.agent_id != filter {
                                    continue;
                                }
                            }
                            shown += 1;
                            let agent_prefix = if show_agent {
                                format!("[{}] ", agent_display_name(&row.agent_id))
                            } else {
                                String::new()
                            };
                            if self.audit_technical {
                                ui.label(format!(
                                    "{} {}{}",
                                    crate::audit_view::format_time_ms(row.ts_ms),
                                    agent_prefix,
                                    row.raw
                                ));
                            } else {
                                ui.label(format!(
                                    "{} {}{}",
                                    crate::audit_view::format_time_ms(row.ts_ms),
                                    agent_prefix,
                                    row.friendly_summary()
                                ));
                            }
                        }
                        if self.audit_rows.is_empty() {
                            ui.label("Chưa có bản ghi. Bấm Làm mới sau một lượt trò chuyện.");
                        } else if shown == 0 {
                            ui.label("Không có bản ghi nào của agent đã chọn.");
                        }
                    });
                });
        }

        // ---- Learning journal (bottom panel, F5-12) ----
        if self.show_journal {
            egui::Panel::bottom("journal_panel")
                .default_size(160.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Nhật ký học tập");
                        if ui.button("Làm mới").clicked() {
                            self.journal_rows = read_audit_tail(&self.audit_path, 200);
                        }
                        ui.label(format!(
                            "{} bản ghi mới nhất (giờ UTC)",
                            self.journal_rows.len()
                        ));
                    });
                    ui.label("Chỉ hiện những gì nhật ký kiểm tra đã ghi. Lượt trò chuyện hàng ngày không có checker thì không được ghi thành kinh nghiệm; kỹ năng chưa từng chạy validation thì không xuất hiện ở đây.");
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        let mut shown = 0;
                        for row in &self.journal_rows {
                            if let Some(line) = row.learning_summary() {
                                ui.label(format!(
                                    "{} {}",
                                    crate::audit_view::format_time_ms(row.ts_ms),
                                    line
                                ));
                                shown += 1;
                            }
                        }
                        if shown == 0 {
                            ui.label("Chưa có bản ghi học tập. Bấm Làm mới sau khi chạy validation/eval (mimint-eval).");
                        }
                    });
                });
        }

        // ---- Input bar (bottom panel): approval card + input row.
        // A bottom panel reserves its height BEFORE the central
        // panel is laid out. (The first layout put the input row
        // after a full-height ScrollArea inside the central panel,
        // which pushed the input below the clip rect: laid out but
        // invisible and unclickable. The demo gate caught it.)
        egui::Panel::bottom("input_bar").show(ui, |ui| {
            // Approval card (P0-17 + §15.2): action-specific
            // content + countdown + multi-agent context.
            if let Some(req) = self.chat.pending_approval.clone() {
                let detail = approval_detail_with_context(&req.tool, &req.args, &req.context);
                // Crop preview, computed once per job (frozen at
                // card time) from the newest stored frame.
                let crop = self.approval_crop_texture(ui.ctx(), &req);
                let (impact, impact_extra) = impact_labels(&req.tool, &req.context);
                ui.group(|ui| {
                    ui.heading(format!("Cần phê duyệt — {}", detail.title));
                    ui.label(format!("Yêu cầu #{} · công cụ: {}", req.id, req.tool));
                    // §15.2: the requesting agent + its task
                    // label, when more than one agent exists.
                    if self.multi_agent || self.agents.len() > 1 {
                        let name = agent_display_name(&req.agent_id);
                        let label = self
                            .agents
                            .get(&req.agent_id)
                            .map(|row| row.label.clone())
                            .unwrap_or_default();
                        // F6-12: two fields, never one fused
                        // line — the agent and its task stay
                        // distinguishable at a glance.
                        ui.label(format!("Agent: {name}"));
                        if !label.is_empty() && label != name {
                            ui.label(format!("Nhiệm vụ: {label}"));
                        }
                    }
                    // §15.2: queue position (a snapshot taken
                    // when the card was created — the app only
                    // holds one card at a time, so the live
                    // queue depth is not observable here).
                    if req.queue_total > 1 {
                        ui.label(format!(
                            "Vị trí hàng đợi: {} / {} ({} yêu cầu đang chờ)",
                            req.queue_position, req.queue_total, req.queue_total
                        ));
                    }
                    // §15.2: the impact label (one primary +
                    // secondaries), from the fixed set.
                    if impact_extra.is_empty() {
                        ui.label(format!("Mức độ ảnh hưởng: {impact}"));
                    } else {
                        ui.label(format!(
                            "Mức độ ảnh hưởng: {impact} (+ {})",
                            impact_extra.join(", ")
                        ));
                    }
                    for (label, value) in &detail.rows {
                        ui.label(format!("{label}: {value}"));
                    }
                    // F6-13: the full code hash is ALWAYS
                    // visible (no expand), selectable, with a
                    // copy button; input hashes (F6-06) ride in
                    // the same section. The short hash stays in
                    // the rows above. Post-run hash verification
                    // is reported in the audit panel (the card
                    // only exists pre-run).
                    if req.tool == "execute_code" {
                        if let Some(code) = req.args.get("code").and_then(|v| v.as_str()) {
                            let full = execute_code_hash(code);
                            ui.label("Hash SHA-256 đầy đủ của code đã phê duyệt:");
                            ui.add(egui::Label::new(full.clone()).selectable(true));
                            if ui.button("Sao chép hash").clicked() {
                                ui.ctx().copy_text(full);
                            }
                            for input in execute_code_input_hashes(&req.args) {
                                ui.label(format!(
                                    "Đầu vào {}: {}",
                                    input.name, input.sha256
                                ));
                            }
                            ui.label(
                                "Sau khi chạy, nhật ký kiểm tra sẽ ghi hash của code và file đầu vào đã chạy, và xác nhận có khớp hash này không.",
                            );
                        }
                    }
                    // §15.2: the full JSON args on demand (the
                    // rows above are the human summary).
                    ui.collapsing("Tham số đầy đủ (JSON)", |ui| {
                        ui.label(
                            serde_json::to_string_pretty(&req.args)
                                .unwrap_or_else(|_| req.args.to_string()),
                        );
                    });
                    // §15.2: token estimate + unmeasured
                    // warning. USD only ever appears when the
                    // ledger measured it (§15.4: never show a
                    // precise-looking USD figure from a reserve).
                    if let Some(cost) = &req.cost {
                        match cost {
                            CostDisplay::MeasuredUsd(usd) => {
                                ui.label(format!("Chi phí đo được: ${usd:.4}"));
                            }
                            CostDisplay::EstimateTokens(tokens) => {
                                ui.label(format!(
                                    "Ước tính token: {tokens} (ƯỚC TÍNH, không phải hóa đơn)"
                                ));
                            }
                            CostDisplay::Unknown => {
                                ui.label("Chi phí: chưa đo được.");
                            }
                        }
                    }
                    if req.unmeasured {
                        ui.colored_label(
                            Color32::from_rgb(0xB4, 0x53, 0x09),
                            "Cảnh báo: một phần chi phí chưa đo được — con số trên là ước tính.",
                        );
                    }
                    // Target identity at request time (F4-05):
                    // what window the action will land on.
                    if let Some(target) = &req.target {
                        let (label, value) = target_row(target);
                        ui.label(format!("{label}: {value}"));
                    } else if is_point_tool(&req.tool) {
                        ui.label(
                            "Cửa sổ đích: không xác định được — hãy kiểm tra kỹ tọa độ trước khi cho phép.",
                        );
                    }
                    if let Some(texture) = &crop {
                        ui.label("Vùng quanh điểm đích, từ ảnh chụp gần nhất (chỉ để xem):");
                        ui.image((texture.id(), Vec2::splat(120.0)));
                    }
                    // Stale-observation warning (F4-08): the
                    // observation the model acted on is older
                    // than the TTL. Allowing still runs the
                    // execution-time re-verify before acting.
                    if let Some(age) = req.observation_age {
                        if age > req.max_observation_age {
                            ui.colored_label(
                                Color32::from_rgb(0xDC, 0x26, 0x26),
                                format!(
                                    "Cảnh báo: ảnh chụp Mimint dựa vào đã cũ ({} giây, giới hạn {} giây). Nếu cho phép, Mimint sẽ kiểm tra lại mục tiêu trước khi thực hiện.",
                                    age.as_secs(),
                                    req.max_observation_age.as_secs()
                                ),
                            );
                        }
                    }
                    ui.label(format!("Hoàn tác: {}", detail.reversible));
                    // Elapsed wait + expiry (§15.2).
                    let waited = now.duration_since(req.created_at).as_secs();
                    if let Some(deadline) = req.deadline {
                        let left = deadline.saturating_duration_since(now).as_secs();
                        // F6-13: under 15s the countdown turns
                        // red and names the urgency.
                        if left < 15 {
                            ui.colored_label(
                                Color32::from_rgb(0xDC, 0x26, 0x26),
                                format!(
                                    "SẮP HẾT HẠN: đã chờ {waited} giây · còn {left} giây — hết giờ sẽ tự từ chối."
                                ),
                            );
                        } else {
                            ui.label(format!(
                                "Đã chờ {waited} giây · còn {left} giây — hết giờ sẽ tự từ chối."
                            ));
                        }
                    } else {
                        ui.label(format!("Đã chờ {waited} giây."));
                    }
                    ui.horizontal(|ui| {
                        // P5.8 (F5-CARRY-1): the labels name the
                        // one-time scope; the hover texts spell
                        // it out. The decision itself is
                        // unchanged: allow runs this one action,
                        // deny runs nothing.
                        let allow = ui
                            .button("Cho phép thực hiện hành động này")
                            .on_hover_text("Chỉ cho phép MỘT lần này: hành động này sẽ chạy ngay, và lần sau công cụ này vẫn cần bạn phê duyệt lại.");
                        if allow.clicked() {
                            self.decide_approval(&req.agent_id, req.id, ApprovalDecision::Allow);
                        }
                        let deny = ui
                            .button("Từ chối hành động này")
                            .on_hover_text("Công cụ sẽ không chạy. Bạn có thể yêu cầu lại sau — mỗi lần đều cần phê duyệt riêng.");
                        // F6-04: keyboard focus defaults to
                        // Deny, re-requested every frame while
                        // the card waits — Enter/Space never
                        // lands on Allow by default.
                        deny.request_focus();
                        if deny.clicked() {
                            self.decide_approval(&req.agent_id, req.id, ApprovalDecision::Deny);
                        }
                    });
                    ui.label("Từ chối là mặc định an toàn: công cụ sẽ không chạy.");
                });
                ui.separator();
            }

            // Status + error lines.
            if let Some(status) = self.status_line() {
                ui.label(status);
            }
            if let Some(error) = self.error_line() {
                ui.colored_label(Color32::from_rgb(0xDC, 0x26, 0x26), error);
            }

            // Input row: multiline (Enter sends, Shift+Enter newline).
            ui.horizontal(|ui| {
                let input = ui
                    .add(
                        egui::TextEdit::multiline(&mut self.chat.input)
                            .hint_text("Nhắn cho Mimint… (Enter để gửi, Shift+Enter xuống dòng)")
                            .desired_rows(2)
                            .desired_width(ui.available_width() - 190.0),
                    )
                    .on_hover_text("Enter để gửi · Shift+Enter để xuống dòng");
                let rect = input.rect;
                if self.last_input_rect != Some(rect) {
                    self.last_input_rect = Some(rect);
                    self.log_event(&format!(
                        "input_rect {} {} {} {}",
                        rect.min.x,
                        rect.min.y,
                        rect.width(),
                        rect.height()
                    ));
                }
                let enter_pressed = input.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                if enter_pressed {
                    if self.chat.input.trim().is_empty() {
                        // Swallow the newline: nothing to send.
                        self.chat.input.clear();
                    } else {
                        self.face.on_activity(now);
                        self.submit(now);
                        input.request_focus();
                    }
                }
                let mic = ui.add_enabled(false, egui::Button::new("🎤"));
                mic.on_hover_text("Voice chưa có ở Phase 3.");
                if self.chat.busy && !self.chat.stopped && ui.button("Hủy lượt").clicked() {
                    self.worker.cancel_turn();
                }
                if ui.button("Gửi").clicked() {
                    self.face.on_activity(now);
                    self.submit(now);
                    input.request_focus();
                }
            });
            if self.chat.busy {
                ui.label("Đang chờ trả lời…");
            }
        });

        // ---- Main area: face + chat ----
        egui::CentralPanel::default().show(ui, |ui| {
            // Face.
            let params = self.face.params();
            let elapsed = now.duration_since(self.started);
            let animate = self.motion.animate_face();
            let breath = if animate { breath_scale(elapsed) } else { 1.0 };
            let blinking = (animate && is_blinking(elapsed)) || params.eyes == EyeShape::Closed;
            let bounce = if !animate || !params.bounce {
                0.0
            } else {
                let since = now.duration_since(self.state_since).as_secs_f32();
                if since < 0.5 {
                    -8.0 * (since / 0.5 * std::f32::consts::PI).sin()
                } else {
                    0.0
                }
            };
            let face_size = 150.0;
            let (face_rect, _) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), face_size),
                egui::Sense::hover(),
            );
            let center = Pos2::new(face_rect.center().x, face_rect.center().y + bounce);
            draw_face(
                ui.painter(),
                center,
                62.0 * breath,
                params,
                blinking,
                params.tilt_degrees,
            );
            // Two lines (F5-10): what the model signaled, and
            // what the turn is waiting on. Never merged: a model
            // emotion code is not system state, and system state
            // (approval pending, errors) overrides the face.
            let emotion_line = match self.last_model_emotion {
                Some(code) => format!("Cảm xúc model gửi: {}", code.label()),
                None => "Cảm xúc model gửi: chưa có trong lượt này".to_string(),
            };
            ui.label(emotion_line);
            let progress_line = if self.chat.pending_approval.is_some() {
                "Tiến trình: đang chờ bạn quyết định trên thẻ phê duyệt".to_string()
            } else {
                match self.turn_phase_line() {
                    Some(line) => format!("Tiến trình: {line}"),
                    None => "Tiến trình: đang chờ bạn".to_string(),
                }
            };
            ui.horizontal(|ui| {
                ui.label(progress_line);
                match self.motion.tier() {
                    MotionTier::Static => {
                        ui.label("(chế độ đơn giản: mặt tĩnh)");
                    }
                    MotionTier::Reduced => {
                        ui.label("(hoạt ảnh tạm tắt)");
                    }
                    MotionTier::Full => {}
                }
            });
            ui.separator();

            // Backend status line: what is in use, and where data goes.
            if let Some(error) = &self.chain_error {
                ui.colored_label(
                    Color32::from_rgb(0xDC, 0x26, 0x26),
                    format!("Backend: {error}"),
                );
            } else if !self.chain_names.is_empty() {
                let in_use = match self.backend_in_use.as_deref() {
                    Some(name) => {
                        let place = match self.backend_info(name) {
                            Some(info) if info.loopback => "cục bộ",
                            Some(_) => "ở xa",
                            None => "không rõ vị trí",
                        };
                        format!(" · đang dùng: {name} ({place})")
                    }
                    None => String::new(),
                };
                ui.label(format!(
                    "Backend: {} · Chế độ riêng tư: {}{}",
                    self.chain_names.join(" -> "),
                    if self.privacy_on { "BẬT" } else { "TẮT" },
                    in_use
                ));
            }

            // Empty-chat onboarding: short suggestions that fill the
            // input (never auto-submit, never promise computer use).
            let show_suggestions =
                !self.chat.busy && self.chat.messages.len() <= 1 && self.chat.input.is_empty();
            if show_suggestions {
                // Wrapped: at narrow widths / high UI scale the
                // row must flow onto more lines, never clip a chip
                // (measured clipping at 500pt before this fix).
                ui.horizontal_wrapped(|ui| {
                    ui.label("Thử hỏi:");
                    for suggestion in [
                        "Bạn có thể làm gì?",
                        "Viết một đoạn thơ ngắn về buổi sáng",
                        "Lưu ghi chú: mua sữa",
                    ] {
                        if ui.button(suggestion).clicked() {
                            self.chat.input = suggestion.to_string();
                        }
                    }
                });
                ui.add_space(4.0);
            }

            // Messages. Cloned for the loop: the Retry/Vietnamese
            // buttons below mutate the app while iterating.
            let messages = self.chat.messages.clone();
            let last_assistant = messages.iter().rposition(|m| m.role == ChatRole::Assistant);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (index, message) in messages.iter().enumerate() {
                        let prefix = match message.role {
                            ChatRole::User => "Bạn: ",
                            ChatRole::Assistant => "Mimint: ",
                            ChatRole::System => "",
                        };
                        ui.label(format!("{prefix}{}", message.text));
                        if !message.tools_used.is_empty() {
                            ui.label(format!(
                                "(đã dùng công cụ: {})",
                                message.tools_used.join(", ")
                            ));
                        }
                        // Retry + Vietnamese buttons under the LAST
                        // assistant message only.
                        if Some(index) == last_assistant
                            && !self.chat.busy
                            && self.chat.pending_approval.is_none()
                            && !self.chat.stopped
                        {
                            ui.horizontal(|ui| {
                                if ui.button("Thử lại").clicked() {
                                    if let Some(prompt) = self.chat.retry_prompt() {
                                        self.resubmit(
                                            prompt,
                                            "↻ Đang thử lại câu hỏi vừa rồi…",
                                            now,
                                        );
                                    }
                                }
                                if ui.button("Trả lời bằng tiếng Việt").clicked() {
                                    if let Some(prompt) = self.chat.vietnamese_prompt() {
                                        self.resubmit(
                                            prompt,
                                            "↻ Đang hỏi lại bằng tiếng Việt…",
                                            now,
                                        );
                                    }
                                }
                            });
                        }
                        ui.add_space(4.0);
                    }
                    if self.chat.busy {
                        if let Some(line) = self.turn_phase_line() {
                            ui.label(line);
                        } else {
                            ui.label("Mimint đang nghĩ…");
                        }
                    }
                });
        });
    }
}

impl MimintApp {
    fn decide_approval(&mut self, agent_id: &str, id: u64, decision: ApprovalDecision) {
        if self.chat.resolve_approval(agent_id, id) {
            self.face.clear_system_override(Instant::now());
            if let Some(job) = self.pending_job.take() {
                self.approval_crop = None;
                if let Some(row) = self.agents.get_mut(agent_id) {
                    row.view.pending_approval = false;
                }
                // Deadline re-check at the UI layer (F4-06):
                // a card that expired between renders must
                // never be answered, even if the button was
                // clicked. The worker already timed out; the
                // decision is dropped, not sent.
                if Instant::now() >= job.deadline {
                    self.chat.add_system(format!(
                        "Thẻ phê duyệt đã hết hạn ({} giây): công cụ đã bị từ chối.",
                        job.timeout.as_secs()
                    ));
                    self.log_event("approval_expired_late_decision_refused");
                    return;
                }
                let _ = job.reply.send(decision);
                self.log_event(if decision == ApprovalDecision::Allow {
                    "approval_decided allow"
                } else {
                    "approval_decided deny"
                });
            }
        }
    }

    /// The card's crop preview texture for one job, computed
    /// once and cached by job id. `None` when there is no
    /// frame store, no stored frame, or the tool has no point.
    /// Display only: the crop never feeds any decision.
    fn approval_crop_texture(
        &mut self,
        ctx: &egui::Context,
        req: &crate::chat::ApprovalRequest,
    ) -> Option<egui::TextureHandle> {
        let key = (req.agent_id.clone(), req.id);
        if let Some((cached_key, cached)) = &self.approval_crop {
            if *cached_key == key {
                return cached.clone();
            }
        }
        let texture = self.build_approval_crop(ctx, req);
        self.approval_crop = Some((key, texture.clone()));
        texture
    }

    fn build_approval_crop(
        &self,
        ctx: &egui::Context,
        req: &crate::chat::ApprovalRequest,
    ) -> Option<egui::TextureHandle> {
        let frames = self.frame_store.as_ref()?;
        let (x, y) = approval_point(&req.tool, &req.args)?;
        let id = frames.latest_id()?;
        let image = frames.with_frame(id, |frame| {
            let (rgba, width, height) = crop_preview(frame, x, y, 120)?;
            Some(egui::ColorImage::from_rgba_unmultiplied(
                [width as usize, height as usize],
                &rgba,
            ))
        })??;
        Some(ctx.load_texture(
            format!("approval-crop-{}-{}", req.agent_id, req.id),
            image,
            egui::TextureOptions::default(),
        ))
    }

    /// The agents panel (§15.1, §15.3). Invisible in
    /// single-agent mode: when parallelism is off and no
    /// multi-agent state exists, the sidebar is exactly as it
    /// was before Phase 6.
    fn agents_panel_ui(&mut self, ui: &mut egui::Ui) {
        let show = self.multi_agent || self.agents.len() > 1 || self.degraded;
        if !show {
            return;
        }
        ui.heading("Agent");
        // The global body state, separate from any one agent.
        if let BodyDisplayState::NotReady(_) = self.body_state {
            let reason = self.body_state.reason_label().unwrap_or("");
            ui.colored_label(
                Color32::from_rgb(0xDC, 0x26, 0x26),
                format!("{}: {}", self.body_state.label(), reason),
            );
            if self.can_rebuild_body {
                let rebuild = ui.button("Khởi tạo lại bộ điều khiển").on_hover_text(
                    "Khởi tạo lại bộ điều khiển: hành động đã dừng không tự chạy lại; không hoàn tác kết quả chưa xác định.",
                );
                if rebuild.clicked() {
                    self.body_state = BodyDisplayState::NotReady(BodyNotReadyReason::Rebuilding);
                    self.worker.rebuild_body();
                }
                ui.label(
                    "Hành động đã dừng không tự chạy lại; không hoàn tác kết quả chưa xác định.",
                );
            }
        }
        if self.degraded {
            ui.colored_label(
                Color32::from_rgb(0xDC, 0x26, 0x26),
                format!(
                    "Degraded: {} — không spawn agent mới tới khi khởi động lại.",
                    self.degraded_reason
                        .as_deref()
                        .unwrap_or("không xác minh được kết thúc")
                ),
            );
        }
        // F6D-19: the cap is a cap, not a promise of speed.
        ui.label(format!(
            "Tối đa {} agent cùng lúc. Thêm agent không đảm bảo tăng tốc (đặc biệt với backend cục bộ trên CPU).",
            self.max_agents
        ));
        // F6-18: why hierarchy defaults OFF, in plain words
        // with the measured token cost (stub harness: 360 vs
        // 600 tokens for the same two clicks). Wall-time
        // numbers live in docs/operations.md.
        if self.hierarchy_enabled {
            ui.colored_label(
                Color32::from_rgb(0xB4, 0x53, 0x09),
                "Thứ bậc (hierarchy): BẬT — thử nghiệm. Ủy quyền cho sub-agent tốn thêm token (đo stub: 1,67× cho cùng một việc) và thời gian; chỉ bật khi thật sự cần chia việc.",
            );
        } else {
            ui.label(
                "Thứ bậc (hierarchy): TẮT (mặc định). Ủy quyền cho sub-agent tốn thêm token (đo stub: 1,67× cho cùng một việc) và thời gian, nên mặc định tắt — bật trong [hierarchy] của file cấu hình nếu cần.",
            );
        }
        // One row per agent, in id order.
        let mut ids: Vec<String> = self.agents.keys().cloned().collect();
        ids.sort();
        let mut stop_agent: Option<String> = None;
        for id in ids {
            let Some(row) = self.agents.get(&id) else {
                continue;
            };
            let name = agent_display_name(&id);
            let state = display_state(&row.view);
            let mut title = if row.label.is_empty() || row.label == name {
                name.clone()
            } else {
                format!("{name} · {}", row.label)
            };
            if let Some(parent) = &row.parent {
                title.push_str(&format!(" (con của {})", agent_display_name(parent)));
            }
            ui.label(format!("• {title}: {}", state.label()));
            // F6-18: an unknown outcome is not a retry
            // prompt — the user checks reality first.
            if matches!(state, AgentDisplayState::UnknownOutcome) {
                ui.label("    Không tự thử lại: kiểm tra thực tế trên màn hình trước, rồi mới quyết định bước tiếp theo.");
            }
            if let Some(role) = row.fail_role {
                ui.label(format!("    Vai trò sau lỗi: {}", role.label()));
            }
            if !row.phase.is_empty() {
                ui.label(format!("    {}", row.phase));
            }
            // The stop transition (§15.3): requested, then
            // confirmed or unverified — never "stopped" just
            // because the cancel was sent.
            if !matches!(row.stop_phase, StopPhase::Idle) {
                ui.label(format!("    Dừng: {}", row.stop_phase.label()));
            }
            if row.view.turn_running
                && matches!(row.stop_phase, StopPhase::Idle)
                && ui.button("Dừng agent này").clicked()
            {
                stop_agent = Some(id.clone());
            }
        }
        if let Some(id) = stop_agent {
            if let Some(row) = self.agents.get_mut(&id) {
                row.view.cancel_requested = true;
                row.stop_phase = next_stop_phase(row.stop_phase, StopEvent::Requested);
            }
            self.worker.cancel_agent(id);
        }
        ui.separator();
        // DỪNG KHẨN CẤP (§15.3): the same latch as the
        // header button, with a scope warning before it runs.
        // Stopping ONE agent keeps its own button above.
        if self.pending_stop_all {
            ui.colored_label(
                Color32::from_rgb(0xDC, 0x26, 0x26),
                "DỪNG KHẨN CẤP sẽ: dừng mọi agent; khóa dừng toàn hệ thống (latch) sẽ bật; thread body đang kẹt (nếu có) không giật được — nó chỉ dừng khi hành động hiện tại xong hoặc hết thời gian chờ.",
            );
            ui.horizontal(|ui| {
                if ui.button("Xác nhận dừng khẩn cấp").clicked() {
                    self.pending_stop_all = false;
                    self.worker.stop_all();
                }
                if ui.button("Hủy").clicked() {
                    self.pending_stop_all = false;
                }
            });
        } else if ui
            .button("DỪNG KHẨN CẤP")
            .on_hover_text("Dừng mọi agent và khóa điều khiển cho tới khi khởi tạo lại bộ điều khiển hoặc khởi động lại ứng dụng.")
            .clicked()
        {
            self.pending_stop_all = true;
        }
        // MCP status: the continuous unsandboxed indicator
        // (§6.3) for every connected server.
        for server in &self.mcp_servers {
            if server.unsandboxed {
                ui.colored_label(
                    Color32::from_rgb(0xDC, 0x26, 0x26),
                    format!(
                        "MCP {}: KHÔNG SANDBOX — server chạy trực tiếp trên máy này{}.",
                        server.name,
                        if server.connected {
                            ""
                        } else {
                            " (chưa nối)"
                        }
                    ),
                );
            } else {
                ui.label(format!(
                    "MCP {}: chạy trong sandbox{}.",
                    server.name,
                    if server.connected {
                        ""
                    } else {
                        " (chưa nối)"
                    }
                ));
            }
        }
        ui.separator();
    }

    /// The backend picker, shared by the full/compact header and the
    /// minimal menu. Switching to a known-remote backend asks first
    /// (the warning bar); the switch only applies on confirmation.
    fn backend_picker_ui(&mut self, ui: &mut egui::Ui) {
        let selected_text = if self.selected_backend.is_empty() {
            "Chuỗi dự phòng".to_string()
        } else {
            self.selected_backend.clone()
        };
        let before = self.selected_backend.clone();
        let response = egui::ComboBox::from_id_salt("backend_picker")
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.selected_backend, String::new(), "Chuỗi dự phòng");
                for name in self.backend_names.clone() {
                    ui.selectable_value(&mut self.selected_backend, name.clone(), name.clone());
                }
            });
        response.response.on_hover_text(
            "Backend dự phòng: thử lần lượt các backend theo thứ tự đã cấu hình. Backend cục bộ chạy trên máy này; backend ở xa nhận câu hỏi qua mạng.",
        );
        if self.selected_backend != before {
            let new = self.selected_backend.clone();
            if !new.is_empty() && self.switch_needs_warning(&new) {
                self.selected_backend = before;
                self.pending_backend_change = Some(new);
            } else {
                self.rebuild();
            }
        }
    }

    /// The Privacy Mode toggle, shared by the header and the menu.
    fn privacy_ui(&mut self, ui: &mut egui::Ui) {
        let before = self.privacy_on;
        let response = ui.checkbox(&mut self.privacy_on, "Chế độ riêng tư");
        response.on_hover_text(
            "Chế độ riêng tư: chỉ cho phép backend chạy trên máy này (cục bộ). Khi BẬT, backend ở xa bị chặn — lượt trò chuyện có thể thất bại nếu không có backend cục bộ.",
        );
        if self.privacy_on != before {
            self.rebuild();
        }
    }
}

/// Plain-language explanations of a profile's attributes
/// (F5-13). Attributes whose engine does not exist yet are
/// marked "chưa khả dụng" — the line never claims behavior
/// the app cannot perform.
fn profile_detail_lines(profile: &Profile) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(if profile.privacy {
        "Chế độ riêng tư: BẬT — chỉ dùng backend trên máy này".to_string()
    } else {
        "Chế độ riêng tư: TẮT — câu hỏi có thể tới backend ở xa".to_string()
    });
    if profile.chain.is_empty() {
        lines.push("Backend: chưa cấu hình".to_string());
    } else {
        lines.push(format!("Backend: {}", profile.chain.join(" -> ")));
    }
    match &profile.skills {
        None => lines.push("Kỹ năng: mọi kỹ năng đã duyệt".to_string()),
        Some(selected) => lines.push(format!("Kỹ năng: chỉ {}", selected.join(", "))),
    }
    if let Some(voice) = &profile.voice {
        lines.push(format!(
            "Giọng nói: {voice} — chưa khả dụng (chưa có engine giọng nói)"
        ));
    }
    if let Some(avatar) = &profile.avatar {
        lines.push(format!(
            "Avatar: {avatar} — chưa khả dụng (mặt hiển thị chưa theo hồ sơ)"
        ));
    }
    if profile.proactive {
        lines.push("Chế độ chủ động: BẬT — chưa khả dụng (chưa có engine chủ động)".to_string());
    }
    if let Some(budget) = profile.budget_usd {
        lines.push(format!(
            "Ngân sách: ${budget:.2} — chưa khả dụng (chưa đo chi tiêu USD)"
        ));
    }
    lines
}

/// "3 phút trước" style age for a file modification time. No
/// date library is pulled in for one clock; the future (clock
/// skew) reads as "vừa xong".
fn ago_text(modified: std::time::SystemTime) -> String {
    let secs = modified.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    if secs < 60 {
        "vừa xong".to_string()
    } else if secs < 3600 {
        format!("{} phút trước", secs / 60)
    } else if secs < 86400 {
        format!("{} giờ trước", secs / 3600)
    } else {
        format!("{} ngày trước", secs / 86400)
    }
}

/// Open a file with the operating system's default application
/// (F5-11). Best effort: a failure leaves the user with the
/// path shown above the button, which they can open by hand.
fn open_file_in_default_app(path: &std::path::Path) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", &path.to_string_lossy()])
        .spawn();
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = path;
}

/// Draw Mint's face: mint fluff ball, off-white face patch, eyes and
/// mouth from `params`, pink cheeks, leaf sprout, two-tone tuft.
/// `tilt_degrees` rotates the facial features around the center.
fn draw_face(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    params: crate::face::FaceParams,
    blinking: bool,
    tilt_degrees: f32,
) {
    let tilt = tilt_degrees.to_radians();
    let rotate = |offset: Vec2| -> Pos2 {
        let (sin, cos) = tilt.sin_cos();
        center
            + Vec2::new(
                offset.x * cos - offset.y * sin,
                offset.x * sin + offset.y * cos,
            )
    };

    // Body: mint ball.
    painter.circle_filled(center, radius, MINT_BODY);
    // Celebrating: star burst around the head.
    if params.star_burst {
        let star = Stroke::new(2.5, Color32::from_rgb(0xF5, 0xC2, 0x42));
        for (dx, dy) in [(-1.15, -0.75), (1.15, -0.75), (-1.25, 0.25), (1.25, 0.25)] {
            let c = center + Vec2::new(radius * dx, radius * dy);
            let r = radius * 0.09;
            painter.line_segment([c + Vec2::new(-r, 0.0), c + Vec2::new(r, 0.0)], star);
            painter.line_segment([c + Vec2::new(0.0, -r), c + Vec2::new(0.0, r)], star);
        }
    }
    // Two-tone fluff tuft on top.
    painter.circle_filled(
        center + Vec2::new(-radius * 0.28, -radius * 0.92),
        radius * 0.22,
        MINT_BODY,
    );
    painter.circle_filled(
        center + Vec2::new(radius * 0.18, -radius * 0.98),
        radius * 0.18,
        LEAF_DARK,
    );
    // Leaf sprout.
    painter.line_segment(
        [
            center + Vec2::new(0.0, -radius * 0.95),
            center + Vec2::new(0.0, -radius * 1.18),
        ],
        Stroke::new(3.0, LEAF_DARK),
    );
    painter.add(egui::Shape::ellipse_filled(
        center + Vec2::new(-radius * 0.12, -radius * 1.18),
        Vec2::new(radius * 0.16, radius * 0.09),
        LEAF_DARK,
    ));
    painter.add(egui::Shape::ellipse_filled(
        center + Vec2::new(radius * 0.12, -radius * 1.18),
        Vec2::new(radius * 0.16, radius * 0.09),
        MINT_BODY,
    ));
    // Face patch.
    painter.add(egui::Shape::ellipse_filled(
        center + Vec2::new(0.0, radius * 0.08),
        Vec2::new(radius * 0.72, radius * 0.62),
        FACE_PATCH,
    ));

    // Eyes.
    let eye_dx = radius * 0.30;
    let eye_y = radius * -0.05;
    let eye_size = Vec2::new(radius * 0.11, radius * 0.16);
    for side in [-1.0, 1.0] {
        let eye_center = rotate(Vec2::new(eye_dx * side, eye_y));
        match params.eyes {
            EyeShape::Closed => {
                painter.line_segment(
                    [
                        eye_center + Vec2::new(-eye_size.x, 0.0),
                        eye_center + Vec2::new(eye_size.x, 0.0),
                    ],
                    Stroke::new(3.0, EYE_BLACK),
                );
            }
            EyeShape::HappyArc => {
                arc(
                    painter,
                    eye_center + Vec2::new(0.0, eye_size.y * 0.4),
                    eye_size.x,
                    std::f32::consts::PI * 1.05,
                    std::f32::consts::PI * 1.95,
                    Stroke::new(3.0, EYE_BLACK),
                );
            }
            EyeShape::Dot => {
                painter.circle_filled(eye_center, radius * 0.05, EYE_BLACK);
            }
            EyeShape::Wide => {
                painter.circle_filled(eye_center, radius * 0.14, EYE_BLACK);
                painter.circle_filled(
                    eye_center + Vec2::new(radius * 0.04, -radius * 0.04),
                    radius * 0.05,
                    Color32::WHITE,
                );
            }
            EyeShape::Oval => {
                if blinking {
                    painter.line_segment(
                        [
                            eye_center + Vec2::new(-eye_size.x, 0.0),
                            eye_center + Vec2::new(eye_size.x, 0.0),
                        ],
                        Stroke::new(3.0, EYE_BLACK),
                    );
                } else {
                    painter.add(egui::Shape::ellipse_filled(eye_center, eye_size, EYE_BLACK));
                    painter.circle_filled(
                        eye_center + Vec2::new(radius * 0.03, -radius * 0.05),
                        radius * 0.04,
                        Color32::WHITE,
                    );
                }
            }
        }
    }

    // Cheeks.
    if params.blush {
        for side in [-1.0, 1.0] {
            painter.circle_filled(
                rotate(Vec2::new(radius * 0.52 * side, radius * 0.22)),
                radius * 0.11,
                blush_color(),
            );
        }
    }

    // Mouth.
    let mouth_center = rotate(Vec2::new(0.0, radius * 0.30));
    let stroke = Stroke::new(3.0, EYE_BLACK);
    match params.mouth {
        MouthShape::Smile => {
            arc(
                painter,
                mouth_center + Vec2::new(0.0, -radius * 0.06),
                radius * 0.20,
                0.15 * std::f32::consts::PI,
                0.85 * std::f32::consts::PI,
                stroke,
            );
        }
        MouthShape::OpenSmile => {
            painter.add(egui::Shape::ellipse_filled(
                mouth_center,
                Vec2::new(radius * 0.16, radius * 0.11),
                EYE_BLACK,
            ));
        }
        MouthShape::SmallO => {
            painter.circle_filled(mouth_center, radius * 0.09, EYE_BLACK);
        }
        MouthShape::Flat => {
            painter.line_segment(
                [
                    mouth_center + Vec2::new(-radius * 0.14, 0.0),
                    mouth_center + Vec2::new(radius * 0.14, 0.0),
                ],
                stroke,
            );
        }
        MouthShape::Wavy => {
            let mut points = Vec::new();
            for i in 0..=12 {
                let t = i as f32 / 12.0;
                points.push(Pos2::new(
                    mouth_center.x + (t - 0.5) * radius * 0.30,
                    mouth_center.y + (t * std::f32::consts::TAU * 1.5).sin() * radius * 0.04,
                ));
            }
            painter.line(points, stroke);
        }
        MouthShape::Yawn => {
            painter.add(egui::Shape::ellipse_filled(
                mouth_center,
                Vec2::new(radius * 0.09, radius * 0.13),
                EYE_BLACK,
            ));
        }
    }
}

/// Polyline approximation of a circular arc (angles in radians,
/// 0 = +x, positive = clockwise in screen coordinates).
fn arc(painter: &egui::Painter, center: Pos2, radius: f32, start: f32, end: f32, stroke: Stroke) {
    let mut points = Vec::new();
    let steps = 16;
    for i in 0..=steps {
        let a = start + (end - start) * (i as f32 / steps as f32);
        points.push(Pos2::new(
            center.x + a.cos() * radius,
            center.y + a.sin() * radius,
        ));
    }
    painter.line(points, stroke);
}
