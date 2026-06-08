param(
    [string]$AgentId = "codex-demo",
    [string]$AgentIp = "",
    [string]$SshUser = "ubuntu",
    [string]$SshKeyPath = ".\.maturana\keys\maturana-agent-ed25519",
    [string]$ProxyBind = "0.0.0.0:47833",
    [int]$UpstreamPort = 47834
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$runId = [Guid]::NewGuid().ToString("N")
$vaultHome = Join-Path $repoRoot ".maturana-ci\guest-proxy-$runId"
$upstreamLog = Join-Path $repoRoot ".maturana-ci\guest-proxy-upstream-$runId.txt"
$upstreamRootCa = Join-Path $repoRoot ".maturana-ci\guest-proxy-upstream-ca-$runId.pem"
$exe = Join-Path $repoRoot "target\x86_64-pc-windows-gnu\debug\maturana.exe"

function ConvertTo-PemCertificate {
    param([Security.Cryptography.X509Certificates.X509Certificate2]$Certificate)
    $base64 = [Convert]::ToBase64String($Certificate.Export([Security.Cryptography.X509Certificates.X509ContentType]::Cert))
    $lines = for ($i = 0; $i -lt $base64.Length; $i += 64) {
        $base64.Substring($i, [Math]::Min(64, $base64.Length - $i))
    }
    return ((@("-----BEGIN CERTIFICATE-----") + $lines + @("-----END CERTIFICATE-----")) -join "`n")
}

function New-TestHttpsCertificate {
    $rootKey = [Security.Cryptography.RSA]::Create(2048)
    $rootReq = [Security.Cryptography.X509Certificates.CertificateRequest]::new(
        "CN=Maturana Live Test Root",
        $rootKey,
        [Security.Cryptography.HashAlgorithmName]::SHA256,
        [Security.Cryptography.RSASignaturePadding]::Pkcs1
    )
    $rootReq.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($true, $false, 0, $true))
    $rootReq.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new([Security.Cryptography.X509Certificates.X509KeyUsageFlags]::KeyCertSign -bor [Security.Cryptography.X509Certificates.X509KeyUsageFlags]::CrlSign, $true))
    $root = $rootReq.CreateSelfSigned([DateTimeOffset]::UtcNow.AddDays(-1), [DateTimeOffset]::UtcNow.AddDays(2))

    $leafKey = [Security.Cryptography.RSA]::Create(2048)
    $leafReq = [Security.Cryptography.X509Certificates.CertificateRequest]::new(
        "CN=localhost",
        $leafKey,
        [Security.Cryptography.HashAlgorithmName]::SHA256,
        [Security.Cryptography.RSASignaturePadding]::Pkcs1
    )
    $san = [Security.Cryptography.X509Certificates.SubjectAlternativeNameBuilder]::new()
    $san.AddDnsName("localhost")
    $leafReq.CertificateExtensions.Add($san.Build())
    $leafReq.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509BasicConstraintsExtension]::new($false, $false, 0, $true))
    $leafReq.CertificateExtensions.Add([Security.Cryptography.X509Certificates.X509KeyUsageExtension]::new([Security.Cryptography.X509Certificates.X509KeyUsageFlags]::DigitalSignature -bor [Security.Cryptography.X509Certificates.X509KeyUsageFlags]::KeyEncipherment, $true))
    $serial = [byte[]]::new(16)
    [Security.Cryptography.RandomNumberGenerator]::Fill($serial)
    $leafWithoutKey = $leafReq.Create($root, [DateTimeOffset]::UtcNow.AddDays(-1), [DateTimeOffset]::UtcNow.AddDays(2), $serial)
    $leaf = [Security.Cryptography.X509Certificates.RSACertificateExtensions]::CopyWithPrivateKey($leafWithoutKey, $leafKey)
    return [pscustomobject]@{ Root = $root; Leaf = $leaf }
}

