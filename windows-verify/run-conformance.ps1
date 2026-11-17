# Windows conformance runner (P4.2). Builds, starts the test
# target, runs the suite twice, and takes three screenshots.
#
# PREREQUISITE: a Windows test target (see README.md section 2).
# Set $TargetExe to its path before running. This script has
# NEVER run: the first run on a real machine is expected to
# need fixes — record them, do not hide them.
#
# Usage (from the repo root, in PowerShell):
#   powershell -ExecutionPolicy Bypass -File windows-verify\run-conformance.ps1

$ErrorActionPreference = "Stop"
$Root = (Get-Item $PSScriptRoot).Parent.FullName
$Out = Join-Path $PSScriptRoot "out"
New-Item -ItemType Directory -Force -Path $Out | Out-Null

# TODO(first run): point this at the built test target.
$TargetExe = Join-Path $Out "wintarget.exe"
if (-not (Test-Path $TargetExe)) {
    Write-Error "test target not found at $TargetExe; build it first (see README.md section 2)."
    exit 2
}

$Log = Join-Path $Out "wintarget.log"
$Control = Join-Path $Out "control.txt"
Set-Content -Path $Control -Value "0" -NoNewline

cargo build -p mimint-body --bin conformance
if ($LASTEXITCODE -ne 0) { exit 1 }
$Runner = Join-Path $Root "target\debug\conformance.exe"

$Target = Start-Process -FilePath $TargetExe -ArgumentList "`"$Log`" `"$Control`"" -PassThru -NoNewWindow
try {
    for ($run = 1; $run -le 2; $run++) {
        & $Runner --body windows --log $Log --control $Control `
            > (Join-Path $Out "run$run.txt") 2>&1
        Write-Output "run $run exit: $LASTEXITCODE"
    }

    Set-Content -Path $Control -Value "0" -NoNewline; Start-Sleep -Seconds 1
    & $Runner --body windows --shot (Join-Path $Out "shot-red.ppm") > $null 2>&1
    Set-Content -Path $Control -Value "1" -NoNewline; Start-Sleep -Seconds 1
    & $Runner --body windows --shot (Join-Path $Out "shot-green.ppm") > $null 2>&1
    Set-Content -Path $Control -Value "3" -NoNewline; Start-Sleep -Seconds 1
    & $Runner --body windows --shot (Join-Path $Out "shot-pattern.ppm") > $null 2>&1
}
finally {
    if (-not $Target.HasExited) { Stop-Process -Id $Target.Id -Force }
}

Write-Output "artifacts in $Out"
Get-Content (Join-Path $Out "run*.txt") | Select-String "conformance PASS|conformance FAIL"
