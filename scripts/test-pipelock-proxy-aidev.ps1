param(
    [string]$AidevHost = "aidev",
    [string]$RemoteRepoRoot = "/home/aj/maturana",
    [string]$RemoteAgentRoot = "/var/tmp/maturana-aidev/.maturana",
    [string]$GuestIp = "172.30.0.2",
    [string]$HostIp = "172.30.0.1",
    [string]$SshUser = "ubuntu",
    [int]$ProxyPort = 47833
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$localScript = Join-Path $repoRoot "scripts\test-pipelock-proxy-firecracker-live.sh"
$remoteScript = "/tmp/maturana-test-pipelock-proxy-firecracker-live.sh"

if (!(Test-Path -LiteralPath $localScript)) {
    throw "missing local script: $localScript"
}

scp $localScript "$AidevHost`:$remoteScript"
if ($LASTEXITCODE -ne 0) {
    throw "failed to copy Firecracker pipelock live test to $AidevHost"
}

$remoteCommand = "chmod +x '$remoteScript' && cd '$RemoteRepoRoot' && bash '$remoteScript' --repo-root '$RemoteRepoRoot' --agent-root '$RemoteAgentRoot' --guest-ip '$GuestIp' --host-ip '$HostIp' --ssh-user '$SshUser' --proxy-port '$ProxyPort'"

ssh $AidevHost $remoteCommand
if ($LASTEXITCODE -ne 0) {
    throw "Firecracker pipelock live test failed on $AidevHost"
}
