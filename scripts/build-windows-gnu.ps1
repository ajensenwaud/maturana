$ErrorActionPreference = "Stop"

$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
if (!(Test-Path $cargo)) {
    throw "cargo.exe not found. Install Rust with winget install --id Rustlang.Rustup -e"
}

$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"
if ([string]::IsNullOrWhiteSpace($env:CARGO_BUILD_JOBS)) {
    $env:CARGO_BUILD_JOBS = "1"
}

& $cargo +stable-x86_64-pc-windows-gnu test --workspace --target x86_64-pc-windows-gnu
