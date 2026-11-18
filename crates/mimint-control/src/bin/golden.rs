//! Golden-task gate (P4.1): GT-01..GT-04 with a deterministic
//! stub provider + one task with real Ollama, through the full
//! pipeline (provider → brain → guards → body tools → actor →
//! X11 body → canvas oracle → binary point-in-box checker).
//!
//! ```text
//! golden --harness <evalharness-bin> --workdir <dir> --log <out.jsonl>
//!        [--tasks GT-01,GT-02,GT-03,GT-04] [--ollama-task GT-01]
//!        [--base-url http://127.0.0.1:11434/v1] [--model qwen2.5:3b]
//!        [--font <ttf>] [--font-size 16]
//! ```
//!
//! The stub provider plays planner+grounder with FIXED
//! coordinates from the task script (the box centers): it
//! proves the pipeline delivers a click to the oracle exactly
//! where the provider said. The Ollama task proves the same
//! pipeline with a real model — and records the TRUE result
//! even when the model misses: model quality is not a
//! pipeline bug, and the two are distinguished in the log.
//! The Ollama task does NOT gate the exit code; the stub
//! tasks do. Exit 0 iff every stub task passes.
//!
//! Approval: the gate runs headless with an auto-allow
//! approver (the approval behavior itself is covered by the
//! safety gate and the UI tests, not here). Every decision is
//! still audited.
//!
//! Each task writes one JSONL record: task id, ref_expr, box,
//! predicted point (from the ORACLE, never from the model's
//! arguments), pass/fail, latency, the SHA-256 of the input
//! image (the PNG bytes of the first screenshot frame — what
//! the grounder saw), canvas version, and font name + size.

// Everything below is Linux-only (X11 canvas + X11 body); the
// module gate keeps other targets compiling with a stub main.
#[cfg(target_os = "linux")]
mod gate {
    use mimint_body::conformance::TargetOracle;
    use mimint_body::StopFlag;
    use mimint_body::{FileOracle, GuardedBody, RateLimits, X11Body};
    use mimint_control::actor::BodyActor;
    use mimint_control::frames::{encode_png, sha256_hex, FrameStore};
    use mimint_control::tools::{register_body_tools, PrivacyFlag};
    use mimint_control::verify::BodyStepVerifier;
    use mimint_core::{
        Approver, AuditLog, Brain, CallContext, ChatMessage, EgressPolicy, FunctionCall,
        GuardedHttpClient, MimintError, Notebook, Provider, ProviderClient, Registry, ToolCall,
    };
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// One golden task: id, referring expression, target box
    /// (inclusive, absolute pixels).
    struct GoldenTask {
        id: &'static str,
        ref_expr: &'static str,
        box_: (i32, i32, i32, i32),
    }

    const TASKS: &[GoldenTask] = &[
        GoldenTask {
            id: "GT-01",
            ref_expr: "nút Lưu",
            box_: (220, 60, 300, 100),
        },
        GoldenTask {
            id: "GT-02",
            ref_expr: "nút Đăng nhập",
            box_: (100, 140, 220, 180),
        },
        GoldenTask {
            id: "GT-03",
            ref_expr: "biểu tượng kính lúp tìm kiếm",
            box_: (600, 60, 632, 92),
        },
        GoldenTask {
            id: "GT-04",
            ref_expr: "ô vuông chọn trước chữ Nhớ tôi",
            box_: (100, 300, 120, 320),
        },
        GoldenTask {
            id: "GT-05",
            ref_expr: "dòng thứ ba trong danh sách (Mục 3 — Báo cáo)",
            box_: (100, 424, 500, 456),
        },
        GoldenTask {
            id: "GT-06",
            ref_expr: "dấu đóng × ở góc trên bên phải",
            box_: (1000, 8, 1016, 24),
        },
        GoldenTask {
            id: "GT-07",
            ref_expr: "nút Lưu bản sao",
            box_: (460, 60, 580, 100),
        },
        GoldenTask {
            id: "GT-08",
            ref_expr: "ô nhập văn bản bên cạnh chữ Tên:",
            box_: (160, 200, 500, 240),
        },
    ];

    fn box_center(box_: (i32, i32, i32, i32)) -> (i32, i32) {
        ((box_.0 + box_.2) / 2, (box_.1 + box_.3) / 2)
    }

