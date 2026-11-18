//! BM25 bench (F5-16): measure `Notebook::search_ranked` on a
//! locally generated 5000-note notebook, and report median +
//! p95 milliseconds per query. The notebook is deterministic
//! (LCG, fixed seed), so re-runs measure the same data.
//!
//! Decision rule (from the Phase 5 fix-pass synthesis): median
//! above 50 ms/query means the full-scan search needs an
//! inverted index (or a cap); at or below it, the scan stands
//! and the measured numbers close the item. Run with
//! `cargo run -p mimint-core --example bm25-bench` (debug
//! numbers are conservative: a release build is only faster).
//!
//! The bench reads no user data and writes only to a temporary
//! directory, which it removes when done.

use mimint_core::Notebook;
use std::time::{Duration, Instant};

const NOTE_COUNT: usize = 5000;
const QUERY_COUNT: usize = 20;

/// Deterministic LCG (Numerical Recipes constants).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0 >> 33
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Vietnamese-ish word pool: common words (appear in many
/// notes) plus rare words (appear in few), so the bench
/// exercises both the common-term and rare-term paths.
const COMMON: [&str; 24] = [
    "mua",
    "sữa",
    "sáng",
    "làm",
    "việc",
    "nhà",
    "đi",
    "về",
    "ăn",
    "uống",
    "học",
    "đọc",
    "viết",
    "ghi",
    "chú",
    "mưa",
    "nắng",
    "đường",
    "xe",
    "bạn",
    "tôi",
    "ngày",
    "tháng",
    "năm",
];
const RARE: [&str; 24] = [
    "khủng",
    "long",
    "bão",
    "lũ",
    "động",
    "đất",
    "sao",
    "chổi",
    "ngọc",
    "trai",
    "san",
    "hô",
    "phượng",
    "hoàng",
    "rồng",
    "thiêng",
    "cổ",
    "tích",
    "huyền",
    "thoại",
    "biển",
    "đảo",
    "núi",
    "rừng",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join(format!("mimint-bm25-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let db_path = dir.join("bench.db");
    let notebook = Notebook::open(&db_path)?;

    let mut rng = Lcg(0x5eed);
    let build_started = Instant::now();
    for i in 0..NOTE_COUNT {
        // 8..24 words per body: short notes and long notes.
        let words = 8 + rng.below(17);
        let mut body = String::new();
        for _ in 0..words {
            let word = if rng.below(10) == 0 {
                RARE[rng.below(RARE.len())]
            } else {
                COMMON[rng.below(COMMON.len())]
            };
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(word);
        }
        notebook.add(&format!("Ghi chú {i}"), &body)?;
    }
    let build_elapsed = build_started.elapsed();

    // Queries: half common terms, half rare terms, half
    // two-term combinations — the shapes real turns send.
    let mut queries = Vec::with_capacity(QUERY_COUNT);
    for i in 0..QUERY_COUNT {
        let query = match i % 4 {
            0 => COMMON[rng.below(COMMON.len())].to_string(),
            1 => RARE[rng.below(RARE.len())].to_string(),
            2 => format!(
                "{} {}",
                COMMON[rng.below(COMMON.len())],
                COMMON[rng.below(COMMON.len())]
            ),
            _ => format!(
                "{} {}",
                RARE[rng.below(RARE.len())],
                COMMON[rng.below(COMMON.len())]
            ),
        };
        queries.push(query);
    }

    // Warm-up: one query, discarded (page cache, allocator).
    let _ = notebook.search_ranked(&queries[0])?;

    let mut samples: Vec<Duration> = Vec::with_capacity(QUERY_COUNT);
    let mut total_hits = 0usize;
    for query in &queries {
        let started = Instant::now();
        let hits = notebook.search_ranked(query)?;
        samples.push(started.elapsed());
        total_hits += hits.len();
    }
    samples.sort();
    let median = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
    let mean_ns: u128 =
        samples.iter().map(Duration::as_nanos).sum::<u128>() / samples.len() as u128;

    println!("notes={NOTE_COUNT}");
    println!("queries={QUERY_COUNT}");
    println!("build_ms={}", build_elapsed.as_millis());
    println!("total_hits={total_hits}");
    println!("median_ms={:.3}", median.as_secs_f64() * 1000.0);
    println!("p95_ms={:.3}", p95.as_secs_f64() * 1000.0);
    println!("mean_ms={:.3}", mean_ns as f64 / 1_000_000.0);
    println!(
        "min_ms={:.3}",
        samples.first().unwrap().as_secs_f64() * 1000.0
    );
    println!(
        "max_ms={:.3}",
        samples.last().unwrap().as_secs_f64() * 1000.0
    );
    println!(
        "verdict={}",
        if median.as_secs_f64() * 1000.0 > 50.0 {
            "OVER-THRESHOLD"
        } else {
            "WITHIN-THRESHOLD"
        }
    );

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
