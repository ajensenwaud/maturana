param(
    [string]$AgentId = "codex-demo",
    [string]$VmName = "",
    [Parameter(Mandatory=$true)]
    [string]$BaseVhdxPath,
    [string]$SwitchName = "Default Switch",
    [string]$SshUser = "ubuntu",
    [Parameter(Mandatory=$true)]
    [string]$SshKeyPath,
    [string]$AgentPrompt = "Inspect /agent/MATURANA.md and /agent/AGENTS.md, then report that the Maturana Codex guest harness is ready.",
    [string]$AgentCommand = "",
    [string]$HarnessAuthSource = "",
    [string]$HarnessAuthGuestPath = "/home/ubuntu/.codex",
    [string]$SessionId = "telegram-main",
    [string]$SessiondUrl = "",
    [string]$SessiondTokenPath = "",
    [int]$DiskSizeGB = 24,
    [int]$Vcpu = 2,
    [int]$MemoryMiB = 2048,
    [int]$ProxyPort = 0,
    [string]$ProxyCaCertPath = "",
    [ValidateSet("codex", "claude-code", "opencode", "none")]
    [string]$Harness = "codex",
    [switch]$InstallHarness,
    [switch]$StartHarness,
    [switch]$ProxyHttps,
    [switch]$ProvisionExisting,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

$script:RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$script:LogDir = Join-Path $script:RepoRoot ".maturana\logs"
New-Item -ItemType Directory -Force -Path $script:LogDir | Out-Null
$script:LogPath = Join-Path $script:LogDir "hyperv-launch-$AgentId.log"

trap {
    $message = @(
        "$(Get-Date -Format o) ERROR: $($_.Exception.Message)"
        "At: $($_.InvocationInfo.PositionMessage)"
        "Category: $($_.CategoryInfo)"
        "FullyQualifiedErrorId: $($_.FullyQualifiedErrorId)"
    ) -join [Environment]::NewLine
    try { Add-Content -LiteralPath $script:LogPath -Value $message } catch {}
    Write-Error $message
    exit 1
}

function Assert-Elevated {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run this from an elevated PowerShell session."
    }
}

function Write-Log {
    param([string]$Message)
    $line = "$(Get-Date -Format o) $Message"
    Add-Content -LiteralPath $script:LogPath -Value $line
    Write-Host $Message
}

function Invoke-Guest {
    param([string]$Ip, [string]$Command)
    ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=10 -i $SshKeyPath "$SshUser@$Ip" $Command
    if ($LASTEXITCODE -ne 0) {
        throw "Guest command failed with exit code ${LASTEXITCODE}: $Command"
    }
}

function Copy-ToGuest {
    param([string]$Ip, [string]$Source, [string]$Destination)
    scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=10 -i $SshKeyPath -r $Source "$SshUser@$Ip`:$Destination"
    if ($LASTEXITCODE -ne 0) {
        throw "Guest copy failed with exit code ${LASTEXITCODE}: $Source -> $Destination"
    }
}

function Get-SshPublicKey {
    $pubPath = "$SshKeyPath.pub"
    if (Test-Path -LiteralPath $pubPath) {
        return (Get-Content -LiteralPath $pubPath -Raw).Trim()
    }
    $publicKey = ssh-keygen.exe -y -f $SshKeyPath
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($publicKey)) {
        throw "Could not derive SSH public key from $SshKeyPath"
    }
    return $publicKey.Trim()
}

