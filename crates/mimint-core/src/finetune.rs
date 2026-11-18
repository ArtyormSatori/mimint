//! Fine-tune hardware gate (Phase 5, P5.7; decision D5).
//!
//! Real training is BLOCKED in this build: no training backend
//! is compiled in, and [`request_training`] always errors. What
//! this module provides is the gate — a hardware probe, the
//! thresholds, and the adapter contract a future backend must
//! implement — plus honest reporting of why the gate fails.
//!
//! Thresholds (from the Phase 0 decision record):
//! - GPU VRAM free ≥ 8 GiB (8,192 MiB),
//! - RAM available ≥ 8 GiB,
//! - AVX2 (x86_64) or NEON (aarch64).
//!
//! The probe measures RAM AVAILABLE (what the machine can
//! actually allocate now), not total: a machine with 16 GiB
//! total but 3 GiB free cannot train. GPU VRAM is measured via
//! `nvidia-smi` (free memory, bounded to 5 seconds); a machine
//! without an NVIDIA GPU reports no GPU. SIMD is detected with
//! the standard library's CPU feature detection. Every gate
//! failure names the gate and the measured numbers, so the
//! verdict is checkable instead of a bare "unavailable".
//!
//! Scope: this gate covers exactly the three criteria above.
//! The Phase 0 record also mentioned thread count and disk
//! space; those are NOT part of this gate (out of scope for
//! Phase 5) and are not probed here.

use crate::error::MimintError;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Minimum free GPU VRAM, in MiB (8 GiB).
pub const MIN_GPU_VRAM_MIB: u64 = 8192;
/// Minimum available RAM, in bytes (8 GiB).
pub const MIN_RAM_AVAILABLE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// How long the `nvidia-smi` probe may run before it is killed.
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// One hardware measurement. `None` fields mean "not detected /
/// not measurable on this machine", which fails the gate: an
/// unmeasured resource is not assumed sufficient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareSnapshot {
    /// Free VRAM of the best NVIDIA GPU, in MiB. `None` when no
    /// NVIDIA GPU was detected or `nvidia-smi` could not run.
    pub gpu_vram_mib: Option<u64>,
    pub gpu_name: Option<String>,
    /// RAM the machine can allocate now, in bytes.
    pub ram_available_bytes: Option<u64>,
    pub ram_total_bytes: Option<u64>,
    /// AVX2 on x86_64, NEON (always present) on aarch64.
    pub avx2_or_neon: bool,
    pub cpu_model: String,
}

/// Which gate a verdict belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateName {
    GpuVram,
    RamAvailable,
    Simd,
}

/// Verdict of one gate, with measured and required values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateVerdict {
    pub gate: GateName,
    pub passed: bool,
    /// Measured value, human-readable (or "không đo được").
    pub measured: String,
    /// Required value, human-readable.
    pub required: String,
    /// Vietnamese reason; empty when the gate passed.
    pub reason: String,
}

/// Full fine-tune status: the snapshot, per-gate verdicts, and
/// the overall verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FineTuneStatus {
    pub available: bool,
    pub snapshot: HardwareSnapshot,
    pub gates: Vec<GateVerdict>,
    /// Reasons of the failing gates, in gate order.
    pub reasons: Vec<String>,
}

