param(
    [switch]$SkipImage,
    [switch]$ForceImage,
    [switch]$SkipHostd
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$imagePath = Join-Path $repoRoot ".maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx"

Push-Location $repoRoot
try {
    Write-Host "Preparing agent SSH key..."
    & .\scripts\init-agent-ssh-key.ps1

    if (!$SkipImage) {
        if ($ForceImage -or !(Test-Path -LiteralPath $imagePath)) {
            Write-Host "Preparing official Ubuntu Hyper-V image..."
            $imageArgs = @()
            if ($ForceImage) {
                $imageArgs += "-Force"
            }
            & .\scripts\get-ubuntu-cloudimg.ps1 @imageArgs
        } else {
            Write-Host "Using existing Ubuntu Hyper-V image: $imagePath"
        }
    }

    Write-Host "Building maturana CLI with the Windows GNU toolchain..."
    & .\scripts\maturana.ps1 --help | Out-Null

    if (!$SkipHostd) {
        Write-Host "Installing privileged host daemon. Windows may show one UAC prompt."
        & .\scripts\install-hostd-task.ps1
    }

    Write-Host "Windows install complete."
    Write-Host "Check hostd with: .\scripts\maturana.ps1 hostd status"
}
finally {
    Pop-Location
}
