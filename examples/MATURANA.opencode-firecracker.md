---
identity:
  id: opencode-firecracker
  name: OpenCode Firecracker Agent
  purpose: Linux Firecracker OpenCode harness.
runtime:
  harness: opencode
vm:
  provider: firecracker
  guest_os: linux
  vcpu: 2
  memory_mib: 4096
  firecracker:
    kernel_image: .maturana/images/firecracker/opencode/vmlinux.bin
    rootfs_image: .maturana/images/firecracker/opencode/ubuntu-rootfs.ext4
    tap_name: tap-mat-open
    host_ip: 172.30.10.5
    guest_ip: 172.30.10.6
    guest_mac: AA:FC:00:00:10:02
    kernel_args: console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5
harness_auth:
  - runtime: opencode
    source_path: .maturana/host-auth/opencode
    guest_path: /home/ubuntu
filesystem:
  mounts:
    - host_path: .maturana/agents/opencode-firecracker/workspace
      guest_path: /workspace
      writable: true
network:
  egress_allowlist:
    - openrouter.ai
    - github.com
memory:
  wiki_path: .maturana/wiki
  agent_memory_path: .maturana/agents/opencode-firecracker/memory
channels:
  tui: true
snapshots:
  on_launch: false
  retain: 3
---

# OpenCode Firecracker Agent
