# Maturana Windows installer. Idempotent leaf adapter: the lifecycle logic
# lives in the Rust CLI (`maturana service`, `maturana pipelock`); this script
# only bootstraps the toolchain, builds, and hands over.
#
#   irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install.ps1 | iex
#
$ErrorActionPreference = "Stop"

$RepoUrl = if ($env:MATURANA_REPO_URL) { $env:MATURANA_REPO_URL } else { "https://github.com/ajensenwaud/maturana.git" }
$Dest = if ($env:MATURANA_DIR) { $env:MATURANA_DIR } else { Join-Path $env:USERPROFILE "maturana" }

function Say($msg) { Write-Host "[maturana] $msg" -ForegroundColor Cyan }

# 1. Base dependencies (winget is present on Win10 21H2+).
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Say "installing git"
    winget install --id Git.Git -e --accept-source-agreements --accept-package-agreements
    $env:PATH = [Environment]::GetEnvironmentVariable("PATH", "Machine") + ";" + [Environment]::GetEnvironmentVariable("PATH", "User")
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path (Join-Path $cargoBin "cargo.exe")) {
        $env:PATH = "$cargoBin;$env:PATH"
    } else {
        Say "installing rustup (MSVC toolchain; Visual Studio Build Tools required)"
        winget install --id Rustlang.Rustup -e --accept-source-agreements --accept-package-agreements
        $env:PATH = "$cargoBin;$env:PATH"
    }
}

# 2. Source: clone or update.
if (Test-Path (Join-Path $Dest ".git")) {
    Say "updating $Dest"
    git -C $Dest pull --ff-only
} elseif (Test-Path $Dest) {
    Say "$Dest exists without git metadata; leaving source as-is"
} else {
    Say "cloning into $Dest"
    git clone $RepoUrl $Dest
}

# 3. Build.
Say "building (release)"
cargo build --release --manifest-path (Join-Path $Dest "Cargo.toml") -p maturana-cli
$Exe = Join-Path $Dest "target\release\maturana.exe"

# 4. Initialize + register services (Rust owns the logic).
Set-Location $Dest
& $Exe pipelock init 2>$null | Out-Null
Say "registering services (maturana up + maturana web)"
& $Exe service install up web

# Optional: the privileged Hyper-V hostd daemon has its own elevated installer.
Say "hostd (Hyper-V VM control) is separate: scripts/install-hostd-task.ps1 (elevated)"

# 5. Orientation: both control surfaces are equals.
Say "install complete"
Write-Host ""
Write-Host "  Two ways to drive Maturana (pick either, or both):"
Write-Host "    1. Codex CLI control plane:  cd $Dest; codex"
Write-Host "       (AGENTS.md + skills/ are the contract that orients it)"
Write-Host "    2. Web cockpit:              http://$($env:COMPUTERNAME):47836"
Write-Host "       token: $Dest\.maturana\web\token"
Write-Host ""
if (-not (Get-Command codex -ErrorAction SilentlyContinue)) {
    Write-Host "  note: codex CLI not found - install with: npm install -g @openai/codex"
}
