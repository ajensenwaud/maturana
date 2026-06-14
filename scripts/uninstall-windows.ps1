# Maturana Windows uninstaller. Removes the boot tasks, running processes, and
# Hyper-V agent VMs. By default it KEEPS your data (the repo + .maturana, which
# holds credentials/agents); -ResetState wipes only the .maturana runtime state
# (clean slate, repo kept); -Purge removes everything.
#
#   .\scripts\uninstall-windows.ps1              # remove services + VMs, keep data
#   .\scripts\uninstall-windows.ps1 -ResetState  # also wipe .maturana, KEEP the repo
#   .\scripts\uninstall-windows.ps1 -Purge       # also delete the whole install dir
#   irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/uninstall-windows.ps1 | iex
#
# It prompts for confirmation (listing the exact Hyper-V VMs it will delete)
# before changing anything; pass -Yes to skip the prompt in scripted runs.
#
# -ResetState is the clean-reinstall path: tear everything down and clear runtime
# state (agents, host-auth, pipelock, keys, images) while leaving the source
# checkout in place, so a re-run of the installer reuses the repo. Back up
# credentials first if you need them - this removes .maturana.
param(
    [switch]$Purge,
    [switch]$ResetState,
    [switch]$Yes,
    [string]$Dir
)
$ErrorActionPreference = "Stop"
if ($env:MATURANA_PURGE -eq '1') { $Purge = $true }
if ($env:MATURANA_RESET_STATE -eq '1') { $ResetState = $true }
if ($env:MATURANA_YES -eq '1') { $Yes = $true }

# Resolve the install dir: explicit -Dir, else this repo (when run from a file),
# else the default clone location.
if (-not $Dir) {
    if ($PSScriptRoot) { $Dir = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path }
    else { $Dir = Join-Path $env:USERPROFILE "maturana" }
}

# Self-elevate: unregistering the SYSTEM hostd task and removing Hyper-V VMs need admin.
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) {
    Write-Host "Requesting elevation (UAC) to remove services + Hyper-V VMs..."
    if ($PSCommandPath) {
        $fwd = @('-Dir', $Dir)
        if ($Purge) { $fwd += '-Purge' }
        if ($ResetState) { $fwd += '-ResetState' }
        if ($Yes) { $fwd += '-Yes' }
        $launch = @('-NoExit','-NoProfile','-ExecutionPolicy','Bypass','-File', $PSCommandPath) + $fwd
    } else {
        # Running via irm|iex (no file on disk): re-fetch elevated. -Purge /
        # -ResetState / -Yes cross the boundary via env vars.
        $pf = if ($Purge) { "`$env:MATURANA_PURGE=1; " } else { "" }
        if ($ResetState) { $pf += "`$env:MATURANA_RESET_STATE=1; " }
        if ($Yes) { $pf += "`$env:MATURANA_YES=1; " }
        $launch = @('-NoExit','-NoProfile','-ExecutionPolicy','Bypass','-Command',
                    "$pf irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/uninstall-windows.ps1 | iex")
    }
    try { Start-Process powershell.exe -Verb RunAs -ArgumentList $launch | Out-Null }
    catch { throw "Elevation declined. Re-run from an elevated PowerShell." }
    Write-Host "Elevated uninstaller launched in a new window."
    return
}

# Confirm before anything destructive. Runs in the elevated window so the user
# can see exactly which Hyper-V VMs will be deleted. Skip with -Yes (e.g. in
# scripted/non-interactive runs).
if (-not $Yes) {
    $vms = @(Get-VM -Name 'maturana-*' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name)
    Write-Host ""
    Write-Host "About to uninstall Maturana. This will:"
    Write-Host "  - stop and remove the boot tasks (MaturanaUp/Web/Hostd) and running processes"
    if ($vms.Count -gt 0) {
        Write-Host "  - PERMANENTLY DELETE these Hyper-V VMs and their virtual disks:"
        foreach ($v in $vms) { Write-Host "        $v" }
    } else {
        Write-Host "  - remove any maturana-* Hyper-V VMs (none found right now)"
    }
    if ($Purge) {
        Write-Host "  - DELETE the entire install dir: $Dir (repo + .maturana, INCLUDING credentials)"
    } elseif ($ResetState) {
        Write-Host ("  - WIPE runtime state: " + (Join-Path $Dir '.maturana') + " (agents, host-auth, pipelock, keys, images)")
        Write-Host "    The source checkout at $Dir is kept."
    } else {
        Write-Host "  - keep your data ($Dir): repo + .maturana"
    }
    Write-Host ""
    $ans = Read-Host "Type 'yes' to permanently delete the above (anything else aborts)"
    if ($ans -ne 'yes') {
        Write-Host "Aborted - nothing was changed."
        return
    }
}

Write-Host "Uninstalling Maturana..."

# 1. Boot tasks (incl. SYSTEM hostd).
foreach ($t in 'MaturanaUp','MaturanaWeb','MaturanaHostd') {
    Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
    Unregister-ScheduledTask -TaskName $t -Confirm:$false -ErrorAction SilentlyContinue
    Write-Host "  removed task $t"
}

# 2. Running processes.
Get-Process maturana -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 2

# 3. Hyper-V agent VMs + their disks.
foreach ($vm in Get-VM -Name 'maturana-*' -ErrorAction SilentlyContinue) {
    $disks = @((Get-VMHardDiskDrive -VM $vm).Path)
    Stop-VM -VM $vm -TurnOff -Force -ErrorAction SilentlyContinue
    Remove-VM -VM $vm -Force -ErrorAction SilentlyContinue
    foreach ($d in $disks) { if ($d -and (Test-Path -LiteralPath $d)) { Remove-Item -LiteralPath $d -Force -ErrorAction SilentlyContinue } }
    Write-Host "  removed VM $($vm.Name)"
}

# 4. Stale Startup-folder launchers (older approach).
Get-ChildItem ([Environment]::GetFolderPath('Startup')) -Filter 'Maturana*.cmd' -ErrorAction SilentlyContinue |
    ForEach-Object { Remove-Item -Force $_.FullName -ErrorAction SilentlyContinue }

# 5. Remove data: whole dir (-Purge), runtime state only (-ResetState), or keep.
# Take ownership + reset ACLs first so inheritance-stripped key/token files delete.
function Remove-Tree($target, $label) {
    if (Test-Path -LiteralPath $target) {
        & takeown.exe /f $target /r /d y *> $null
        & icacls.exe $target /grant "*S-1-5-32-544:(F)" /t /c *> $null
        Remove-Item -Recurse -Force -LiteralPath $target -ErrorAction SilentlyContinue
        if (Test-Path -LiteralPath $target) {
            Write-Host "  WARNING: $target still present after removal"
        } else {
            Write-Host "  removed $label"
        }
    }
}
if ($Purge) {
    Set-Location $env:USERPROFILE   # don't sit inside the directory we delete
    Remove-Tree $Dir "$Dir (repo + .maturana, including credentials)"
} elseif ($ResetState) {
    $state = Join-Path $Dir ".maturana"
    Remove-Tree $state "$state (runtime state; repo kept)"
    Write-Host "  kept the repo checkout at $Dir"
} else {
    Write-Host "  kept $Dir (repo + .maturana data). Re-run with -ResetState (wipe state, keep repo) or -Purge (remove all)."
}

Write-Host "Maturana uninstalled."
