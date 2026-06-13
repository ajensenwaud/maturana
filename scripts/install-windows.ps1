param(
    [switch]$SkipImage,
    [switch]$ForceImage,
    [switch]$SkipHostd,
    [switch]$SkipServices,
    # The current user's Windows password. Needed to register the up/web boot
    # tasks with logon type Password so they run at boot WITHOUT an interactive
    # login (codex/claude auth lives in the user profile). Prompted securely if
    # omitted. Windows stores it in the LSA vault, never on disk.
    [System.Security.SecureString]$WindowsPassword
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$imagePath = Join-Path $repoRoot ".maturana\images\ubuntu-noble\noble-server-cloudimg-amd64.vhdx"

Push-Location $repoRoot
try {
    Write-Host "Building maturana CLI with the Windows GNU toolchain..."
    & .\scripts\maturana.ps1 --help | Out-Null

    Write-Host "Preparing agent SSH key..."
    & .\scripts\maturana.ps1 repair ssh-key

    if (!$SkipImage) {
        if ($ForceImage -or !(Test-Path -LiteralPath $imagePath)) {
            Write-Host "Preparing official Ubuntu Hyper-V image..."
            $imageArgs = @()
            if ($ForceImage) {
                $imageArgs += "--force"
            }
            & .\scripts\maturana.ps1 repair ubuntu-cloudimg @imageArgs
        } else {
            Write-Host "Using existing Ubuntu Hyper-V image: $imagePath"
        }
    }

    if (!$SkipHostd) {
        Write-Host "Installing privileged host daemon. Windows may show one UAC prompt."
        & .\scripts\install-hostd-task.ps1
    }

    if (!$SkipServices) {
        # Register the up + web boot tasks. They run at startup as the current
        # user via a stored password (zero-touch reboot recovery, no login).
        if (-not $WindowsPassword) {
            Write-Host "Enter your Windows password (for boot tasks that run without login):"
            $WindowsPassword = Read-Host -AsSecureString
        }
        $plainPw = [System.Net.NetworkCredential]::new("", $WindowsPassword).Password
        if ([string]::IsNullOrEmpty($plainPw)) {
            throw "A Windows password is required to register boot services (or pass -SkipServices)."
        }
        Write-Host "Registering Maturana services (up + web) for boot..."
        try {
            & .\scripts\maturana.ps1 service install up web --windows-password $plainPw
        }
        finally {
            $plainPw = $null
            [System.GC]::Collect()
        }
        # Make the Hyper-V agent VMs auto-boot with the host too.
        & .\scripts\set-vm-autostart.ps1
    }

    Write-Host "Windows install complete."
    Write-Host "Check hostd with: .\scripts\maturana.ps1 hostd status"
}
finally {
    Pop-Location
}
