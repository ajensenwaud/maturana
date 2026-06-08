param(
    [string]$Release = "noble",
    [string]$Arch = "amd64",
    [string]$ImageUrl = "",
    [string]$Sha256SumsUrl = "",
    [string]$QemuImgPath = "",
    [switch]$Force
)

$ErrorActionPreference = "Stop"

function Write-Step {
    param([string]$Message)
    Write-Host $Message
}

function Get-QemuImg {
    param([string]$Requested)
    if (![string]::IsNullOrWhiteSpace($Requested)) {
        if (!(Test-Path -LiteralPath $Requested)) {
            throw "qemu-img not found at $Requested"
        }
        return (Resolve-Path $Requested).Path
    }

    $cmd = Get-Command qemu-img.exe -ErrorAction SilentlyContinue
    if ($cmd) {
        return $cmd.Source
    }

    $candidates = @(
        "C:\Program Files\qemu\qemu-img.exe",
        "C:\Program Files (x86)\qemu\qemu-img.exe",
        "C:\msys64\mingw64\bin\qemu-img.exe",
        "C:\msys64\ucrt64\bin\qemu-img.exe",
        (Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages\cloudbase.qemu-img_Microsoft.Winget.Source_8wekyb3d8bbwe\qemu-img.exe")
    )
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate) {
            return $candidate
        }
    }

    throw "qemu-img.exe is required to convert the official Ubuntu cloud image to VHDX. Install QEMU for Windows, then rerun this script."
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$imageDir = Join-Path $repoRoot ".maturana\images\ubuntu-$Release"
New-Item -ItemType Directory -Force -Path $imageDir | Out-Null

if ([string]::IsNullOrWhiteSpace($ImageUrl)) {
    $ImageUrl = "https://cloud-images.ubuntu.com/$Release/current/$Release-server-cloudimg-$Arch.img"
}
if ([string]::IsNullOrWhiteSpace($Sha256SumsUrl)) {
    $Sha256SumsUrl = "https://cloud-images.ubuntu.com/$Release/current/SHA256SUMS"
}

$imgName = Split-Path -Leaf $ImageUrl
$imgPath = Join-Path $imageDir $imgName
$shaPath = Join-Path $imageDir "SHA256SUMS"
$vhdxPath = Join-Path $imageDir "$Release-server-cloudimg-$Arch.vhdx"

if ($Force -or !(Test-Path -LiteralPath $imgPath)) {
    Write-Step "Downloading official Ubuntu cloud image..."
    curl.exe -L --fail --output $imgPath $ImageUrl
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to download $ImageUrl"
    }
} else {
    Write-Step "Using existing image $imgPath"
}

if ($Force -or !(Test-Path -LiteralPath $shaPath)) {
    Write-Step "Downloading SHA256SUMS..."
    curl.exe -L --fail --output $shaPath $Sha256SumsUrl
    if ($LASTEXITCODE -ne 0) {
        throw "Failed to download $Sha256SumsUrl"
    }
}

$shaLine = Get-Content -LiteralPath $shaPath |
    Where-Object { $_ -match [regex]::Escape($imgName) } |
    Select-Object -First 1
if (!$shaLine) {
    throw "No checksum entry for $imgName in $shaPath"
}
$expected = ($shaLine -split '\s+')[0].ToLowerInvariant()
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $imgPath).Hash.ToLowerInvariant()
if ($actual -ne $expected) {
    throw "Checksum mismatch for $imgPath. Expected $expected but got $actual."
}
Write-Step "Checksum OK."

if ($Force -or !(Test-Path -LiteralPath $vhdxPath)) {
    $qemuImg = Get-QemuImg -Requested $QemuImgPath
    Write-Step "Converting image to VHDX with $qemuImg..."
    & $qemuImg convert -p -O vhdx -o subformat=dynamic $imgPath $vhdxPath
    if ($LASTEXITCODE -ne 0) {
        throw "qemu-img conversion failed."
    }
} else {
    Write-Step "Using existing VHDX $vhdxPath"
}

Write-Host "VHDX: $vhdxPath"
