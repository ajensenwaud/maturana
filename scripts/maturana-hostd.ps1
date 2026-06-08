param(
    [string]$BindPrefix = "http://127.0.0.1:47832/",
    [string]$TokenPath = "",
    [string]$LogPath = ""
)

$ErrorActionPreference = "Stop"

function Write-HostdLog {
    param([string]$Message)
    $line = "$(Get-Date -Format o) $Message"
    if (![string]::IsNullOrWhiteSpace($script:LogPath)) {
        Add-Content -LiteralPath $script:LogPath -Value $line
    }
    Write-Host $Message
}

trap {
    try {
        Write-HostdLog "fatal: $($_.Exception.Message)"
        Write-HostdLog "fatal-at: $($_.InvocationInfo.PositionMessage)"
    } catch {}
    exit 1
}

function Assert-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Write-HostdLog "startup failed: not elevated; user=$($identity.Name)"
        throw "Run maturana-hostd from an elevated PowerShell session."
    }
}

function Send-Json {
    param($Context, $Object, [int]$StatusCode = 200)
    $json = $Object | ConvertTo-Json -Depth 20
    $bytes = [Text.Encoding]::UTF8.GetBytes($json)
    $Context.Response.StatusCode = $StatusCode
    $Context.Response.ContentType = "application/json"
    $Context.Response.OutputStream.Write($bytes, 0, $bytes.Length)
    $Context.Response.Close()
}

function Initialize-Token {
    param([string]$Path)

    $dir = Split-Path -Parent $Path
    Write-HostdLog "initializing hostd token at $Path"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Write-HostdLog "hostd token directory ready"
    if (!(Test-Path -LiteralPath $Path)) {
        $bytes = [byte[]]::new(32)
        $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
        try {
            $rng.GetBytes($bytes)
        } finally {
            $rng.Dispose()
        }
        $token = [Convert]::ToBase64String($bytes)
        Set-Content -LiteralPath $Path -Value $token -NoNewline
        Write-HostdLog "hostd token file created"
        try {
            icacls.exe $Path /inheritance:r /grant:r "$env:USERNAME`:R" | Out-Null
            Write-HostdLog "hostd token ACL restricted"
        } catch {
            Write-HostdLog "warning: could not restrict hostd token ACL: $($_.Exception.Message)"
        }
    }
    $token = (Get-Content -LiteralPath $Path -Raw).Trim()
    Write-HostdLog "hostd token loaded"
    return $token
}

function Assert-Authorized {
    param($Context, [string]$ExpectedToken)

    $path = $Context.Request.Url.AbsolutePath.Trim("/")
    if ($path -eq "health") {
        return $true
    }

    $actual = $Context.Request.Headers.Get("X-Maturana-Hostd-Token")
    if ([string]::IsNullOrWhiteSpace($actual) -or $actual -ne $ExpectedToken) {
        Send-Json $Context @{ ok = $false; error = "unauthorized" } 401
        return $false
    }

    return $true
}

function Read-JsonBody {
    param($Request)
    $reader = [IO.StreamReader]::new($Request.InputStream, $Request.ContentEncoding)
    try {
        $raw = $reader.ReadToEnd()
        if ([string]::IsNullOrWhiteSpace($raw)) {
            return @{}
        }
        return $raw | ConvertFrom-Json
    }
    finally {
        $reader.Close()
    }
}

function Add-Arg {
    param(
        [System.Collections.Generic.List[string]]$ArgList,
        [string]$Name,
        [object]$Value
    )
    if ($null -ne $Value -and ![string]::IsNullOrWhiteSpace([string]$Value)) {
        $ArgList.Add($Name)
        $ArgList.Add([string]$Value)
    }
}

