param(
    [string]$KeyPath = ".\.maturana\keys\maturana-agent-ed25519",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
Push-Location $repoRoot
try {
    $resolvedKeyPath = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($KeyPath)
    $keyDir = Split-Path -Parent $resolvedKeyPath
    New-Item -ItemType Directory -Force -Path $keyDir | Out-Null

    if ((Test-Path -LiteralPath $resolvedKeyPath) -and !$Force) {
        Write-Host "Using existing SSH key: $resolvedKeyPath"
        return
    }

    if ($Force) {
        Remove-Item -LiteralPath $resolvedKeyPath, "$resolvedKeyPath.pub" -Force -ErrorAction SilentlyContinue
    }

    ssh-keygen.exe -t ed25519 -N "" -f $resolvedKeyPath -C "maturana-agent" | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "ssh-keygen failed"
    }

    icacls.exe $resolvedKeyPath /inheritance:r /grant:r "$env:USERNAME`:R" | Out-Null
    Write-Host "SSH key: $resolvedKeyPath"
}
finally {
    Pop-Location
}
