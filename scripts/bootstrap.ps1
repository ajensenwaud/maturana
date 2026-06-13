# Maturana Windows bootstrap (download-based, no Rust/MSYS2 toolchain).
#
#   irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/bootstrap.ps1 | iex
#
# Clones the repo (for skills/, AGENTS.md, scripts, examples), downloads the
# signed prebuilt maturana.exe from the latest GitHub Release, verifies its
# SHA256, then runs the real installer against that binary. Configure via env:
#   $env:MATURANA_DIR   install dir (default %USERPROFILE%\maturana)
#   $env:MATURANA_REF   git ref to clone (default main)
$ErrorActionPreference = "Stop"

$repo    = "https://github.com/ajensenwaud/maturana.git"
$relBase = "https://github.com/ajensenwaud/maturana/releases/latest/download"
$asset   = "maturana-x86_64-pc-windows-msvc.zip"
$dir     = if ($env:MATURANA_DIR) { $env:MATURANA_DIR } else { Join-Path $env:USERPROFILE "maturana" }
$ref     = if ($env:MATURANA_REF) { $env:MATURANA_REF } else { "main" }

function Say($m) { Write-Host "[maturana] $m" -ForegroundColor Cyan }

# 1. git (clone the repo for the non-binary assets the installer/runtime need).
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Say "git not found; installing via winget"
    winget install --id Git.Git -e --source winget --accept-source-agreements --accept-package-agreements
    $env:PATH = "$env:ProgramFiles\Git\cmd;$env:PATH"
}
if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    throw "git is required. Install it (https://git-scm.com/download/win) and re-run."
}

# 2. Clone or update.
if (Test-Path -LiteralPath (Join-Path $dir ".git")) {
    Say "updating $dir"
    git -C $dir fetch --depth 1 origin $ref
    git -C $dir checkout $ref
    git -C $dir reset --hard "origin/$ref"
} else {
    Say "cloning into $dir"
    git clone --depth 1 --branch $ref $repo $dir
}

# 3. Download + verify the signed prebuilt binary.
$binDir = Join-Path $dir "bin"
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
$zip  = Join-Path $env:TEMP $asset
$sums = Join-Path $env:TEMP "maturana-SHA256SUMS"
Say "downloading $asset"
Invoke-WebRequest -Uri "$relBase/$asset" -OutFile $zip -UseBasicParsing
Invoke-WebRequest -Uri "$relBase/SHA256SUMS" -OutFile $sums -UseBasicParsing

$want = (Get-Content $sums | Where-Object { $_ -match [regex]::Escape($asset) } |
         Select-Object -First 1) -replace '\s.*$', ''
$got = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
if (-not $want) { throw "no SHA256 entry for $asset in SHA256SUMS" }
if ($got -ne $want.ToLower()) { throw "checksum mismatch for $asset (want $want, got $got)" }
Say "checksum OK"

Expand-Archive -Path $zip -DestinationPath $binDir -Force
$bin = Join-Path $binDir "maturana.exe"
if (-not (Test-Path -LiteralPath $bin)) { throw "maturana.exe missing after extract" }

# Authenticode is best-effort until a signing cert is wired into the release.
$sig = (Get-AuthenticodeSignature $bin).Status
if ($sig -eq "Valid") { Say "Authenticode signature: valid" }
else { Say "Authenticode signature: $sig (release not code-signed yet)" }

Remove-Item $zip, $sums -Force -ErrorAction SilentlyContinue

# 4. Hand off to the real installer using the prebuilt binary.
Say "running installer (it will request elevation + your Windows password)"
& (Join-Path $dir "scripts\install-windows.ps1") -MaturanaBin $bin
