# Maturana Windows installer - the single entry point.
#
#   irm https://www.maturana.sh/install.ps1 | iex
#
# Downloads the signed prebuilt maturana.exe from the latest GitHub Release (no
# Rust toolchain needed), clones the repo for skills/AGENTS.md/scripts/examples,
# then does the full host setup: hostd (Hyper-V control), the Ubuntu Hyper-V
# image, and the up/web boot services that survive a reboot without an
# interactive login. Self-elevates once (one UAC prompt) and asks for your
# Windows password, which Windows stores in the LSA vault (never on disk) so the
# boot tasks can run as you without a login.
#
# Flags (all optional):
#   -FromSource     build with the Rust MSVC toolchain instead of downloading
#   -SkipImage      don't prepare the Ubuntu Hyper-V image
#   -ForceImage     rebuild the Ubuntu image even if present
#   -SkipHostd      don't install the privileged hostd task
#   -SkipServices   don't register boot services (no password needed, no reboot recovery)
#   -CodexPrompts / -NoCodexPrompts   install skills as Codex skills (default: ask)
param(
    [switch]$FromSource,
    [switch]$SkipImage,
    [switch]$ForceImage,
    [switch]$SkipHostd,
    [switch]$SkipServices,
    [switch]$CodexPrompts,
    [switch]$NoCodexPrompts
)
$ErrorActionPreference = "Stop"

# Env-var overrides double as the way switches cross the UAC boundary on the
# irm|iex path (where there is no script file to forward -Args to).
if ($env:MATURANA_FROM_SOURCE     -eq '1') { $FromSource = $true }
if ($env:MATURANA_SKIP_IMAGE      -eq '1') { $SkipImage = $true }
if ($env:MATURANA_FORCE_IMAGE     -eq '1') { $ForceImage = $true }
if ($env:MATURANA_SKIP_HOSTD      -eq '1') { $SkipHostd = $true }
if ($env:MATURANA_SKIP_SERVICES   -eq '1') { $SkipServices = $true }
if ($env:MATURANA_CODEX_PROMPTS   -eq '1') { $CodexPrompts = $true }
if ($env:MATURANA_NO_CODEX_PROMPTS -eq '1') { $NoCodexPrompts = $true }

$RepoUrl = if ($env:MATURANA_REPO_URL) { $env:MATURANA_REPO_URL } else { "https://github.com/ajensenwaud/maturana.git" }
$Dir     = if ($env:MATURANA_DIR) { $env:MATURANA_DIR } else { Join-Path $env:USERPROFILE "maturana" }
$Ref     = if ($env:MATURANA_REF) { $env:MATURANA_REF } else { "main" }
$RelBase = "https://github.com/ajensenwaud/maturana/releases/latest/download"
$Asset   = "maturana-x86_64-pc-windows-msvc.zip"

function Say($m) { Write-Host "[maturana] $m" -ForegroundColor Cyan }

