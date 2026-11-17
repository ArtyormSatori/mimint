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

use crate::approval_text::approval_detail;
use crate::approver::{ApprovalJob, RepaintSignal};
use crate::audit_view::{read_audit_tail, AuditRow};
use crate::chat::{ChatRole, ChatState, ControlState, TurnStatus};
use crate::face::{breath_scale, is_blinking, EyeShape, FaceMachine, MouthShape};
use crate::motion::{MotionController, MotionTier};
use crate::worker::{FailureKind, WorkerEvent, WorkerHandle};
use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use mimint_body::ScreenRect;
use mimint_core::ApprovalDecision;
use std::path::PathBuf;
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
}

impl MimintApp {
    pub fn new(
        worker: WorkerHandle,
        approvals: std::sync::mpsc::Receiver<ApprovalJob>,
        options: AppOptions,
    ) -> Self {
        let now = Instant::now();
        let mut chat = ChatState::new();
        // The settled greeting, from the design spec.
        chat.add_system("Hôm nay bạn muốn làm gì nào? Mình có thể giúp gì cho bạn".to_string());
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
                    text,
                    tools_used,
                    partial,
                    backend,
                } => {
                    let mut text = text;
                    if partial {
                        text.push_str("\n(câu trả lời chưa xong: hết ngân sách bước)");
                    }
                    let had_tools = !tools_used.is_empty();
                    self.backend_in_use = backend;
                    self.chat.on_reply(text, tools_used);
                    self.face.on_turn_finished(now, had_tools);
                    self.state_since = now;
                    self.clear_stale_card();
                    self.log_event("reply_displayed");
                }
                WorkerEvent::TurnFailed { error, kind } => {
                    self.chat.on_error(error, kind);
                    self.face.on_error(now);
                    self.state_since = now;
                    self.clear_stale_card();
                }
                WorkerEvent::TurnCancelled => {
                    self.chat.on_cancelled();
                    self.chat.add_system(
                        "Đã hủy lượt. Câu hỏi của bạn vẫn còn — bấm Thử lại để hỏi tiếp."
                            .to_string(),
                    );
                    self.face.on_error(now);
                    self.state_since = now;
                    self.clear_stale_card();
                }
                WorkerEvent::TryingBackend { name } => {
                    self.backend_in_use = Some(name.clone());
                    self.chat.status = TurnStatus::TryingBackend(name);
                }
                WorkerEvent::WorkerFault { error } => {
                    self.chat.control_error = true;
                    self.chat.on_error(error, FailureKind::Other);
                    self.face.on_error(now);
                    self.state_since = now;
                    self.clear_stale_card();
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
                WorkerEvent::StopTripped { reason } => {
                    // The body's emergency stop latched: the lamp
                    // shows Stopped, turns block, and only a
                    // restart clears it. The running turn reports
                    // its own cancellation separately.
                    self.chat.request_body_stop();
                    self.chat.on_cancelled();
                    self.chat.add_system(format!(
                        "Đã DỪNG KHẨN CẤP — phiên điều khiển đã dừng và sẽ không chạy tiếp. Khởi động lại ứng dụng để điều khiển tiếp. ({reason})"
                    ));
                    self.face.on_error(now);
                    self.state_since = now;
                    self.clear_stale_card();
                }
            }
        }
    }

    /// A turn ended: any card still on screen is stale (the worker
    /// already resolved it). Clear it silently — the turn's own
    /// outcome is the news, not the card.
    fn clear_stale_card(&mut self) {
        self.pending_job = None;
        self.chat.expire_approval();
    }

    fn drain_approval_jobs(&mut self) {
        if self.pending_job.is_some() {
            return;
        }
        if let Ok(job) = self.approvals.try_recv() {
            self.chat
                .request_approval(job.id, job.tool.clone(), job.args.clone(), job.deadline);
            self.pending_job = Some(job);
            self.log_event("approval_shown");
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
            self.pending_job = None;
            if self.chat.expire_approval() {
                self.chat.add_system(
                    "Hết thời gian chờ phê duyệt (120 giây): công cụ đã bị từ chối.".to_string(),
                );
                self.log_event("approval_expired");
            }
        }
    }

    fn submit(&mut self, now: Instant) {
        if let Some(prompt) = self.chat.submit_input() {
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
        if self.chat.stopped {
            return Some("Đã dừng. Bấm Tiếp tục để trò chuyện tiếp.".to_string());
        }
        if self.chat.pending_approval.is_some() {
            return Some("Chờ bạn xác nhận trên thẻ phê duyệt…".to_string());
        }
        match &self.chat.status {
            TurnStatus::Connecting => Some("Đang kết nối…".to_string()),
            TurnStatus::TryingBackend(name) => Some(format!("Đang thử backend {name}…")),
            TurnStatus::Idle => None,
        }
    }

    fn error_line(&self) -> Option<String> {
        let error = self.chat.last_error.as_ref()?;
        let heading = match self.chat.last_error_kind {
            Some(FailureKind::Auth) => "Lỗi xác thực",
            Some(FailureKind::PolicyBlocked) => "Bị chặn bởi Chế độ riêng tư",
            Some(FailureKind::OutOfBackends) => "Hết backend",
            Some(FailureKind::Other) | None => "Lỗi",
        };
        Some(format!("{heading}: {error}"))
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
        self.drain_approval_jobs();
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
                    // A body stop only clears on restart: offering
                    // "Tiếp tục" would lie (the body flag stays
                    // tripped). Say so, plainly.
                    if !minimal {
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
                    let stop =
                        ui.add_enabled(can_stop, egui::Button::new("DỪNG NGAY"));
                    if stop.clicked() {
                        self.worker.stop_now();
                        self.chat.request_stop_now();
                    }
                    if !can_stop {
                        stop.on_hover_text(
                            "DỪNG NGAY chỉ dùng khi đang có lượt trò chuyện, thẻ phê duyệt, hoặc phiên điều khiển.",
                        );
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    if minimal {
                        // Everything else hides behind one menu.
                        ui.menu_button("⋯", |ui| {
                            ui.label("Hồ sơ: Mặc định");
                            self.backend_picker_ui(ui);
                            self.privacy_ui(ui);
                        });
                    } else {
                        if compact {
                            ui.label("👤").on_hover_text("Hồ sơ: Mặc định");
                        } else {
                            ui.label("Hồ sơ: Mặc định");
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
                    ui.heading("Cuộc trò chuyện");
                    ui.label("• Phiên hiện tại");
                    ui.separator();
                    ui.heading("Hồ sơ");
                    ui.label("• Mặc định (đang dùng)");
                    ui.label("Quản lý hồ sơ đầy đủ là việc Phase 5+.");
                    ui.separator();
                    ui.heading("Cài đặt");
                    ui.checkbox(&mut self.show_audit, "Nhật ký kiểm tra");
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
                    ui.heading("Chó robot (ESP32)");
                    ui.label("Trạng thái: chưa kết nối");
                    ui.label("Kết nối robot là việc Phase 7.");
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
                        ui.label(format!(
                            "{} bản ghi mới nhất (giờ UTC)",
                            self.audit_rows.len()
                        ));
                    });
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for row in &self.audit_rows {
                            if self.audit_technical {
                                ui.label(format!(
                                    "{} {}",
                                    crate::audit_view::format_time_ms(row.ts_ms),
                                    row.raw
                                ));
                            } else {
                                ui.label(format!(
                                    "{} {}",
                                    crate::audit_view::format_time_ms(row.ts_ms),
                                    row.friendly_summary()
                                ));
                            }
                        }
                        if self.audit_rows.is_empty() {
                            ui.label("Chưa có bản ghi. Bấm Làm mới sau một lượt trò chuyện.");
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
            // Approval card (P0-17): action-specific content + countdown.
            if let Some(req) = self.chat.pending_approval.clone() {
                let detail = approval_detail(&req.tool, &req.args);
                ui.group(|ui| {
                    ui.heading(format!("Cần phê duyệt — {}", detail.title));
                    ui.label(format!("Yêu cầu #{} · công cụ: {}", req.id, req.tool));
                    for (label, value) in &detail.rows {
                        ui.label(format!("{label}: {value}"));
                    }
                    ui.label(format!("Hoàn tác: {}", detail.reversible));
                    if let Some(deadline) = req.deadline {
                        let left = deadline.saturating_duration_since(now).as_secs();
                        ui.label(format!("Còn {left} giây — hết giờ sẽ tự từ chối."));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Cho phép").clicked() {
                            self.decide_approval(req.id, ApprovalDecision::Allow);
                        }
                        if ui.button("Từ chối").clicked() {
                            self.decide_approval(req.id, ApprovalDecision::Deny);
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
            ui.horizontal(|ui| {
                ui.label(format!("Trạng thái: {}", self.face.state().label()));
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
                        ui.label("Mimint đang nghĩ…");
                    }
                });
        });
    }
}

impl MimintApp {
    fn decide_approval(&mut self, id: u64, decision: ApprovalDecision) {
        if self.chat.resolve_approval(id) {
            if let Some(job) = self.pending_job.take() {
                let _ = job.reply.send(decision);
                self.log_event(if decision == ApprovalDecision::Allow {
                    "approval_decided allow"
                } else {
                    "approval_decided deny"
                });
            }
        }
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
