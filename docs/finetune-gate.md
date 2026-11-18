# Fine-tune hardware gate (Phase 5, D5)

Fine-tuning is GATED on hardware, and real training is
BLOCKED in this build: there is no training backend compiled
in. This document states the gate, the probe, and the
training-adapter contract — and states plainly that no
training has happened.

## The gate

Three gates, all measured at probe time (Phase 0, G2):

| Gate | Threshold | Measured how |
|---|---|---|
| GPU VRAM | ≥ 8 GiB free | `nvidia-smi`, best GPU wins; bounded to 5 seconds; absent means no CUDA GPU |
| RAM available | ≥ 8 GiB | `MemAvailable` from `/proc/meminfo` (Linux); available, not total |
| SIMD | AVX2 (x86_64) or NEON (aarch64) | `is_x86_feature_detected!("avx2")` on x86_64; NEON assumed on aarch64 |

All three must pass. The verdict names each failing gate with
its measured value and the required threshold. An
unmeasurable value fails closed (treated as not passing).

The RAM gate is load-dependent: available memory moves with
what else is running. The probe reports the measured value at
probe time and never fabricates a failure. On the Phase 5
build machine the verdict is UNAVAILABLE: no NVIDIA GPU and
no AVX2, with RAM available measured at 9.6 GiB at probe time
(Phase 0 measured 3.8 GiB under load on the same machine).
The verdict stays UNAVAILABLE regardless of the RAM reading,
because the GPU and SIMD gates fail permanently on that
hardware.

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