# --- Self-elevate once, up front. The privileged steps (boot tasks written to
# the LSA vault, hostd as SYSTEM, Hyper-V VM autostart) need admin; running the
# whole install elevated keeps it to a single UAC prompt. Skip with -SkipServices.
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $SkipServices -and -not $isAdmin) {
    Say "requesting elevation (UAC) for the privileged setup..."
    if ($PSCommandPath) {
        $fwd = @()
        if ($FromSource)     { $fwd += '-FromSource' }
        if ($SkipImage)      { $fwd += '-SkipImage' }
        if ($ForceImage)     { $fwd += '-ForceImage' }
        if ($SkipHostd)      { $fwd += '-SkipHostd' }
        if ($CodexPrompts)   { $fwd += '-CodexPrompts' }
        if ($NoCodexPrompts) { $fwd += '-NoCodexPrompts' }
        $launch = @('-NoExit','-NoProfile','-ExecutionPolicy','Bypass','-File', $PSCommandPath) + $fwd
    } else {
        # irm|iex: no file on disk, so re-fetch elevated; switches cross via env.
        $pf = ""
        if ($FromSource)     { $pf += "`$env:MATURANA_FROM_SOURCE=1; " }
        if ($SkipImage)      { $pf += "`$env:MATURANA_SKIP_IMAGE=1; " }
        if ($ForceImage)     { $pf += "`$env:MATURANA_FORCE_IMAGE=1; " }
        if ($SkipHostd)      { $pf += "`$env:MATURANA_SKIP_HOSTD=1; " }
        if ($CodexPrompts)   { $pf += "`$env:MATURANA_CODEX_PROMPTS=1; " }
        if ($NoCodexPrompts) { $pf += "`$env:MATURANA_NO_CODEX_PROMPTS=1; " }
        if ($env:MATURANA_DIR) { $pf += "`$env:MATURANA_DIR='$($env:MATURANA_DIR)'; " }
        # Re-fetch from the raw URL (200), NOT the maturana.sh vanity URL: the
        # vanity URL is a redirect and Windows PowerShell's irm won't follow a
        # 307/308, which would break the elevated re-fetch.
        $launch = @('-NoExit','-NoProfile','-ExecutionPolicy','Bypass','-Command',
                    "$pf irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install.ps1 | iex")
    }
    try { Start-Process powershell.exe -Verb RunAs -ArgumentList $launch | Out-Null }
    catch { throw "Elevation was declined. Re-run from an elevated PowerShell, or pass -SkipServices to install without boot services." }
    Say "elevated installer launched in a new window - continue there (the password prompt is in that window). You can close this one."
    return
}

# ============ everything below runs elevated (or under -SkipServices) ============

# 1. git: needed for the repo (skills/, AGENTS.md, scripts/, examples/).
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Say "installing git (winget)"
    winget install --id Git.Git -e --source winget --accept-source-agreements --accept-package-agreements
    $env:PATH = "$env:ProgramFiles\Git\cmd;$env:PATH"
}
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    throw "git is required. Install it (https://git-scm.com/download/win) and re-run."
}

# 2. Clone or update the repo at $Dir.
if (Test-Path -LiteralPath (Join-Path $Dir ".git")) {
    Say "updating $Dir"
    git -C $Dir fetch --depth 1 origin $Ref
    git -C $Dir checkout $Ref
    git -C $Dir reset --hard "origin/$Ref"
} else {
    Say "cloning into $Dir"
    git clone --depth 1 --branch $Ref $RepoUrl $Dir
}
Set-Location $Dir

# --- Structured progression log (logs/setup.log): one append-only file with a
# header, a per-step line (name + duration + status), and a footer. The terminal
# stays the level-1 summary; this is the level-2 record you'd paste when setup
# misbehaves. Pure logging — it never changes control flow.
$script:SetupLog = Join-Path $Dir "logs\setup.log"
New-Item -ItemType Directory -Force -Path (Split-Path $SetupLog) | Out-Null
"## $(Get-Date -Format o) - maturana install started (dir=$Dir ref=$Ref from_source=$FromSource)" |
    Out-File -FilePath $SetupLog -Append -Encoding utf8
$script:StepClock = [System.Diagnostics.Stopwatch]::StartNew()
function Log-Milestone($name, $status = 'success') {
    $secs = '{0:n1}' -f $script:StepClock.Elapsed.TotalSeconds
    "=== [$(Get-Date -Format o)] $name [${secs}s] -> $status ===" |
        Out-File -FilePath $script:SetupLog -Append -Encoding utf8
    $script:StepClock.Restart()
}

