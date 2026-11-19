//! Golden-task gate (P4.1): GT-01..GT-04 with a deterministic
//! stub provider + one task with real Ollama, through the full
//! pipeline (provider → brain → guards → body tools → actor →
//! X11 body → canvas oracle → binary point-in-box checker).
//!
//! ```text
//! golden --harness <evalharness-bin> --workdir <dir> --log <out.jsonl>
//!        [--tasks GT-01,GT-02,GT-03,GT-04] [--ollama-task GT-01]
//!        [--ollama-tasks GT-01,GT-02,GT-03,GT-04,GT-05,GT-06,GT-07,GT-08]
//!        [--drill] [--report <report.json>]
//!        [--base-url http://127.0.0.1:11434/v1] [--model qwen2.5:3b]
//!        [--font <ttf>] [--font-size 16]
//! ```
//!
//! Drill mode (`--drill`, P8.2): runs ALL 8 golden tasks through
//! real Ollama (unless `--ollama-tasks` names a subset), prints a
//! per-task table, writes a JSON report with 4 separate sections
//! (harness OK / tasks PASS-n-of-8 / latency under stated
//! conditions / reproducibility via input SHA-256), and exits 0
//! iff 8/8 Ollama tasks PASS. The drill uses the DEV set only
//! (GT-01..GT-08); the held-out VALIDATION set stays sealed
//! (phase5/HELDOUT-SEAL.md) and is never opened here.
//! `--report` defaults to `<workdir>/drill-report.json` in drill
//! mode. `--ollama-tasks` (list) wins over legacy `--ollama-task`
//! (single) when both are given.
//!
//! The stub provider plays planner+grounder with FIXED
//! coordinates from the task script (the box centers): it
//! proves the pipeline delivers a click to the oracle exactly
//! where the provider said. The Ollama task proves the same
//! pipeline with a real model — and records the TRUE result
//! even when the model misses: model quality is not a
//! pipeline bug, and the two are distinguished in the log.
//! Outside drill mode the Ollama task(s) do NOT gate the
//! exit code; the stub tasks do. Exit 0 iff every stub task
//! passes. In drill mode the Ollama tasks DO gate: exit 0 iff
//! every Ollama task passes (8/8 by default).
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
            ref_expr: "hàng Mục 3 — Báo cáo trong danh sách",
            box_: (100, 424, 500, 456),
        },
        GoldenTask {
            id: "GT-06",
            ref_expr: "dấu X đóng ở góc trên bên phải màn hình",
            box_: (1000, 8, 1016, 24),
        },
        GoldenTask {
            id: "GT-07",
            ref_expr: "nút Lưu bản sao",
            box_: (460, 60, 580, 100),
        },
        GoldenTask {
            id: "GT-08",
            ref_expr: "ô nhập liệu bên cạnh chữ Tên",
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
        tasks_explicit: bool,
        ollama_tasks: Vec<String>,
        drill: bool,
        report: Option<PathBuf>,
        base_url: String,
        model: String,
        ollama_timeout_secs: u64,
        font: Option<String>,
        font_size: Option<f32>,
    }

    const USAGE: &str = "usage: golden --harness <evalharness-bin> --workdir <dir> --log <out.jsonl> [--tasks GT-01,GT-02,GT-03,GT-04|none] [--ollama-task GT-01|none] [--ollama-tasks GT-01,...,GT-08] [--drill] [--report <report.json>] [--base-url URL] [--model NAME] [--ollama-timeout-secs N] [--font <ttf>] [--font-size N]\n       --drill runs all 8 tasks through real Ollama, writes a 4-section JSON report, exits 0 iff 8/8 PASS.\n       --ollama-tasks (list) wins over legacy --ollama-task (single) when both are given.";

    fn parse_args() -> Result<GateArgs, String> {
        let args: Vec<String> = std::env::args().collect();
        parse_args_from(&args)
    }

    fn parse_args_from(args: &[String]) -> Result<GateArgs, String> {
        let mut harness = String::new();
        let mut workdir = String::new();
        let mut log_path = String::new();
        let mut tasks = "GT-01,GT-02,GT-03,GT-04".to_string();
        let mut tasks_explicit = false;
        // Legacy single Ollama task; the list flag wins when given.
        let mut ollama_task: Option<String> = Some("GT-01".to_string());
        let mut ollama_tasks: Vec<String> = Vec::new();
        let mut ollama_tasks_explicit = false;
        let mut drill = false;
        let mut report: Option<PathBuf> = None;
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
                "--help" | "-h" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                "--harness" => harness = next(&mut i),
                "--workdir" => workdir = next(&mut i),
                "--log" => log_path = next(&mut i),
                "--tasks" => {
                    tasks = next(&mut i);
                    tasks_explicit = true;
                }
                "--ollama-task" => {
                    let value = next(&mut i);
                    ollama_task = if value.is_empty() || value == "none" {
                        None
                    } else {
                        Some(value)
                    };
                }
                "--ollama-tasks" => {
                    let value = next(&mut i);
                    ollama_tasks = if value.is_empty() || value == "none" {
                        Vec::new()
                    } else {
                        value.split(',').map(|t| t.trim().to_string()).collect()
                    };
                    ollama_tasks_explicit = true;
                }
                "--drill" => drill = true,
                "--report" => report = Some(PathBuf::from(next(&mut i))),
                "--base-url" => base_url = next(&mut i),
                "--model" => model = next(&mut i),
                "--ollama-timeout-secs" => {
                    ollama_timeout_secs = next(&mut i).parse().unwrap_or(600);
                }
                "--font" => font = Some(next(&mut i)),
                "--font-size" => {
                    font_size = next(&mut i).parse().ok();
                }
                other => return Err(format!("unknown argument: {other}\n{USAGE}")),
            }
            i += 1;
        }
        if harness.is_empty() || workdir.is_empty() || log_path.is_empty() {
            return Err(USAGE.to_string());
        }
        // Resolve the Ollama task list: explicit list wins over the
        // legacy single flag; otherwise the single flag (default
        // GT-01) applies. In drill mode the default is all 8.
        let resolved_ollama: Vec<String> = if ollama_tasks_explicit {
            ollama_tasks
        } else if let Some(single) = ollama_task {
            vec![single]
        } else {
            Vec::new()
        };
        // Drill with no explicit Ollama selection means all 8,
        // not the legacy single-task default. An explicit
        // --ollama-tasks list (even a subset) is honored as-is.
        let resolved_ollama = if drill && !ollama_tasks_explicit {
            TASKS.iter().map(|t| t.id.to_string()).collect()
        } else {
            resolved_ollama
        };
        if drill && resolved_ollama.is_empty() {
            return Err(
                "--drill needs at least one Ollama task (remove --ollama-tasks none)".to_string(),
            );
        }
        let report = report.or_else(|| {
            if drill {
                Some(PathBuf::from(&workdir).join("drill-report.json"))
            } else {
                None
            }
        });
        Ok(GateArgs {
            harness: PathBuf::from(harness),
            workdir: PathBuf::from(workdir),
            log_path: PathBuf::from(log_path),
            tasks: tasks.split(',').map(|t| t.trim().to_string()).collect(),
            tasks_explicit,
            ollama_tasks: resolved_ollama,
            drill,
            report,
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
        let mut ollama_results: Vec<(String, bool)> = Vec::new();
        let mut records: Vec<Value> = Vec::new();

        // Shared Ollama client (built once, reused by every task)
        // + preflight: the server must answer AND carry the model
        // before any measurement starts. A dead server or a missing
        // model is BLOCKED, not a silent stub substitution.
        let mut ollama_provider: Option<Arc<ProviderClient>> = None;
        let mut preflight: Option<OllamaPreflight> = None;
        if !args.ollama_tasks.is_empty() {
            let http = GuardedHttpClient::new(
                EgressPolicy::privacy_off(),
                Duration::from_secs(args.ollama_timeout_secs),
            )
            .map_err(|err| format!("cannot build HTTP client: {err}"))?;
            let pf = ollama_preflight(&http, &args.base_url, &args.model).await;
            if !pf.responded {
                return Err(format!(
                    "Ollama không trả lời tại {} — BỊ CHẶN: kiểm tra Ollama có chạy không; không đo bằng stub rồi báo như thật",
                    ollama_root(&args.base_url)
                ));
            }
            if !pf.model_present {
                return Err(format!(
                    "Ollama không có model {} (có: {}) — BỊ CHẶN: ollama pull {} trước",
                    args.model,
                    pf.models.join(", "),
                    args.model
                ));
            }
            preflight = Some(pf);
            ollama_provider = Some(Arc::new(
                ProviderClient::new(&args.base_url, &args.model, None, http)
                    .map_err(|err| format!("cannot build Ollama provider: {err}"))?,
            ));
        }

        // Stub tasks. In drill mode the drill IS the Ollama run:
        // stubs are skipped unless --tasks was given explicitly.
        // (`--tasks none` skips them: the stub gate already passed,
        // and the re-run is Ollama-only.)
        if !args.drill || args.tasks_explicit {
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
                writeln!(log_file, "{}", record)
                    .map_err(|err| format!("cannot write log: {err}"))?;
                records.push(record.clone());
                println!(
                    "{} {} predicted={:?} pass={passed}",
                    task.id, task.ref_expr, record["predicted"]
                );
            }
        }

        // Real-Ollama tasks: every one recorded, with its TRUE
        // result even when the model misses (model quality is not
        // a pipeline bug). Outside drill mode they do not gate;
        // in drill mode they do (exit 0 iff all PASS).
        if let Some(provider) = &ollama_provider {
            for task_id in &args.ollama_tasks {
                let Some(task) = TASKS.iter().find(|t| t.id == task_id) else {
                    return Err(format!("unknown ollama task: {task_id}"));
                };
                let prompt = ollama_prompt(task);
                let mut record = run_one_task(
                    task,
                    &format!("ollama:{}", args.model),
                    Arc::clone(provider) as Arc<dyn Provider>,
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
                let passed = record["pass"].as_bool().unwrap_or(false);
                ollama_results.push((task.id.to_string(), passed));
                writeln!(log_file, "{}", record)
                    .map_err(|err| format!("cannot write log: {err}"))?;
                records.push(record.clone());
                println!(
                    "{} (ollama) {} predicted={:?} pass={} [recorded{}]",
                    task.id,
                    task.ref_expr,
                    record["predicted"],
                    record["pass"],
                    if args.drill {
                        ", gating"
                    } else {
                        ", not gating"
                    }
                );
            }
        }

        // JSON report with the 4 separate sections (EVAL-PLAN §2):
        // 1. harness OK / not, 2. tasks PASS-n-of-8, 3. latency
        // under stated conditions, 4. reproducibility via input
        // SHA-256. Never merged into a single number.
        if let Some(report_path) = &args.report {
            let mode = if args.drill { "drill" } else { "gate" };
            let report = build_report(
                mode,
                &canvas_version,
                &font,
                &font_size,
                &preflight,
                &args.model,
                &args.base_url,
                &records,
            );
            let text = serde_json::to_string_pretty(&report)
                .map_err(|err| format!("cannot serialize report: {err}"))?;
            std::fs::write(report_path, format!("{text}\n"))
                .map_err(|err| format!("cannot write report: {err}"))?;
            println!("report: {}", report_path.display());
        }

        if args.drill {
            print_drill_table(&records);
            let total = ollama_results.len();
            let passed = ollama_results.iter().filter(|(_, p)| *p).count();
            let stub_failed: Vec<&str> = stub_results
                .iter()
                .filter(|(_, passed)| !passed)
                .map(|(id, _)| id.as_str())
                .collect();
            let ollama_failed: Vec<&str> = ollama_results
                .iter()
                .filter(|(_, passed)| !passed)
                .map(|(id, _)| id.as_str())
                .collect();
            if ollama_failed.is_empty() && stub_failed.is_empty() {
                println!("drill: {passed}/{total} Ollama tasks PASS");
                Ok(())
            } else {
                Err(format!(
                    "drill: Ollama tasks failed: {}; stub tasks failed: {}",
                    ollama_failed.join(", "),
                    stub_failed.join(", ")
                ))
            }
        } else {
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
    }

    /// Ollama server root for the /api/* endpoints: the base URL
    /// without its trailing `/v1` (Ollama convention).
    fn ollama_root(base_url: &str) -> String {
        let trimmed = base_url.trim_end_matches('/');
        trimmed
            .strip_suffix("/v1")
            .unwrap_or(trimmed)
            .trim_end_matches('/')
            .to_string()
    }

    struct OllamaPreflight {
        responded: bool,
        model_present: bool,
        models: Vec<String>,
        warmup_ok: bool,
        warmup_ms: u64,
        error: Option<String>,
    }

    /// Preflight before any measurement: does the Ollama server
    /// answer, does it carry the model, and can it generate one
    /// token (warm-up, so the first measured task does not absorb
    /// model-load time)? Uses the in-tree guarded HTTP client;
    /// a dead server is BLOCKED, never silently stubbed.
    async fn ollama_preflight(
        http: &GuardedHttpClient,
        base_url: &str,
        model: &str,
    ) -> OllamaPreflight {
        let root = ollama_root(base_url);
        let mut pf = OllamaPreflight {
            responded: false,
            model_present: false,
            models: Vec::new(),
            warmup_ok: false,
            warmup_ms: 0,
            error: None,
        };
        let resp = match http.get(&format!("{root}/api/tags")).await {
            Ok(resp) => resp,
            Err(err) => {
                pf.error = Some(format!("{err}"));
                return pf;
            }
        };
        if !resp.status().is_success() {
            pf.error = Some(format!("HTTP {}", resp.status()));
            return pf;
        }
        let body = match resp.text().await {
            Ok(text) => text,
            Err(err) => {
                pf.error = Some(format!("{err}"));
                return pf;
            }
        };
        let parsed: Value = match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(err) => {
                pf.error = Some(format!("{err}"));
                return pf;
            }
        };
        pf.responded = true;
        if let Some(models) = parsed.get("models").and_then(|m| m.as_array()) {
            for m in models {
                if let Some(name) = m.get("name").and_then(|n| n.as_str()) {
                    pf.models.push(name.to_string());
                }
            }
        }
        pf.model_present = pf.models.iter().any(|n| n == model);
        let start = Instant::now();
        let warm = http
            .post_json(
                &format!("{root}/api/generate"),
                &json!({
                    "model": model,
                    "prompt": "hi",
                    "stream": false,
                    "options": {"num_predict": 1}
                }),
                &[],
            )
            .await;
        pf.warmup_ms = start.elapsed().as_millis() as u64;
        pf.warmup_ok = matches!(&warm, Ok(resp) if resp.status().is_success());
        if !pf.warmup_ok {
            pf.error = Some("warm-up generate failed".to_string());
        }
        pf
    }

    /// UTC timestamp (ISO-8601) without a date dependency.
    fn utc_iso_now() -> String {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let days = (secs / 86_400) as i64;
        let secs_of_day = secs % 86_400;
        let (year, month, day) = civil_from_days(days);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
            secs_of_day / 3600,
            (secs_of_day % 3600) / 60,
            secs_of_day % 60
        )
    }

    /// Days since 1970-01-01 -> (year, month, day); Howard
    /// Hinnant's civil-from-days algorithm.
    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    fn read_first_line(path: &str) -> String {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|line| !line.trim().is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn machine_info() -> Value {
        let hostname = read_first_line("/proc/sys/kernel/hostname");
        let cpu = std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find(|line| line.starts_with("model name"))
                    .and_then(|line| line.split_once(':').map(|x| x.1))
                    .map(|name| name.trim().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string());
        json!({"hostname": hostname, "cpu": cpu})
    }

    /// The drill report: exactly the 4 sections of EVAL-PLAN §2,
    /// kept separate — never merged into a single number.
    #[allow(clippy::too_many_arguments)]
    fn build_report(
        mode: &str,
        canvas_version: &str,
        font: &str,
        font_size: &str,
        preflight: &Option<OllamaPreflight>,
        model: &str,
        base_url: &str,
        records: &[Value],
    ) -> Value {
        let ollama_records: Vec<&Value> = records
            .iter()
            .filter(|r| {
                r.get("provider")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| p.starts_with("ollama:"))
            })
            .collect();
        let ollama_pass = ollama_records
            .iter()
            .filter(|r| r.get("pass").and_then(|p| p.as_bool()).unwrap_or(false))
            .count();
        let mut per_task_ms = serde_json::Map::new();
        let mut input_sha = serde_json::Map::new();
        for record in &ollama_records {
            let id = record
                .get("task")
                .and_then(|t| t.as_str())
                .unwrap_or("?")
                .to_string();
            per_task_ms.insert(
                id.clone(),
                record.get("latency_ms").cloned().unwrap_or(Value::Null),
            );
            input_sha.insert(
                id,
                record.get("input_sha256").cloned().unwrap_or(Value::Null),
            );
        }
        let sha_values: Vec<&Value> = input_sha.values().collect();
        let all_equal = !sha_values.is_empty()
            && sha_values.iter().all(|v| *v == sha_values[0])
            && !sha_values[0].is_null();
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| "unset".to_string());
        let harness_ok =
            canvas_version != "unknown" && preflight.as_ref().is_none_or(|pf| pf.responded);
        let (preflight_json, warmup_json) = match preflight {
            Some(pf) => (
                json!({
                    "responded": pf.responded,
                    "model": model,
                    "base_url": base_url,
                    "model_present": pf.model_present,
                    "models": pf.models,
                    "error": pf.error,
                }),
                json!({"ok": pf.warmup_ok, "ms": pf.warmup_ms}),
            ),
            None => (
                json!({"responded": false, "model": model, "base_url": base_url, "model_present": false, "models": [], "error": "no Ollama tasks in this run"}),
                json!({"ok": false, "ms": 0}),
            ),
        };
        json!({
            "schema_version": 1,
            "mode": mode,
            "timestamp_unix": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            "timestamp_utc": utc_iso_now(),
            "set": "DEV (GT-01..GT-08)",
            "validation_set": "held-out sealed (phase5/HELDOUT-SEAL.md) — NOT used in this run",
            "harness": {
                "ok": harness_ok,
                "canvas_version": canvas_version,
                "font": font,
                "font_size": font_size,
                "display": display,
                "wm": "none (override-redirect window)",
                "ollama": preflight_json,
                "warmup": warmup_json,
            },
            "tasks": {
                "pass": ollama_pass,
                "total": ollama_records.len(),
                "results": records,
            },
            "latency": {
                "model": model,
                "machine": machine_info(),
                "canvas_version": canvas_version,
                "font": font,
                "font_size": font_size,
                "per_task_ms": per_task_ms,
            },
            "reproducibility": {
                "input_sha256": input_sha,
                "all_equal_within_run": all_equal,
                "note": "Same canvas+font must give the same input SHA-256. Compare across runs by diffing report files; a different SHA means a different canvas/font/geometry — stop and find the cause before trusting the score.",
            },
        })
    }

    fn print_drill_table(records: &[Value]) {
        println!("{:-<100}", "");
        println!(
            "{:<8} {:<8} {:<18} {:>10}  fail_reason",
            "task", "pass", "predicted", "latency_ms"
        );
        println!("{:-<100}", "");
        for record in records {
            if !record
                .get("provider")
                .and_then(|p| p.as_str())
                .is_some_and(|p| p.starts_with("ollama:"))
            {
                continue;
            }
            let id = record.get("task").and_then(|t| t.as_str()).unwrap_or("?");
            let pass = record
                .get("pass")
                .and_then(|p| p.as_bool())
                .unwrap_or(false);
            let predicted = record
                .get("predicted")
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            let latency = record
                .get("latency_ms")
                .and_then(|l| l.as_u64())
                .unwrap_or(0);
            let reason = record
                .get("fail_reason")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            println!("{id:<8} {pass:<8} {predicted:<18} {latency:>10}  {reason}");
        }
        println!("{:-<100}", "");
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
    #[cfg(test)]
    mod tests {
        use super::*;

        fn args_of(flags: &[&str]) -> Vec<String> {
            let mut v = vec!["golden".to_string()];
            v.extend(flags.iter().map(|f| f.to_string()));
            v
        }

        fn base_flags() -> Vec<&'static str> {
            vec![
                "--harness",
                "/bin/evalharness",
                "--workdir",
                "/tmp/wd",
                "--log",
                "/tmp/wd/out.jsonl",
            ]
        }

        #[test]
        fn parse_defaults_to_single_ollama_task() {
            let args = parse_args_from(&args_of(&base_flags())).unwrap();
            assert_eq!(args.ollama_tasks, vec!["GT-01".to_string()]);
            assert!(!args.drill);
            assert!(args.report.is_none());
            assert_eq!(args.tasks, vec!["GT-01", "GT-02", "GT-03", "GT-04"]);
        }

        #[test]
        fn parse_ollama_tasks_list_wins_over_single() {
            let mut flags = base_flags();
            flags.extend(["--ollama-tasks", "GT-02,GT-05", "--ollama-task", "GT-01"]);
            let args = parse_args_from(&args_of(&flags)).unwrap();
            assert_eq!(
                args.ollama_tasks,
                vec!["GT-02".to_string(), "GT-05".to_string()]
            );
        }

        #[test]
        fn parse_drill_defaults_to_all_eight() {
            let mut flags = base_flags();
            flags.push("--drill");
            let args = parse_args_from(&args_of(&flags)).unwrap();
            assert!(args.drill);
            assert_eq!(args.ollama_tasks.len(), 8);
            assert_eq!(args.ollama_tasks[0], "GT-01");
            assert_eq!(args.ollama_tasks[7], "GT-08");
            assert_eq!(
                args.report,
                Some(PathBuf::from("/tmp/wd/drill-report.json"))
            );
        }

        #[test]
        fn parse_drill_honors_explicit_subset() {
            let mut flags = base_flags();
            flags.extend(["--drill", "--ollama-tasks", "GT-01,GT-03"]);
            let args = parse_args_from(&args_of(&flags)).unwrap();
            assert_eq!(
                args.ollama_tasks,
                vec!["GT-01".to_string(), "GT-03".to_string()]
            );
        }

        #[test]
        fn parse_drill_with_empty_ollama_list_is_an_error() {
            let mut flags = base_flags();
            flags.extend(["--drill", "--ollama-tasks", "none"]);
            assert!(parse_args_from(&args_of(&flags)).is_err());
        }

        #[test]
        fn parse_tasks_none_and_unknown_arg() {
            let mut flags = base_flags();
            flags.extend(["--tasks", "none", "--ollama-task", "none"]);
            let args = parse_args_from(&args_of(&flags)).unwrap();
            assert_eq!(args.tasks, vec!["none".to_string()]);
            assert!(args.ollama_tasks.is_empty());
            let mut bad = base_flags();
            bad.push("--nope");
            assert!(parse_args_from(&args_of(&bad)).is_err());
            // Missing a required argument errors.
            assert!(parse_args_from(&args_of(&["--harness", "/bin/x"])).is_err());
        }

        #[test]
        fn ollama_root_strips_v1_and_slash() {
            assert_eq!(
                ollama_root("http://127.0.0.1:11434/v1"),
                "http://127.0.0.1:11434"
            );
            assert_eq!(
                ollama_root("http://127.0.0.1:11434/v1/"),
                "http://127.0.0.1:11434"
            );
            assert_eq!(
                ollama_root("http://127.0.0.1:11434"),
                "http://127.0.0.1:11434"
            );
        }

        #[test]
        fn civil_from_days_known_dates() {
            assert_eq!(civil_from_days(0), (1970, 1, 1));
            assert_eq!(civil_from_days(19_723), (2024, 1, 1));
            assert_eq!(civil_from_days(20_776), (2026, 11, 19));
        }

        #[test]
        fn utc_iso_now_shape() {
            let now = utc_iso_now();
            assert!(now.starts_with("20"), "{now}");
            assert!(now.ends_with('Z'), "{now}");
            assert_eq!(now.len(), 20, "{now}");
        }

        fn sample_record(task: &str, pass: bool, latency: u64, sha: &str) -> Value {
            json!({
                "task": task,
                "ref_expr": "x",
                "box": [0, 0, 10, 10],
                "predicted": [5, 5],
                "pass": pass,
                "latency_ms": latency,
                "input_sha256": sha,
                "canvas_version": "canvas-v1",
                "font": "DejaVuSans.ttf",
                "font_size": "16",
                "provider": "ollama:qwen2.5:3b",
            })
        }

        #[test]
        fn report_has_four_sections_and_counts_ollama_only() {
            let records = vec![
                sample_record("GT-01", true, 100, "aaa"),
                sample_record("GT-02", false, 200, "aaa"),
                json!({
                    "task": "GT-01", "pass": true, "latency_ms": 1,
                    "input_sha256": "aaa", "provider": "stub",
                }),
            ];
            let report = build_report(
                "drill",
                "canvas-v1",
                "DejaVuSans.ttf",
                "16",
                &None,
                "qwen2.5:3b",
                "http://127.0.0.1:11434/v1",
                &records,
            );
            // Exactly the 4 sections of EVAL-PLAN §2, plus metadata.
            for section in ["harness", "tasks", "latency", "reproducibility"] {
                assert!(report.get(section).is_some(), "missing {section}");
            }
            assert_eq!(report["tasks"]["pass"], 1);
            assert_eq!(report["tasks"]["total"], 2);
            assert_eq!(report["latency"]["per_task_ms"]["GT-01"], 100);
            assert_eq!(
                report["reproducibility"]["all_equal_within_run"], true,
                "same SHA across tasks"
            );
            assert_eq!(report["harness"]["ok"], true);
        }

        #[test]
        fn report_reproducibility_false_when_sha_differs() {
            let records = vec![
                sample_record("GT-01", true, 100, "aaa"),
                sample_record("GT-02", true, 200, "bbb"),
            ];
            let report = build_report(
                "drill",
                "canvas-v1",
                "DejaVuSans.ttf",
                "16",
                &None,
                "qwen2.5:3b",
                "http://127.0.0.1:11434/v1",
                &records,
            );
            assert_eq!(report["reproducibility"]["all_equal_within_run"], false);
            assert_eq!(report["harness"]["ok"], true);
        }

        #[test]
        fn report_harness_not_ok_when_canvas_unknown() {
            let report = build_report(
                "gate",
                "unknown",
                "unknown",
                "unknown",
                &None,
                "qwen2.5:3b",
                "http://127.0.0.1:11434/v1",
                &[],
            );
            assert_eq!(report["harness"]["ok"], false);
            assert_eq!(report["tasks"]["total"], 0);
        }
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