function New-SeedVhdx {
    param(
        [string]$Path,
        [string]$Hostname,
        [string]$PublicKey
    )

    if (Test-Path -LiteralPath $Path) {
        Dismount-VHD -Path $Path -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $Path -Force
    }

    $seedDir = Join-Path ([IO.Path]::GetDirectoryName($Path)) "cloud-init"
    New-Item -ItemType Directory -Force -Path $seedDir | Out-Null
    $userDataPath = Join-Path $seedDir "user-data"
    $metaDataPath = Join-Path $seedDir "meta-data"

    $userData = @"
#cloud-config
hostname: $Hostname
manage_etc_hosts: true
ssh_pwauth: false
disable_root: true
users:
  - default
  - name: $SshUser
    gecos: Maturana Agent
    groups: [adm, sudo]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    lock_passwd: true
    ssh_authorized_keys:
      - $PublicKey
runcmd:
  - [ systemctl, enable, --now, ssh ]
"@
    $metaData = @"
instance-id: $AgentId
local-hostname: $Hostname
"@
    Set-Content -LiteralPath $userDataPath -Value $userData -NoNewline
    Set-Content -LiteralPath $metaDataPath -Value $metaData -NoNewline

    Write-Log "Creating cloud-init seed VHDX..."
    New-VHD -Path $Path -SizeBytes 64MB -Dynamic | Out-Null
    $mounted = Mount-VHD -Path $Path -PassThru
    try {
        $disk = $mounted | Get-Disk
        Initialize-Disk -Number $disk.Number -PartitionStyle MBR
        $partition = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter
        Format-Volume -Partition $partition -FileSystem FAT32 -NewFileSystemLabel cidata -Confirm:$false | Out-Null
        $drive = "$($partition.DriveLetter):"
        Copy-Item -LiteralPath $userDataPath, $metaDataPath -Destination $drive -Force
    } finally {
        Dismount-VHD -Path $Path
    }
}

function Wait-GuestIp {
    param([string]$Name)
    for ($i = 0; $i -lt 90; $i++) {
        $adapter = Get-VMNetworkAdapter -VMName $Name
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
        if ($mac) {
            $neighbors = @(Get-NetNeighbor -AddressFamily IPv4 -ErrorAction SilentlyContinue |
                Where-Object {
                    ($_.LinkLayerAddress -replace '[^0-9A-Fa-f]', '').ToUpperInvariant() -eq $mac -and
                    $_.IPAddress -match '^\d+\.\d+\.\d+\.\d+$' -and
                    $_.IPAddress -notlike '169.254.*' -and
                    $_.IPAddress -notlike '0.*' -and
                    $_.IPAddress -notlike '127.*'
                })
            if ($neighbors.Count -gt 0) {
                return $neighbors[0].IPAddress
            }
        }
        Start-Sleep -Seconds 5
    }

    $adapterSummary = Get-VMNetworkAdapter -VMName $Name |
        Select-Object Name, Status, SwitchName, MacAddress, IPAddresses |
        ConvertTo-Json -Compress -Depth 3
    $integrationSummary = Get-VMIntegrationService -VMName $Name -ErrorAction SilentlyContinue |
        Select-Object Name, Enabled, PrimaryStatusDescription, SecondaryStatusDescription |
        ConvertTo-Json -Compress -Depth 3
    Write-Log "Network adapter diagnostic: $adapterSummary"
    Write-Log "Integration service diagnostic: $integrationSummary"
    throw "VM started but no IPv4 address was discovered."
}

function Wait-Ssh {
    param([string]$Ip)
    for ($i = 0; $i -lt 180; $i++) {
        $previousNativeErrorMode = $global:PSNativeCommandUseErrorActionPreference
        $previousErrorAction = $ErrorActionPreference
        try {
            $global:PSNativeCommandUseErrorActionPreference = $false
            $ErrorActionPreference = "Continue"
            $result = ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=5 -i $SshKeyPath "$SshUser@$Ip" "echo alive" 2>$null
            if ($result -match "alive") {
                return
            }
        } catch {
            # SSH may accept TCP before the daemon has completed banner exchange.
        } finally {
            $global:PSNativeCommandUseErrorActionPreference = $previousNativeErrorMode
            $ErrorActionPreference = $previousErrorAction
        }
        Start-Sleep -Seconds 5
    }
    throw "SSH did not become ready at $Ip."
}

