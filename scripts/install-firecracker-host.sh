#!/usr/bin/env bash
# Provision a fresh Linux host into a Maturana Firecracker agent host: the
# firecracker binary, KVM access, the libguestfs/qemu image-build toolchain,
# and IPv4 forwarding for guest egress. Idempotent. Run this BEFORE
# `maturana repair firecracker-harnesses` (which builds images + launches VMs).
#
#   curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/install-firecracker-host.sh | bash
#
# The control-plane CLI + web cockpit come from scripts/install.sh; this script
# only adds the Linux-specific microVM substrate. Needs sudo for packages, KVM
# group membership, and sysctl.
set -euo pipefail

FC_VERSION="${MATURANA_FIRECRACKER_VERSION:-v1.10.1}"
say() { printf '\033[36m[maturana-fc]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[maturana-fc] %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "this script provisions a Linux Firecracker host"

case "$(uname -m)" in
  x86_64)        ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null 2>&1 || die "sudo is required (or run as root)"
  SUDO="sudo"
fi

# 1. KVM: the provider boots microVMs through /dev/kvm.
say "checking virtualization (KVM)"
if [ ! -e /dev/kvm ]; then
  if grep -Eqc '(vmx|svm)' /proc/cpuinfo; then
    die "CPU supports virtualization but /dev/kvm is missing — enable KVM (load kvm_intel/kvm_amd, and enable VT-x/AMD-V in BIOS, or nested virt on a cloud VM)"
  fi
  die "/dev/kvm absent and no vmx/svm in /proc/cpuinfo — this host cannot run Firecracker"
fi
if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
  say "granting KVM access to $USER (kvm group)"
  $SUDO groupadd -r kvm 2>/dev/null || true
  $SUDO usermod -aG kvm "$USER" || true
  say "NOTE: log out/in (or 'newgrp kvm') for kvm group membership to take effect"
fi

# 2. Packages: image-build toolchain used by firecracker-prepare-assets.sh
#    (guestfish/virt-*), plus qemu-img, e2fsprogs, cloud tools, networking.
say "installing image-build + networking packages (sudo)"
if command -v apt-get >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  $SUDO apt-get update -qq
  $SUDO apt-get install -y -qq \
    curl tar e2fsprogs iproute2 iptables \
    qemu-utils libguestfs-tools cloud-image-utils
elif command -v dnf >/dev/null 2>&1; then
  $SUDO dnf install -y \
    curl tar e2fsprogs iproute iptables \
    qemu-img libguestfs-tools-c cloud-utils
else
  say "unknown package manager: install qemu-img, libguestfs-tools (guestfish/virt-*),"
  say "e2fsprogs, cloud-image-utils, iproute2 and iptables yourself, then re-run"
fi

# libguestfs on some distros needs a readable kernel to build its appliance.
if [ -f "/boot/vmlinuz-$(uname -r)" ] && [ ! -r "/boot/vmlinuz-$(uname -r)" ]; then
  say "making /boot/vmlinuz readable for libguestfs"
  $SUDO chmod 0644 "/boot/vmlinuz-$(uname -r)" || true
fi

# 3. extract-vmlinux: prepare-assets uses it to unpack the guest kernel; it's a
#    standalone kernel-source script not always packaged.
if ! command -v extract-vmlinux >/dev/null 2>&1; then
  say "installing extract-vmlinux"
  $SUDO curl -fsSL -o /usr/local/bin/extract-vmlinux \
    https://raw.githubusercontent.com/torvalds/linux/master/scripts/extract-vmlinux
  $SUDO chmod 0755 /usr/local/bin/extract-vmlinux
fi

# 4. Firecracker binary.
if command -v firecracker >/dev/null 2>&1; then
  say "firecracker present: $(firecracker --version 2>/dev/null | head -1)"
else
  say "installing firecracker $FC_VERSION ($ARCH)"
  tmp="$(mktemp -d)"
  url="https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-${ARCH}.tgz"
  curl -fsSL -o "$tmp/fc.tgz" "$url" || die "failed to download firecracker from $url"
  tar -xzf "$tmp/fc.tgz" -C "$tmp"
  $SUDO install -m 0755 "$tmp/release-${FC_VERSION}-${ARCH}/firecracker-${FC_VERSION}-${ARCH}" /usr/local/bin/firecracker
  rm -rf "$tmp"
  say "installed $(firecracker --version 2>/dev/null | head -1)"
fi

# 5. IPv4 forwarding so guests reach their egress allowlist through the host.
#    (firecracker-setup-tap.sh adds the per-agent TAP + NAT rule at launch.)
say "enabling IPv4 forwarding (persistent)"
$SUDO sysctl -w net.ipv4.ip_forward=1 >/dev/null
echo 'net.ipv4.ip_forward=1' | $SUDO tee /etc/sysctl.d/99-maturana.conf >/dev/null

# 6. Passwordless sudo reminder — the per-agent TAP setup needs it.
if ! sudo -n true 2>/dev/null && [ "$(id -u)" -ne 0 ]; then
  say "NOTE: firecracker-setup-tap.sh needs passwordless sudo. Add a late-ordering rule:"
  say "      echo '$USER ALL=(ALL) NOPASSWD: ALL' | sudo tee /etc/sudoers.d/90-maturana"
fi

say "firecracker host ready"
echo
echo "  Next:"
echo "    1. Install the control plane:  curl -fsSL .../scripts/install.sh | bash"
echo "    2. Build images + launch agents: maturana repair firecracker-harnesses"
echo "       (set credentials under .maturana/host-auth/<harness>/ first)"
echo "    3. Drive agents: codex in the repo, or the web cockpit at :47836"
