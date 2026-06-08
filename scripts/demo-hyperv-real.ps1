param(
    [string]$AgentId = "codex-demo",
    [ValidateSet("codex", "claude-code")]
    [string]$Harness = "codex",
    [string]$BaseVhdxPath = ".\.maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx",
    [string]$SshKeyPath = ".\.maturana\keys\maturana-agent-ed25519",
    [switch]$StartHarness,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$args = @(
    "-Harness", $Harness,
    "-AgentId", $AgentId,
    "-BaseVhdxPath", $BaseVhdxPath,
    "-SshUser", "ubuntu",
    "-SshKeyPath", $SshKeyPath,
    "-InstallHarness"
)
if ($StartHarness) { $args += "-StartHarness" }
if ($Force) { $args += "-Force" }

& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot "start-hyperv-agent.ps1") @args