    fn in_box(point: (i32, i32), box_: (i32, i32, i32, i32)) -> bool {
        point.0 >= box_.0 && point.0 <= box_.2 && point.1 >= box_.1 && point.1 <= box_.3
    }

    /// The deterministic stub: screenshot, then one click at the
    /// scripted point, then a text answer. It ignores the messages
    /// it is given — the script is the whole provider.
    struct StubProvider {
        replies: Mutex<VecDeque<ChatMessage>>,
    }

    impl StubProvider {
        fn for_task(point: (i32, i32)) -> StubProvider {
            let screenshot = ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_shot".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "body_screenshot".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
                usage: None,
            };
            let click = ChatMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_click".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "body_click".to_string(),
                        arguments: format!("{{\"x\": {}, \"y\": {}}}", point.0, point.1),
                    },
                }]),
                tool_call_id: None,
                usage: None,
            };
            let done = ChatMessage {
                role: "assistant".to_string(),
                content: Some("Clicked.".to_string()),
                tool_calls: None,
                tool_call_id: None,
                usage: None,
            };
            StubProvider {
                replies: Mutex::new(VecDeque::from([screenshot, click, done])),
            }
        }
    }

    impl Provider for StubProvider {
        fn name(&self) -> &str {
            "golden-stub"
        }

        fn model(&self) -> &str {
            "stub-scripted"
        }

        fn endpoint_url(&self) -> &str {
            "stub://golden"
        }

        fn is_paid(&self) -> bool {
            false
        }

        fn chat<'a>(
            &'a self,
            _ctx: &'a CallContext<'a>,
            _messages: &'a [ChatMessage],
            _tools: &'a [Value],
        ) -> mimint_core::BoxFuture<'a, Result<ChatMessage, MimintError>> {
            let message = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ChatMessage {
                    role: "assistant".to_string(),
                    content: Some("Done.".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    usage: None,
                });
            Box::pin(async move { Ok(message) })
        }
    }

    /// Auto-allow approver for the headless gate. The approval
    /// behavior itself is gated elsewhere (safety gate, UI tests).
    struct AutoAllow;

    impl Approver for AutoAllow {
        fn approve(&self, _tool: &str, _args: &Value) -> Result<bool, MimintError> {
            Ok(true)
        }

        fn label(&self) -> &'static str {
            "golden-gate auto-allow"
        }
    }

    struct HarnessGuard {
        child: Child,
    }

    impl Drop for HarnessGuard {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    struct GateArgs {
        harness: PathBuf,
        workdir: PathBuf,
        log_path: PathBuf,
        tasks: Vec<String>,
        ollama_task: Option<String>,
        base_url: String,
        model: String,
        ollama_timeout_secs: u64,
        font: Option<String>,
        font_size: Option<f32>,
    }

    fn parse_args() -> Result<GateArgs, String> {
        let args: Vec<String> = std::env::args().collect();
        let mut harness = String::new();
        let mut workdir = String::new();
        let mut log_path = String::new();
        let mut tasks = "GT-01,GT-02,GT-03,GT-04".to_string();
        let mut ollama_task: Option<String> = Some("GT-01".to_string());
        let mut base_url = "http://127.0.0.1:11434/v1".to_string();
        let mut model = "qwen2.5:3b".to_string();
        // Local CPU inference is slow (a trivial request measured
        // ~28s on vega); the default leaves room for a full turn.
        let mut ollama_timeout_secs: u64 = 600;
        let mut font = None;
        let mut font_size = None;
        let mut i = 1;
        while i < args.len() {
            let next = |i: &mut usize| -> String {
                *i += 1;
                args.get(*i).cloned().unwrap_or_default()
            };
            match args[i].as_str() {
                "--harness" => harness = next(&mut i),
                "--workdir" => workdir = next(&mut i),
                "--log" => log_path = next(&mut i),
                "--tasks" => tasks = next(&mut i),
                "--ollama-task" => {
                    let value = next(&mut i);
                    ollama_task = if value.is_empty() || value == "none" {
                        None
                    } else {
                        Some(value)
                    };
                }
                "--base-url" => base_url = next(&mut i),
                "--model" => model = next(&mut i),
                "--ollama-timeout-secs" => {
                    ollama_timeout_secs = next(&mut i).parse().unwrap_or(600);
                }
                "--font" => font = Some(next(&mut i)),
                "--font-size" => {
                    font_size = next(&mut i).parse().ok();
                }
                other => return Err(format!("unknown argument: {other}")),
            }
            i += 1;
        }
        if harness.is_empty() || workdir.is_empty() || log_path.is_empty() {
            return Err(
            "usage: golden --harness <evalharness-bin> --workdir <dir> --log <out.jsonl> [--tasks ...] [--ollama-task GT-01|none]".to_string(),
        );
        }
        Ok(GateArgs {
            harness: PathBuf::from(harness),
            workdir: PathBuf::from(workdir),
            log_path: PathBuf::from(log_path),
            tasks: tasks.split(',').map(|t| t.trim().to_string()).collect(),
            ollama_task,
            base_url,
            model,
            ollama_timeout_secs,
            font,
            font_size,
        })
    }

    fn read_canvas_line(log_path: &Path) -> (String, String, String) {
        // CANVAS canvas-v1 font=<path> size=<px>
        let mut version = "unknown".to_string();
        let mut font = "unknown".to_string();
        let mut size = "unknown".to_string();
        if let Ok(text) = std::fs::read_to_string(log_path) {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("CANVAS ") {
                    let mut parts = rest.split_whitespace();
                    version = parts.next().unwrap_or("unknown").to_string();
                    for part in parts {
                        if let Some(value) = part.strip_prefix("font=") {
                            font = value.to_string();
                        }
                        if let Some(value) = part.strip_prefix("size=") {
                            size = value.to_string();
                        }
                    }
                }
            }
        }
        (version, font, size)
    }

    pub async fn main_inner() {
        let args = match parse_args() {
            Ok(args) => args,
            Err(err) => {
                eprintln!("{err}");
                std::process::exit(2);
            }
        };
        if let Err(err) = run(args).await {
            eprintln!("golden gate failed: {err}");
            std::process::exit(2);
        }
    }
    async fn run(args: GateArgs) -> Result<(), String> {
        std::fs::create_dir_all(&args.workdir)
            .map_err(|err| format!("cannot create workdir: {err}"))?;
        let harness_log = args.workdir.join("harness.log");
        let control_path = args.workdir.join("control.txt");
        std::fs::write(&control_path, "0").map_err(|err| format!("cannot write control: {err}"))?;
        // The oracle requires the log file to exist; create it empty
        // before spawning so open() never races the harness startup.
        std::fs::File::create(&harness_log)
            .map_err(|err| format!("cannot create harness log: {err}"))?;
        let dump_path = args.workdir.join("canvas.ppm");

        // Start the canvas harness.
        let mut cmd = Command::new(&args.harness);
        cmd.arg("--log")
            .arg(&harness_log)
            .arg("--dump")
            .arg(&dump_path)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(font) = &args.font {
            cmd.arg("--font").arg(font);
        }
        if let Some(size) = args.font_size {
            cmd.arg("--font-size").arg(size.to_string());
        }
        let child = cmd
            .spawn()
            .map_err(|err| format!("cannot start harness: {err}"))?;
        let _guard = HarnessGuard { child };

        let mut oracle = FileOracle::open(&harness_log, &control_path)
            .map_err(|err| format!("cannot open oracle: {err}"))?;
        if !oracle.wait_ready(Duration::from_secs(10)) {
            return Err("harness did not report READY within 10s".to_string());
        }
        let (canvas_version, font, font_size) = read_canvas_line(&harness_log);

        // The body stack: X11 through the guards, behind the actor.
        let body = X11Body::connect().map_err(|err| format!("cannot connect X11 body: {err}"))?;
        let guarded = GuardedBody::new(body, RateLimits::default());
        let stop = StopFlag::new();
        let actor = BodyActor::spawn(Box::new(guarded), Arc::clone(&stop));
        let frames = Arc::new(FrameStore::new());

        let notebook = Arc::new(
            Notebook::open(args.workdir.join("golden.db"))
                .map_err(|err| format!("cannot open notebook: {err}"))?,
        );
        let audit = Arc::new(
            AuditLog::open(args.workdir.join("golden-audit.jsonl"))
                .map_err(|err| format!("cannot open audit: {err}"))?,
        );

        let mut log_file = std::fs::File::create(&args.log_path)
            .map_err(|err| format!("cannot create log: {err}"))?;

        let mut stub_results: Vec<(String, bool)> = Vec::new();

        // Stub tasks (`--tasks none` skips them: the stub gate
        // already passed, and the re-run is Ollama-only).
        for task_id in &args.tasks {
            if task_id == "none" {
                continue;
            }
            let Some(task) = TASKS.iter().find(|t| t.id == task_id) else {
                return Err(format!("unknown task: {task_id}"));
            };
            let point = box_center(task.box_);
            let provider = Arc::new(StubProvider::for_task(point));
            let record = run_one_task(
                task,
                "stub",
                provider,
                &actor,
                &frames,
                &notebook,
                Arc::clone(&audit),
                &mut oracle,
                &canvas_version,
                &font,
                &font_size,
                None,
            )
            .await;
            let passed = record["pass"].as_bool().unwrap_or(false);
            stub_results.push((task.id.to_string(), passed));
            writeln!(log_file, "{}", record).map_err(|err| format!("cannot write log: {err}"))?;
            println!(
                "{} {} predicted={:?} pass={passed}",
                task.id, task.ref_expr, record["predicted"]
            );
        }

        // One real-Ollama task: recorded, never gating.
        if let Some(ollama_task_id) = &args.ollama_task {
            let Some(task) = TASKS.iter().find(|t| t.id == ollama_task_id) else {
                return Err(format!("unknown ollama task: {ollama_task_id}"));
            };
            let http = GuardedHttpClient::new(
                EgressPolicy::privacy_off(),
                Duration::from_secs(args.ollama_timeout_secs),
            )
            .map_err(|err| format!("cannot build HTTP client: {err}"))?;
            let provider = Arc::new(
                ProviderClient::new(&args.base_url, &args.model, None, http)
                    .map_err(|err| format!("cannot build Ollama provider: {err}"))?,
            );
            let prompt = ollama_prompt(task);
            let mut record = run_one_task(
                task,
                &format!("ollama:{}", args.model),
                provider,
                &actor,
                &frames,
                &notebook,
                Arc::clone(&audit),
                &mut oracle,
                &canvas_version,
                &font,
                &font_size,
                Some(&prompt),
            )
            .await;
            record["http_timeout_secs"] = json!(args.ollama_timeout_secs);
            writeln!(log_file, "{}", record).map_err(|err| format!("cannot write log: {err}"))?;
            println!(
                "{} (ollama) {} predicted={:?} pass={} [recorded, not gating]",
                task.id, task.ref_expr, record["predicted"], record["pass"]
            );
        }

        let failed: Vec<&str> = stub_results
            .iter()
            .filter(|(_, passed)| !passed)
            .map(|(id, _)| id.as_str())
            .collect();
        if stub_results.is_empty() {
            println!("golden gate: no stub tasks run (ollama-only run)");
            Ok(())
        } else if failed.is_empty() {
            println!("golden gate: all {} stub tasks PASS", stub_results.len());
            Ok(())
        } else {
            Err(format!(
                "golden gate: stub tasks failed: {}",
                failed.join(", ")
            ))
        }
    }

    fn ollama_prompt(task: &GoldenTask) -> String {
        format!(
        "You control a computer screen of 1024x768 pixels. The screen shows a test interface with buttons labeled in Vietnamese (Mở, Lưu, Đóng, Lưu bản sao, Đăng nhập), a search magnifier icon, a text field labeled Tên:, a checkbox labeled Nhớ tôi, and a list of five rows. First call body_screenshot to see the screen state. Then click the requested element by calling body_click with your best-guess pixel coordinates (integers, absolute pixels, origin at the top-left). Requested element: {}. After the click, reply with a short text answer.",
        task.ref_expr
    )
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_one_task(
        task: &GoldenTask,
        provider_name: &str,
        provider: Arc<dyn Provider>,
        actor: &BodyActor,
        frames: &Arc<FrameStore>,
        notebook: &Arc<Notebook>,
        audit: Arc<AuditLog>,
        oracle: &mut FileOracle,
        canvas_version: &str,
        font: &str,
        font_size: &str,
        prompt_override: Option<&str>,
    ) -> Value {
        oracle.reset_events();
        let first_frame_before = frames.len();
        let mut registry = Registry::new();
        register_body_tools(
            &mut registry,
            actor.clone(),
            Arc::clone(frames),
            PrivacyFlag::new(false),
        );
        let approver = Arc::new(AutoAllow);
        let registry = Arc::new(registry);
        let mut brain = match Brain::new(
            provider,
            registry,
            Some(approver),
            Arc::clone(&audit),
            Arc::clone(notebook),
            format!("golden-{}", task.id),
            "agent-001",
            6,
        ) {
            Ok(brain) => brain,
            Err(err) => {
                return fail_record(
                    task,
                    provider_name,
                    canvas_version,
                    font,
                    font_size,
                    None,
                    0,
                    &format!("brain setup failed: {err}"),
                );
            }
        };
        brain.set_step_verifier(Arc::new(BodyStepVerifier::new(
            actor.clone(),
            Arc::clone(frames),
        )));
        let prompt = prompt_override
            .map(str::to_string)
            .unwrap_or_else(|| format!("Click the element: {}", task.ref_expr));
        let start = Instant::now();
        let turn = brain.run(&prompt).await;
        let latency_ms = start.elapsed().as_millis() as u64;
        let clicks = oracle.received_clicks();
        let predicted = clicks.last().copied();
        let pass = match (&turn, predicted) {
            (Ok(_), Some(point)) => in_box(point, task.box_),
            _ => false,
        };
        // The input image: the first frame stored during this task,
        // PNG-encoded and hashed. If the turn stored no frame, the
        // input hash is null and the task fails. Frame ids are
        // dense from 1 and nothing purges during the gate, so the
        // first new frame has id (count before + 1).
        let input_sha256 = if frames.len() > first_frame_before {
            let id = (first_frame_before + 1) as u64;
            frames.with_frame(id, |frame| {
                encode_png(frame).ok().map(|png| sha256_hex(&png))
            })
        } else {
            None
        }
        .flatten();
        let reason = match (&turn, predicted, &input_sha256) {
            (Err(err), _, _) => Some(format!("turn error: {err}")),
            (Ok(_), None, _) => Some("no click recorded by the oracle".to_string()),
            (Ok(_), Some(_), None) => Some("no input frame recorded".to_string()),
            (Ok(_), Some(point), Some(_)) if !in_box(point, task.box_) => {
                Some(format!("predicted {point:?} outside box {:?}", task.box_))
            }
            _ => None,
        };
        let mut record = json!({
            "task": task.id,
            "ref_expr": task.ref_expr,
            "box": [task.box_.0, task.box_.1, task.box_.2, task.box_.3],
            "predicted": predicted.map(|(x, y)| json!([x, y])),
            "clicks_recorded": clicks.len(),
            "pass": pass,
            "latency_ms": latency_ms,
            "input_sha256": input_sha256,
            "canvas_version": canvas_version,
            "font": font,
            "font_size": font_size,
            "provider": provider_name,
            "display_size": [1024, 768],
        });
        if let Some(reason) = reason {
            record["fail_reason"] = json!(reason);
        }
        if let Ok(outcome) = &turn {
            record["tools_used"] = json!(outcome.tools_used);
            record["steps"] = json!(outcome.steps);
        }
        record
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_record(
        task: &GoldenTask,
        provider_name: &str,
        canvas_version: &str,
        font: &str,
        font_size: &str,
        predicted: Option<(i32, i32)>,
        latency_ms: u64,
        reason: &str,
    ) -> Value {
        json!({
            "task": task.id,
            "ref_expr": task.ref_expr,
            "box": [task.box_.0, task.box_.1, task.box_.2, task.box_.3],
            "predicted": predicted.map(|(x, y)| json!([x, y])),
            "clicks_recorded": 0,
            "pass": false,
            "latency_ms": latency_ms,
            "input_sha256": Value::Null,
            "canvas_version": canvas_version,
            "font": font,
            "font_size": font_size,
            "provider": provider_name,
            "display_size": [1024, 768],
            "fail_reason": reason,
        })
    }
} // mod gate

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() {
    gate::main_inner().await;
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("golden gate is Linux-only (X11 canvas + X11 body)");
    std::process::exit(2);
}
