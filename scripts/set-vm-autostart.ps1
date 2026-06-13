# Enable zero-touch reboot recovery for existing Maturana Hyper-V agent VMs:
# set AutomaticStartAction = Start so they boot with the host without a login.
# Staggered AutomaticStartDelay avoids a thundering herd of microVMs at boot.
# Idempotent; safe to re-run. New VMs get this from launch-ubuntu-cloudimg-hyperv.ps1.
param(
    [string]$NamePattern = "maturana-*",
    [int]$BaseDelaySeconds = 30,
    [int]$StaggerSeconds = 15
)

$ErrorActionPreference = "Stop"

$vms = @(Get-VM -Name $NamePattern -ErrorAction SilentlyContinue)
if ($vms.Count -eq 0) {
    Write-Host "No VMs matched '$NamePattern' — nothing to do."
    return
}

$i = 0
foreach ($vm in $vms) {
    $delay = $BaseDelaySeconds + ($StaggerSeconds * $i)
    Set-VM -VM $vm -AutomaticStartAction Start -AutomaticStartDelay $delay
    Write-Host "  $($vm.Name): AutomaticStartAction=Start, delay=${delay}s"
    $i++
}

Write-Host "Set auto-start on $($vms.Count) VM(s)."
