param(
    [switch]$SkipImage,
    [switch]$ForceImage,
    [switch]$SkipHostd,
    [switch]$SkipServices,
    # The current user's Windows password. Needed to register the up/web boot
    # tasks with logon type Password so they run at boot WITHOUT an interactive
    # login (codex/claude auth lives in the user profile). Prompted securely if
    # omitted. Windows stores it in the LSA vault, never on disk.
    [System.Security.SecureString]$WindowsPassword,
    # Path to a prebuilt maturana.exe (set by bootstrap.ps1). When provided, the
    # whole install runs the signed release binary and skips the local Rust/MSYS2
    # build entirely.
    [string]$MaturanaBin
)

$ErrorActionPreference = "Stop"

# Make the prebuilt binary visible to the child scripts (maturana.ps1,
# install-hostd-task.ps1) which check MATURANA_BIN to skip building.
if ($MaturanaBin -and (Test-Path -LiteralPath $MaturanaBin)) {
    $env:MATURANA_BIN = (Resolve-Path -LiteralPath $MaturanaBin).Path
}

# Registering the up/web boot tasks (logon type Password, -AtStartup, -RunLevel
# Highest) writes a credential into the LSA vault and therefore REQUIRES an
# elevated session - as does the Hyper-V VM autostart step (Get-VM/Set-VM). Rather
# than make the user open an admin shell, self-elevate once via UAC up front. This
# also covers hostd (its installer's own elevation becomes a no-op), so the whole
# install needs a single UAC prompt. (On Win11 you could equivalently run
# `sudo .\scripts\install-windows.ps1`; self-elevation makes that optional.)
if (-not $SkipServices) {
    $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $isAdmin) {
        Write-Host "This installer needs admin to register the boot tasks + Hyper-V VM autostart."
        Write-Host "Requesting elevation (UAC). The password prompt will be in the new window..."
        # Re-launch elevated. The password is prompted inside the elevated window
        # (a SecureString can't safely cross the UAC process boundary), so we
        # forward only the switches. -NoExit keeps the window open to read output.
        $fwd = @()
        if ($SkipImage)  { $fwd += '-SkipImage' }
        if ($ForceImage) { $fwd += '-ForceImage' }
        if ($SkipHostd)  { $fwd += '-SkipHostd' }
        if ($env:MATURANA_BIN) { $fwd += @('-MaturanaBin', $env:MATURANA_BIN) }
        $launchArgs = @('-NoExit','-NoProfile','-ExecutionPolicy','Bypass','-File', $PSCommandPath) + $fwd
        try {
            Start-Process powershell.exe -Verb RunAs -ArgumentList $launchArgs | Out-Null
        } catch {
            throw "Elevation was declined. Re-run from an elevated PowerShell, run 'sudo .\scripts\install-windows.ps1', or pass -SkipServices."
        }
        Write-Host "Elevated installer launched in a new window - finish the password prompt there. You can close this window."
        return
    }
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$imagePath = Join-Path $repoRoot ".maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx"

Push-Location $repoRoot
try {
    if ($env:MATURANA_BIN) {
        Write-Host "Using prebuilt maturana binary: $env:MATURANA_BIN"
    } else {
        Write-Host "Building maturana CLI with the Windows GNU toolchain..."
    }
    & .\scripts\maturana.ps1 --help | Out-Null

    Write-Host "Preparing agent SSH key..."
    & .\scripts\maturana.ps1 repair ssh-key

    if (!$SkipImage) {
        if ($ForceImage -or !(Test-Path -LiteralPath $imagePath)) {
            Write-Host "Preparing official Ubuntu Hyper-V image..."
            $imageArgs = @()
            if ($ForceImage) {
                $imageArgs += "--force"
            }
            & .\scripts\maturana.ps1 repair ubuntu-cloudimg @imageArgs
        } else {
            Write-Host "Using existing Ubuntu Hyper-V image: $imagePath"
        }
    }

    if (!$SkipHostd) {
        Write-Host "Installing privileged host daemon. Windows may show one UAC prompt."
        & .\scripts\install-hostd-task.ps1
    }

    if (!$SkipServices) {
        # Remove stale Startup-folder launchers from the OLD per-logon approach
        # (MaturanaSessiond.cmd / MaturanaTelegramChannel*.cmd). They start a
        # second, --home-less plane at logon that grabs sessiond's port 47834 and
        # races the MaturanaUp boot task (-> up's critical sessiond dies with
        # address-in-use). The scheduled-task model supersedes them.
        $startupDir = [Environment]::GetFolderPath('Startup')
        Get-ChildItem -Path $startupDir -Filter 'Maturana*.cmd' -ErrorAction SilentlyContinue | ForEach-Object {
            Write-Host "Removing stale startup launcher: $($_.Name)"
            Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue
        }

        # Register the up + web boot tasks. They run at startup as the current
        # user via a stored password (zero-touch reboot recovery, no login).
        if (-not $WindowsPassword) {
            Write-Host "Enter your Windows password (for boot tasks that run without login):"
            $WindowsPassword = Read-Host -AsSecureString
        }
        $plainPw = [System.Net.NetworkCredential]::new("", $WindowsPassword).Password
        if ([string]::IsNullOrEmpty($plainPw)) {
            throw "A Windows password is required to register boot services (or pass -SkipServices)."
        }
        Write-Host "Registering Maturana services (up + web) for boot..."
        try {
            & .\scripts\maturana.ps1 service install up web --windows-password $plainPw
        }
        finally {
            $plainPw = $null
            [System.GC]::Collect()
        }
        # Make the Hyper-V agent VMs auto-boot with the host too.
        & .\scripts\set-vm-autostart.ps1
    }

    # Put `maturana` on PATH (prebuilt-binary install) so `maturana --help` works.
    if ($env:MATURANA_BIN) {
        $binDir = Split-Path -Parent $env:MATURANA_BIN
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if (($userPath -split ';') -notcontains $binDir) {
            [Environment]::SetEnvironmentVariable('Path', (($userPath.TrimEnd(';')) + ';' + $binDir), 'User')
        }
        $env:Path = "$binDir;$env:Path"
    }

    # Harness credential pre-check: agents can't run without an authenticated
    # harness, so check codex + claude (CLI present? logged in?) and guide.
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
    $token = Get-Content (Join-Path $repoRoot ".maturana\web\token") -ErrorAction SilentlyContinue | Select-Object -First 1

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
    Write-Host "     cd `"$repoRoot`""
    Write-Host "     codex"
    Write-Host "   then ask Codex: ""create and launch a new agent""."
    Write-Host ""
    Write-Host "Web cockpit:  http://localhost:47836"
    if ($token) { Write-Host "     token:  $token" } else { Write-Host "     token:  (run: maturana web token)" }
    Write-Host ""
    Write-Host "Help:  maturana --help        (open a new terminal first)"
    Write-Host "Docs:  $repoRoot\docs"
    Write-Host "========================================================"
}
finally {
    Pop-Location
}