function Expand-GuestRoot {
    param([string]$Ip)
    $command = @'
set -eu
root_source="$(findmnt -n -o SOURCE /)"
disk_name="$(lsblk -no PKNAME "$root_source" | head -n1)"
part_name="$(basename "$root_source")"
part_num="$(cat "/sys/class/block/$part_name/partition")"
if [ -n "$disk_name" ] && [ -n "$part_num" ]; then
  sudo growpart "/dev/$disk_name" "$part_num" || true
fi
fs_type="$(findmnt -n -o FSTYPE /)"
if [ "$fs_type" = "xfs" ]; then
  sudo xfs_growfs /
else
  sudo resize2fs "$root_source" || true
fi
df -h /
'@
    Invoke-Guest -Ip $Ip -Command $command
}

function Install-Harness {
    param([string]$Ip)
    if ($Harness -eq "none") {
        return
    }
    $packageCommand = @'
set -eu
export DEBIAN_FRONTEND=noninteractive
sudo cloud-init status --wait || true
while sudo fuser /var/lib/dpkg/lock-frontend /var/lib/dpkg/lock /var/lib/apt/lists/lock >/dev/null 2>&1; do
  sleep 5
done
sudo dpkg --configure -a
sudo apt-get clean
sudo apt-get update
sudo apt-get install -y ca-certificates curl git nodejs npm ripgrep
'@
    if ($Harness -eq "codex") {
        $packageCommand = "$packageCommand`nsudo npm install -g @openai/codex"
    } elseif ($Harness -eq "claude-code") {
        $packageCommand = "$packageCommand`nsudo npm install -g @anthropic-ai/claude-code"
    } elseif ($Harness -eq "opencode") {
        $packageCommand = "$packageCommand`nsudo npm install -g opencode-ai"
    }
    Invoke-Guest -Ip $Ip -Command $packageCommand
}

Assert-Elevated

if ([string]::IsNullOrWhiteSpace($VmName)) {
    $VmName = "maturana-$AgentId"
}

$baseDisk = Resolve-Path $BaseVhdxPath
$agentDir = Join-Path $script:RepoRoot ".maturana\agents\$AgentId"
$stateDir = Join-Path $agentDir "state"
New-Item -ItemType Directory -Force -Path $stateDir | Out-Null

Write-Log "launch starting; agent=$AgentId vm=$VmName image=$baseDisk"

$existingVm = Get-VM -Name $VmName -ErrorAction SilentlyContinue
$usingExistingVm = $false
if ($existingVm -and $Force) {
    Write-Log "Removing existing VM $VmName..."
    Stop-VM -Name $VmName -Force -TurnOff -ErrorAction SilentlyContinue
    Remove-VM -Name $VmName -Force
    $existingVm = $null
}
if ($existingVm -and $ProvisionExisting) {
    Write-Log "Provisioning existing VM $VmName..."
    $usingExistingVm = $true
} elseif ($existingVm) {
    throw "VM already exists: $VmName. Use -Force to replace it."
}

$agentDisk = Join-Path $stateDir "$VmName-os.vhdx"
$seedDisk = Join-Path $stateDir "$VmName-seed.vhdx"
if ($Force -and !$usingExistingVm) {
    Dismount-VHD -Path $seedDisk -ErrorAction SilentlyContinue
    Get-ChildItem -LiteralPath $stateDir -Filter "$VmName-*" -ErrorAction SilentlyContinue |
        Remove-Item -Force -Recurse
}

