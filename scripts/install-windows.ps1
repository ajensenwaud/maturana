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
    Write-Host "Building maturana CLI with the Windows GNU toolchain..."
    & .\scripts\maturana.ps1 --help | Out-Null

    Write-Host "Preparing agent SSH key..."
    & .\scripts\maturana.ps1 repair ssh-key

    if (!$SkipImage) {
        if ($ForceImage -or !(Test-Path -LiteralPath $imagePath)) {
            Write-Host "Preparing official Ubuntu Hyper-V image..."
            $imageArgs = @()
            if ($ForceImage) {
                $imageArgs += "--force"
            }
            & .\scripts\maturana.ps1 repair ubuntu-cloudimg @imageArgs
        } else {
            Write-Host "Using existing Ubuntu Hyper-V image: $imagePath"
        }
    }

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