function Get-VMIPv4 {
    param([string]$Name)

    $adapter = Get-VMNetworkAdapter -VMName $Name -ErrorAction SilentlyContinue
    if (!$adapter) {
        return ""
    }

    $addresses = @($adapter.IPAddresses |
        Where-Object {
            $_ -match '^\d+\.\d+\.\d+\.\d+$' -and
            $_ -notlike '169.254.*' -and
            $_ -notlike '0.*' -and
            $_ -notlike '127.*'
        })
    if ($addresses.Count -gt 0) {
        return $addresses[0]
    }

    $mac = ($adapter.MacAddress -replace '[^0-9A-Fa-f]', '').ToUpperInvariant()
    if (!$mac) {
        return ""
    }

    $neighbor = Get-NetNeighbor -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object {
            ($_.LinkLayerAddress -replace '[^0-9A-Fa-f]', '').ToUpperInvariant() -eq $mac -and
            $_.IPAddress -match '^\d+\.\d+\.\d+\.\d+$' -and
            $_.IPAddress -notlike '169.254.*' -and
            $_.IPAddress -notlike '0.*' -and
            $_.IPAddress -notlike '127.*'
        } |
        Select-Object -First 1
    if ($neighbor) {
        return $neighbor.IPAddress
    }

    return ""
}

function Get-AgentVmName {
    param([string]$AgentId)
    if ($AgentId -notmatch '^[a-z0-9-]+$') {
        throw "invalid agent id: $AgentId"
    }
    return "maturana-$AgentId"
}

function Get-SnapshotName {
    param([string]$Name)
    if ($Name -notmatch '^[A-Za-z0-9._-]+$') {
        throw "invalid snapshot name: $Name"
    }
    return $Name
}

function New-LaunchJobId {
    param([string]$AgentId)
    $stamp = Get-Date -Format "yyyyMMddHHmmss"
    $suffix = [Guid]::NewGuid().ToString("N").Substring(0, 8)
    return "$AgentId-$stamp-$suffix"
}

function Write-LaunchStatus {
    param(
        [string]$Path,
        [string]$JobId,
        [string]$AgentId,
        [string]$Status,
        [string]$Log,
        [Nullable[int]]$ProcessId = $null,
        [Nullable[int]]$ExitCode = $null,
        [string]$ErrorMessage = ""
    )
    $payload = @{
        ok = $true
        job_id = $JobId
        agent_id = $AgentId
        status = $Status
        pid = $ProcessId
        exit_code = $ExitCode
        log = $Log
        error = $ErrorMessage
        updated_at = (Get-Date -Format o)
    }
    $payload | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $Path -Encoding UTF8
}

