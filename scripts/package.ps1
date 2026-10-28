# Synso local one-shot packaging (Task 21).
#
# Runs the same gates CI runs, then produces the Windows NSIS installer via
# `npm run tauri build`, and prints the artifact paths at the end.
#
# Usage (from anywhere):  powershell -File scripts\package.ps1
# Needs: rustup stable (fmt/clippy), node >= 20, and `npm ci` already run
# inside crates\synso-app\ui.

$ErrorActionPreference = 'Stop'

# Repo root = the directory that contains this script's parent.
$root = Split-Path -Parent $PSScriptRoot
$app  = Join-Path $root 'crates\synso-app'
$target = Join-Path $root 'target'

# Run one command; abort on the first non-zero exit (the $LASTEXITCODE
# check is the PS 5.1-compatible equivalent of `set -e` for native tools).
function Invoke-Step {
    param([string]$Name, [string]$WorkingDir, [scriptblock]$Command)
    Write-Host "==> $Name" -ForegroundColor Cyan
    Push-Location $WorkingDir
    try {
        & $Command
        if ($LASTEXITCODE -ne 0) {
            throw "$Name failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

# ------------------------------ gates ------------------------------------

Invoke-Step 'cargo fmt --check' $root { cargo fmt --all --check }
Invoke-Step 'cargo clippy (-D warnings)' $root { cargo clippy --workspace --all-targets -- -D warnings }
Invoke-Step 'cargo test (workspace)' $root { cargo test --workspace }

Invoke-Step 'svelte-check' $app { npm run check }
Invoke-Step 'i18n parity check' $app { npm run check:i18n }
Invoke-Step 'vite build' $app { npm run build }

# ------------------------------ bundle ------------------------------------

Invoke-Step 'tauri build (NSIS)' $app { npm run tauri build }

# ------------------------------ artifacts ---------------------------------

$nsisDir = Join-Path $target 'release\bundle\nsis'
$exe     = Join-Path $target 'release\synso-app.exe'

Write-Host ''
Write-Host 'Package OK. Artifacts:' -ForegroundColor Green
if (Test-Path $exe) {
    $size = '{0:N1} MiB' -f ((Get-Item $exe).Length / 1MiB)
    Write-Host "  app exe     : $exe ($size)"
}
if (Test-Path $nsisDir) {
    Get-ChildItem $nsisDir -Filter *.exe | ForEach-Object {
        $size = '{0:N1} MiB' -f ($_.Length / 1MiB)
        Write-Host "  NSIS bundle : $($_.FullName) ($size)"
    }
} else {
    Write-Warning "no NSIS output directory at $nsisDir"
}