if (!$usingExistingVm) {
    Write-Log "Copying base VHDX..."
    Copy-Item -LiteralPath $baseDisk -Destination $agentDisk -Force
    Write-Log "Expanding agent VHDX to ${DiskSizeGB}GB..."
    Resize-VHD -Path $agentDisk -SizeBytes ($DiskSizeGB * 1GB)
    New-SeedVhdx -Path $seedDisk -Hostname $VmName -PublicKey (Get-SshPublicKey)

    Write-Log "Creating Hyper-V Generation 2 VM..."
    $memoryBytes = [int64]$MemoryMiB * 1MB
    New-VM -Name $VmName -Generation 2 -MemoryStartupBytes $memoryBytes -VHDPath $agentDisk -SwitchName $SwitchName | Out-Null
    Set-VMFirmware -VMName $VmName -EnableSecureBoot Off
    Set-VMProcessor -VMName $VmName -Count $Vcpu
    Set-VMMemory -VMName $VmName -DynamicMemoryEnabled $false -StartupBytes $memoryBytes
    Set-VM -Name $VmName -AutomaticCheckpointsEnabled $false
    Add-VMHardDiskDrive -VMName $VmName -ControllerType SCSI -Path $seedDisk
}

if ((Get-VM -Name $VmName).State -ne "Running") {
    Write-Log "Starting VM..."
    Start-VM -Name $VmName
} else {
    Write-Log "VM already running."
}
$ip = Wait-GuestIp -Name $VmName
Write-Log "VM IPv4: $ip"
Wait-Ssh -Ip $ip
Write-Log "SSH ready."
Write-Log "Expanding guest root filesystem..."
Expand-GuestRoot -Ip $ip

Invoke-Guest -Ip $ip -Command "sudo mkdir -p /agent /workspace /memory /wiki /opt/maturana/bin /var/log/maturana && sudo chown -R ${SshUser}:${SshUser} /agent /workspace /memory /wiki /var/log/maturana"
foreach ($name in @("MATURANA.md", "AGENTS.md", "SOUL.md")) {
    $path = Join-Path $agentDir $name
    if (Test-Path $path) {
        Copy-ToGuest -Ip $ip -Source $path -Destination "/tmp/$name"
        Invoke-Guest -Ip $ip -Command "sudo mv /tmp/$name /agent/$name && sudo chown ${SshUser}:${SshUser} /agent/$name"
    }
}

if ($ProxyPort -gt 0) {
    if ($ProxyHttps -and [string]::IsNullOrWhiteSpace($ProxyCaCertPath)) {
        throw "ProxyHttps requires ProxyCaCertPath."
    }
    $proxyHttpsValue = if ($ProxyHttps) { "1" } else { "0" }
    $proxyEnv = @"
MATURANA_USE_HOST_PROXY=1
MATURANA_PROXY_PORT=$ProxyPort
MATURANA_PROXY_HTTPS=$proxyHttpsValue
NO_PROXY=localhost,127.0.0.1,::1
"@
    $proxyEnvPath = Join-Path $stateDir "proxy.env"
    $proxyEnv = $proxyEnv -replace "`r`n", "`n"
    Set-Content -LiteralPath $proxyEnvPath -Value $proxyEnv -NoNewline
    Copy-ToGuest -Ip $ip -Source $proxyEnvPath -Destination "/tmp/proxy.env"
    Invoke-Guest -Ip $ip -Command "sudo mv /tmp/proxy.env /agent/proxy.env && sudo chown ${SshUser}:${SshUser} /agent/proxy.env && sudo chmod 0644 /agent/proxy.env"
    if ($ProxyHttps) {
        if (!(Test-Path -LiteralPath $ProxyCaCertPath)) {
            throw "Proxy CA certificate does not exist: $ProxyCaCertPath"
        }
        Copy-ToGuest -Ip $ip -Source $ProxyCaCertPath -Destination "/tmp/maturana-pipelock-ca.crt"
        Invoke-Guest -Ip $ip -Command "sudo mv /tmp/maturana-pipelock-ca.crt /usr/local/share/ca-certificates/maturana-pipelock-ca.crt && sudo chmod 0644 /usr/local/share/ca-certificates/maturana-pipelock-ca.crt && sudo update-ca-certificates"
        Write-Log "Installed Maturana pipelock CA in guest trust store."
    }
    Write-Log "Configured guest proxy environment on port $ProxyPort."
}

