param(
    [string]$TaskName = "MaturanaHostd",
    [string]$BindPrefix = "http://127.0.0.1:47832/",
    [string]$TokenPath = "",
    [string]$LogPath = "",
    [switch]$NoElevate,
    [switch]$ElevatedChild
)

$ErrorActionPreference = "Stop"

function Test-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function ConvertTo-ProcessArgument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') {
        return $Value
    }
    return '"' + ($Value -replace '"', '\"') + '"'
}

function Wait-HostdHealth {
    param(
        [string]$Url,
        [int]$TimeoutSeconds = 30
    )
    $deadline = [DateTimeOffset]::Now.AddSeconds($TimeoutSeconds)
    do {
        try {
            $response = Invoke-RestMethod -Method Get -Uri "$($Url.TrimEnd('/'))/health" -TimeoutSec 2
            if ($response.ok) {
                return $true
            }
        } catch {
            Start-Sleep -Seconds 1
        }
    } while ([DateTimeOffset]::Now -lt $deadline)
    return $false
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$exePath = Join-Path $repoRoot "target\x86_64-pc-windows-gnu\debug\maturana.exe"
if ([string]::IsNullOrWhiteSpace($TokenPath)) {
    $TokenPath = Join-Path $repoRoot ".maturana\hostd\token"
}
if ([string]::IsNullOrWhiteSpace($LogPath)) {
    $LogPath = Join-Path $repoRoot ".maturana\logs\hostd.log"
}
if (!(Test-Path -LiteralPath $exePath)) {
    $buildScript = Join-Path $repoRoot "scripts\build-windows-gnu.ps1"
    if (!(Test-Path -LiteralPath $buildScript)) {
        throw "maturana.exe is missing and $buildScript was not found."
    }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $buildScript
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to build maturana.exe before installing hostd."
    }
}

if (-not (Test-Elevated)) {
    if ($NoElevate) {
        throw "Run this from an elevated PowerShell session, or omit -NoElevate to allow a one-time UAC prompt."
    }

    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LogPath) | Out-Null
    Write-Host "Requesting one-time UAC elevation to install $TaskName..."
    Write-Host "If the UAC prompt is not visible, open PowerShell as Administrator and run:"
    Write-Host "  powershell -NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -NoElevate"
    $childArgs = @(
        "-NoProfile",
        "-ExecutionPolicy", "Bypass",
        "-File", $PSCommandPath,
        "-TaskName", $TaskName,
        "-BindPrefix", $BindPrefix,
        "-TokenPath", $TokenPath,
        "-LogPath", $LogPath,
        "-ElevatedChild"
    ) | ForEach-Object { ConvertTo-ProcessArgument $_ }

    $process = Start-Process powershell.exe -Verb RunAs -WindowStyle Hidden -ArgumentList ($childArgs -join " ") -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        throw "Elevated hostd install failed with exit code $($process.ExitCode). See $LogPath."
    }

    if (!(Wait-HostdHealth -Url $BindPrefix -TimeoutSeconds 30)) {
        throw "Installed $TaskName, but hostd health did not become reachable at $($BindPrefix.TrimEnd('/'))/health. See $LogPath."
    }

    Write-Host "Installed and started $TaskName"
    Write-Host "Health: $($BindPrefix.TrimEnd('/'))/health"
    Write-Host "Token: $TokenPath"
    Write-Host "Log: $LogPath"
    return
}

$argument = @(
    "hostd",
    "serve",
    "--bind-prefix", (ConvertTo-ProcessArgument $BindPrefix),
    "--token-path", (ConvertTo-ProcessArgument $TokenPath),
    "--log-path", (ConvertTo-ProcessArgument $LogPath)
) -join " "

$action = New-ScheduledTaskAction -Execute $exePath -Argument $argument -WorkingDirectory $repoRoot
# Zero-touch reboot recovery: hostd starts at boot as SYSTEM (no stored password
# needed - it binds 127.0.0.1 and reads its own token), so it's up before any
# interactive login.
$trigger = New-ScheduledTaskTrigger -AtStartup
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -RunLevel Highest -LogonType ServiceAccount
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)

Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null

# hostd now runs as SYSTEM, but the hostd token is a shared secret: the
# user-context cockpit (maturana up/web, running as the installing user) also
# reads it to authenticate to hostd. The token may have been created with an
# ACL restricted to a single identity (older builds did `icacls /inheritance:r`
# granting only the user) - which would deny SYSTEM and make hostd exit before
# it can even log. Grant read to BOTH SYSTEM (*S-1-5-18, locale-independent)
# and the installing user so either side can read it. New ACEs are added, not
# replaced, so this is safe and idempotent.
$tokenDir = Split-Path -Parent $TokenPath
if (Test-Path -LiteralPath $tokenDir) {
    & icacls $tokenDir /grant "*S-1-5-18:(OI)(CI)R" "${env:USERNAME}:(OI)(CI)R" 2>&1 | Out-Null
}
if (Test-Path -LiteralPath $TokenPath) {
    & icacls $TokenPath /grant "*S-1-5-18:R" "${env:USERNAME}:R" 2>&1 | Out-Null
}

Start-ScheduledTask -TaskName $TaskName

if (!(Wait-HostdHealth -Url $BindPrefix -TimeoutSeconds 30)) {
    throw "Started $TaskName, but hostd health did not become reachable at $($BindPrefix.TrimEnd('/'))/health. See $LogPath."
}

Write-Host "Installed and started $TaskName"
Write-Host "Health: $($BindPrefix.TrimEnd('/'))/health"
Write-Host "Token: $TokenPath"
Write-Host "Log: $LogPath"
