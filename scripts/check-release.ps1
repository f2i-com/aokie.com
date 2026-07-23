# One local release bar for the Aokie repo (audit XR-01).
#
# Runs the gates a plugin release must pass, in order, propagating the first
# failure. Cheap gates always run; the expensive ones are opt-in flags so they
# never silently self-skip.
#
#   powershell -File scripts/check-release.ps1               # standard gates
#   powershell -File scripts/check-release.ps1 -Companion    # + Companion npm app
#   powershell -File scripts/check-release.ps1 -Audit        # + cargo audit (network)
#
# Hardware-dependent suites (live radio, dongle-attached) stay out of scope
# here by design — they are labelled and run supervised on the dev machine.

param(
    [switch]$Companion,
    [switch]$Audit
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$failed = $false

function Invoke-Gate {
    param([string]$Name, [string]$WorkDir, [scriptblock]$Body)
    Write-Host "`n=== $Name ===" -ForegroundColor Cyan
    Push-Location $WorkDir
    try {
        & $Body
        if ($LASTEXITCODE -ne 0) { throw "$Name exited $LASTEXITCODE" }
        Write-Host "=== $Name OK ===" -ForegroundColor Green
    } catch {
        Write-Host "=== $Name FAILED: $_ ===" -ForegroundColor Red
        $script:failed = $true
    } finally {
        Pop-Location
    }
}

# ── Feature-combination compile matrix (the repo's standing rule) ──
Invoke-Gate 'check: default features' $repo { cargo check -p aokie-plugin }
Invoke-Gate 'check: voice' $repo { cargo check -p aokie-plugin --features voice }
Invoke-Gate 'check: voice,managed-beta-driver (shipping combo)' $repo {
    cargo check -p aokie-plugin --features voice,managed-beta-driver
}
Invoke-Gate 'check: driver helper (managed-beta-driver)' $repo {
    cargo check -p aokie-dongle --features managed-beta-driver --bins
}

# ── Tests ──
Invoke-Gate 'test: workspace (default features)' $repo { cargo test --workspace }
# --test-threads=2: the synthetic-audio/QuickJS timing tests are a documented
# flake class under full parallel load.
Invoke-Gate 'test: plugin (voice)' $repo {
    cargo test -p aokie-plugin --features voice -- --test-threads=2
}

# ── Lints ──
Invoke-Gate 'clippy: workspace (deny-level lints)' $repo { cargo clippy --workspace --all-targets }
Invoke-Gate 'clippy: plugin (voice)' $repo {
    cargo clippy -p aokie-plugin --all-targets --features voice
}

# ── Companion app — opt-in (npm toolchain) ──
if ($Companion) {
    Invoke-Gate 'companion: npm ci' (Join-Path $repo 'apps/aokie-mobile') { npm ci }
    Invoke-Gate 'companion: vitest' (Join-Path $repo 'apps/aokie-mobile') { npm test }
    Invoke-Gate 'companion: build (tsc + vite)' (Join-Path $repo 'apps/aokie-mobile') { npm run build }
} else {
    Write-Host "`n(skipped: Companion app — pass -Companion to include)" -ForegroundColor Yellow
}

# ── Supply chain — opt-in (network) ──
if ($Audit) {
    Invoke-Gate 'cargo audit (RustSec)' $repo { cargo audit }
} else {
    Write-Host "(skipped: cargo audit — pass -Audit to include)" -ForegroundColor Yellow
}

if ($failed) {
    Write-Host "`ncheck-release: FAILED (see gates above)" -ForegroundColor Red
    exit 1
}
Write-Host "`ncheck-release: all requested gates passed" -ForegroundColor Green
