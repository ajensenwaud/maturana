# Maturana Windows uninstaller. Removes the boot tasks, running processes, and
# Hyper-V agent VMs. By default it KEEPS your data (the repo + .maturana, which
# holds credentials/agents); pass -Purge to remove everything.
#
#   .\scripts\uninstall-windows.ps1            # remove services + VMs, keep data
#   .\scripts\uninstall-windows.ps1 -Purge     # also delete the install dir + data
#   irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/uninstall-windows.ps1 | iex
param(
    [switch]$Purge,
    [string]$Dir
)
$ErrorActionPreference = "Stop"
if ($env:MATURANA_PURGE -eq '1') { $Purge = $true }

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
        $launch = @('-NoExit','-NoProfile','-ExecutionPolicy','Bypass','-File', $PSCommandPath) + $fwd
    } else {
        # Running via irm|iex (no file on disk): re-fetch elevated. -Purge crosses
        # the boundary via the MATURANA_PURGE env var.
        $pf = if ($Purge) { "`$env:MATURANA_PURGE=1; " } else { "" }
        $launch = @('-NoExit','-NoProfile','-ExecutionPolicy','Bypass','-Command',
                    "$pf irm https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/uninstall-windows.ps1 | iex")
    }
    try { Start-Process powershell.exe -Verb RunAs -ArgumentList $launch | Out-Null }
    catch { throw "Elevation declined. Re-run from an elevated PowerShell." }
    Write-Host "Elevated uninstaller launched in a new window."
    return
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

# 5. Purge data + binary, or keep it.
if ($Purge) {
    Set-Location $env:USERPROFILE   # don't sit inside the directory we delete
    if (Test-Path -LiteralPath $Dir) {
        # Take ownership + reset ACLs so inheritance-stripped key/token files delete.
        & takeown.exe /f $Dir /r /d y *> $null
        & icacls.exe $Dir /grant "*S-1-5-32-544:(F)" /t /c *> $null
        Remove-Item -Recurse -Force -LiteralPath $Dir -ErrorAction SilentlyContinue
        Write-Host "  purged $Dir (repo + .maturana, including credentials)"
    }
} else {
    Write-Host "  kept $Dir (repo + .maturana data). Re-run with -Purge to remove it too."
}

Write-Host "Maturana uninstalled."