if (![string]::IsNullOrWhiteSpace($HarnessAuthSource)) {
    if (!(Test-Path $HarnessAuthSource)) {
        throw "Harness auth source does not exist: $HarnessAuthSource"
    }
    Write-Log "Injecting harness auth to $HarnessAuthGuestPath..."
    Copy-ToGuest -Ip $ip -Source $HarnessAuthSource -Destination "/tmp/maturana-harness-auth"
    if ($Harness -eq "opencode") {
        Invoke-Guest -Ip $ip -Command "sudo mkdir -p '$HarnessAuthGuestPath' && sudo cp -a /tmp/maturana-harness-auth/. '$HarnessAuthGuestPath/' && sudo rm -rf /tmp/maturana-harness-auth && sudo chown -R ${SshUser}:${SshUser} '$HarnessAuthGuestPath/.config' '$HarnessAuthGuestPath/.local' 2>/dev/null || true && chmod -R go-rwx '$HarnessAuthGuestPath/.config' '$HarnessAuthGuestPath/.local' 2>/dev/null || true && if [ -f '$HarnessAuthGuestPath/.maturana-env' ]; then sudo chown ${SshUser}:${SshUser} '$HarnessAuthGuestPath/.maturana-env' && sudo chmod 0600 '$HarnessAuthGuestPath/.maturana-env'; fi"
    } else {
        Invoke-Guest -Ip $ip -Command "sudo mkdir -p '$([IO.Path]::GetDirectoryName($HarnessAuthGuestPath).Replace('\','/'))' && sudo rm -rf '$HarnessAuthGuestPath' && sudo mv /tmp/maturana-harness-auth '$HarnessAuthGuestPath' && sudo chown -R ${SshUser}:${SshUser} '$HarnessAuthGuestPath' && chmod -R go-rwx '$HarnessAuthGuestPath'"
    }
}

if ($InstallHarness) {
    Write-Log "Installing harness $Harness..."
    Install-Harness -Ip $ip
}

