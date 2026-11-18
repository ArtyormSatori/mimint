//! Fine-tune hardware gate (Phase 5, P5.7; decision D5).
//!
//! Real training is BLOCKED in this build: no training backend
//! is compiled in, and [`request_training`] always errors. What
//! this module provides is the gate — a hardware probe, the
//! thresholds, and the adapter contract a future backend must
//! implement — plus honest reporting of why the gate fails.
//!
//! Thresholds (from the Phase 0 decision record, corrected in
//! the Phase 5 fix pass, F5-09):
//! - GPU VRAM free ≥ 8 GiB (8,192 MiB),
//! - RAM TOTAL ≥ 8 GiB (feasibility: what the machine HAS),
//! - AVX2 (x86_64) or NEON (aarch64).
//!
//! The RAM feasibility gate uses MemTotal, not MemAvailable:
//! MemAvailable is load-dependent — the same machine passes in
//! the morning and fails under load — so gating on it makes the
//! verdict flicker with whatever else is running. MemAvailable
//! is still measured (median of 3 samples) and reported as a
//! separate "busy" WARNING: it never gates feasibility, but it
//! tells the user the machine is loaded right now. GPU VRAM is
//! measured via `nvidia-smi` (free memory, bounded to 5 seconds);
//! a machine without an NVIDIA GPU reports no GPU, which is
//! DISTINCT from a probe that could not complete (timeout,
//! spawn failure): the latter is reported as "unmeasurable"
//! with its own reason, and fails closed without claiming there
//! is no GPU. SIMD is detected with the standard library's CPU
//! feature detection. Every gate failure names the gate, the
//! measured numbers, and the SHORTFALL against the threshold, so
//! the verdict is checkable instead of a bare "unavailable".
//!
//! Scope: this gate covers exactly the three criteria above,
//! on x86_64/NVIDIA hardware via Linux `/proc` and `nvidia-smi`.
//! Apple Silicon is NOT covered: there is no NVIDIA GPU and no
//! `/proc/meminfo`, so the probe reports unmeasured values and
//! the gate fails closed — the gate says nothing about whether
//! an Apple Silicon machine could train (see
//! `docs/finetune-gate.md`). The Phase 0 record also mentioned
//! thread count and disk space; those are NOT part of this gate
//! (out of scope for Phase 5) and are not probed here.

use crate::audit::now_ms;
use crate::error::MimintError;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Minimum free GPU VRAM, in MiB (8 GiB).
pub const MIN_GPU_VRAM_MIB: u64 = 8192;
/// Minimum TOTAL RAM, in bytes (8 GiB): the feasibility gate
/// (F5-09). A machine with less total RAM can never train,
/// however idle it is.
pub const MIN_RAM_TOTAL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Minimum available RAM, in bytes (8 GiB): the "busy" WARNING
/// threshold, not a gate. Below this, the machine is loaded
/// right now; feasibility is decided by total RAM.
pub const MIN_RAM_AVAILABLE_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// How many MemAvailable samples the probe takes (median wins).
const RAM_SAMPLE_COUNT: usize = 3;
/// Delay between MemAvailable samples.
const RAM_SAMPLE_DELAY: Duration = Duration::from_millis(100);
/// How long the `nvidia-smi` probe may run before it is killed.
const NVIDIA_SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// One hardware measurement. `None` fields mean "not detected /
/// not measurable on this machine", which fails the gate: an
/// unmeasured resource is not assumed sufficient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardwareSnapshot {
    /// Free VRAM of the best NVIDIA GPU, in MiB. `None` when no
    /// NVIDIA GPU was detected OR the probe could not complete —
    /// see `gpu_unmeasurable_reason` to tell the two apart.
    pub gpu_vram_mib: Option<u64>,
    pub gpu_name: Option<String>,
    /// Why the GPU probe could not complete (timeout, spawn
    /// failure, ...). `Some` means "unmeasurable", which is NOT
    /// the same as "no GPU": `None` here with no VRAM means
    /// `nvidia-smi` ran and found nothing.
    pub gpu_unmeasurable_reason: Option<String>,
    /// MEDIAN of the MemAvailable samples: the "busy" signal.
    /// Never gates feasibility (see `ram_total_bytes`).
    pub ram_available_bytes: Option<u64>,
    /// The MemAvailable samples (bytes), in measurement order.
    /// Empty when RAM could not be measured on this platform.
    pub ram_available_samples: Vec<u64>,
    /// Total installed RAM, in bytes: the feasibility gate input.
    pub ram_total_bytes: Option<u64>,
    /// AVX2 on x86_64, NEON (always present) on aarch64.
    pub avx2_or_neon: bool,
    pub cpu_model: String,
    /// Measurement time, milliseconds since the Unix epoch.
    pub measured_at_ms: u64,
}

