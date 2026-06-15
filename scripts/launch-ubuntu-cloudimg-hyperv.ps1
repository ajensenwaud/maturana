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
    [int]$AutomaticStartDelaySeconds = 30,
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
    # Pipe the script to `bash -s` over STDIN rather than passing it as an ssh
    # argument. Windows PowerShell mangles native-exe arguments that contain
    # quotes / newlines / $(...), and this .ps1's CRLF line endings would reach
    # the remote shell as ^M — together that made multi-line guest commands fail
    # with ssh exit 255. Normalize to LF and feed stdin to sidestep both.
    $clean = $Command -replace "`r", ""
    $clean | ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o BatchMode=yes -o ConnectTimeout=10 -i $SshKeyPath "$SshUser@$Ip" "bash -s"
    if ($LASTEXITCODE -ne 0) {
        throw "Guest command failed with exit code ${LASTEXITCODE}"
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
    # The caller may already have rendered the files in place (source ==
    # destination); Copy-Item refuses to overwrite an item with itself.
    if ([IO.Path]::GetFullPath($CloudInitUserDataPath) -ne [IO.Path]::GetFullPath($userDataPath)) {
        Copy-Item -LiteralPath $CloudInitUserDataPath -Destination $userDataPath -Force
    }
    if ([IO.Path]::GetFullPath($CloudInitMetaDataPath) -ne [IO.Path]::GetFullPath($metaDataPath)) {
        Copy-Item -LiteralPath $CloudInitMetaDataPath -Destination $metaDataPath -Force
    }

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
    # Keyless TCP reachability check on port 22. This launcher runs under hostd as
    # SYSTEM, and Windows OpenSSH's strict-mode rejects the (aj-owned) agent key
    # when invoked as SYSTEM, so a key-based `ssh ... echo alive` here fails
    # silently for the whole window. We only need to confirm sshd is listening;
    # the real key-authenticated SSH (readiness, root-expand, provisioning) is
    # done afterwards by the Rust CLI, which runs as the user whose key it is.
    param([string]$Ip)
    for ($i = 0; $i -lt 180; $i++) {
        try {
            $client = New-Object System.Net.Sockets.TcpClient
            $async = $client.BeginConnect($Ip, 22, $null, $null)
            $ok = $async.AsyncWaitHandle.WaitOne(3000)
            if ($ok -and $client.Connected) {
                $client.Close()
                return
            }
            $client.Close()
        } catch {
            # Not listening yet; keep polling.
        }
        Start-Sleep -Seconds 5
    }
    throw "SSH port 22 did not open at $Ip within the wait window."
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
    # Zero-touch reboot recovery: the VM auto-boots with the host (staggered
    # delay to avoid a thundering herd of microVMs starting at once).
    Set-VM -Name $VmName -AutomaticCheckpointsEnabled $false `
        -AutomaticStartAction Start -AutomaticStartDelay $AutomaticStartDelaySeconds
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
Write-Log "sshd is listening; handing off to the Rust CLI for key-authenticated provisioning."
# NOTE: key-authenticated SSH readiness, root-filesystem expansion, and all
# guest provisioning are done by the Rust CLI (running as the user that owns the
# agent SSH key), not here under SYSTEM. See provision_hyperv_guest in
# crates/maturana-core/src/providers/hyperv.rs.

Write-Log "create-only launch complete."
$result = @{ ok = $true; agent_id = $AgentId; vm = $VmName; ipv4 = $ip; log = $script:LogPath }
Write-Host ("MATURANA_RESULT_JSON=" + ($result | ConvertTo-Json -Compress))
Write-Host "SSH: ssh -i `"$SshKeyPath`" $SshUser@$ip"
Write-Host "Log: $script:LogPath"
