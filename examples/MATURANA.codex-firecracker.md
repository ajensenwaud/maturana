---
identity:
  id: codex-firecracker
  name: Codex Firecracker Agent
  purpose: Linux Firecracker Codex harness.
runtime:
  harness: codex
vm:
  provider: firecracker
  guest_os: linux
  vcpu: 2
  memory_mib: 2048
  firecracker:
    kernel_image: .maturana/images/firecracker/codex/vmlinux.bin
    rootfs_image: .maturana/images/firecracker/codex/ubuntu-rootfs.ext4
    tap_name: tap-mat-codex
    host_ip: 172.30.10.1
    guest_ip: 172.30.10.2
    guest_mac: AA:FC:00:00:10:01
    kernel_args: console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5
harness_auth:
  - runtime: codex
    source_path: .maturana/host-auth/codex
    guest_path: /home/ubuntu/.codex
filesystem:
  mounts:
    - host_path: .maturana/agents/codex-firecracker/workspace
      guest_path: /workspace
      writable: true
network:
  egress_allowlist:
    - api.openai.com
    - chatgpt.com
    - github.com
memory:
  wiki_path: .maturana/wiki
  agent_memory_path: .maturana/agents/codex-firecracker/memory
channels:
  tui: true
snapshots:
  on_launch: false
  retain: 3
---

# Codex Firecracker Agent
