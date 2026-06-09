param(
    [string]$AgentId = "codex-demo",
    [string]$VmName = "",
    [Parameter(Mandatory=$true)]
    [string]$BaseVhdxPath,
    [string]$SwitchName = "Default Switch",
    [string]$SshUser = "ubuntu",
    [Parameter(Mandatory=$true)]
    [string]$SshKeyPath,
    [string]$CloudInitUserDataPath = "",
    [string]$CloudInitMetaDataPath = "",
    [int]$DiskSizeGB = 24,
    [int]$Vcpu = 2,
    [int]$MemoryMiB = 2048,
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

function New-SeedVhdx {
    param(
        [string]$Path
    )

    if (Test-Path -LiteralPath $Path) {
        Dismount-VHD -Path $Path -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $Path -Force
    }

    $seedDir = Join-Path ([IO.Path]::GetDirectoryName($Path)) "cloud-init"
    New-Item -ItemType Directory -Force -Path $seedDir | Out-Null
    $userDataPath = Join-Path $seedDir "user-data"
    $metaDataPath = Join-Path $seedDir "meta-data"

    if ([string]::IsNullOrWhiteSpace($CloudInitUserDataPath) -or [string]::IsNullOrWhiteSpace($CloudInitMetaDataPath)) {
        throw "CloudInitUserDataPath and CloudInitMetaDataPath are required. Rust must render cloud-init user-data and meta-data."
    }
    if (!(Test-Path -LiteralPath $CloudInitUserDataPath)) {
        throw "CloudInitUserDataPath does not exist: $CloudInitUserDataPath"
    }
    if (!(Test-Path -LiteralPath $CloudInitMetaDataPath)) {
        throw "CloudInitMetaDataPath does not exist: $CloudInitMetaDataPath"
    }
    Copy-Item -LiteralPath $CloudInitUserDataPath -Destination $userDataPath -Force
    Copy-Item -LiteralPath $CloudInitMetaDataPath -Destination $metaDataPath -Force

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
    New-SeedVhdx -Path $seedDisk

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

Write-Log "create-only launch complete."
$result = @{ ok = $true; agent_id = $AgentId; vm = $VmName; ipv4 = $ip; log = $script:LogPath }
Write-Host ("MATURANA_RESULT_JSON=" + ($result | ConvertTo-Json -Compress))
Write-Host "SSH: ssh -i `"$SshKeyPath`" $SshUser@$ip"
Write-Host "Log: $script:LogPath"