if ($Harness -ne "none") {
    if ([string]::IsNullOrWhiteSpace($SessiondUrl)) {
        $SessiondUrl = "__MATURANA_DEFAULT_SESSIOND_URL__"
    }
    $sessiondToken = ""
    if (![string]::IsNullOrWhiteSpace($SessiondTokenPath) -and (Test-Path -LiteralPath $SessiondTokenPath)) {
        $sessiondToken = (Get-Content -LiteralPath $SessiondTokenPath -Raw).Trim()
    }
    $sessiondEnv = @"
MATURANA_AGENT_ID=$AgentId
MATURANA_SESSION_ID=$SessionId
MATURANA_SESSIOND_URL=$SessiondUrl
MATURANA_SESSIOND_TOKEN=$sessiondToken
MATURANA_HARNESS=$Harness
CODEX_HOME=$HarnessAuthGuestPath
"@
    $sessiondEnvPath = Join-Path $stateDir "sessiond.env"
    $sessiondEnv = $sessiondEnv -replace "`r`n", "`n"
    Set-Content -LiteralPath $sessiondEnvPath -Value $sessiondEnv -NoNewline
    Copy-ToGuest -Ip $ip -Source $sessiondEnvPath -Destination "/tmp/sessiond.env"
    Invoke-Guest -Ip $ip -Command "sudo mv /tmp/sessiond.env /agent/sessiond.env && sudo chown ${SshUser}:${SshUser} /agent/sessiond.env && sudo chmod 0600 /agent/sessiond.env"

    if (![string]::IsNullOrWhiteSpace($AgentCommand)) {
        $commandPath = Join-Path $stateDir "run-command"
        Set-Content -LiteralPath $commandPath -Value $AgentCommand -NoNewline
        Copy-ToGuest -Ip $ip -Source $commandPath -Destination "/tmp/run-command"
        Invoke-Guest -Ip $ip -Command "sudo mv /tmp/run-command /agent/run-command && sudo chown ${SshUser}:${SshUser} /agent/run-command && sudo chmod 0644 /agent/run-command"
    } elseif (![string]::IsNullOrWhiteSpace($AgentPrompt)) {
        $promptPath = Join-Path $stateDir "prompt.txt"
        Set-Content -LiteralPath $promptPath -Value $AgentPrompt -NoNewline
        Copy-ToGuest -Ip $ip -Source $promptPath -Destination "/tmp/prompt.txt"
        Invoke-Guest -Ip $ip -Command "sudo mv /tmp/prompt.txt /agent/prompt.txt && sudo chown ${SshUser}:${SshUser} /agent/prompt.txt && sudo chmod 0644 /agent/prompt.txt"
    }

    $runner = @"
#!/usr/bin/env bash
set -euo pipefail
if [ -f /agent/sessiond.env ]; then
  set -a
  . /agent/sessiond.env
  set +a
fi
export MATURANA_AGENT_ID="`${MATURANA_AGENT_ID:-$AgentId}"
export MATURANA_SESSION_ID="`${MATURANA_SESSION_ID:-$SessionId}"
export MATURANA_HARNESS="`${MATURANA_HARNESS:-$Harness}"
export CODEX_HOME="`${CODEX_HOME:-${HarnessAuthGuestPath}}"
if [ "`${MATURANA_HARNESS}" = "opencode" ] && [ -f "`$HOME/.maturana-env" ]; then
  set -a
  . "`$HOME/.maturana-env"
  set +a
fi
if [ -f /agent/proxy.env ]; then
  set -a
  . /agent/proxy.env
  set +a
  if [ "`${MATURANA_USE_HOST_PROXY:-0}" = "1" ] && [ -n "`${MATURANA_PROXY_PORT:-}" ]; then
    host_gateway="`$(ip route | awk '/default/ {print `$3; exit}')"
    if [ -n "`$host_gateway" ]; then
      export HTTP_PROXY="http://`$host_gateway:`${MATURANA_PROXY_PORT}"
      export http_proxy="`$HTTP_PROXY"
      if [ "`${MATURANA_PROXY_HTTPS:-0}" = "1" ]; then
        export HTTPS_PROXY="`$HTTP_PROXY"
        export https_proxy="`$HTTP_PROXY"
      fi
      export NO_PROXY="`${NO_PROXY:-localhost,127.0.0.1,::1}"
      export no_proxy="`$NO_PROXY"
    fi
  fi
fi
mkdir -p /var/log/maturana /workspace
cd /workspace

echo "Maturana $Harness agent $AgentId starting"

echo "Maturana $Harness agent $AgentId ready"
sessiond_url="`${MATURANA_SESSIOND_URL}"
if [ "`$sessiond_url" = "__MATURANA_DEFAULT_SESSIOND_URL__" ]; then
  host_gateway="`$(ip route | awk '/default/ {print `$3; exit}')"
  sessiond_url="http://`$host_gateway:47834"
fi
headers=(-H "content-type: application/json")
if [ -n "`${MATURANA_SESSIOND_TOKEN:-}" ]; then
  headers+=(-H "x-maturana-session-token: `${MATURANA_SESSIOND_TOKEN}")
fi
heartbeat() {
  status="`$1"
  message_id="`${2:-}"
  error="`${3:-}"
  heartbeat_body="`$(MATURANA_WORKER_STATUS="`$status" MATURANA_WORKER_MESSAGE_ID="`$message_id" MATURANA_WORKER_ERROR="`$error" python3 - <<'PY'
import json, os
print(json.dumps({
  "agent_id": os.environ["MATURANA_AGENT_ID"],
  "session_id": os.environ["MATURANA_SESSION_ID"],
  "status": os.environ["MATURANA_WORKER_STATUS"],
  "message_id": os.environ.get("MATURANA_WORKER_MESSAGE_ID") or None,
  "error": os.environ.get("MATURANA_WORKER_ERROR") or None,
}))
PY
)"
  curl -fsS -X POST "`$sessiond_url/session/heartbeat" "`${headers[@]}" --data "`$heartbeat_body" >/dev/null 2>>/var/log/maturana/worker.err.log || true
}
while true; do
  date -Is > /var/log/maturana/heartbeat
  heartbeat idle
  claim_body="`$(python3 - <<'PY'
import json, os
print(json.dumps({"agent_id": os.environ["MATURANA_AGENT_ID"], "session_id": os.environ["MATURANA_SESSION_ID"], "limit": 1}))
PY
)"
  claim="`$(curl -fsS -X POST "`$sessiond_url/session/claim" "`${headers[@]}" --data "`$claim_body" 2>>/var/log/maturana/worker.err.log || true)"
  count="`$(printf '%s' "`$claim" | python3 -c 'import json,sys; print(len(json.loads(sys.stdin.read() or "{\"messages\":[]}").get("messages", [])))' 2>/dev/null || echo 0)"
  if [ "`$count" = "0" ]; then
    sleep 2
    continue
  fi
  printf '%s' "`$claim" > /tmp/maturana-session-claim.json
  msg_id="`$(python3 - <<'PY'
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
print(d["messages"][0]["id"])
PY
)"
  heartbeat claimed "`$msg_id"
  channel="`$(python3 - <<'PY'
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
print(d["messages"][0]["channel"])
PY
)"
  platform_id="`$(python3 - <<'PY'
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
print(d["messages"][0]["platform_id"])
PY
)"
  thread_id="`$(python3 - <<'PY'
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
print(d["messages"][0].get("thread_id") or "")
PY
)"
  python3 - <<'PY' >/tmp/maturana-session-prompt.txt
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
c=json.loads(d["messages"][0]["content"])
print(c.get("prompt") or c.get("text") or "")
PY
  response=""
  if [ "$Harness" = "codex" ]; then
    if codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -C /workspace -o /tmp/maturana-session-response.txt "`$(cat /tmp/maturana-session-prompt.txt)" >>/var/log/maturana/worker.out.log 2>>/var/log/maturana/worker.err.log; then
      response="`$(cat /tmp/maturana-session-response.txt)"
    else
      response="I hit an error while processing that message."
    fi
  elif [ "$Harness" = "claude-code" ]; then
    if claude -p "`$(cat /tmp/maturana-session-prompt.txt)" >/tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log; then
      response="`$(cat /tmp/maturana-session-response.txt)"
    else
      response="I hit an error while processing that message."
    fi
  elif [ "$Harness" = "opencode" ]; then
    opencode_args=(run)
    if [ -n "`${OPENROUTER_API_KEY:-}" ]; then
      opencode_args+=(-m openrouter/anthropic/claude-sonnet-4.5)
    fi
    opencode_args+=("`$(cat /tmp/maturana-session-prompt.txt)")
    if opencode "`${opencode_args[@]}" >/tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log; then
      response="`$(cat /tmp/maturana-session-response.txt)"
      if [ -z "`$response" ] && [ -f "`$HOME/.local/share/opencode/opencode.db" ]; then
        response="`$(python3 - <<'PY'
