# One local release bar for the Aokie repo (audit XR-01).
#
# Runs the gates a plugin release must pass, in order, propagating the first
# failure. Cheap gates always run; the expensive ones are opt-in flags so they
# never silently self-skip.
#
#   powershell -File scripts/check-release.ps1               # standard gates
#   powershell -File scripts/check-release.ps1 -Companion    # + Companion npm app
#   powershell -File scripts/check-release.ps1 -Audit        # + cargo audit (network)
#   powershell -File scripts/check-release.ps1 -Fmt          # + cargo fmt --check (AK-19)
#   powershell -File scripts/check-release.ps1 -Msrv         # + compile with the pinned MSRV (AK-19)
#   powershell -File scripts/check-release.ps1 -AndroidTarget # + Companion Android target check (AK-19)
#
# Hardware-dependent suites (live radio, dongle-attached) stay out of scope
# here by design — they are labelled and run supervised on the dev machine.
# The self-host container smoke is the selfhost-smoke.yml manual workflow
# (needs Linux + Docker); render-test.sh runs anywhere with sh.

param(
    [switch]$Companion,
    [switch]$Audit,
    [switch]$Fmt,
    [switch]$Msrv,
    [switch]$AndroidTarget
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
Invoke-Gate 'check: ONNX CUDA API and Candle engines' $repo {
    cargo check -p aokie-ai --features onnx-cuda,candle
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

# ── Cross-repo contract digest (audit FL-34) — cheap, always on; fails loudly
# when the formlogic checkout is missing (set FORMLOGIC_REPO) ──
Invoke-Gate 'contracts: cross-repo digest (FL-34)' $repo {
    node scripts/check-contracts.mjs
}

# ── Self-host template rendering (audit AK-14) — cheap, always on ──
Invoke-Gate 'self-host: render acceptance (AK-14)' $repo {
    sh deploy/companion-self-host/render-test.sh
}

# ── Hygiene gates (audit AK-19) — opt-in, each fails loudly when its
# toolchain is missing rather than self-skipping ──
if ($Fmt) {
    Invoke-Gate 'cargo fmt --check' $repo { cargo fmt --check }
} else {
    Write-Host "(skipped: cargo fmt --check — pass -Fmt to include; NOTE the tree carries pre-audit fmt drift)" -ForegroundColor Yellow
}
if ($Msrv) {
    # rust-version pins live in the shipping crates' Cargo.tomls; compiling
    # WITH that toolchain is what detects an accidental MSRV increase.
    Invoke-Gate 'MSRV: plugin (cargo +1.88.0, shipping combo)' $repo {
        cargo +1.88.0 check -p aokie-plugin --features voice,managed-beta-driver
    }
    Invoke-Gate 'MSRV: voice-server + dongle + signer' $repo {
        cargo +1.88.0 check -p aokie-voice-server -p package-signer
        if ($LASTEXITCODE -ne 0) { throw 'MSRV check failed' }
        cargo +1.88.0 check -p aokie-dongle --features managed-beta-driver --bins
    }
} else {
    Write-Host "(skipped: MSRV compile — pass -Msrv to include; requires 'rustup toolchain install 1.88.0')" -ForegroundColor Yellow
}
if ($AndroidTarget) {
    Invoke-Gate 'companion: Android target check' $repo {
        cargo check -p aokie-mobile --target aarch64-linux-android
    }
} else {
    Write-Host "(skipped: Android target compile — pass -AndroidTarget to include; requires the NDK + 'rustup target add aarch64-linux-android')" -ForegroundColor Yellow
}

if ($failed) {
    Write-Host "`ncheck-release: FAILED (see gates above)" -ForegroundColor Red
    exit 1
}
Write-Host "`ncheck-release: all requested gates passed" -ForegroundColor Green
