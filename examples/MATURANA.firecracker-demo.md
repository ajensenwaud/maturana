---
identity:
  id: firecracker-demo
  name: Firecracker Demo Agent
  purpose: A bounded Linux worker used to prove Maturana Firecracker launch planning on aidev.
runtime:
  harness: codex
vm:
  provider: firecracker
  guest_os: linux
  vcpu: 2
  memory_mib: 2048
  firecracker:
    kernel_image: .maturana/images/firecracker/vmlinux.bin
    rootfs_image: .maturana/images/firecracker/ubuntu-rootfs.ext4
    tap_name: tap-maturana0
    guest_mac: AA:FC:00:00:00:01
    kernel_args: console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5
harness_auth:
  - runtime: codex
    source_path: .maturana/host-auth/codex
    guest_path: /home/ubuntu/.codex
filesystem:
  mounts:
    - host_path: .maturana/agents/firecracker-demo/workspace
      guest_path: /workspace
      writable: true
network:
  egress_allowlist:
    - api.openai.com
    - github.com
  proxy:
    enabled: true
    bind: 0.0.0.0:47833
    inject_headers: []
credentials:
  - name: codex-oauth
    source: env:CODEX_OAUTH_JSON
memory:
  wiki_path: .maturana/wiki
  agent_memory_path: .maturana/agents/firecracker-demo/memory
browser:
  headless_chrome: true
channels:
  tui: true
snapshots:
  on_launch: true
  retain: 5
---

# Firecracker Demo Agent

This spec is for `aidev`, where Firecracker is already installed.