import json
import os
import sqlite3

db = os.path.expanduser("~/.local/share/opencode/opencode.db")
con = sqlite3.connect(db)
rows = con.execute(
    """
    select part.data
    from part
    join message on message.id = part.message_id
    where json_extract(part.data, '$.type') = 'text'
      and json_extract(message.data, '$.role') = 'assistant'
      and message.data like '%"cwd":"/workspace"%'
    order by part.time_updated desc
    limit 1
    """
).fetchall()
if rows:
    print(json.loads(rows[0][0]).get("text", ""))
PY
)"
      fi
    else
      response="I hit an error while processing that message."
    fi
  else
    response="Unsupported harness: $Harness"
  fi
  if [ -z "`$response" ]; then
    response="I processed that message but did not receive a text response from the harness."
  fi
  export MATURANA_MSG_ID="`$msg_id" MATURANA_CHANNEL="`$channel" MATURANA_PLATFORM_ID="`$platform_id" MATURANA_THREAD_ID="`$thread_id" MATURANA_RESPONSE="`$response"
  outbound_body="`$(python3 - <<'PY'
import json, os
print(json.dumps({
  "agent_id": os.environ["MATURANA_AGENT_ID"],
  "session_id": os.environ["MATURANA_SESSION_ID"],
  "in_reply_to": os.environ["MATURANA_MSG_ID"],
  "kind": "chat",
  "channel": os.environ["MATURANA_CHANNEL"],
  "platform_id": os.environ["MATURANA_PLATFORM_ID"],
  "thread_id": os.environ.get("MATURANA_THREAD_ID") or None,
  "content": json.dumps({"text": os.environ["MATURANA_RESPONSE"]}),
}))
PY
)"
  if ! curl -fsS -X POST "`$sessiond_url/session/outbound" "`${headers[@]}" --data "`$outbound_body" >/dev/null 2>>/var/log/maturana/worker.err.log; then
    heartbeat error "`$msg_id" "failed to post outbound"
    sleep 2
    continue
  fi
  complete_body="`$(python3 - <<'PY'
