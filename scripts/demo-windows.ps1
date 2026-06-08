param(
    [string]$Spec = "examples/MATURANA.codex-demo.md"
)

$ErrorActionPreference = "Stop"

$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"

& $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate $Spec
& $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- agent launch $Spec
& $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- agent inspect codex-demo
