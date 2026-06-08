param(
    [string]$TaskName = "MaturanaSessiond",
    [string]$Bind = "0.0.0.0:47834",
    [string]$TokenPath = "",
    [string]$LogPath = "",
    [string]$ErrPath = "",
    [switch]$StartOnly
)

$ErrorActionPreference = "Stop"

function Quote-Argument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') {
        return $Value
    }
    return '"' + ($Value -replace '"', '\"') + '"'
}

function New-Token {
    $bytes = [byte[]]::new(32)
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($bytes)
    } finally {
        $rng.Dispose()
    }
    [Convert]::ToBase64String($bytes)
}

function Start-SessiondProcess {
    param(
        [string]$Exe,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        [string]$LogPath,
        [string]$ErrPath,
        [string]$PidPath
    )
    $existing = Get-CimInstance Win32_Process |
        Where-Object { $_.CommandLine -like "*maturana.exe*session serve*" } |
        Select-Object -First 1
    if ($existing) {
        Set-Content -LiteralPath $PidPath -Value $existing.ProcessId -NoNewline
        Write-Host "Sessiond already running pid=$($existing.ProcessId)"
        return
    }
    $process = Start-Process -FilePath $Exe -ArgumentList $Arguments -WorkingDirectory $WorkingDirectory -RedirectStandardOutput $LogPath -RedirectStandardError $ErrPath -WindowStyle Hidden -PassThru
    Set-Content -LiteralPath $PidPath -Value $process.Id -NoNewline
    Write-Host "Started sessiond pid=$($process.Id)"
}

function Install-StartupFallback {
    param(
        [string]$Exe,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        [string]$LogPath,
        [string]$ErrPath
    )
    $startupDir = [Environment]::GetFolderPath("Startup")
    if ([string]::IsNullOrWhiteSpace($startupDir)) {
        return
    }
    $cmdPath = Join-Path $startupDir "MaturanaSessiond.cmd"
    $quotedArgs = ($Arguments | ForEach-Object { Quote-Argument $_ }) -join " "
    $content = @(
        "@echo off",
        "cd /d ""$WorkingDirectory""",
        "start ""Maturana Sessiond"" /min ""$Exe"" $quotedArgs >> ""$LogPath"" 2>> ""$ErrPath"""
    ) -join [Environment]::NewLine
    Set-Content -LiteralPath $cmdPath -Value $content -Encoding ASCII
    Write-Host "Installed Startup fallback: $cmdPath"
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$exe = Join-Path $repoRoot "target\x86_64-pc-windows-msvc\debug\maturana.exe"
if (!(Test-Path -LiteralPath $exe)) {
    throw "maturana.exe not found at $exe. Run scripts\build-windows-msvc.ps1 first."
}

if ([string]::IsNullOrWhiteSpace($TokenPath)) {
    $TokenPath = Join-Path $repoRoot ".maturana\sessiond\token"
}
if ([string]::IsNullOrWhiteSpace($LogPath)) {
    $LogPath = Join-Path $repoRoot ".maturana\logs\sessiond.out.log"
}
if ([string]::IsNullOrWhiteSpace($ErrPath)) {
    $ErrPath = Join-Path $repoRoot ".maturana\logs\sessiond.err.log"
}
$pidPath = Join-Path $repoRoot ".maturana\sessiond\runner.pid"
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $TokenPath) | Out-Null
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LogPath) | Out-Null
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ErrPath) | Out-Null
if (!(Test-Path -LiteralPath $TokenPath)) {
    Set-Content -LiteralPath $TokenPath -Value (New-Token) -NoNewline
}
$token = (Get-Content -LiteralPath $TokenPath -Raw).Trim()

$args = @("session", "serve", "--bind", $Bind, "--token", $token)
if ($StartOnly) {
    Start-SessiondProcess -Exe $exe -Arguments $args -WorkingDirectory $repoRoot -LogPath $LogPath -ErrPath $ErrPath -PidPath $pidPath
    return
}

$quotedExe = Quote-Argument $exe
$quotedArgs = ($args | ForEach-Object { Quote-Argument $_ }) -join " "
$quotedLogPath = Quote-Argument $LogPath
$quotedErrPath = Quote-Argument $ErrPath
$argument = "-NoProfile -ExecutionPolicy Bypass -Command `"& $quotedExe $quotedArgs >> $quotedLogPath 2>> $quotedErrPath`""
$action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $argument -WorkingDirectory $repoRoot
$trigger = New-ScheduledTaskTrigger -AtLogOn
$principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)

try {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
    Start-ScheduledTask -TaskName $TaskName
    Write-Host "Installed and started $TaskName"
} catch {
    Write-Warning "Could not register scheduled task ($($_.Exception.Message)); starting sessiond directly."
    Install-StartupFallback -Exe $exe -Arguments $args -WorkingDirectory $repoRoot -LogPath $LogPath -ErrPath $ErrPath
    Start-SessiondProcess -Exe $exe -Arguments $args -WorkingDirectory $repoRoot -LogPath $LogPath -ErrPath $ErrPath -PidPath $pidPath
}
Write-Host "Bind: $Bind"
Write-Host "Token: $TokenPath"
Write-Host "Out log: $LogPath"
Write-Host "Err log: $ErrPath"