impl FineTuneStatus {
    /// Vietnamese multi-line summary for CLI/UI display.
    pub fn summary(&self) -> String {
        let mut lines = Vec::new();
        lines.push(if self.available {
            "Fine-tune: KHẢ DỤNG trên phần cứng này (chưa có backend huấn luyện trong bản này)."
                .to_string()
        } else {
            "Fine-tune: KHÔNG KHẢ DỤNG trên phần cứng này.".to_string()
        });
        for gate in &self.gates {
            let mark = if gate.passed {
                "ĐẠT"
            } else {
                "KHÔNG ĐẠT"
            };
            lines.push(format!(
                "- {:?}: {} (đo được: {}, yêu cầu: {})",
                gate.gate, mark, gate.measured, gate.required
            ));
        }
        for reason in &self.reasons {
            lines.push(format!("Lý do: {reason}"));
        }
        lines.push(
            "Huấn luyện thật bị chặn trong bản này: chưa có backend huấn luyện nào được biên dịch kèm."
                .to_string(),
        );
        lines.join("\n")
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// Pure gate evaluation: the same snapshot always yields the
/// same verdict. Tests cover both sides of every threshold,
/// including the exact boundary values.
pub fn evaluate(snapshot: &HardwareSnapshot) -> FineTuneStatus {
    let gpu = match snapshot.gpu_vram_mib {
        Some(mib) if mib >= MIN_GPU_VRAM_MIB => GateVerdict {
            gate: GateName::GpuVram,
            passed: true,
            measured: format!(
                "{mib} MiB trống{}",
                snapshot
                    .gpu_name
                    .as_ref()
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default()
            ),
            required: format!("≥ {MIN_GPU_VRAM_MIB} MiB VRAM trống"),
            reason: String::new(),
        },
        Some(mib) => GateVerdict {
            gate: GateName::GpuVram,
            passed: false,
            measured: format!("{mib} MiB trống"),
            required: format!("≥ {MIN_GPU_VRAM_MIB} MiB VRAM trống"),
            reason: format!("GPU chỉ còn {mib} MiB VRAM trống, cần ít nhất {MIN_GPU_VRAM_MIB} MiB"),
        },
        None => GateVerdict {
            gate: GateName::GpuVram,
            passed: false,
            measured: "không phát hiện GPU NVIDIA".to_string(),
            required: format!("≥ {MIN_GPU_VRAM_MIB} MiB VRAM trống"),
            reason: "không phát hiện GPU NVIDIA (cần ít nhất 8 GiB VRAM trống)".to_string(),
        },
    };
    let ram = match snapshot.ram_available_bytes {
        Some(bytes) if bytes >= MIN_RAM_AVAILABLE_BYTES => GateVerdict {
            gate: GateName::RamAvailable,
            passed: true,
            measured: format_bytes(bytes),
            required: format_bytes(MIN_RAM_AVAILABLE_BYTES),
            reason: String::new(),
        },
        Some(bytes) => GateVerdict {
            gate: GateName::RamAvailable,
            passed: false,
            measured: format_bytes(bytes),
            required: format_bytes(MIN_RAM_AVAILABLE_BYTES),
            reason: format!(
                "RAM khả dụng chỉ {} (cần ít nhất {})",
                format_bytes(bytes),
                format_bytes(MIN_RAM_AVAILABLE_BYTES)
            ),
        },
        None => GateVerdict {
            gate: GateName::RamAvailable,
            passed: false,
            measured: "không đo được".to_string(),
            required: format_bytes(MIN_RAM_AVAILABLE_BYTES),
            reason: "không đo được RAM khả dụng trên máy này".to_string(),
        },
    };
    let simd = if snapshot.avx2_or_neon {
        GateVerdict {
            gate: GateName::Simd,
            passed: true,
            measured: "có AVX2/NEON".to_string(),
            required: "AVX2 (x86_64) hoặc NEON (aarch64)".to_string(),
            reason: String::new(),
        }
    } else {
        GateVerdict {
            gate: GateName::Simd,
            passed: false,
            measured: "không có AVX2".to_string(),
            required: "AVX2 (x86_64) hoặc NEON (aarch64)".to_string(),
            reason: "CPU không có AVX2".to_string(),
        }
    };
    let gates = vec![gpu, ram, simd];
    let reasons: Vec<String> = gates
        .iter()
        .filter(|gate| !gate.passed)
        .map(|gate| gate.reason.clone())
        .collect();
    FineTuneStatus {
        available: gates.iter().all(|gate| gate.passed),
        snapshot: snapshot.clone(),
        gates,
        reasons,
    }
}

/// Probe the real hardware. Never panics: every measurement that
/// cannot be taken degrades to `None`/`false` (fail-closed).
/// Linux-only RAM/CPU parsing; other platforms report
/// unmeasured values and the gate fails closed with a reason.
pub fn probe() -> HardwareSnapshot {
    let (ram_available_bytes, ram_total_bytes, cpu_model) = probe_linux();
    let (gpu_vram_mib, gpu_name) = probe_nvidia();
    HardwareSnapshot {
        gpu_vram_mib,
        gpu_name,
        ram_available_bytes,
        ram_total_bytes,
        avx2_or_neon: detect_simd(),
        cpu_model,
    }
}

fn detect_simd() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2")
    }
    #[cfg(target_arch = "aarch64")]
    {
        // NEON is mandatory on aarch64.
        true
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn probe_linux() -> (Option<u64>, Option<u64>, String) {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut total = None;
    let mut available = None;
    for line in meminfo.lines() {
        let mut parts = line.split_whitespace();
        let key = parts.next().unwrap_or("");
        let value: Option<u64> = parts.next().and_then(|v| v.parse().ok());
        match (key, value) {
            ("MemTotal:", Some(kib)) => total = Some(kib * 1024),
            ("MemAvailable:", Some(kib)) => available = Some(kib * 1024),
            _ => {}
        }
    }
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu_model = cpuinfo
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split(':').nth(1))
        .map(|name| name.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    (available, total, cpu_model)
}

#[cfg(not(target_os = "linux"))]
fn probe_linux() -> (Option<u64>, Option<u64>, String) {
    (None, None, "unknown".to_string())
}

/// Query `nvidia-smi` for free VRAM, bounded by
/// [`NVIDIA_SMI_TIMEOUT`]. Returns the GPU with the most free
/// VRAM. Any failure (binary missing, timeout, unparsable
/// output) yields `None`: no GPU is assumed.
fn probe_nvidia() -> (Option<u64>, Option<String>) {
    let mut child = match Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return (None, None),
    };
    let start = Instant::now();
    let exited = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) => {
                if start.elapsed() > NVIDIA_SMI_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break false,
        }
    };
    if !exited {
        return (None, None);
    }
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    let _ = child.wait();
    let mut best: Option<(u64, String)> = None;
    for line in stdout.lines() {
        let mut parts = line.splitn(2, ',');
        let name = parts.next().unwrap_or("").trim().to_string();
        let mib: Option<u64> = parts.next().and_then(|v| v.trim().parse().ok());
        if let Some(mib) = mib {
            let better = best
                .as_ref()
                .map(|(best_mib, _)| mib > *best_mib)
                .unwrap_or(true);
            if better {
                best = Some((mib, name));
            }
        }
    }
    match best {
        Some((mib, name)) => (Some(mib), Some(name)),
        None => (None, None),
    }
}