import json, os
print(json.dumps({"agent_id": os.environ["MATURANA_AGENT_ID"], "session_id": os.environ["MATURANA_SESSION_ID"], "message_ids": [os.environ["MATURANA_MSG_ID"]]}))
PY
)"
  if curl -fsS -X POST "`$sessiond_url/session/complete" "`${headers[@]}" --data "`$complete_body" >/dev/null 2>>/var/log/maturana/worker.err.log; then
    heartbeat completed "`$msg_id"
  else
    heartbeat error "`$msg_id" "failed to mark complete"
  fi
done
"@
    $runnerPath = Join-Path $stateDir "run-agent.sh"
    $runner = $runner -replace "`r`n", "`n"
    Set-Content -LiteralPath $runnerPath -Value $runner -NoNewline
    Copy-ToGuest -Ip $ip -Source $runnerPath -Destination "/tmp/run-agent.sh"
    Invoke-Guest -Ip $ip -Command "sudo mv /tmp/run-agent.sh /opt/maturana/bin/run-agent.sh && sudo chmod 0755 /opt/maturana/bin/run-agent.sh"

    $service = @"
[Unit]
Description=Maturana $Harness agent $AgentId
After=network-online.target
Wants=network-online.target

[Service]
User=$SshUser
WorkingDirectory=/workspace
ExecStart=/opt/maturana/bin/run-agent.sh
Restart=on-failure
RestartSec=10
StandardOutput=append:/var/log/maturana/agent.log
StandardError=append:/var/log/maturana/agent.err.log

[Install]
WantedBy=multi-user.target
"@
    $servicePath = Join-Path $stateDir "maturana-agent.service"
    $service = $service -replace "`r`n", "`n"
    Set-Content -LiteralPath $servicePath -Value $service -NoNewline
    Copy-ToGuest -Ip $ip -Source $servicePath -Destination "/tmp/maturana-agent.service"
    Invoke-Guest -Ip $ip -Command "sudo mv /tmp/maturana-agent.service /etc/systemd/system/maturana-agent.service && sudo systemctl daemon-reload && sudo systemctl enable maturana-agent.service"
    if ($StartHarness) {
        Invoke-Guest -Ip $ip -Command "sudo systemctl restart maturana-agent.service"
    }
}

Write-Log "launch complete."
Write-Host "SSH: ssh -i `"$SshKeyPath`" $SshUser@$ip"
Write-Host "Log: $script:LogPath"