/// Which gate a verdict belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateName {
    GpuVram,
    RamTotal,
    Simd,
}

impl GateName {
    /// Human-readable gate name for status displays.
    pub fn label(self) -> &'static str {
        match self {
            GateName::GpuVram => "GPU VRAM",
            GateName::RamTotal => "RAM tổng",
            GateName::Simd => "SIMD (AVX2/NEON)",
        }
    }
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

/// Full fine-tune status: the snapshot, per-gate verdicts, the
/// overall verdict, and the separate "busy" warning (which never
/// gates feasibility).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FineTuneStatus {
    pub available: bool,
    pub snapshot: HardwareSnapshot,
    pub gates: Vec<GateVerdict>,
    /// Reasons of the failing gates, in gate order.
    pub reasons: Vec<String>,
    /// The "busy" warning, when MemAvailable is measured and
    /// below the warning threshold (or unmeasurable while total
    /// RAM is known). Informational only: it never changes
    /// `available`.
    pub busy_warning: Option<String>,
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
                "- {}: {} (đo được: {}, yêu cầu: {})",
                gate.gate.label(),
                mark,
                gate.measured,
                gate.required
            ));
        }
        for reason in &self.reasons {
            lines.push(format!("Lý do: {reason}"));
        }
        if let Some(warning) = &self.busy_warning {
            lines.push(format!("Cảnh báo: {warning}"));
        }
        if !self.snapshot.ram_available_samples.is_empty() {
            lines.push(format!(
                "RAM khả dụng: {} (trung vị {} mẫu, đo lúc {} ms kể từ epoch; RAM khả dụng dao động theo tải máy).",
                self.snapshot
                    .ram_available_bytes
                    .map(format_bytes)
                    .unwrap_or_else(|| "không đo được".to_string()),
                self.snapshot.ram_available_samples.len(),
                self.snapshot.measured_at_ms
            ));
        }
        lines.push(
            "Huấn luyện thật bị chặn trong bản này: chưa có backend huấn luyện nào được biên dịch kèm."
                .to_string(),
        );
        lines.join("\n")
    }
}

/// Hysteresis for the DISPLAYED verdict (F5-09): the displayed
/// availability flips only after TWO consecutive raw verdicts
/// that differ from what is displayed. A single flickering
/// measurement (a transient load spike, one slow `nvidia-smi`)
/// never flips the UI. The raw status is always kept alongside:
/// hysteresis affects only what is shown as available, never the
/// recorded gates or the training block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerdictTracker {
    displayed: bool,
    pending: Option<bool>,
}

impl VerdictTracker {
    /// Start displaying the first raw verdict.
    pub fn new(first_raw: bool) -> Self {
        Self {
            displayed: first_raw,
            pending: None,
        }
    }

    /// Feed one raw verdict; return what should be displayed.
    pub fn update(&mut self, raw: bool) -> bool {
        if raw == self.displayed {
            self.pending = None;
            return self.displayed;
        }
        match self.pending {
            Some(pending) if pending == raw => {
                // Second consecutive differing verdict: flip.
                self.displayed = raw;
                self.pending = None;
                self.displayed
            }
            _ => {
                self.pending = Some(raw);
                self.displayed
            }
        }
    }