# 3. Obtain the maturana binary: prebuilt download (default) or build (-FromSource).
$binDir = Join-Path $Dir "bin"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
$Exe = Join-Path $binDir "maturana.exe"
if (-not $FromSource) {
    $zip  = Join-Path $env:TEMP $Asset
    $sums = Join-Path $env:TEMP "maturana-SHA256SUMS"
    Say "downloading prebuilt $Asset"
    try {
        Invoke-WebRequest -Uri "$RelBase/$Asset" -OutFile $zip -UseBasicParsing
        Invoke-WebRequest -Uri "$RelBase/SHA256SUMS" -OutFile $sums -UseBasicParsing
        $want = ((Get-Content $sums | Where-Object { $_ -match [regex]::Escape($Asset) } | Select-Object -First 1) -replace '\s.*$','')
        $got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if (-not $want) { throw "no SHA256 entry for $Asset" }
        if ($got -ne $want.ToLower()) { throw "checksum mismatch (want $want, got $got)" }
        Say "checksum OK"
        Expand-Archive -Path $zip -DestinationPath $binDir -Force
        Remove-Item $zip, $sums -Force -ErrorAction SilentlyContinue
        $sig = (Get-AuthenticodeSignature $Exe).Status
        if ($sig -eq "Valid") { Say "Authenticode signature: valid" } else { Say "Authenticode: $sig (release not code-signed yet)" }
    } catch {
        Say "prebuilt download failed ($($_.Exception.Message)); falling back to source build"
        $FromSource = $true
    }
}
if ($FromSource) {
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
    Say "building (release)"
    cargo build --release --manifest-path (Join-Path $Dir "Cargo.toml") -p maturana-cli
    Copy-Item (Join-Path $Dir "target\release\maturana.exe") $Exe -Force
}
if (-not (Test-Path -LiteralPath $Exe)) { throw "maturana.exe missing after install" }
$env:MATURANA_BIN = $Exe
Log-Milestone ("binary ({0})" -f $(if ($FromSource) { 'built' } else { 'prebuilt' }))

# Put bin on PATH (User scope) so `maturana` works in new shells.
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $binDir) {
    [Environment]::SetEnvironmentVariable('Path', (($userPath.TrimEnd(';')) + ';' + $binDir), 'User')
}
$env:Path = "$binDir;$env:Path"

# 4. Initialize the pipelock vault (idempotent).
& $Exe pipelock init 2>$null | Out-Null

# 5. Agent SSH key + Ubuntu Hyper-V image.
Say "preparing agent SSH key"
& $Exe setup ssh-key
$imagePath = Join-Path $Dir ".maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx"
if (-not $SkipImage) {
    if ($ForceImage -or -not (Test-Path -LiteralPath $imagePath)) {
        # The slowest step by far (download + convert to VHDX). Set the
        # expectation and report elapsed time so it never looks frozen.
        Say "preparing Ubuntu Hyper-V image - one-time, ~3-8 min (downloading + converting)..."
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        if ($ForceImage) { & $Exe setup ubuntu-cloudimg --force } else { & $Exe setup ubuntu-cloudimg }
        $sw.Stop()
        Say ("Ubuntu image ready in {0:n0}s" -f $sw.Elapsed.TotalSeconds)
    } else {
        Say "using existing Ubuntu image: $imagePath"
    }
}
Log-Milestone 'ubuntu-image'

# 6. hostd (privileged Hyper-V control, runs as SYSTEM). Already elevated here.
if (-not $SkipHostd) {
    Say "installing privileged host daemon (hostd)"
    & (Join-Path $Dir "scripts\install-hostd-task.ps1")
    Log-Milestone 'hostd'
} else {
    Log-Milestone 'hostd' 'skipped'
}

