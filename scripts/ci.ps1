$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"

function Invoke-Checked {
    param(
        [scriptblock]$Command,
        [string]$Name
    )
    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE"
    }
}

Push-Location $repoRoot
try {
    Invoke-Checked { & $cargo fmt --all -- --check } "cargo fmt"
    Invoke-Checked { & powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-windows-gnu.ps1 } "windows build"

    $powerShellScripts = @(
        "scripts\launch-ubuntu-cloudimg-hyperv.ps1",
        "scripts\maturana-hostd.ps1",
        "scripts\invoke-hostd-ubuntu-launch.ps1",
        "scripts\start-hyperv-agent.ps1",
        "scripts\test-pipelock-proxy-aidev.ps1",
        "scripts\test-pipelock-proxy-live.ps1"
    )
    foreach ($scriptPath in $powerShellScripts) {
        $tokens = $null
        $parseErrors = $null
        [System.Management.Automation.Language.Parser]::ParseFile(
            (Resolve-Path $scriptPath),
            [ref]$tokens,
            [ref]$parseErrors
        ) | Out-Null
        if ($parseErrors.Count -gt 0) {
            $messages = $parseErrors | ForEach-Object { $_.Message }
            throw "PowerShell parse failed for ${scriptPath}: $($messages -join '; ')"
        }
    }

    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate examples/MATURANA.codex-hyperv.md } "codex spec validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate examples/MATURANA.claude-hyperv.md } "claude spec validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate examples/MATURANA.opencode-hyperv.md } "opencode spec validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate examples/MATURANA.firecracker-demo.md } "firecracker spec validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- agent launch examples/MATURANA.firecracker-demo.md } "firecracker materialization"

    $runId = [Guid]::NewGuid().ToString("N")
    $pipelockHome = ".maturana-ci\pipelock-home-$runId"
    Remove-Item -LiteralPath $pipelockHome -Recurse -Force -ErrorAction SilentlyContinue
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $pipelockHome pipelock init } "pipelock init"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $pipelockHome pipelock set telegram/bot-token --value "ci-pipelock-secret" } "pipelock set"
    $pipelockSecret = & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $pipelockHome pipelock get telegram/bot-token
    if ($LASTEXITCODE -ne 0 -or (($pipelockSecret | Select-Object -Last 1) -ne "ci-pipelock-secret")) {
        throw "pipelock get failed"
    }
    $pipelockCaCert = & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $pipelockHome pipelock ca-cert
    if ($LASTEXITCODE -ne 0 -or !(Test-Path -LiteralPath ($pipelockCaCert | Select-Object -Last 1))) {
        throw "pipelock ca-cert failed"
    }
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $pipelockHome pipelock proxy --help } "pipelock proxy cli"

    $personalHome = ".maturana-ci\personal-home-$runId"
    Remove-Item -LiteralPath $personalHome -Recurse -Force -ErrorAction SilentlyContinue
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome personal init codex-demo --spec examples/MATURANA.codex-hyperv.md } "personal init"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome wiki ingest AGENTS.md --title Repo-Agents --chunk-chars 800 } "wiki ingest"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome wiki search secure --limit 3 } "wiki search"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome heartbeat beat codex-demo --status alive --message ci } "heartbeat beat"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome heartbeat status codex-demo } "heartbeat status"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome schedule add codex-demo morning --cron "0 9 * * *" --prompt "Send a morning brief" --channel telegram } "schedule add"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome schedule list codex-demo } "schedule list"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome notify discord --webhook-source env:MATURANA_TEST_DISCORD_WEBHOOK --message "ci" --dry-run } "discord dry-run"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome channel pair telegram --help } "telegram pair cli"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome channel pair telegram status } "telegram pair status"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome channel serve telegram --help } "telegram serve cli"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- --home $personalHome channel status } "channel status"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- deploy skill --help } "deploy skill cli"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- deploy tool --help } "deploy tool cli"

    $bash = "C:\Program Files\Git\bin\bash.exe"
    if (Test-Path $bash) {
        Invoke-Checked { & $bash -n scripts/firecracker-prepare-assets.sh scripts/firecracker-doctor.sh scripts/firecracker-launch.sh scripts/firecracker-stop.sh scripts/firecracker-inspect.sh scripts/firecracker-setup-tap.sh scripts/test-pipelock-proxy-firecracker-live.sh } "firecracker shell syntax"
    } else {
        Write-Host "warning: Git Bash not found; skipping Firecracker shell syntax check"
    }

    $prefix = "883130" + "4031"
    $fragment = "AAFd" + "R6g0"
    $matches = rg "$prefix|$fragment" -n
    if ($LASTEXITCODE -eq 0) {
        throw "secret scan found sensitive test or Telegram token material"
    }
    Write-Host "ci passed"
}
finally {
    Pop-Location
}
