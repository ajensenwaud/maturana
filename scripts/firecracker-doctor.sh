#!/usr/bin/env bash
set -euo pipefail

kernel_image="${1:-.maturana/images/firecracker/vmlinux.bin}"
rootfs_image="${2:-.maturana/images/firecracker/ubuntu-rootfs.ext4}"
tap_name="${3:-tap-maturana0}"

ok=true

check() {
  local name="$1"
  shift
  if "$@"; then
    echo "ok: $name"
  else
    echo "error: $name" >&2
    ok=false
  fi
}

echo "host: $(uname -srm)"

check "linux host" test "$(uname -s)" = "Linux"
check "firecracker on PATH" command -v firecracker
if command -v firecracker >/dev/null 2>&1; then
  firecracker --version 2>/dev/null || true
fi

check "/dev/kvm exists" test -e /dev/kvm
check "/dev/kvm readable" test -r /dev/kvm
check "/dev/kvm writable" test -w /dev/kvm
check "kernel image exists: $kernel_image" test -f "$kernel_image"
if [[ -f "$kernel_image" ]]; then
  check "kernel image is ELF vmlinux: $kernel_image" sh -c "file '$kernel_image' | grep -q 'ELF'"
fi
check "rootfs image exists: $rootfs_image" test -f "$rootfs_image"

if command -v ip >/dev/null 2>&1; then
  check "tap exists: $tap_name" ip link show "$tap_name"
else
  echo "error: ip command not found" >&2
  ok=false
fi

if [[ "$ok" = true ]]; then
  echo "firecracker doctor passed"
else
  echo "firecracker doctor failed" >&2
  exit 1
fi