// ---------------------------------------------------------------------------
// Training adapter contract (no backend compiled in).
// ---------------------------------------------------------------------------

/// A fine-tuning job request. This is the contract a future
/// training backend must accept; nothing in this build executes
/// one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainingRequest {
    /// Base model identifier (e.g. a local Ollama model name).
    pub base_model: String,
    /// Path to the training dataset file (JSONL).
    pub dataset_path: String,
    /// LoRA rank.
    pub rank: u32,
    pub learning_rate: f64,
    pub epochs: u32,
    /// Directory the adapter must write to.
    pub output_dir: String,
}

/// Report a training backend returns on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrainingReport {
    pub adapter: String,
    pub output_dir: String,
    pub examples: u64,
    pub epochs_completed: u32,
}

/// The contract a training backend implements. No implementation
/// ships in this build.
pub trait TrainingAdapter: Send + Sync {
    fn name(&self) -> &str;
    fn train(&self, request: &TrainingRequest) -> Result<TrainingReport, MimintError>;
}

/// Request training. ALWAYS errors in this build:
/// - when the hardware gate fails, with the failing gates' reasons;
/// - when the gate passes, stating that no training backend is
///   compiled in.
///
/// There is no code path that starts training, and no caller may
/// claim training happened: this function is the only entry
/// point, and it never succeeds.
pub fn request_training(
    snapshot: &HardwareSnapshot,
    _request: &TrainingRequest,
) -> Result<TrainingReport, MimintError> {
    let status = evaluate(snapshot);
    if !status.available {
        return Err(MimintError::Config(format!(
            "fine-tune unavailable on this hardware: {}",
            status.reasons.join("; ")
        )));
    }
    Err(MimintError::Config(
        "fine-tune gate passed, but no training backend is compiled into this build: real training is blocked".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(gpu: Option<u64>, ram: Option<u64>, simd: bool) -> HardwareSnapshot {
        HardwareSnapshot {
            gpu_vram_mib: gpu,
            gpu_name: gpu.map(|_| "Test GPU".to_string()),
            ram_available_bytes: ram,
            ram_total_bytes: ram.map(|bytes| bytes * 2),
            avx2_or_neon: simd,
            cpu_model: "Test CPU".to_string(),
        }
    }

    fn gib(bytes: u64) -> u64 {
        bytes * 1024 * 1024 * 1024
    }

    #[test]
    fn all_gates_pass_at_and_above_threshold() {
        let status = evaluate(&snapshot(Some(8192), Some(gib(8)), true));
        assert!(status.available);
        assert!(status.reasons.is_empty());
        assert!(status.gates.iter().all(|gate| gate.passed));
        let status = evaluate(&snapshot(Some(24576), Some(gib(32)), true));
        assert!(status.available);
    }

    #[test]
    fn each_gate_fails_below_threshold_with_numbers() {
        // GPU 1 MiB short.
        let status = evaluate(&snapshot(Some(8191), Some(gib(8)), true));
        assert!(!status.available);
        assert_eq!(status.reasons.len(), 1);
        assert!(status.reasons[0].contains("8191"), "{}", status.reasons[0]);
        // No GPU at all.
        let status = evaluate(&snapshot(None, Some(gib(8)), true));
        assert!(!status.available);
        assert!(
            status.reasons[0].contains("không phát hiện GPU"),
            "{}",
            status.reasons[0]
        );
        // RAM 1 byte short.
        let status = evaluate(&snapshot(
            Some(8192),
            Some(MIN_RAM_AVAILABLE_BYTES - 1),
            true,
        ));
        assert!(!status.available);
        assert_eq!(status.reasons.len(), 1);
        // RAM unmeasurable fails closed.
        let status = evaluate(&snapshot(Some(8192), None, true));
        assert!(!status.available);
        // SIMD missing.
        let status = evaluate(&snapshot(Some(8192), Some(gib(8)), false));
        assert!(!status.available);
        assert!(status.reasons[0].contains("AVX2"), "{}", status.reasons[0]);
    }

    #[test]
    fn vega_like_hardware_reports_all_three_failures() {
        // The Phase 0 verdict shape: no GPU, little RAM, no AVX2.
        let status = evaluate(&snapshot(None, Some(gib(3)), false));
        assert!(!status.available);
        assert_eq!(status.reasons.len(), 3);
        assert_eq!(status.gates.len(), 3);
        assert!(status.gates.iter().all(|gate| !gate.passed));
    }

    #[test]
    fn request_training_always_errors() {
        let request = TrainingRequest {
            base_model: "qwen2.5:3b".to_string(),
            dataset_path: "/tmp/data.jsonl".to_string(),
            rank: 8,
            learning_rate: 0.0002,
            epochs: 3,
            output_dir: "/tmp/out".to_string(),
        };
        // Gate fails: the error names the reasons.
        let err = request_training(&snapshot(None, Some(gib(3)), false), &request)
            .expect_err("training must be blocked when the gate fails");
        assert!(err.to_string().contains("unavailable"), "{err}");
        // Gate passes: still blocked, no backend.
        let err = request_training(&snapshot(Some(16384), Some(gib(16)), true), &request)
            .expect_err("training must be blocked: no backend compiled in");
        assert!(err.to_string().contains("no training backend"), "{err}");
    }

    #[test]
    fn probe_runs_and_returns_a_well_formed_snapshot() {
        let snapshot = probe();
        // Consistency: available never exceeds total when both
        // are measured. No availability assertion: this test must
        // pass on any machine, capable or not.
        if let (Some(available), Some(total)) =
            (snapshot.ram_available_bytes, snapshot.ram_total_bytes)
        {
            assert!(available <= total);
        }
        let status = evaluate(&snapshot);
        assert_eq!(status.gates.len(), 3);
        assert_eq!(
            status.available,
            status.gates.iter().all(|gate| gate.passed)
        );
        // The summary always states the training block.
        assert!(status.summary().contains("bị chặn"));
    }
}