    /// What is currently displayed.
    pub fn displayed(&self) -> bool {
        self.displayed
    }
}

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// Pure gate evaluation: the same snapshot always yields the
/// same verdict. Tests cover both sides of every threshold,
/// including the exact boundary values. The RAM gate reads
/// MemTotal; MemAvailable produces only the busy warning.
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
            reason: format!(
                "GPU chỉ còn {mib} MiB VRAM trống, cần ít nhất {MIN_GPU_VRAM_MIB} MiB (thiếu {} MiB)",
                MIN_GPU_VRAM_MIB - mib
            ),
        },
        None => {
            if let Some(reason) = &snapshot.gpu_unmeasurable_reason {
                GateVerdict {
                    gate: GateName::GpuVram,
                    passed: false,
                    measured: "không đo được".to_string(),
                    required: format!("≥ {MIN_GPU_VRAM_MIB} MiB VRAM trống"),
                    reason: format!(
                        "không đo được GPU ({reason}); không giả định là đủ, cũng không kết luận là không có GPU"
                    ),
                }
            } else {
                GateVerdict {
                    gate: GateName::GpuVram,
                    passed: false,
                    measured: "không phát hiện GPU NVIDIA".to_string(),
                    required: format!("≥ {MIN_GPU_VRAM_MIB} MiB VRAM trống"),
                    reason: "không phát hiện GPU NVIDIA (cần ít nhất 8 GiB VRAM trống)".to_string(),
                }
            }
        }
    };
    let ram = match snapshot.ram_total_bytes {
        Some(bytes) if bytes >= MIN_RAM_TOTAL_BYTES => GateVerdict {
            gate: GateName::RamTotal,
            passed: true,
            measured: format!("{} tổng", format_bytes(bytes)),
            required: format!("≥ {}", format_bytes(MIN_RAM_TOTAL_BYTES)),
            reason: String::new(),
        },
        Some(bytes) => GateVerdict {
            gate: GateName::RamTotal,
            passed: false,
            measured: format!("{} tổng", format_bytes(bytes)),
            required: format!("≥ {}", format_bytes(MIN_RAM_TOTAL_BYTES)),
            reason: format!(
                "RAM tổng chỉ {} (cần ít nhất {}, thiếu {})",
                format_bytes(bytes),
                format_bytes(MIN_RAM_TOTAL_BYTES),
                format_bytes(MIN_RAM_TOTAL_BYTES - bytes)
            ),
        },
        None => GateVerdict {
            gate: GateName::RamTotal,
            passed: false,
            measured: "không đo được".to_string(),
            required: format!("≥ {}", format_bytes(MIN_RAM_TOTAL_BYTES)),
            reason: "không đo được RAM tổng trên máy này".to_string(),
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
    // The busy warning: MemAvailable is load-dependent, so it
    // warns, never gates. It fires whenever the median available
    // RAM is measured and below the warning threshold.
    let busy_warning = match snapshot.ram_available_bytes {
        Some(available) if available < MIN_RAM_AVAILABLE_BYTES => Some(format!(
            "máy đang bận: RAM khả dụng chỉ {} (trung vị {} mẫu), trong khi RAM tổng là {}; RAM khả dụng dao động theo tải máy — cổng khả thi dùng RAM tổng, nhưng nên đóng ứng dụng khác rồi đo lại trước khi huấn luyện",
            format_bytes(available),
            snapshot.ram_available_samples.len(),
            snapshot
                .ram_total_bytes
                .map(format_bytes)
                .unwrap_or_else(|| "không đo được".to_string()),
        )),
        None if snapshot.ram_total_bytes.is_some() => Some(
            "không đo được RAM khả dụng (chỉ đo được RAM tổng); cổng khả thi dùng RAM tổng"
                .to_string(),
        ),
        _ => None,
    };
    FineTuneStatus {
        available: gates.iter().all(|gate| gate.passed),
        snapshot: snapshot.clone(),
        gates,
        reasons,
        busy_warning,
    }
}

/// Probe the real hardware. Never panics: every measurement that
/// cannot be taken degrades to `None`/`false` (fail-closed).
/// Linux-only RAM/CPU parsing; other platforms report
/// unmeasured values and the gate fails closed with a reason.
/// MemAvailable is sampled [`RAM_SAMPLE_COUNT`] times and the
/// median is kept, so one transient spike does not move the
/// busy warning.
pub fn probe() -> HardwareSnapshot {
    let measured_at_ms = now_ms();
    let (samples, ram_total_bytes, cpu_model) = probe_linux();
    let ram_available_bytes = median(&samples);
    let (gpu_vram_mib, gpu_name, gpu_unmeasurable_reason) = probe_nvidia();
    HardwareSnapshot {
        gpu_vram_mib,
        gpu_name,
        gpu_unmeasurable_reason,
        ram_available_bytes,
        ram_available_samples: samples,
        ram_total_bytes,
        avx2_or_neon: detect_simd(),
        cpu_model,
        measured_at_ms,
    }
}

/// Median of the samples (middle value after sorting). `None`
/// when there are no samples.
fn median(samples: &[u64]) -> Option<u64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Some(sorted[sorted.len() / 2])
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
fn probe_linux() -> (Vec<u64>, Option<u64>, String) {
    let mut samples = Vec::new();
    let mut total = None;
    for attempt in 0..RAM_SAMPLE_COUNT {
        if attempt > 0 {
            std::thread::sleep(RAM_SAMPLE_DELAY);
        }
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mut available_this_read = None;
        for line in meminfo.lines() {
            let mut parts = line.split_whitespace();
            let key = parts.next().unwrap_or("");
            let value: Option<u64> = parts.next().and_then(|v| v.parse().ok());
            match (key, value) {
                ("MemTotal:", Some(kib)) => total = Some(kib * 1024),
                ("MemAvailable:", Some(kib)) => available_this_read = Some(kib * 1024),
                _ => {}
            }
        }
        // A read without a MemAvailable line contributes no
        // sample: a missing sample stays missing, never padded.
        if let Some(available) = available_this_read {
            samples.push(available);
        }
    }
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpu_model = cpuinfo
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split(':').nth(1))
        .map(|name| name.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    (samples, total, cpu_model)
}

#[cfg(not(target_os = "linux"))]
fn probe_linux() -> (Vec<u64>, Option<u64>, String) {
    (Vec::new(), None, "unknown".to_string())
}

/// Query `nvidia-smi` for free VRAM, bounded by
/// [`NVIDIA_SMI_TIMEOUT`]. Returns the GPU with the most free
/// VRAM, its name, and — when the probe could not complete — the
/// reason it is unmeasurable. A missing `nvidia-smi` binary is
/// NOT unmeasurable: on a machine with no NVIDIA driver there is
/// nothing to measure, so it reports "no GPU". A timeout or a
/// spawn/wait failure IS unmeasurable: the probe cannot say
/// whether a GPU exists.
fn probe_nvidia() -> (Option<u64>, Option<String>, Option<String>) {
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
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (None, None, None),
        Err(err) => {
            return (
                None,
                None,
                Some(format!("không chạy được nvidia-smi: {err}")),
            );
        }
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
            Err(err) => {
                return (
                    None,
                    None,
                    Some(format!("không chờ được nvidia-smi: {err}")),
                );
            }
        }
    };
    if !exited {
        return (
            None,
            None,
            Some(format!(
                "nvidia-smi quá {} giây không trả lời và đã bị dừng",
                NVIDIA_SMI_TIMEOUT.as_secs()
            )),
        );
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
        Some((mib, name)) => (Some(mib), Some(name), None),
        None => (None, None, None),
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

    fn snapshot(gpu: Option<u64>, ram_total: Option<u64>, simd: bool) -> HardwareSnapshot {
        HardwareSnapshot {
            gpu_vram_mib: gpu,
            gpu_name: gpu.map(|_| "Test GPU".to_string()),
            gpu_unmeasurable_reason: None,
            // Idle machine by default: plenty available, so no
            // busy warning interferes with gate assertions.
            ram_available_bytes: Some(gib(16)),
            ram_available_samples: vec![gib(16); RAM_SAMPLE_COUNT],
            ram_total_bytes: ram_total,
            avx2_or_neon: simd,
            cpu_model: "Test CPU".to_string(),
            measured_at_ms: 1_700_000_000_000,
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
        // GPU 1 MiB short: the reason names the shortfall.
        let status = evaluate(&snapshot(Some(8191), Some(gib(8)), true));
        assert!(!status.available);
        assert_eq!(status.reasons.len(), 1);
        assert!(status.reasons[0].contains("8191"), "{}", status.reasons[0]);
        assert!(
            status.reasons[0].contains("thiếu 1 MiB"),
            "{}",
            status.reasons[0]
        );
        // No GPU at all.
        let status = evaluate(&snapshot(None, Some(gib(8)), true));
        assert!(!status.available);
        assert!(
            status.reasons[0].contains("không phát hiện GPU"),
            "{}",
            status.reasons[0]
        );
        // RAM total 1 byte short.
        let status = evaluate(&snapshot(Some(8192), Some(MIN_RAM_TOTAL_BYTES - 1), true));
        assert!(!status.available);
        assert_eq!(status.reasons.len(), 1);
        assert!(
            status.reasons[0].contains("RAM tổng"),
            "{}",
            status.reasons[0]
        );
        // RAM total unmeasurable fails closed.
        let status = evaluate(&snapshot(Some(8192), None, true));
        assert!(!status.available);
        // SIMD missing.
        let status = evaluate(&snapshot(Some(8192), Some(gib(8)), false));
        assert!(!status.available);
        assert!(status.reasons[0].contains("AVX2"), "{}", status.reasons[0]);
    }

    #[test]
    fn busy_ram_warns_without_gating_and_total_decides() {
        // Plenty of total RAM, little available right now: the
        // RAM gate PASSES (total decides) and a busy warning
        // appears — availability stays true.
        let mut snap = snapshot(Some(16384), Some(gib(16)), true);
        snap.ram_available_bytes = Some(gib(3));
        snap.ram_available_samples = vec![gib(2), gib(3), gib(4)];
        let status = evaluate(&snap);
        assert!(status.available);
        let warning = status.busy_warning.as_ref().expect("busy warning");
        assert!(warning.contains("đang bận"), "{warning}");
        assert!(warning.contains("trung vị 3 mẫu"), "{warning}");
        assert!(status.summary().contains("Cảnh báo:"));
        // The reverse: little total RAM, plenty available (an
        // idle small machine) still FAILS the gate — available
        // can never rescue an infeasible machine.
        let mut snap = snapshot(Some(16384), Some(gib(4)), true);
        snap.ram_available_bytes = Some(gib(4));
        snap.ram_available_samples = vec![gib(4); RAM_SAMPLE_COUNT];
        let status = evaluate(&snap);
        assert!(!status.available);
        assert!(status.reasons.iter().any(|r| r.contains("RAM tổng")));
    }

    #[test]
    fn unmeasurable_gpu_is_distinct_from_no_gpu() {
        let mut snap = snapshot(None, Some(gib(16)), true);
        snap.gpu_unmeasurable_reason = Some("nvidia-smi quá 5 giây không trả lời".to_string());
        let status = evaluate(&snap);
        assert!(!status.available);
        assert!(
            status.reasons[0].contains("không đo được GPU"),
            "{}",
            status.reasons[0]
        );
        assert!(
            !status.reasons[0].contains("không phát hiện GPU"),
            "{}",
            status.reasons[0]
        );
        assert_eq!(status.gates[0].measured, "không đo được");
    }

    #[test]
    fn verdict_tracker_needs_two_consecutive_readings_to_flip() {
        let mut tracker = VerdictTracker::new(true);
        assert!(tracker.update(true));
        // One differing reading: still displayed as available.
        assert!(tracker.update(false));
        assert!(tracker.displayed());
        // Back to agreeing: the pending flip is forgotten.
        assert!(tracker.update(true));
        // Two consecutive differing readings: flips.
        assert!(tracker.update(false));
        assert!(!tracker.update(false));
        assert!(!tracker.displayed());
        // And two consecutive readings flip it back.
        assert!(!tracker.update(true));
        assert!(tracker.update(true));
    }

    #[test]
    fn median_picks_the_middle_sample() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[5]), Some(5));
        assert_eq!(median(&[1, 9, 3]), Some(3));
        assert_eq!(median(&[9, 1, 3]), Some(3));
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
        // The median is the middle sample, when sampled.
        if !snapshot.ram_available_samples.is_empty() {
            assert_eq!(
                snapshot.ram_available_bytes,
                median(&snapshot.ram_available_samples)
            );
            assert!(snapshot.measured_at_ms > 0);
        }
        // Unmeasurable GPU and detected GPU are mutually exclusive.
        assert!(snapshot.gpu_vram_mib.is_none() || snapshot.gpu_unmeasurable_reason.is_none());
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