Push-Location $repoRoot
try {
    if (!(Test-Path -LiteralPath $exe)) {
        & .\scripts\maturana.ps1 --help | Out-Null
    }

    if ([string]::IsNullOrWhiteSpace($AgentIp)) {
        $inspect = .\scripts\maturana.ps1 agent inspect $AgentId --live
        $AgentIp = ($inspect | Select-String 'live\.ipv4:\s+"([^"]+)"' | ForEach-Object { $_.Matches[0].Groups[1].Value } | Select-Object -First 1)
        if ([string]::IsNullOrWhiteSpace($AgentIp)) {
            throw "could not discover live IP for $AgentId; pass -AgentIp explicitly"
        }
    }

    Remove-Item -LiteralPath $vaultHome -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $upstreamLog -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $upstreamRootCa -Force -ErrorAction SilentlyContinue

    & .\scripts\maturana.ps1 --home $vaultHome pipelock init | Out-Host
    & .\scripts\maturana.ps1 --home $vaultHome pipelock set api/token --value X-From-Pipelock | Out-Host
    $proxyCaCert = (& .\scripts\maturana.ps1 --home $vaultHome pipelock ca-cert | Select-Object -Last 1).Trim()
    $certs = New-TestHttpsCertificate
    Set-Content -LiteralPath $upstreamRootCa -Value (ConvertTo-PemCertificate -Certificate $certs.Root) -NoNewline
    $leafPfx = [Convert]::ToBase64String($certs.Leaf.Export([Security.Cryptography.X509Certificates.X509ContentType]::Pfx, ""))

    $proxySpecPath = Join-Path $vaultHome "MATURANA.proxy-live.md"
    $proxySpec = @"
---
identity:
  id: pipelock-live-test
  name: Pipelock Live Test
  purpose: A temporary live test for proxy allowlists, injection, and audit logging.
runtime:
  harness: codex
vm:
  provider: hyper-v
network:
  egress_allowlist:
    - localhost
  proxy:
    enabled: true
    bind: "$ProxyBind"
    inject_headers:
      - host: localhost
        header: X-Test-Token
        source: pipelock:api/token
---

# Pipelock Live Test
"@
    Set-Content -LiteralPath $proxySpecPath -Value ($proxySpec -replace "`r`n", "`n") -NoNewline

    $upstreamJob = Start-Job -ScriptBlock {
        param($Port, $LogPath, $LeafPfx)
        $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $Port)
        $listener.Start()
        try {
            $leaf = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
                [Convert]::FromBase64String($LeafPfx),
                "",
                [Security.Cryptography.X509Certificates.X509KeyStorageFlags]::Exportable
            )
            $client = $listener.AcceptTcpClient()
            $stream = [Net.Security.SslStream]::new($client.GetStream(), $false)
            $stream.AuthenticateAsServer($leaf, $false, [Security.Authentication.SslProtocols]::Tls12, $false)
            $bytes = New-Object byte[] 4096
            $data = New-Object System.Collections.Generic.List[byte]
            while ($true) {
                $read = $stream.Read($bytes, 0, $bytes.Length)
                if ($read -le 0) { break }
                for ($i = 0; $i -lt $read; $i++) { $data.Add($bytes[$i]) }
                $text = [Text.Encoding]::ASCII.GetString($data.ToArray())
                if ($text.Contains("`r`n`r`n")) { break }
            }
            Set-Content -LiteralPath $LogPath -Value $text
            $response = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 200 OK`r`ncontent-length: 2`r`nconnection: close`r`n`r`nok")
            $stream.Write($response, 0, $response.Length)
            $stream.Flush()
            $client.Close()
        } finally {
            $listener.Stop()
        }
    } -ArgumentList $UpstreamPort, $upstreamLog, $leafPfx

    $homeAbs = (Resolve-Path $vaultHome).Path
    $proxySpecAbs = (Resolve-Path $proxySpecPath).Path
    $proxyArgs = @(
        "--home", $homeAbs,
        "pipelock", "proxy",
        "--spec", $proxySpecAbs
    )
    $oldSslCertFile = $env:SSL_CERT_FILE
    $env:SSL_CERT_FILE = (Resolve-Path $upstreamRootCa).Path
    $proxy = Start-Process -FilePath $exe -ArgumentList $proxyArgs -PassThru -WindowStyle Hidden
    $env:SSL_CERT_FILE = $oldSslCertFile
    Start-Sleep -Seconds 2

    try {
        $hostIp = ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=10 -i $SshKeyPath "$SshUser@$AgentIp" "ip route | awk '/default/ {print `$3; exit}'"
        $hostIp = ($hostIp | Select-Object -Last 1).Trim()
        if ([string]::IsNullOrWhiteSpace($hostIp)) {
            throw "could not discover guest default gateway"
        }

        scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=10 -i $SshKeyPath $proxyCaCert "$SshUser@$AgentIp`:/tmp/maturana-pipelock-ca.crt"
        if ($LASTEXITCODE -ne 0) {
            throw "failed to copy Maturana pipelock CA to guest"
        }

        $response = ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=10 -i $SshKeyPath "$SshUser@$AgentIp" "curl -sS --max-time 20 --proxy http://$hostIp`:47833 --cacert /tmp/maturana-pipelock-ca.crt https://localhost:$UpstreamPort/test"
        $response = ($response | Out-String).Trim()
        if ($response -ne "ok") {
            throw "guest HTTPS curl through proxy failed: $response"
        }

        Wait-Job $upstreamJob -Timeout 20 | Out-Null
        Receive-Job $upstreamJob -ErrorAction SilentlyContinue | Out-Null
        $request = Get-Content -LiteralPath $upstreamLog -Raw
        if ($request -notmatch "X-Test-Token: X-From-Pipelock") {
            throw "upstream did not receive injected header. Request was:`n$request"
        }

        $auditPath = Join-Path $vaultHome "audit\pipelock-live-test-pipelock-proxy.jsonl"
        if (!(Test-Path -LiteralPath $auditPath)) {
            throw "proxy audit log was not written: $auditPath"
        }
        $audit = Get-Content -LiteralPath $auditPath -Raw
        if ($audit -notmatch "pipelock\.proxy\.allowed" -or $audit -notmatch '"injected_headers":1') {
            throw "proxy audit log did not record the allowed injected request. Audit was:`n$audit"
        }
        if ($audit -notmatch '"tls_intercepted":true') {
            throw "proxy audit log did not record TLS interception. Audit was:`n$audit"
        }

        Write-Host "live HTTPS pipelock proxy test passed"
        Write-Host "guest response: $response"
        Write-Host "host gateway: $hostIp"
        Write-Host "audit: $auditPath"
    } finally {
        if ($proxy -and !$proxy.HasExited) {
            Stop-Process -Id $proxy.Id -Force
        }
        Stop-Job $upstreamJob -ErrorAction SilentlyContinue
        Remove-Job $upstreamJob -Force -ErrorAction SilentlyContinue
    }
}
finally {
    Pop-Location
}
