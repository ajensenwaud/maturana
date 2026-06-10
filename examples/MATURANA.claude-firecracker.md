---
identity:
  id: claude-firecracker
  name: Claude Code Firecracker Agent
  purpose: Linux Firecracker Claude Code harness.
runtime:
  harness: claude-code
vm:
  provider: firecracker
  guest_os: linux
  vcpu: 2
  memory_mib: 2048
  firecracker:
    kernel_image: .maturana/images/firecracker/claude/vmlinux.bin
    rootfs_image: .maturana/images/firecracker/claude/ubuntu-rootfs.ext4
    tap_name: tap-mat-claude
    host_ip: 172.30.10.9
    guest_ip: 172.30.10.10
    guest_mac: AA:FC:00:00:10:03
    kernel_args: console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5
harness_auth:
  - runtime: claude-code
    source_path: .maturana/host-auth/claude-code
    guest_path: /home/ubuntu/.claude
filesystem:
  mounts:
    - host_path: .maturana/agents/claude-firecracker/workspace
      guest_path: /workspace
      writable: true
network:
  egress_allowlist:
    - api.anthropic.com
    - platform.claude.com
    - github.com
memory:
  wiki_path: .maturana/wiki
  agent_memory_path: .maturana/agents/claude-firecracker/memory
channels:
  tui: true
snapshots:
  on_launch: false
  retain: 3
---

# Claude Code Firecracker Agent