# 7. Boot services (up) via a stored password; clear stale launchers; VM autostart.
if (-not $SkipServices) {
    # Remove stale Startup-folder launchers from the old per-logon approach.
    $startupDir = [Environment]::GetFolderPath('Startup')
    Get-ChildItem -Path $startupDir -Filter 'Maturana*.cmd' -ErrorAction SilentlyContinue | ForEach-Object {
        Say "removing stale startup launcher: $($_.Name)"
        Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue
    }
    Write-Host "Enter your Windows password (registers boot tasks that run without login; stored in the LSA vault, never on disk):"
    $sec = Read-Host -AsSecureString
    $pw = [System.Net.NetworkCredential]::new("", $sec).Password
    if ([string]::IsNullOrEmpty($pw)) { throw "A Windows password is required to register boot services (or pass -SkipServices)." }
    Say "registering Maturana boot services (up)"
    try { & $Exe service install up --windows-password $pw }
    finally { $pw = $null; [System.GC]::Collect() }
    # Make existing maturana-* Hyper-V VMs auto-boot with the host (staggered to
    # avoid a boot thundering-herd). New VMs get this from the Hyper-V launcher.
    $autostartVms = @(Get-VM -Name 'maturana-*' -ErrorAction SilentlyContinue)
    $vmIdx = 0
    foreach ($vm in $autostartVms) {
        $delay = 30 + (15 * $vmIdx)
        Set-VM -VM $vm -AutomaticStartAction Start -AutomaticStartDelay $delay
        Say "  $($vm.Name): auto-start on (delay ${delay}s)"
        $vmIdx++
    }
    Log-Milestone 'boot-services'
} else {
    Log-Milestone 'boot-services' 'skipped'
}

# 8. Skills as native Codex skills (~/.agents/skills) vs repo-only. Ask unless told.
$doPrompts = $true
if ($NoCodexPrompts) {
    $doPrompts = $false
} elseif (-not $CodexPrompts) {
    $ans = Read-Host "Install Maturana skills as Codex skills (~/.agents/skills)? [Y/n]"
    if ($ans -match '^(n|no)$') { $doPrompts = $false }
}
if ($doPrompts) {
    try {
        & $Exe skill codex-prompts (Join-Path $Dir 'skills') 2>$null | Out-Null
        Say "skills installed as Codex skills (use /skills or `$<name> in Codex)"
    } catch { Say "could not install Codex skills (they still load via AGENTS.md)" }
} else {
    Say "skills kept in the repo (Codex loads them on demand via AGENTS.md)"
}
Log-Milestone 'skills'
"## $(Get-Date -Format o) - install completed" | Out-File -FilePath $script:SetupLog -Append -Encoding utf8

# 9. Harness credential pre-check + orientation.
function Test-Harness($cli, $authPath, $loginHint, $installHint) {
    if ((Get-Command $cli -ErrorAction SilentlyContinue) -and (Test-Path -LiteralPath $authPath)) {
        return "ready"
    } elseif (Get-Command $cli -ErrorAction SilentlyContinue) {
        return "installed, NOT logged in -> run: $loginHint"
    } else {
        return "missing -> install: $installHint  then: $loginHint"
    }
}
$codexStatus  = Test-Harness 'codex'  "$env:USERPROFILE\.codex\auth.json" 'codex login' 'npm install -g @openai/codex'
$claudeStatus = Test-Harness 'claude' "$env:USERPROFILE\.claude\.credentials.json" 'claude (then /login)' 'npm install -g @anthropic-ai/claude-code'

Write-Host ""
Write-Host "==================== Maturana ready ===================="
Write-Host "A Codex-native agent framework. Build agents from Codex,"
Write-Host "which is oriented by this repo's AGENTS.md + skills/."
Write-Host ""
Write-Host "1) Authenticate a harness (agents need at least one):"
Write-Host "     codex  : $codexStatus"
Write-Host "     claude : $claudeStatus"
Write-Host ""
Write-Host "2) Build your first agent:"
Write-Host "     cd `"$Dir`""
Write-Host "     codex"
Write-Host "   then ask Codex: ""create and launch a new agent"", or invoke a"
Write-Host "   skill directly: type /skills, or `$maturana-agent-create"
Write-Host ""
Write-Host "Web cockpit:  experimental, off by default. To try it once you're ready:"
Write-Host "     maturana service install web   (then: http://localhost:47836)"
Write-Host ""
Write-Host "Help:  maturana --help        (open a new terminal first)"
Write-Host "Docs:  $Dir\docs"
Write-Host "========================================================"
