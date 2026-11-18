# Fine-tune hardware gate (Phase 5, D5)

Fine-tuning is GATED on hardware, and real training is
BLOCKED in this build: there is no training backend compiled
in. This document states the gate, the probe, and the
training-adapter contract — and states plainly that no
training has happened.

## The gate

Three gates, all measured at probe time (Phase 0, G2).
The RAM gate uses TOTAL RAM (fix pass, F5-09): total memory
is a property of the machine, while available memory moves
with load and made the verdict flicker between probes.

| Gate | Threshold | Measured how |
|---|---|---|
| GPU VRAM | ≥ 8 GiB free | `nvidia-smi`, best GPU wins; bounded to 5 seconds; a missing binary means no CUDA GPU, while a timeout or spawn failure means "unmeasurable" (a distinct state, not "no GPU") |
| RAM total | ≥ 8 GiB | `MemTotal` from `/proc/meminfo` (Linux) |
| SIMD | AVX2 (x86_64) or NEON (aarch64) | `is_x86_feature_detected!("avx2")` on x86_64; NEON assumed on aarch64 |

All three must pass. The verdict names each failing gate with
its measured value, the required threshold, and the shortfall.
An unmeasurable value fails closed (treated as not passing),
except the GPU query's timeout/spawn-failure case, which is
reported as "không đo được" with its own reason — the user
must be able to tell "no GPU" from "the measurement failed".

### The "busy" warning

`MemAvailable` is measured separately, as a warning only: 3
samples 100ms apart, the median reported, with the
measurement time. When available RAM is below 8 GiB while
total RAM passes, the verdict stays available but carries a
"đang bận" warning explaining that RAM moves with load and
naming the shortfall. The warning never changes the verdict.

### Hysteresis

The UI displays the verdict through a 2-consecutive-reading
hysteresis tracker: a differing raw verdict must repeat
before the displayed verdict flips, so borderline hardware
does not flicker between probes. The CLI prints the raw
verdict of its single probe.

### Platform limits

The probe is x86/NVIDIA-shaped: GPU detection is
`nvidia-smi` only, and SIMD detection is AVX2 on x86_64.
On Apple Silicon there is no `nvidia-smi` and no AVX2, so
the probe reports no CUDA GPU and (on aarch64) assumes NEON
— the gate does not measure Apple GPUs or unified memory,
and the verdict on a Mac is not evidence about Apple
hardware capability. The probe is Linux-first:
`/proc/meminfo` does not exist on macOS/Windows, where RAM
reads as unmeasurable and the gate fails closed.

On the Phase 5 build machine the verdict is UNAVAILABLE: no
NVIDIA GPU and no AVX2. The verdict stays UNAVAILABLE
regardless of the RAM reading, because the GPU and SIMD
gates fail permanently on that hardware.

Phase 0's additional gates (≥ 4 threads, ≥ 30 GB free disk)
are out of scope for this gate and are not evaluated here.

## Where the status shows

- CLI: `mimint-cli --finetune-status` (a pre-scan flag, like
  `--help`: it prints the verdict and exits).
- Eval: `mimint-eval finetune` (same verdict, plus the
  training-blocked statement).
- UI: the sidebar "Fine-tune" section shows per-gate verdicts
  and the reasons, with a "Kiểm tra lại" button that re-runs
  the bounded probe.

## Training-adapter contract

`TrainingAdapter` is the trait a future training backend
would implement; `TrainingRequest` carries the dataset path,
base model, output path, and hyperparameters;
`TrainingReport` carries the outcome. `request_training`
ALWAYS errors in this build:

- gate failed → the error lists the failing gates;
- gate passed → the error states that no training backend is
  compiled in this build.

There is no code path that starts training, and no training
run is claimed anywhere. Mock-probe tests cover both sides of
every threshold, including the exact boundary values (8192
MiB VRAM passes, 8191 fails). No unit test asserts a
particular machine's verdict: the tests must pass on any
hardware, and the machine-specific verdict belongs in CLI
output and the eval report, not in a test.
