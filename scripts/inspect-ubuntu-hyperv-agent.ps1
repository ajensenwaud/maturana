param(
    [string]$AgentId = "codex-demo",
    [string]$VmName = "",
    [string]$Ip = "",
    [string]$SshUser = "ubuntu",
    [Parameter(Mandatory=$true)]
    [string]$SshKeyPath
)

$ErrorActionPreference = "Stop"

function Get-GuestIp {
    param([string]$Name)

    $vm = Get-VM -Name $Name -ErrorAction SilentlyContinue
    if (!$vm) {
        throw "VM not found: $Name"
    }

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
    $neighbor = Get-NetNeighbor -AddressFamily IPv4 -ErrorAction SilentlyContinue |
        Where-Object {
            ($_.LinkLayerAddress -replace '[^0-9A-Fa-f]', '').ToUpperInvariant() -eq $mac -and
            $_.IPAddress -match '^\d+\.\d+\.\d+\.\d+$' -and
            $_.IPAddress -notlike '169.254.*'
        } |
        Select-Object -First 1
    if ($neighbor) {
        return $neighbor.IPAddress
    }

    throw "No IPv4 address found for $Name"
}

if ([string]::IsNullOrWhiteSpace($VmName)) {
    $VmName = "maturana-$AgentId"
}

if ([string]::IsNullOrWhiteSpace($Ip)) {
    $Ip = Get-GuestIp -Name $VmName
}

$remote = @'
set -eu
echo "guest: $(hostname)"
echo "codex: $(command -v codex 2>/dev/null || true)"
codex --version 2>/dev/null || true
echo "service: $(systemctl is-active maturana-agent.service 2>/dev/null || true)"
echo "rootfs: $(df -h / | awk 'NR==2 {print $2 " total, " $4 " free"}')"
echo "heartbeat: $(cat /var/log/maturana/heartbeat 2>/dev/null || true)"
echo "--- last-message ---"
cat /var/log/maturana/last-message.txt 2>/dev/null || true
echo
echo "--- agent-log-tail ---"
tail -n 20 /var/log/maturana/agent.log 2>/dev/null || true
'@

Write-Host "VM: $VmName"
Write-Host "IP: $Ip"
ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=10 -i $SshKeyPath "$SshUser@$Ip" $remote
if ($LASTEXITCODE -ne 0) {
    throw "SSH inspect failed with exit code $LASTEXITCODE"
}
