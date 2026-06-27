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

    $allowedScripts = @(
        "build-windows-gnu.ps1",
        "build-windows-msvc.ps1",
        "ci.ps1",
        "firecracker-prepare-assets.sh",
        "firecracker-setup-tap.sh",
        "install-hostd-task.ps1",
        "install.ps1",
        "launch-ubuntu-cloudimg-hyperv.ps1",
        "maturana.ps1",
        "release.sh",
        "test-pipelock-proxy-aidev.ps1",
        "test-pipelock-proxy-firecracker-live.sh",
        "test-pipelock-proxy-live.ps1",
        "uninstall-hostd-task.ps1"
    )
    $actualScripts = @(Get-ChildItem -LiteralPath scripts -File | Select-Object -ExpandProperty Name | Sort-Object)
    $unexpectedScripts = @($actualScripts | Where-Object { $allowedScripts -notcontains $_ })
    $missingScripts = @($allowedScripts | Where-Object { $actualScripts -notcontains $_ })
    if ($unexpectedScripts.Count -gt 0 -or $missingScripts.Count -gt 0) {
        throw "script inventory drifted; update scripts/ci.ps1 and docs/script-boundary.md. Unexpected: $($unexpectedScripts -join ', '); Missing: $($missingScripts -join ', ')"
    }

    $powerShellScripts = @(Get-ChildItem -LiteralPath scripts -Filter *.ps1 -File | Sort-Object Name)
    foreach ($scriptPath in $powerShellScripts) {
        $tokens = $null
        $parseErrors = $null
        [System.Management.Automation.Language.Parser]::ParseFile(
            $scriptPath.FullName,
            [ref]$tokens,
            [ref]$parseErrors
        ) | Out-Null
        if ($parseErrors.Count -gt 0) {
            $messages = $parseErrors | ForEach-Object { $_.Message }
            throw "PowerShell parse failed for $($scriptPath.FullName): $($messages -join '; ')"
        }
    }
    $hypervLauncher = Get-Content -LiteralPath scripts\launch-ubuntu-cloudimg-hyperv.ps1 -Raw
    $forbiddenLauncherDecisions = @(
        "apt-get install",
        "npm install",
        "@openai/codex",
        "@anthropic-ai/claude-code",
        "opencode-ai",
        "AgentCommand",
        "/agent/run-command",
        "run-command",
        "#cloud-config",
        "ssh_authorized_keys",
        "ssh-keygen.exe -y",
        "MATURANA_PROXY_HTTPS=1",
        "playwright install",
        "chromium",
        "browser-smoke",
        "Copy-ToGuest",
        "-HarnessAuthSource",
        "-HarnessInstallPath",
        "-SessiondEnvPath",
        "-RunnerPath",
        "-ServicePath",
        "-ProxyEnvPath",
        "-ProxyCaCertPath",
        "-InstallHarness",
        "-StartHarness",
        "/opt/maturana/bin/run-agent.sh",
        "maturana-agent.service",
        "/agent/proxy.env"
    )
    foreach ($forbidden in $forbiddenLauncherDecisions) {
        if ($hypervLauncher.Contains($forbidden)) {
            throw "Hyper-V launcher contains orchestration decision '$forbidden'; render guest bootstrap/install scripts from Rust instead"
        }
    }
    $cliSource = Get-Content -LiteralPath crates\maturana-cli\src\main.rs -Raw
    $hostdStart = $cliSource.IndexOf("fn run_hostd_server(")
    $hostdEnd = $cliSource.IndexOf("fn hostd_url(")
    if ($hostdStart -lt 0 -or $hostdEnd -lt $hostdStart) {
        throw "Rust hostd serve implementation was not found"
    }
    $hostdSource = $cliSource.Substring($hostdStart, $hostdEnd - $hostdStart)
    $forbiddenHostdQueuePatterns = @(
        "agents/launch/status",
        "Start-LaunchJob",
        "New-LaunchJobId",
        ".runner.ps1",
        ".status.json",
        ".args.json"
    )
    foreach ($forbidden in $forbiddenHostdQueuePatterns) {
        if ($hostdSource.Contains($forbidden)) {
            throw "hostd contains launch queue pattern '$forbidden'; hostd launch must remain synchronous and fixed-purpose"
        }
    }
    $forbiddenHostdGuestProvisioningArgs = @(
        "-BootstrapPath",
        "-HarnessInstallPath",
        "-SessiondEnvPath",
        "-RunnerPath",
        "-ServicePath",
        "-ProxyEnvPath",
        "-ProxyCaCertPath",
        "-HarnessAuthSource",
        "-InstallHarness",
        "-StartHarness"
    )
    foreach ($forbidden in $forbiddenHostdGuestProvisioningArgs) {
        if ($hostdSource.Contains($forbidden)) {
            throw "hostd passes guest provisioning argument '$forbidden'; Rust provider owns guest provisioning after Hyper-V create"
        }
    }
    $hostdInstaller = Get-Content -LiteralPath scripts\install-hostd-task.ps1 -Raw
    if ($hostdInstaller.Contains("maturana-hostd.ps1")) {
        throw "hostd installer must start `maturana hostd serve`; the PowerShell daemon has been removed"
    }
    $firecrackerImagePrep = Get-Content -LiteralPath scripts\firecracker-prepare-assets.sh -Raw
    $forbiddenHarnessPackages = @(
        "@openai/codex",
        "@anthropic-ai/claude-code",
        "opencode-ai"
    )
    foreach ($forbidden in $forbiddenHarnessPackages) {
        if ($firecrackerImagePrep.Contains($forbidden)) {
            throw "Firecracker image prep contains harness package '$forbidden'; render install-harness.sh from Rust instead"
        }
    }
    $forbiddenGuestBootstrap = @(
        "openssh-server",
        "nodejs npm",
        "90-maturana-ubuntu",
        "systemctl enable ssh.service",
        "playwright install",
        "chromium",
        "browser-smoke"
    )
    foreach ($forbidden in $forbiddenGuestBootstrap) {
        if ($firecrackerImagePrep.Contains($forbidden)) {
            throw "Firecracker image prep contains guest bootstrap decision '$forbidden'; render firecracker-bootstrap.sh from Rust instead"
        }
    }
    foreach ($required in @("MATURANA_FIRECRACKER_ASSET_MANIFEST_PATH", "asset-manifest.json", "kernel_sha256", "rootfs_sha256")) {
        if (!$firecrackerImagePrep.Contains($required)) {
            throw "Firecracker image prep must emit an asset manifest containing '$required'"
        }
    }
    if ($firecrackerImagePrep.Contains('.maturana/host-auth/codex')) {
        throw "Firecracker image prep must not default to Codex auth; Rust must pass the selected harness auth source explicitly"
    }
    $firecrackerProvider = Get-Content -LiteralPath crates\maturana-core\src\providers\firecracker.rs -Raw
    foreach ($required in @(
        "fn validate_firecracker_plan_files(",
        "validate_firecracker_plan_files(spec, agent_dir)?;",
        '"firecracker-config.json"',
        '"firecracker-metadata.json"',
        "fn validate_state_path("
    )) {
        if (!$firecrackerProvider.Contains($required)) {
            throw "Firecracker provider must validate materialized config/metadata contract before launch and inspect; missing '$required'"
        }
    }
    $sessionSource = Get-Content -LiteralPath crates\maturana-cli\src\session.rs -Raw
    $channelSource = Get-Content -LiteralPath crates\maturana-cli\src\channels.rs -Raw
    foreach ($source in @($sessionSource, $channelSource)) {
        if ($source.Contains("codex-ssh")) {
            throw "host-side codex-ssh session provider is not allowed; harness turns must run inside the guest worker"
        }
    }
    $specSource = Get-Content -LiteralPath crates\maturana-core\src\spec.rs -Raw
    $forbiddenAgentRunFields = @(
        "pub prompt: Option<String>",
        "pub command: Option<String>"
    )
    foreach ($forbidden in $forbiddenAgentRunFields) {
        if ($specSource.Contains($forbidden)) {
            throw "AgentRun contains stale one-shot field '$forbidden'; live turns must enter through sessiond"
        }
    }
    $forbiddenCliFirecrackerRenderers = @(
        "fn render_firecracker_bootstrap(",
        "fn render_firecracker_netplan(",
        "fn render_firecracker_cloud_cfg(",
        "fn render_firecracker_proxy_env("
    )
    foreach ($forbidden in $forbiddenCliFirecrackerRenderers) {
        if ($cliSource.Contains($forbidden)) {
            throw "CLI contains Firecracker guest renderer '$forbidden'; render guest artifacts from maturana-core::worker"
        }
    }
    foreach ($required in @("fn validate_firecracker_asset_manifest(", "fn validate_elf_file(", "fn validate_manifest_sha256(")) {
        if (!$cliSource.Contains($required)) {
            throw "CLI must validate Firecracker asset manifests before launch/repair continues"
        }
    }
    $readme = Get-Content -LiteralPath README.md -Raw
    $forbiddenReadmeClaims = @(
        "Firecracker provider planning",
        "skill stubs that call the CLI",
        "Windows MVP",
        "Linux MVP",
        "MVP boundary",
        "guest work loop for the Windows MVP",
        "Linux MVP image prep"
    )
    foreach ($forbidden in $forbiddenReadmeClaims) {
        if ($readme.Contains($forbidden)) {
            throw "README still contains stale MVP claim '$forbidden'"
        }
    }

    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- skill validate skills } "skill workflow validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate examples/MATURANA.codex-hyperv.md } "codex spec validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate examples/MATURANA.claude-hyperv.md } "claude spec validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate examples/MATURANA.opencode-hyperv.md } "opencode spec validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- spec validate examples/MATURANA.firecracker-demo.md } "firecracker spec validation"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- agent launch examples/MATURANA.firecracker-demo.md } "firecracker materialization"
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- setup ubuntu-cloudimg --help } "ubuntu cloudimg repair cli"

    $runId = [Guid]::NewGuid().ToString("N")
    $sshKeyDir = ".maturana-ci\ssh-key-$runId"
    $sshKeyPath = Join-Path $sshKeyDir "maturana-agent-ed25519"
    Remove-Item -LiteralPath $sshKeyDir -Recurse -Force -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $sshKeyDir | Out-Null
    Set-Content -LiteralPath $sshKeyPath -Value "ci-existing-key" -NoNewline
    Invoke-Checked { & $cargo +stable-x86_64-pc-windows-gnu run -p maturana-cli --target x86_64-pc-windows-gnu -- setup ssh-key --key-path $sshKeyPath } "ssh key repair"
    if ((Get-Content -LiteralPath $sshKeyPath -Raw) -ne "ci-existing-key") {
        throw "ssh key repair should keep an existing key unless --force is passed"
    }

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
        $shellScripts = @(Get-ChildItem -LiteralPath scripts -Filter *.sh -File | Sort-Object Name | ForEach-Object { $_.FullName })
        Invoke-Checked { & $bash -n @shellScripts } "shell syntax"
    } else {
        Write-Host "warning: Git Bash not found; skipping shell syntax check"
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
