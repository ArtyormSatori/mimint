//! Framebuffer pool bench (P4.6.2): 200 X11 screenshots without
//! recycling vs 200 with recycling, on the real `X11Body` under
//! Xvfb. Prints allocations/reuses + wall time for both phases.
//!
//! The numbers are the before/after measurement for the pooling
//! work; the bench itself asserts nothing about speed (the pool's
//! contract is allocation reuse, measured by the counters —
//! wall time is reported alongside, honestly, either way).
//!
//! Usage: `pool-bench [N]` (default 200 screenshots per phase).

#[cfg(target_os = "linux")]
mod bench {
    use mimint_body::{Body, X11Body};
    use std::time::Instant;

    pub fn run(n: usize) {
        // Phase A: no recycling — every capture allocates.
        let mut body = X11Body::connect().expect("connect X11 body");
        let start = Instant::now();
        for _ in 0..n {
            let _frame = body.screenshot().expect("screenshot");
        }
        let unrecycled_ms = start.elapsed().as_millis();
        let unrecycled = body.framebuffer_pool().unwrap().stats();
        println!(
            "unrecycled: shots={n} allocations={} reuses={} spare={} wall_ms={unrecycled_ms}",
            unrecycled.allocations, unrecycled.reuses, unrecycled.spare
        );

        // Phase B: recycle every frame — one buffer serves all.
        let mut body = X11Body::connect().expect("connect X11 body");
        let start = Instant::now();
        for _ in 0..n {
            let frame = body.screenshot().expect("screenshot");
            frame.recycle(body.framebuffer_pool().unwrap());
        }
        let recycled_ms = start.elapsed().as_millis();
        let recycled = body.framebuffer_pool().unwrap().stats();
        println!(
            "recycled: shots={n} allocations={} reuses={} spare={} wall_ms={recycled_ms}",
            recycled.allocations, recycled.reuses, recycled.spare
        );
    }
}

#[cfg(target_os = "linux")]
fn main() {
    let n = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(200);
    bench::run(n);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pool-bench is Linux-only (X11 body)");
    std::process::exit(2);
}
