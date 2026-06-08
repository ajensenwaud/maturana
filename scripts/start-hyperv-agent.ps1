param(
    [ValidateSet("codex", "claude-code", "opencode")]
    [string]$Harness = "codex",
    [string]$AgentId = "",
    [string]$BaseVhdxPath = ".\.maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx",
    [string]$SshUser = "ubuntu",
    [string]$SshKeyPath = ".\.maturana\keys\maturana-agent-ed25519",
    [string]$AgentPrompt = "",
    [int]$ProxyPort = 0,
    [string]$ProxyCaCertPath = "",
    [switch]$InstallHarness,
    [switch]$StartHarness,
    [switch]$ProxyHttps,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"

if ([string]::IsNullOrWhiteSpace($AgentId)) {
    $AgentId = if ($Harness -eq "claude-code") { "claude-demo" } elseif ($Harness -eq "opencode") { "opencode-demo" } else { "codex-demo" }
}

if ($Harness -eq "claude-code") {
    $spec = "examples/MATURANA.claude-hyperv.md"
    $authSource = Join-Path $repoRoot ".maturana\host-auth\claude-code"
    $authGuestPath = "/home/ubuntu/.claude"
} elseif ($Harness -eq "opencode") {
    $spec = "examples/MATURANA.opencode-hyperv.md"
    $authSource = Join-Path $repoRoot ".maturana\host-auth\opencode"
    $authGuestPath = "/home/ubuntu"
} else {
    $spec = "examples/MATURANA.codex-hyperv.md"
    $authSource = Join-Path $repoRoot ".maturana\host-auth\codex"
    $authGuestPath = "/home/ubuntu/.codex"
}

Push-Location $repoRoot
try {
    & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate $spec
    if ($LASTEXITCODE -ne 0) { throw "spec validation failed" }

    & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- agent launch $spec
    if ($LASTEXITCODE -ne 0) { throw "agent materialization failed" }

    $launchArgs = @(
        "-AgentId", $AgentId,
        "-BaseVhdxPath", $BaseVhdxPath,
        "-SshUser", $SshUser,
        "-SshKeyPath", $SshKeyPath,
        "-Harness", $Harness,
        "-HarnessAuthSource", $authSource,
        "-HarnessAuthGuestPath", $authGuestPath
    )
    if (![string]::IsNullOrWhiteSpace($AgentPrompt)) {
        $launchArgs += @("-AgentPrompt", $AgentPrompt)
    }
    if ($ProxyPort -gt 0) { $launchArgs += @("-ProxyPort", $ProxyPort) }
    if (![string]::IsNullOrWhiteSpace($ProxyCaCertPath)) { $launchArgs += @("-ProxyCaCertPath", $ProxyCaCertPath) }
    if ($InstallHarness) { $launchArgs += "-InstallHarness" }
    if ($StartHarness) { $launchArgs += "-StartHarness" }
    if ($ProxyHttps) { $launchArgs += "-ProxyHttps" }
    if ($Force) { $launchArgs += "-Force" }

    & powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $repoRoot "scripts\launch-ubuntu-cloudimg-hyperv.ps1") @launchArgs
    if ($LASTEXITCODE -ne 0) { throw "Hyper-V launch failed" }
}
finally {
    Pop-Location
}
