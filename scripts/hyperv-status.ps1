param(
    [string]$AgentId = "codex-demo",
    [string]$VmName = ""
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($VmName)) {
    $VmName = "maturana-$AgentId"
}

$vm = Get-VM -Name $VmName -ErrorAction SilentlyContinue
if (!$vm) {
    Write-Host "VM not found: $VmName"
    exit 1
}

$adapter = Get-VMNetworkAdapter -VMName $VmName
$ipv4 = $adapter.IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' -and $_ -notlike '169.254.*' }

[pscustomobject]@{
    Name = $vm.Name
    State = $vm.State
    Generation = $vm.Generation
    ProcessorCount = $vm.ProcessorCount
    MemoryStartup = $vm.MemoryStartup
    IPv4 = ($ipv4 -join ",")
} | Format-List
