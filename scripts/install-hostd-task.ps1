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
$scriptPath = Join-Path $repoRoot "scripts\maturana-hostd.ps1"
if ([string]::IsNullOrWhiteSpace($TokenPath)) {
    $TokenPath = Join-Path $repoRoot ".maturana\hostd\token"
}
if ([string]::IsNullOrWhiteSpace($LogPath)) {
    $LogPath = Join-Path $repoRoot ".maturana\logs\hostd.log"
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

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$userId = $identity.Name
if ($userId -notmatch '\\' -and ![string]::IsNullOrWhiteSpace($env:USERDOMAIN)) {
    $userId = "$env:USERDOMAIN\$env:USERNAME"
}
$argument = @(
    "-NoProfile",
    "-ExecutionPolicy", "Bypass",
    "-File", (ConvertTo-ProcessArgument $scriptPath),
    "-BindPrefix", (ConvertTo-ProcessArgument $BindPrefix),
    "-TokenPath", (ConvertTo-ProcessArgument $TokenPath),
    "-LogPath", (ConvertTo-ProcessArgument $LogPath)
) -join " "

$action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $argument -WorkingDirectory $repoRoot
$trigger = New-ScheduledTaskTrigger -AtLogOn
$principal = New-ScheduledTaskPrincipal -UserId $userId -RunLevel Highest -LogonType Interactive
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)

Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
Start-ScheduledTask -TaskName $TaskName

if (!(Wait-HostdHealth -Url $BindPrefix -TimeoutSeconds 30)) {
    throw "Started $TaskName, but hostd health did not become reachable at $($BindPrefix.TrimEnd('/'))/health. See $LogPath."
}

Write-Host "Installed and started $TaskName"
Write-Host "Health: $($BindPrefix.TrimEnd('/'))/health"
Write-Host "Token: $TokenPath"
Write-Host "Log: $LogPath"
