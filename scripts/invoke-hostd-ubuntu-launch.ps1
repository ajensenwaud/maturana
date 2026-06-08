param(
    [string]$HostdUrl = "http://127.0.0.1:47832",
    [string]$TokenPath = ".\.maturana\hostd\token",
    [string]$AgentId = "codex-demo",
    [ValidateSet("codex", "claude-code", "opencode", "none")]
    [string]$Harness = "codex",
    [string]$BaseVhdxPath = ".\.maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx",
    [string]$SshUser = "ubuntu",
    [string]$SshKeyPath = ".\.maturana\keys\maturana-agent-ed25519",
    [string]$HarnessAuthSource = ".\.maturana\host-auth\codex",
    [string]$HarnessAuthGuestPath = "/home/ubuntu/.codex",
    [string]$AgentPrompt = "Inspect /agent/MATURANA.md and /agent/AGENTS.md, then report that the Maturana Codex guest harness is ready.",
    [string]$SessionId = "telegram-main",
    [string]$SessiondUrl = "",
    [string]$SessiondTokenPath = ".\.maturana\sessiond\token",
    [int]$DiskSizeGB = 24,
    [int]$Vcpu = 2,
    [int]$MemoryMiB = 2048,
    [int]$ProxyPort = 0,
    [string]$ProxyCaCertPath = "",
    [switch]$InstallHarness,
    [switch]$StartHarness,
    [switch]$ProxyHttps,
    [switch]$ProvisionExisting,
    [switch]$Force,
    [switch]$Wait,
    [int]$PollSeconds = 5
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $repoRoot
try {
    $body = @{
        agent_id = $AgentId
        harness = $Harness
        base_vhdx_path = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($BaseVhdxPath)
        ssh_user = $SshUser
        ssh_key_path = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($SshKeyPath)
        harness_auth_source = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($HarnessAuthSource)
        harness_auth_guest_path = $HarnessAuthGuestPath
        agent_prompt = $AgentPrompt
        session_id = $SessionId
        sessiond_url = $SessiondUrl
        sessiond_token_path = if (![string]::IsNullOrWhiteSpace($SessiondTokenPath)) { $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($SessiondTokenPath) } else { "" }
        disk_size_gb = $DiskSizeGB
        vcpu = $Vcpu
        memory_mib = $MemoryMiB
        install_harness = [bool]$InstallHarness
        start_harness = [bool]$StartHarness
        proxy_port = $ProxyPort
        proxy_https = [bool]$ProxyHttps
        proxy_ca_cert_path = if (![string]::IsNullOrWhiteSpace($ProxyCaCertPath)) { $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($ProxyCaCertPath) } else { "" }
        provision_existing = [bool]$ProvisionExisting
        force = [bool]$Force
    }

    $json = $body | ConvertTo-Json -Depth 10
    $headers = @{}
    $resolvedTokenPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($TokenPath)
    if (Test-Path -LiteralPath $resolvedTokenPath) {
        $headers["X-Maturana-Hostd-Token"] = (Get-Content -LiteralPath $resolvedTokenPath -Raw).Trim()
    }
    $response = Invoke-RestMethod -Method Post -Uri "$HostdUrl/agents/launch/ubuntu" -ContentType "application/json" -Headers $headers -Body $json
    $response | ConvertTo-Json -Depth 20
    if (!$response.ok) {
        throw "hostd launch failed"
    }
    if ($Wait -and $response.job_id) {
        do {
            Start-Sleep -Seconds ([Math]::Max(1, $PollSeconds))
            $status = Invoke-RestMethod -Method Get -Uri "$HostdUrl/agents/launch/status?job_id=$($response.job_id)" -Headers $headers
            $status | ConvertTo-Json -Depth 20
        } while ($status.status -in @("starting", "running"))

        if ($status.status -ne "succeeded") {
            throw "hostd launch job failed with status $($status.status). See $($status.log)"
        }
    }
}
finally {
    Pop-Location
}