function Read-LaunchStatus {
    param([string]$Path)
    if (!(Test-Path -LiteralPath $Path)) {
        return $null
    }
    return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

function Get-LaunchStatusPath {
    param([string]$JobId)
    if ($JobId -notmatch '^[A-Za-z0-9._-]+$') {
        throw "invalid launch job id: $JobId"
    }
    return Join-Path $script:JobDir "$JobId.status.json"
}

function Start-LaunchJob {
    param(
        [string]$AgentId,
        [System.Collections.Generic.List[string]]$LauncherArgs
    )

    $jobId = New-LaunchJobId -AgentId $AgentId
    $argsPath = Join-Path $script:JobDir "$jobId.args.json"
    $statusPath = Get-LaunchStatusPath -JobId $jobId
    $runnerPath = Join-Path $script:JobDir "$jobId.runner.ps1"
    $launchLogPath = Join-Path $repoRoot ".maturana\logs\hyperv-launch-$AgentId.log"
    $jobOutputLogPath = Join-Path $repoRoot ".maturana\logs\hostd-launch-$jobId.output.log"

    @($LauncherArgs) | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $argsPath -Encoding UTF8
    Write-LaunchStatus -Path $statusPath -JobId $jobId -AgentId $AgentId -Status "starting" -Log $launchLogPath

    $runner = @'
param(
    [Parameter(Mandatory=$true)][string]$ArgsPath,
    [Parameter(Mandatory=$true)][string]$StatusPath,
    [Parameter(Mandatory=$true)][string]$JobId,
    [Parameter(Mandatory=$true)][string]$AgentId,
    [Parameter(Mandatory=$true)][string]$LogPath,
    [Parameter(Mandatory=$true)][string]$OutputLogPath
)

$ErrorActionPreference = "Stop"

function Write-Status {
    param([string]$Status, [Nullable[int]]$ExitCode = $null, [string]$ErrorMessage = "")
    $payload = @{
        ok = $true
        job_id = $JobId
        agent_id = $AgentId
        status = $Status
        pid = $PID
        exit_code = $ExitCode
        log = $LogPath
        error = $ErrorMessage
        updated_at = (Get-Date -Format o)
    }
    $payload | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $StatusPath -Encoding UTF8
}

try {
    $launcherArgs = @(Get-Content -LiteralPath $ArgsPath -Raw | ConvertFrom-Json)
    Write-Status -Status "running"
    Add-Content -LiteralPath $LogPath -Value "$(Get-Date -Format o) hostd launch job $JobId started"
    $ErrorActionPreference = "Continue"
    & powershell @launcherArgs *>> $OutputLogPath
    $exitCode = if ($null -ne $LASTEXITCODE) { [int]$LASTEXITCODE } else { 0 }
    $ErrorActionPreference = "Stop"
    Add-Content -LiteralPath $LogPath -Value "$(Get-Date -Format o) hostd launch job $JobId finished exit_code=$exitCode"
    if ($exitCode -eq 0) {
        Write-Status -Status "succeeded" -ExitCode $exitCode
    } else {
        Write-Status -Status "failed" -ExitCode $exitCode -ErrorMessage "launcher exited with code $exitCode"
    }
    exit $exitCode
}
catch {
    $message = $_.Exception.Message
    try { Add-Content -LiteralPath $LogPath -Value "$(Get-Date -Format o) hostd launch job $JobId failed: $message" } catch {}
    Write-Status -Status "failed" -ExitCode 1 -ErrorMessage $message
    exit 1
}
'@
    Set-Content -LiteralPath $runnerPath -Value $runner -Encoding UTF8

    $process = Start-Process powershell -WindowStyle Hidden -PassThru -ArgumentList @(
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        $runnerPath,
        "-ArgsPath",
        $argsPath,
        "-StatusPath",
        $statusPath,
        "-JobId",
        $jobId,
        "-AgentId",
        $AgentId,
        "-LogPath",
        $launchLogPath,
        "-OutputLogPath",
        $jobOutputLogPath
    )
    Write-LaunchStatus -Path $statusPath -JobId $jobId -AgentId $AgentId -Status "running" -Log $launchLogPath -ProcessId $process.Id
    return @{
        job_id = $jobId
        agent_id = $AgentId
        status = "running"
        pid = $process.Id
        log = $launchLogPath
        status_path = $statusPath
    }
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
if ([string]::IsNullOrWhiteSpace($TokenPath)) {
    $TokenPath = Join-Path $repoRoot ".maturana\hostd\token"
}
if ([string]::IsNullOrWhiteSpace($LogPath)) {
    $LogPath = Join-Path $repoRoot ".maturana\logs\hostd.log"
}
$script:LogPath = $LogPath
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $script:LogPath) | Out-Null
$script:JobDir = Join-Path $repoRoot ".maturana\hostd\jobs"
New-Item -ItemType Directory -Force -Path $script:JobDir | Out-Null
Write-HostdLog "maturana-hostd starting; user=$([Security.Principal.WindowsIdentity]::GetCurrent().Name)"

Write-HostdLog "checking elevation"
Assert-Elevated
Write-HostdLog "elevation ok"

$hostdToken = Initialize-Token -Path $TokenPath
Write-HostdLog "hostd token ready"
$listener = [Net.HttpListener]::new()
$listener.Prefixes.Add($BindPrefix)
Write-HostdLog "starting listener"
$listener.Start()
Write-HostdLog "maturana-hostd listening on $BindPrefix"

while ($listener.IsListening) {
    $context = $listener.GetContext()
    try {
        if (!(Assert-Authorized -Context $context -ExpectedToken $hostdToken)) {
            continue
        }
        $path = $context.Request.Url.AbsolutePath.Trim("/")
        if ($context.Request.HttpMethod -eq "GET" -and $path -eq "health") {
            Send-Json $context @{ ok = $true }
            continue
        }

        if ($context.Request.HttpMethod -eq "GET" -and $path -eq "vms") {
            $vms = Get-VM | Where-Object { $_.Name -like "maturana-*" } |
                ForEach-Object {
                    [pscustomobject]@{
                        name = $_.Name
                        state = "$($_.State)"
                        status = "$($_.Status)"
                        uptime = "$($_.Uptime)"
                        generation = $_.Generation
                        processor_count = $_.ProcessorCount
                        memory_startup = $_.MemoryStartup
                        ipv4 = Get-VMIPv4 -Name $_.Name
                    }
                }
            Send-Json $context @{ ok = $true; vms = @($vms) }
            continue
        }

        if ($context.Request.HttpMethod -eq "GET" -and $path -eq "agents/launch/status") {
            $jobId = $context.Request.QueryString.Get("job_id")
            if ([string]::IsNullOrWhiteSpace($jobId)) {
                throw "job_id is required"
            }
            $statusPath = Get-LaunchStatusPath -JobId $jobId
            $status = Read-LaunchStatus -Path $statusPath
            if (!$status) {
                Send-Json $context @{ ok = $false; error = "launch job not found: $jobId" } 404
                continue
            }
            if ($status.status -eq "running" -and $status.pid) {
                $process = Get-Process -Id ([int]$status.pid) -ErrorAction SilentlyContinue
                if (!$process) {
                    $status.status = "unknown"
                    $status.error = "launch process exited before writing final status"
                    $status.updated_at = Get-Date -Format o
                    $status | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $statusPath -Encoding UTF8
                }
            }
            Send-Json $context $status
            continue
        }

        if ($context.Request.HttpMethod -eq "POST" -and $path -eq "agents/launch/ubuntu") {
            $body = Read-JsonBody $context.Request
            $agentId = if ($body.agent_id) { [string]$body.agent_id } else { "codex-demo" }
            $harness = if ($body.harness) { [string]$body.harness } else { "codex" }
            if ($harness -notin @("codex", "claude-code", "opencode", "none")) {
                Send-Json $context @{ ok = $false; error = "unsupported harness: $harness" } 400
                continue
            }

            $args = [System.Collections.Generic.List[string]]::new()
            $args.Add("-NoProfile")
            $args.Add("-ExecutionPolicy")
            $args.Add("Bypass")
            $args.Add("-File")
            $args.Add((Join-Path $repoRoot "scripts\launch-ubuntu-cloudimg-hyperv.ps1"))
            Add-Arg -ArgList $args -Name "-AgentId" -Value $agentId
            Add-Arg -ArgList $args -Name "-BaseVhdxPath" -Value $body.base_vhdx_path
            Add-Arg -ArgList $args -Name "-SwitchName" -Value $body.switch_name
            Add-Arg -ArgList $args -Name "-SshUser" -Value $(if ($body.ssh_user) { $body.ssh_user } else { "ubuntu" })
            Add-Arg -ArgList $args -Name "-SshKeyPath" -Value $body.ssh_key_path
            Add-Arg -ArgList $args -Name "-Harness" -Value $harness
            Add-Arg -ArgList $args -Name "-HarnessAuthSource" -Value $body.harness_auth_source
            Add-Arg -ArgList $args -Name "-HarnessAuthGuestPath" -Value $body.harness_auth_guest_path
            Add-Arg -ArgList $args -Name "-AgentPrompt" -Value $body.agent_prompt
            Add-Arg -ArgList $args -Name "-AgentCommand" -Value $body.agent_command
            Add-Arg -ArgList $args -Name "-SessionId" -Value $body.session_id
            Add-Arg -ArgList $args -Name "-SessiondUrl" -Value $body.sessiond_url
            Add-Arg -ArgList $args -Name "-SessiondTokenPath" -Value $body.sessiond_token_path
            Add-Arg -ArgList $args -Name "-DiskSizeGB" -Value $body.disk_size_gb
            Add-Arg -ArgList $args -Name "-Vcpu" -Value $body.vcpu
            Add-Arg -ArgList $args -Name "-MemoryMiB" -Value $body.memory_mib
            Add-Arg -ArgList $args -Name "-ProxyPort" -Value $body.proxy_port
            Add-Arg -ArgList $args -Name "-ProxyCaCertPath" -Value $body.proxy_ca_cert_path
            if ($body.proxy_https) { $args.Add("-ProxyHttps") }
            if ($body.install_harness) { $args.Add("-InstallHarness") }
            if ($body.start_harness) { $args.Add("-StartHarness") }
            if ($body.provision_existing) { $args.Add("-ProvisionExisting") }
            if ($body.force) { $args.Add("-Force") }

            Write-HostdLog "launch requested; agent=$agentId harness=$harness"
            $job = Start-LaunchJob -AgentId $agentId -LauncherArgs $args
            Write-HostdLog "launch job started; agent=$agentId job_id=$($job.job_id) pid=$($job.pid)"
            Send-Json $context @{
                ok = $true
                accepted = $true
                job_id = $job.job_id
                agent_id = $agentId
                status = $job.status
                pid = $job.pid
                log = $job.log
                status_url = "/agents/launch/status?job_id=$($job.job_id)"
            } 202
            continue
        }

        if ($context.Request.HttpMethod -eq "POST" -and $path -eq "agents/stop") {
            $body = Read-JsonBody $context.Request
            $agentId = if ($body.agent_id) { [string]$body.agent_id } else { throw "agent_id is required" }
            $vmName = Get-AgentVmName -AgentId $agentId
            if (!(Get-VM -Name $vmName -ErrorAction SilentlyContinue)) {
                Send-Json $context @{ ok = $false; error = "VM not found: $vmName" } 404
                continue
            }
            Stop-VM -Name $vmName -Force -TurnOff
            Send-Json $context @{ ok = $true; vm = $vmName; state = "stopped" }
            continue
        }

        if ($context.Request.HttpMethod -eq "POST" -and $path -eq "agents/snapshot/take") {
            $body = Read-JsonBody $context.Request
            $agentId = if ($body.agent_id) { [string]$body.agent_id } else { throw "agent_id is required" }
            $name = if ($body.name) { Get-SnapshotName -Name ([string]$body.name) } else { throw "name is required" }
            $vmName = Get-AgentVmName -AgentId $agentId
            if (!(Get-VM -Name $vmName -ErrorAction SilentlyContinue)) {
                Send-Json $context @{ ok = $false; error = "VM not found: $vmName" } 404
                continue
            }
            Checkpoint-VM -Name $vmName -SnapshotName $name | Out-Null
            Send-Json $context @{ ok = $true; vm = $vmName; snapshot = $name }
            continue
        }

        if ($context.Request.HttpMethod -eq "POST" -and $path -eq "agents/snapshot/restore") {
            $body = Read-JsonBody $context.Request
            $agentId = if ($body.agent_id) { [string]$body.agent_id } else { throw "agent_id is required" }
            $name = if ($body.name) { Get-SnapshotName -Name ([string]$body.name) } else { throw "name is required" }
            $vmName = Get-AgentVmName -AgentId $agentId
            if (!(Get-VM -Name $vmName -ErrorAction SilentlyContinue)) {
                Send-Json $context @{ ok = $false; error = "VM not found: $vmName" } 404
                continue
            }
            $snapshot = Get-VMSnapshot -VMName $vmName -Name $name -ErrorAction SilentlyContinue
            if (!$snapshot) {
                Send-Json $context @{ ok = $false; error = "Snapshot not found: $name" } 404
                continue
            }
            Restore-VMSnapshot -VMSnapshot $snapshot -Confirm:$false
            Send-Json $context @{ ok = $true; vm = $vmName; snapshot = $name; restored = $true }
            continue
        }

        if ($context.Request.HttpMethod -eq "GET" -and $path -eq "agents/snapshot/list") {
            $agentId = $context.Request.QueryString.Get("agent_id")
            if ([string]::IsNullOrWhiteSpace($agentId)) {
                throw "agent_id is required"
            }
            $vmName = Get-AgentVmName -AgentId $agentId
            if (!(Get-VM -Name $vmName -ErrorAction SilentlyContinue)) {
                Send-Json $context @{ ok = $false; error = "VM not found: $vmName" } 404
                continue
            }
            $snapshots = @(Get-VMSnapshot -VMName $vmName -ErrorAction SilentlyContinue |
                Select-Object Name, CreationTime, SnapshotType)
            Send-Json $context @{ ok = $true; vm = $vmName; snapshots = $snapshots }
            continue
        }

        Send-Json $context @{ ok = $false; error = "unknown endpoint" } 404
    }
    catch {
        Write-HostdLog "request failed: $($_.Exception.Message)"
        Send-Json $context @{ ok = $false; error = $_.Exception.Message } 500
    }
}
