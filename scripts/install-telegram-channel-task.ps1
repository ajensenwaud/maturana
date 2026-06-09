param(
    [string]$TaskName = "MaturanaTelegramChannel",
    [string]$AgentId = "codex-demo",
    [string]$SessionId = "telegram-main",
    [string]$TokenSource = "pipelock:telegram/bot-token",
    [string]$LogPath = "",
    [string]$ErrPath = "",
    [switch]$StartOnly,
    [switch]$NoRegister
)

$ErrorActionPreference = "Stop"

function Quote-Argument {
    param([string]$Value)
    if ($Value -notmatch '[\s"]') {
        return $Value
    }
    return '"' + ($Value -replace '"', '\"') + '"'
}

function Get-ExistingRunner {
    param([string]$AgentId)
    Get-CimInstance Win32_Process |
        Where-Object {
            $_.CommandLine -like "*maturana.exe*channel serve telegram*" -and
            $_.CommandLine -like "*--agent-id $AgentId*"
        } |
        Select-Object -First 1
}

function Start-ChannelProcess {
    param(
        [string]$Exe,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        [string]$LogPath,
        [string]$ErrPath,
        [string]$PidPath
    )
    $existing = Get-ExistingRunner -AgentId $AgentId
    if ($existing) {
        Set-Content -LiteralPath $PidPath -Value $existing.ProcessId -NoNewline
        Write-Host "Telegram channel already running pid=$($existing.ProcessId)"
        return
    }
    $process = Start-Process -FilePath $Exe -ArgumentList $Arguments -WorkingDirectory $WorkingDirectory -RedirectStandardOutput $LogPath -RedirectStandardError $ErrPath -WindowStyle Hidden -PassThru
    Set-Content -LiteralPath $PidPath -Value $process.Id -NoNewline
    Write-Host "Started Telegram channel pid=$($process.Id)"
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
    $safeTaskName = $TaskName -replace '[^A-Za-z0-9_.-]', '-'
    $cmdPath = Join-Path $startupDir "$safeTaskName.cmd"
    $quotedArgs = ($Arguments | ForEach-Object { Quote-Argument $_ }) -join " "
    $content = @(
        "@echo off",
        "cd /d ""$WorkingDirectory""",
        "start ""$TaskName"" /min ""$Exe"" $quotedArgs >> ""$LogPath"" 2>> ""$ErrPath"""
    ) -join [Environment]::NewLine
    Set-Content -LiteralPath $cmdPath -Value $content -Encoding ASCII
    Write-Host "Installed Startup fallback: $cmdPath"
}

$script:RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$safeAgentId = $AgentId -replace '[^A-Za-z0-9_.-]', '-'
if ($TaskName -eq "MaturanaTelegramChannel") {
    $TaskName = "MaturanaTelegramChannel-$safeAgentId"
}
$exe = Join-Path $script:RepoRoot "target\x86_64-pc-windows-msvc\debug\maturana.exe"
if (!(Test-Path -LiteralPath $exe)) {
    throw "maturana.exe not found at $exe. Run scripts\build-windows-msvc.ps1 first."
}

if ([string]::IsNullOrWhiteSpace($LogPath)) {
    $LogPath = Join-Path $script:RepoRoot ".maturana\logs\telegram-channel-$safeAgentId.out.log"
}
if ([string]::IsNullOrWhiteSpace($ErrPath)) {
    $ErrPath = Join-Path $script:RepoRoot ".maturana\logs\telegram-channel-$safeAgentId.err.log"
}
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $LogPath) | Out-Null
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $ErrPath) | Out-Null
$pidPath = Join-Path $script:RepoRoot ".maturana\agents\$safeAgentId\channels\telegram\runner.pid"
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $pidPath) | Out-Null

$channelArgs = @(
    "channel", "serve", "telegram",
    "--agent-id", $AgentId,
    "--session-id", $SessionId,
    "--token-source", $TokenSource
)
if ($NoRegister) {
    & $exe @channelArgs
    exit $LASTEXITCODE
}

if ($StartOnly) {
    Start-ChannelProcess -Exe $exe -Arguments $channelArgs -WorkingDirectory $script:RepoRoot -LogPath $LogPath -ErrPath $ErrPath -PidPath $pidPath
    return
}

$quotedExe = Quote-Argument $exe
$quotedArgs = ($channelArgs | ForEach-Object { Quote-Argument $_ }) -join " "
$quotedLogPath = Quote-Argument $LogPath
$quotedErrPath = Quote-Argument $ErrPath
$argument = "-NoProfile -ExecutionPolicy Bypass -Command `"& $quotedExe $quotedArgs >> $quotedLogPath 2>> $quotedErrPath`""

$action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $argument -WorkingDirectory $script:RepoRoot
$trigger = New-ScheduledTaskTrigger -AtLogOn
$principal = New-ScheduledTaskPrincipal -UserId ([Security.Principal.WindowsIdentity]::GetCurrent().Name) -LogonType Interactive
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)

try {
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
    Start-ScheduledTask -TaskName $TaskName
} catch {
    Write-Warning "Could not register scheduled task ($($_.Exception.Message)); starting channel process directly."
    Install-StartupFallback -Exe $exe -Arguments $channelArgs -WorkingDirectory $script:RepoRoot -LogPath $LogPath -ErrPath $ErrPath
    Start-ChannelProcess -Exe $exe -Arguments $channelArgs -WorkingDirectory $script:RepoRoot -LogPath $LogPath -ErrPath $ErrPath -PidPath $pidPath
    Write-Host "Run this script from an elevated shell later to install $TaskName as a persistent scheduled task."
    return
}

Write-Host "Installed and started $TaskName"
Write-Host "Agent: $AgentId"
if (![string]::IsNullOrWhiteSpace($Ip)) {
    Write-Host "VM IP: $Ip"
}
Write-Host "Out log: $LogPath"
Write-Host "Err log: $ErrPath"
