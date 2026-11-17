# Windows verification package

Everything needed to verify Phase 4's Windows body on a real
Windows machine. Nothing in this package has ever run on
Windows: the body is code only, written against the vendored
`windows` 0.62 registry source. **No runtime claim about
Windows is true until this package runs on the machine.**

## 0. Machine configuration (record first)

```powershell
$PSVersionTable.PSVersion
[System.Environment]::OSVersion.VersionString
(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB
rustc --version; cargo --version
rustup target list --installed
```

Also record: the display scaling setting (Settings > Display >
Scale), the number of monitors, and whether the account is a
standard user or an administrator.

## 1. Build + type-check (on the Windows machine)

```powershell
cargo check -p mimint-body
cargo build -p mimint-body --bin conformance --bin testwin
```

The first command that fails is a finding: record the exact
error. A link failure here (missing import libraries, wrong
target) is as much a result as a compile error.

## 2. Conformance target

The Windows run needs a test target that draws the variant
patterns and logs the `FileOracle` line protocol. No Windows
test target exists yet — it is the first thing to write on
the machine. Options:

1. A small Win32 target in Rust (a window proc that draws
   with GDI and logs GEOM/READY/BUTTON/KEY/COLOR lines to a
   file, polling a control file for the variant) — the same
   arrangement as `testwin`. Preferred: same log format,
   same suite, no new parser.
2. A C# / WinForms target doing the same. Acceptable if
   faster on the machine; the log format must match exactly.

Whichever is built, it goes in this package as source + build
instructions, and the runner mode is
`conformance --body windows --log <target-log> --control
<target-control-file>` (to be added to the runner alongside
the target — the `x11`/`macos` arms are the template).

`run-conformance.ps1` in this package is the runner script
once the target exists: it builds, starts the target, runs
the suite twice, and takes three screenshots. It has never
run; fix it on the machine and record the fixes.

## 3. Expected result

Both runs must print `conformance PASS: 16/16`, and G1
(self-exclusion) + G2 (focus check) must pass — the Windows
body implements focus verification via the foreground window.
Any failure is a real finding, except UIPI-related refusals
when the target runs elevated (see §5).

## 4. Screenshot color verification (mechanical)

`run-conformance.ps1` writes `shot-red.ppm` (variant 0),
`shot-green.ppm` (variant 1), and `shot-pattern.ppm`
(variant 3). Read pixels from the PPMs the same way
`mac-verify/README.md` §3 does, using the GEOM line for the
target's real position (the window manager may place the
window anywhere; do NOT assume (100,100)):

- Red shot center: ~(255, 0, 0). Green shot center: ~(0, 255, 0).
- Pattern quadrants: TL red, TR green, BL blue, BR yellow.

If red reads back blue, the DIB→RGBA assumption is wrong on
this machine: stop, record it, fix the conversion.

## 5. UIPI check (by hand)

1. Run the target UNELEVATED, the runner UNELEVATED: the
   suite passes.
2. Run the target ELEVATED (Run as administrator), the
   runner UNELEVATED: input to the target must fail with
   typed `UipiBlocked` (or the OS blocks it and the oracle
   detects the missing input — record which). Screenshots
   must still work.
3. Record the integrity-level probe's behavior: does the
   probe read the target's level, or does it fall back to
   "undeterminable → allow"? (Add a debug print if needed;
   remove it before committing.)

Do NOT "fix" UIPI by running everything as administrator:
elevated is an explicit opt-in mode with a warning (D8), and
the uiAccess + signing path is the main path — both are the
user's decisions, not this package's.

## 6. DPI check (by hand)

Set display scaling to 125% or 150% and re-run the suite.
Clicks must land where the suite aimed (the body converts
framebuffer pixels to normalized absolute coordinates using
the virtual screen size). A systematic offset means the DPI
handling is wrong: record the offset, in pixels, with the
scaling setting.

## 7. What to send back

The whole `windows-verify/out/` directory, plus:

- the machine configuration output from step 0,
- the test target source + build instructions, as a diff,
- answers to CHECKLIST.md (each item pass/fail + evidence),
- any fix applied to the Rust code, as a diff.

Do not edit `README.md` or `assets/` in the repo as part of
this verification.
