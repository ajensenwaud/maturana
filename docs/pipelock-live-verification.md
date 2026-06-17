# Pipelock Live Verification

This document records the repeatable checks for declaring the pipelock MVP
working on both supported host families.

> Host names, repo paths (e.g. `/var/tmp/maturana-…`), SSH key paths, and guest/host
> IPs below are examples from one test environment. Substitute the values from your
> own host and agent spec.

## What Must Pass

Before calling pipelock done for the MVP, verify both paths:

- Windows host with Hyper-V guest.
- Linux host with a Firecracker guest.

Each live test must prove:

- The guest can make an HTTPS request through the host pipelock proxy.
- The proxy enforces the egress allowlist.
- The proxy injects a header from a `pipelock:` secret.
- The upstream receives the injected value.
- The audit log records `pipelock.proxy.allowed`.
- The audit log records `"injected_headers":1`.
- The audit log records `"tls_intercepted":true`.

Codex and Claude OAuth credentials are not tested through pipelock. Those are
guest harness credentials and are injected directly into the VM.

## Windows / Hyper-V

Use this from the Windows repo checkout after launching the Hyper-V Ubuntu
agent:

```powershell
.\scripts\test-pipelock-proxy-live.ps1
```

If the agent IP cannot be discovered through hostd inspection, pass it
explicitly:

```powershell
.\scripts\test-pipelock-proxy-live.ps1 -AgentIp 172.26.x.y
```

Expected success output includes:

```text
live HTTPS pipelock proxy test passed
guest response: ok
audit: ...\pipelock-live-test-pipelock-proxy.jsonl
```

## Linux / Firecracker

On the reference test host the Firecracker runtime is under:

```text
/var/tmp/maturana-aidev
```

The running guest uses:

```text
guest IP: 172.30.0.2
host tap IP: 172.30.0.1
ssh key: /var/tmp/maturana-aidev/.maturana/images/firecracker/maturana-firecracker.id_rsa
```

From Windows, run the wrapper:

```powershell
.\scripts\test-pipelock-proxy-aidev.ps1
```

From the Linux host, run the script directly:

```bash
cd /home/aj/maturana
bash scripts/test-pipelock-proxy-firecracker-live.sh \
  --repo-root /home/aj/maturana \
  --agent-root /var/tmp/maturana-aidev/.maturana \
  --guest-ip 172.30.0.2 \
  --host-ip 172.30.0.1
```

Expected success output includes:

```text
firecracker pipelock live test passed
guest: ubuntu@172.30.0.2
proxy: http://172.30.0.1:47833
```

The final audit line should look like:

```json
{"action":"pipelock.proxy.allowed","injected_headers":1,"tls_intercepted":true}
```

The live test starts a temporary HTTPS upstream on the Linux host, trusts its
temporary CA on the host for the proxy's upstream TLS validation, starts the
Maturana pipelock proxy, copies the Maturana pipelock CA into the guest, and
runs guest `curl` through the proxy.

## Firecracker State Check

To confirm the existing VM is running:

```bash
cd /var/tmp/maturana-aidev
maturana agent inspect firecracker-demo --live
ssh -i .maturana/images/firecracker/maturana-firecracker.id_rsa ubuntu@172.30.0.2 \
  'cat /workspace/firecracker-run.txt; date -Is'
```

Known-good marker:

```text
Maturana Firecracker run OK
```

## Do Not Rebuild First

Do not rebuild Firecracker assets as the first response to a pipelock test
failure. Check the running VM and existing assets first:

```bash
ps -ef | grep firecracker | grep -v grep
ip addr show tap-maturana0
ls -la /var/tmp/maturana-aidev/.maturana/images/firecracker
```

Only refresh Firecracker assets when the kernel or rootfs is actually missing
or intentionally being rebuilt. Use the Rust-owned repair flow first:

```bash
maturana setup firecracker-harnesses --agent-id codex-firecracker
```

Run `scripts/firecracker-prepare-assets.sh` directly only for adapter-level
maintenance, and only with Rust-rendered `MATURANA_SESSIOND_ENV_PATH`,
`MATURANA_RUN_AGENT_PATH`, and `MATURANA_AGENT_SERVICE_PATH` inputs.
