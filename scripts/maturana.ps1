param(
    [Parameter(ValueFromRemainingArguments=$true)]
    [string[]]$Arguments = @()
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
$exe = Join-Path $repoRoot "target\x86_64-pc-windows-gnu\debug\maturana.exe"

if (!(Test-Path -LiteralPath $cargo)) {
    throw "cargo.exe not found. Install Rust with: winget install --id Rustlang.Rustup -e"
}

$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"

Push-Location $repoRoot
try {
    & $cargo +stable-x86_64-pc-windows-gnu build -p maturana-cli --target x86_64-pc-windows-gnu
    if ($LASTEXITCODE -ne 0) {
        throw "maturana GNU build failed"
    }
}
finally {
    Pop-Location
}

& $exe @Arguments
if ($LASTEXITCODE -ne 0) {
    throw "maturana exited with code $LASTEXITCODE"
}
