param(
  [string]$Target = "x86_64-pc-windows-msvc",
  [switch]$Test,
  [switch]$CheckFormat
)

$ErrorActionPreference = "Stop"

$vcvars = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvarsall.bat"
if (!(Test-Path -LiteralPath $vcvars)) {
  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
  if (Test-Path -LiteralPath $vswhere) {
    $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ($installPath) {
      $candidate = Join-Path $installPath "VC\Auxiliary\Build\vcvarsall.bat"
      if (Test-Path -LiteralPath $candidate) {
        $vcvars = $candidate
      }
    }
  }
}

if (!(Test-Path -LiteralPath $vcvars)) {
  throw "Could not find vcvarsall.bat. Install Visual Studio Build Tools with the C++ x64 toolchain."
}

$commands = @()
if ($CheckFormat) {
  $commands += "cargo +stable-$Target fmt -- --check"
}
if ($Test) {
  $commands += "cargo +stable-$Target test --target $Target"
}
$commands += "cargo +stable-$Target build --target $Target"

$joined = $commands -join " && "
cmd.exe /d /c "call `"$vcvars`" x64 && $joined"
if ($LASTEXITCODE -ne 0) {
  exit $LASTEXITCODE
}
