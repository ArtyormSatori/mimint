//! The eframe application: top bar, sidebar, ball avatar, chat,
//! approval card, control lamp + STOP, and the audit viewer.
//!
//! Layout follows `design/DESIGN-BALL-UI.md` §3. Everything the user
//! sees is state from [`ChatState`]/[`FaceMachine`]; this module only
//! renders it and forwards input to the worker.
//!
//! Test hooks (for the demo gate, not for users):
//! - `MIMINT_UI_EVENT_LOG=<path>`: appends `window_ready`,
//!   `input_rect x y w h`, `prompt_submitted`, `reply_displayed`,
//!   `approval_shown`, `approval_decided` lines.
//! - `MIMINT_UI_BENCH_SECS=<n>`: run N seconds, print frame stats,
//!   exit (the Phase 0 lane-3 method).
//! - `MIMINT_UI_DEGRADED=1`: static face + visible "simple mode"
//!   label (weak machines).

use crate::approver::ApprovalJob;
use crate::audit_view::{read_audit_tail, AuditRow};
use crate::chat::{ChatRole, ChatState};
use crate::face::{breath_scale, is_blinking, EyeShape, FaceMachine, MouthShape};
use crate::worker::{WorkerEvent, WorkerHandle};
use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use std::path::PathBuf;
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

/// Options for one app instance (from flags/env, decided by `main`).
pub struct AppOptions {
    pub privacy_on: bool,
    /// Backend names in configured chain order.
    pub backend_names: Vec<String>,
    pub audit_path: PathBuf,
    pub degraded: bool,
    pub bench_secs: Option<f64>,
    pub event_log: Option<PathBuf>,
    /// Onboarding suggestion shown when no backend is configured.
    pub onboarding_text: Option<String>,
}

pub struct MimintApp {
    chat: ChatState,
    face: FaceMachine,
    worker: WorkerHandle,
    approvals: std::sync::mpsc::Receiver<ApprovalJob>,
    pending_job: Option<ApprovalJob>,
    privacy_on: bool,
    backend_names: Vec<String>,
    /// "" means the full configured chain order.
    selected_backend: String,
    chain_names: Vec<String>,
    chain_error: Option<String>,
    show_audit: bool,
    audit_rows: Vec<AuditRow>,
    audit_path: PathBuf,
    degraded: bool,
    onboarding_text: Option<String>,
    onboarding_shown: bool,
    started: Instant,
    state_since: Instant,
    first_frame: bool,
    bench_secs: Option<f64>,
    frames: u64,
    event_log: Option<PathBuf>,
    last_input_rect: Option<Rect>,
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
            selected_backend: String::new(),
            chain_names: Vec::new(),
            chain_error: None,
            show_audit: false,
            audit_rows: Vec::new(),
            audit_path: options.audit_path,
            degraded: options.degraded,
            onboarding_text: options.onboarding_text,
            onboarding_shown: false,
            started: now,
            state_since: now,
            first_frame: true,
            bench_secs: options.bench_secs,
            frames: 0,
            event_log: options.event_log,
            last_input_rect: None,
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

