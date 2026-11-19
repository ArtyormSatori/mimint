//! `mimint-cli friends bench` — measure budgets B1–B8 for real
//! (§10.2). Everything runs in isolated temp dirs under
//! /tmp (never the user's data dir); the two bench bodies pair
//! with the auto UIs, so the ceremony times are the MACHINE
//! part only (no user SAS time). A number that was not
//! measured is printed as unmeasured, never filled in. Over
//! budget prints the real number and the command exits 1.

use crate::friends::{
    bench_context, ceremony_in_process, handshake_in_process, sync_both_in_process, FriendsArgs,
};
use mimint_core::friends::bench::{
    idle_cpu_percent, mean, p95, rss_mib, BudgetMeasurement, B1_HANDSHAKE_P95_MS, B2_CEREMONY_MS,
    B3_SYNC_100_MS, B4_ONE_RECORD_64K_MS, B5_ONE_MESSAGE_MS, B6_HUB_START_MS, B7_RSS_MIB,
    B8_IDLE_CPU_PCT,
};
use mimint_core::friends::crypto::{agree_and_derive, fresh_nonce16, generate_ephemeral};
use mimint_core::friends::hub::HubConfig;
use mimint_core::friends::pairing::SasFlow;
use mimint_core::friends::session::{SessionConfig, SessionCrypto};
use mimint_core::friends::sync::SyncKinds;
use mimint_core::friends::util::Clock;
use mimint_core::friends::wire::MessageKind;
use mimint_core::MimintError;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(0)
}

fn temp_bench_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mimint-p7-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn measure(
    id: &'static str,
    value: f64,
    unit: &'static str,
    budget: f64,
    method: String,
) -> BudgetMeasurement {
    BudgetMeasurement {
        id,
        value,
        unit,
        budget,
        passed: value < budget,
        method,
    }
}

fn print_results(results: &[BudgetMeasurement]) {
    let header = format!(
        "{:<4} {:>12} {:<6} {:>10}  {:<5}  {}",
        "id", "value", "unit", "budget", "pass", "method"
    );
    println!("{header}");
    for r in results {
        // F7-26: 3 decimals (B5 is sub-millisecond; "0.00ms"
        // hid the real value and looked like a resolution
        // limit — it is a mean over 1000 rounds, measured
        // with `Instant` at nanosecond resolution).
        println!(
            "{:<4} {:>12.3} {:<6} {:>10.2}  {:<5}  {}",
            r.id,
            r.value,
            r.unit,
            r.budget,
            if r.passed { "PASS" } else { "FAIL" },
            r.method
        );
    }
    if let Some(b5) = results.iter().find(|r| r.id == "B5") {
        println!(
            "B5 in nanoseconds: {:.0} ns per message (mean seal+open, 5-byte payload, in process)",
            b5.value * 1_000_000.0
        );
    }
    // F7-26: B7's scope, stated on the output itself.
    println!("B7 scope: RSS delta of THIS process only (context open + hub start).");
    match ollama_rss_mib() {
        Some(rss) => println!(
            "Ollama RSS (separate process, OUTSIDE the B7 budget): {rss:.1} MiB"
        ),
        None => println!("Ollama RSS: Ollama is not running right now (outside the B7 budget either way)."),
    }
}

/// RSS of the Ollama server process, if one is running
/// (F7-26: reported separately, never folded into B7).
fn ollama_rss_mib() -> Option<f64> {
    let mut total_kib: u64 = 0;
    let mut found = false;
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let cmdline = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
        let first = cmdline.split(|b| *b == 0).next().unwrap_or(&[]);
        if !first.ends_with(b"ollama") && first != b"ollama" {
            continue;
        }
        if let Ok(status) = std::fs::read_to_string(entry.path().join("status")) {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    if let Some(num) = rest.split_whitespace().next() {
                        if let Ok(kib) = num.parse::<u64>() {
                            total_kib += kib;
                            found = true;
                        }
                    }
                }
            }
        }
    }
    if found {
        Some(total_kib as f64 / 1024.0)
    } else {
        None
    }
}

