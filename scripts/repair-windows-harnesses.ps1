param(
    [string[]]$AgentId = @("codex-demo", "opencode-demo", "claude-demo"),
    [string[]]$SessionId = @("codex-main", "opencode-main", "claude-main"),
    [string[]]$Harness = @("codex", "opencode", "claude-code"),
    [string[]]$HarnessAuthGuestPath = @("/home/ubuntu/.codex", "/home/ubuntu", "/home/ubuntu/.claude"),
    [string[]]$TelegramTokenSource = @(
        "pipelock:telegram/bot-token",
        "pipelock:telegram/opencode-bot-token",
        "pipelock:telegram/claude-bot-token"
    ),
    [switch]$RegisterTasks
)

$ErrorActionPreference = "Stop"
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")

function Stop-MaturanaProcess {
    param([string]$Pattern)
    Get-CimInstance Win32_Process |
        Where-Object { $_.CommandLine -like $Pattern } |
        ForEach-Object {
            Write-Host "Stopping pid=$($_.ProcessId) $($_.CommandLine)"
            Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
        }
}

if ($AgentId.Count -ne $SessionId.Count -or
    $AgentId.Count -ne $Harness.Count -or
    $AgentId.Count -ne $HarnessAuthGuestPath.Count -or
    $AgentId.Count -ne $TelegramTokenSource.Count) {
    throw "AgentId, SessionId, Harness, HarnessAuthGuestPath, and TelegramTokenSource must have the same number of entries."
}

Push-Location $repoRoot
try {
    Stop-MaturanaProcess -Pattern "*maturana.exe*session serve*"
    Stop-MaturanaProcess -Pattern "*maturana.exe*channel serve telegram*"

    if ($RegisterTasks) {
        powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-sessiond-task.ps1
    } else {
        powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\install-sessiond-task.ps1 -StartOnly
    }

    for ($i = 0; $i -lt $AgentId.Count; $i++) {
        powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\refresh-guest-worker.ps1 `
            -AgentId $AgentId[$i] `
            -SessionId $SessionId[$i] `
            -Harness $Harness[$i] `
            -HarnessAuthGuestPath $HarnessAuthGuestPath[$i]

        $args = @(
            "-NoProfile", "-ExecutionPolicy", "Bypass",
            "-File", ".\scripts\install-telegram-channel-task.ps1",
            "-AgentId", $AgentId[$i],
            "-SessionId", $SessionId[$i],
            "-TokenSource", $TelegramTokenSource[$i]
        )
        if (!$RegisterTasks) {
            $args += "-StartOnly"
        }
        powershell @args
    }

    $doctorArgs = @("doctor")
    foreach ($agent in $AgentId) {
        $doctorArgs += @("--agent-id", $agent)
    }
    .\target\x86_64-pc-windows-msvc\debug\maturana.exe @doctorArgs
} finally {
    Pop-Location
}