    fn drain_worker_events(&mut self, now: Instant) {
        while let Some(event) = self.worker.try_recv_event() {
            match event {
                WorkerEvent::TurnDone {
                    text,
                    tools_used,
                    partial,
                } => {
                    let mut text = text;
                    if partial {
                        text.push_str("\n(câu trả lời chưa xong: hết ngân sách bước)");
                    }
                    let had_tools = !tools_used.is_empty();
                    self.chat.on_reply(text, tools_used);
                    self.face.on_turn_finished(now, had_tools);
                    self.state_since = now;
                    self.log_event("reply_displayed");
                }
                WorkerEvent::TurnFailed { error } => {
                    self.chat.on_error(error);
                    self.face.on_error(now);
                    self.state_since = now;
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
            }
        }
    }

    fn drain_approval_jobs(&mut self) {
        if self.pending_job.is_some() {
            return;
        }
        if let Ok(job) = self.approvals.try_recv() {
            self.chat
                .request_approval(job.tool.clone(), job.args.clone());
            self.pending_job = Some(job);
            self.log_event("approval_shown");
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
}

impl eframe::App for MimintApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.frames += 1;
        if self.first_frame {
            self.first_frame = false;
            self.log_event("window_ready");
        }
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
        } else {
            // Keep animations and timers alive at ~10fps when idle.
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }

        self.drain_worker_events(now);
        self.drain_approval_jobs();
        self.face.tick(now);

        // ---- Top bar ----
        egui::Panel::top("top_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
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
                ui.heading("Mimint");
                ui.separator();
                // Control lamp + STOP (P0-17/P0-22 components).
                let lamp = if self.chat.control_active {
                    Color32::from_rgb(0x22, 0xC5, 0x5E)
                } else {
                    Color32::from_rgb(0x9C, 0xA3, 0xAF)
                };
                let (lamp_rect, _) =
                    ui.allocate_exact_size(Vec2::new(12.0, 12.0), egui::Sense::hover());
                ui.painter().circle_filled(lamp_rect.center(), 5.0, lamp);
                ui.label(if self.chat.control_active {
                    "Đang điều khiển"
                } else {
                    "Không điều khiển"
                });
                let stop = ui.add_enabled(self.chat.control_active, egui::Button::new("DỪNG NGAY"));
                if stop.clicked() {
                    self.chat.request_stop();
                }
                if !self.chat.control_active {
                    stop.on_hover_text(
                        "Chưa có phiên điều khiển máy (body-driving là việc Phase 4+).",
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label("Hồ sơ: Mặc định");
                    ui.separator();
                    // Backend picker.
                    let selected_text = if self.selected_backend.is_empty() {
                        "Chuỗi dự phòng".to_string()
                    } else {
                        self.selected_backend.clone()
                    };
                    let mut changed = false;
                    egui::ComboBox::from_id_salt("backend_picker")
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_value(
                                    &mut self.selected_backend,
                                    String::new(),
                                    "Chuỗi dự phòng",
                                )
                                .changed()
                            {
                                changed = true;
                            }
                            for name in self.backend_names.clone() {
                                if ui
                                    .selectable_value(
                                        &mut self.selected_backend,
                                        name.clone(),
                                        name.clone(),
                                    )
                                    .changed()
                                {
                                    changed = true;
                                }
                            }
                        });
                    if changed {
                        self.rebuild();
                    }
                    ui.separator();
                    // Privacy toggle.
                    let before = self.privacy_on;
                    ui.checkbox(&mut self.privacy_on, "Riêng tư");
                    if self.privacy_on != before {
                        self.rebuild();
                    }
                });
            });
        });

        // ---- Sidebar ----
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
                if self.degraded {
                    ui.label("Chế độ đơn giản: BẬT");
                }
                ui.separator();
                ui.heading("Chó robot (ESP32)");
                ui.label("Trạng thái: chưa kết nối");
                ui.label("Kết nối robot là việc Phase 7.");
            });

        // ---- Audit panel (bottom, collapsible) ----
        if self.show_audit {
            egui::Panel::bottom("audit_panel")
                .default_size(160.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("Nhật ký kiểm tra");
                        if ui.button("Làm mới").clicked() {
                            self.audit_rows = read_audit_tail(&self.audit_path, 100);
                        }
                        ui.label(format!(
                            "{} bản ghi mới nhất (giờ UTC)",
                            self.audit_rows.len()
                        ));
                    });
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for row in &self.audit_rows {
                            ui.label(format!(
                                "{} {} {} {}{} {}",
                                crate::audit_view::format_time_ms(row.ts_ms),
                                row.kind,
                                row.tool.as_deref().unwrap_or("-"),
                                row.decision.as_deref().unwrap_or(""),
                                if row.decision.is_some() { ":" } else { "" },
                                row.detail
                            ));
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
            // Approval card (P0-17).
            if let Some(req) = self.chat.pending_approval.clone() {
                ui.group(|ui| {
                    ui.heading("Cần phê duyệt");
                    ui.label(format!("Công cụ: {}", req.tool));
                    ui.label(format!(
                        "Tham số: {}",
                        serde_json::to_string_pretty(&req.args)
                            .unwrap_or_else(|_| req.args.to_string())
                    ));
                    ui.horizontal(|ui| {
                        if ui.button("Cho phép").clicked() {
                            self.decide_approval(req.id, true);
                        }
                        if ui.button("Từ chối").clicked() {
                            self.decide_approval(req.id, false);
                        }
                    });
                    ui.label("Từ chối là mặc định an toàn: công cụ sẽ không chạy.");
                });
                ui.separator();
            }

            // Input row.
            ui.horizontal(|ui| {
                let input = ui.add(
                    egui::TextEdit::singleline(&mut self.chat.input)
                        .hint_text("Nhắn cho Mimint…")
                        .desired_width(ui.available_width() - 130.0),
                );
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
                if input.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !self.chat.input.trim().is_empty()
                {
                    self.face.on_activity(now);
                    self.submit(now);
                    input.request_focus();
                }
                let mic = ui.add_enabled(false, egui::Button::new("🎤"));
                mic.on_hover_text("Voice chưa có ở Phase 3.");
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
            let breath = if self.degraded {
                1.0
            } else {
                breath_scale(elapsed)
            };
            let blinking = is_blinking(elapsed) || params.eyes == EyeShape::Closed;
            let bounce = if self.degraded || !params.bounce {
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
                if self.degraded {
                    ui.label("(chế độ đơn giản: mặt tĩnh)");
                }
            });
            ui.separator();

            // Backend status line.
            if let Some(error) = &self.chain_error {
                ui.colored_label(
                    Color32::from_rgb(0xDC, 0x26, 0x26),
                    format!("Backend: {error}"),
                );
            } else if !self.chain_names.is_empty() {
                ui.label(format!(
                    "Backend: {} · Riêng tư: {}",
                    self.chain_names.join(" -> "),
                    if self.privacy_on { "BẬT" } else { "TẮT" }
                ));
            }
            if let Some(error) = &self.chat.last_error {
                ui.colored_label(Color32::from_rgb(0xDC, 0x26, 0x26), format!("Lỗi: {error}"));
            }

            // Messages.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for message in &self.chat.messages {
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
    fn decide_approval(&mut self, id: u64, allowed: bool) {
        if self.chat.resolve_approval(id) {
            if let Some(job) = self.pending_job.take() {
                let _ = job.reply.send(allowed);
                self.log_event(if allowed {
                    "approval_decided allow"
                } else {
                    "approval_decided deny"
                });
            }
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