pub fn cmd_bench(_args: &FriendsArgs, rest: &[String]) -> Result<(), MimintError> {
    let quick = rest.iter().any(|a| a == "--quick");
    let mut results: Vec<BudgetMeasurement> = Vec::new();

    let dir_a = temp_bench_dir("a");
    let dir_b = temp_bench_dir("b");
    let ctx_a = bench_context(&dir_a)?;
    let ctx_b = bench_context(&dir_b)?;
    let clock = Clock::system();

    // B2 first (it pairs the bodies; B1 needs the pairing).
    let start = Instant::now();
    ceremony_in_process(&ctx_a, &ctx_b, SasFlow::Compare)?;
    let compare_ms = start.elapsed().as_secs_f64() * 1000.0;
    let start = Instant::now();
    ceremony_in_process(&ctx_a, &ctx_b, SasFlow::Typing)?;
    let typing_ms = start.elapsed().as_secs_f64() * 1000.0;
    let b2 = compare_ms.max(typing_ms);
    results.push(measure(
        "B2",
        b2,
        "ms",
        B2_CEREMONY_MS,
        format!("ceremony machine part, auto UIs: compare={compare_ms:.1}ms typing={typing_ms:.1}ms (slower reported)"),
    ));

    // B1: 20 handshakes over loopback, p95.
    let mut samples = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        handshake_in_process(&ctx_a, &ctx_b)?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    results.push(measure(
        "B1",
        p95(samples),
        "ms",
        B1_HANDSHAKE_P95_MS,
        "handshake p95 over 20 loopback runs, paired bodies".to_string(),
    ));

    // B3: 100 shared notes, one two-way sync.
    for i in 0..100 {
        ctx_a
            .notebook
            .add_for("agent-001", &format!("bench note {i}"), "bench body", true)?;
    }
    let start = Instant::now();
    let (a_counts, _b_counts) = sync_both_in_process(&ctx_a, &ctx_b, &SyncKinds::all())?;
    let b3 = start.elapsed().as_secs_f64() * 1000.0;
    results.push(measure(
        "B3",
        b3,
        "ms",
        B3_SYNC_100_MS,
        format!("two-way sync of 100 shared notes (A sent {})", a_counts.0),
    ));

    // B4: one 64 KiB record, one two-way sync.
    let big = "x".repeat(64 * 1024);
    ctx_a
        .notebook
        .add_for("agent-001", "bench big note", &big, true)?;
    let start = Instant::now();
    sync_both_in_process(&ctx_a, &ctx_b, &SyncKinds::all())?;
    results.push(measure(
        "B4",
        start.elapsed().as_secs_f64() * 1000.0,
        "ms",
        B4_ONE_RECORD_64K_MS,
        "two-way sync of one 64 KiB shared note".to_string(),
    ));

    // B5: 1000 seal+open round-trips in process.
    let (priv_a, _pub_a) = generate_ephemeral();
    let (_priv_b, pub_b) = generate_ephemeral();
    let na = fresh_nonce16();
    let nb = fresh_nonce16();
    let (key_ab, key_ba) = agree_and_derive(priv_a, &pub_b, &na, &nb, None)?;
    // Two session ends: A seals, B opens (sealing then opening
    // on ONE end would use the wrong direction key).
    let mut crypto_a = SessionCrypto::new(
        "device-a",
        "device-b",
        key_ab.clone(),
        key_ba.clone(),
        clock.clone(),
        SessionConfig::default(),
    );
    let mut crypto_b = SessionCrypto::new(
        "device-b",
        "device-a",
        key_ab,
        key_ba,
        clock,
        SessionConfig::default(),
    );
    let mut samples = Vec::new();
    for _ in 0..1000 {
        let start = Instant::now();
        let sealed = crypto_a
            .seal(MessageKind::Ping, b"bench")
            .map_err(|err| MimintError::Config(format!("seal failed: {err:?}")))?;
        let _ = crypto_b
            .open(&sealed.header, &sealed.ciphertext)
            .map_err(|err| MimintError::Config(format!("open failed: {err:?}")))?;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    results.push(measure(
        "B5",
        mean(&samples),
        "ms",
        B5_ONE_MESSAGE_MS,
        "mean seal+open per message, 1000 messages, 5-byte payload, in process (Instant, ns resolution)".to_string(),
    ));

    // B6 + B7: hub start time and RSS delta of starting a hub.
    // Free loopback ports so the bench never collides with a
    // hub the user already runs on the design ports.
    let dir_hub = temp_bench_dir("hub");
    let mut hub_config = HubConfig::default();
    let fp = free_port();
    let xp = free_port();
    let op = free_port();
    if fp != 0 && xp != 0 && op != 0 {
        hub_config.friends_port = fp;
        hub_config.xiaozhi_port = xp;
        hub_config.ota_port = op;
    }
    let rss_before = rss_mib()?;
    let start = Instant::now();
    let ctx_hub = mimint_core::friends::context::FriendsContext::open(
        &dir_hub,
        dir_hub.join("config.toml"),
        dir_hub.join("notebook.db"),
        hub_config,
        std::sync::Arc::new(crate::friends::NullDriverBackend),
        None,
        Clock::system(),
    )?;
    ctx_hub.hub.start()?;
    let b6_ms = start.elapsed().as_secs_f64() * 1000.0;
    let rss_after = rss_mib()?;
    results.push(measure(
        "B6",
        b6_ms,
        "ms",
        B6_HUB_START_MS,
        "fresh context open + hub start (free loopback ports)".to_string(),
    ));
    results.push(measure(
        "B7",
        (rss_after - rss_before).max(0.0),
        "MiB",
        B7_RSS_MIB,
        "RSS delta of opening a context + starting a hub, this process only (Ollama is a separate process, outside this budget)".to_string(),
    ));

    // B8: idle CPU with the hub running.
    let window = if quick {
        Duration::from_secs(10)
    } else {
        Duration::from_secs(60)
    };
    println!(
        "B8: measuring idle CPU over {}s (hub running)...",
        window.as_secs()
    );
    let idle = idle_cpu_percent(window)?;
    results.push(measure(
        "B8",
        idle,
        "%",
        B8_IDLE_CPU_PCT,
        format!(
            "idle CPU over {}s with a hub running{}",
            window.as_secs(),
            if quick {
                " (quick: 10s window, not the 60s budget window)"
            } else {
                ""
            }
        ),
    ));

    ctx_hub.hub.stop();
    print_results(&results);
    let _ = std::fs::remove_dir_all(&dir_a);
    let _ = std::fs::remove_dir_all(&dir_b);
    let _ = std::fs::remove_dir_all(&dir_hub);
    if results.iter().any(|r| !r.passed) {
        return Err(MimintError::Config(
            "one or more budgets over limit (real numbers above)".to_string(),
        ));
    }
    Ok(())
}
