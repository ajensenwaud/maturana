#!/usr/bin/env bash
# Provision a fresh Linux host into a Maturana Firecracker agent host: the
# firecracker binary, KVM access, the libguestfs/qemu image-build toolchain,
# and IPv4 forwarding for guest egress. Idempotent. Run this BEFORE
# `maturana setup firecracker-harnesses` (which builds images + launches VMs).
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

# 1. KVM: the provider boots microVMs through /dev/kvm. Detect + ENABLE it
#    (load the vendor module, persist it, grant access) rather than only
#    checking. Delegated to the shared, idempotent kvm-enable.sh so install.sh
#    and this script behave identically. A Firecracker host without KVM is
#    pointless, so a failure here is fatal.
say "ensuring virtualization (KVM)"
KVM_HELPER="$(dirname "$0")/kvm-enable.sh"
if [ -f "$KVM_HELPER" ]; then
  bash "$KVM_HELPER"
else
  curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/kvm-enable.sh | bash
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

# libguestfs (supermin) builds its appliance from the host kernel and picks the
# NEWEST /boot/vmlinuz-*, which Ubuntu ships mode 0600. As a non-root user the
# firecracker image build then dies with a cryptic "supermin exited with error
# status 1". Make ALL installed kernels readable — not just the running one: a
# pending-reboot kernel upgrade is exactly when newest != $(uname -r), which is
# the case that slips through a $(uname -r)-only fix. Then install a kernel
# postinst hook so a future upgrade can't silently re-break it.
if ls /boot/vmlinuz-* >/dev/null 2>&1; then
  say "making /boot/vmlinuz-* readable for libguestfs"
  $SUDO chmod 0644 /boot/vmlinuz-* 2>/dev/null || true
fi
if [ -d /etc/kernel/postinst.d ]; then
  $SUDO tee /etc/kernel/postinst.d/zz-maturana-readable-vmlinuz >/dev/null <<'HOOK' 2>/dev/null || true
#!/bin/sh
# maturana: keep /boot/vmlinuz-* world-readable so the non-root firecracker
# image build (libguestfs/supermin) can read the host kernel after a kernel
# upgrade. See scripts/firecracker-prepare-assets.sh.
chmod 0644 /boot/vmlinuz-* 2>/dev/null || true
HOOK
  $SUDO chmod 0755 /etc/kernel/postinst.d/zz-maturana-readable-vmlinuz 2>/dev/null || true
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

# 6. SCOPED passwordless sudo for the per-agent TAP/NAT setup, so agent launches
#    don't block on an interactive password. This grants NOPASSWD for ONLY the
#    exact net commands firecracker-setup-tap.sh runs (ip / iptables / sysctl) —
#    NOT a blanket "NOPASSWD: ALL". Paths are resolved on this host so the rule
#    matches what the script actually executes, and validated with visudo (a
#    malformed sudoers file is dangerous, so roll back on parse error).
if [ "$(id -u)" -ne 0 ]; then
  ip_bin="$(command -v ip || echo /usr/sbin/ip)"
  iptables_bin="$(command -v iptables || echo /usr/sbin/iptables)"
  sysctl_bin="$(command -v sysctl || echo /usr/sbin/sysctl)"
  say "granting scoped passwordless sudo for TAP setup (ip, iptables, sysctl)"
  printf '%s ALL=(root) NOPASSWD: %s, %s, %s\n' \
    "$USER" "$ip_bin" "$iptables_bin" "$sysctl_bin" \
    | $SUDO tee /etc/sudoers.d/90-maturana-net >/dev/null
  $SUDO chmod 0440 /etc/sudoers.d/90-maturana-net
  if ! $SUDO visudo -cf /etc/sudoers.d/90-maturana-net >/dev/null 2>&1; then
    say "WARNING: sudoers validation failed; removing the rule (fix paths and re-run)"
    $SUDO rm -f /etc/sudoers.d/90-maturana-net
  fi
fi

say "firecracker host ready"
echo
echo "  Next:"
echo "    1. Install the control plane:  curl -fsSL .../scripts/install.sh | bash"
echo "    2. Build images + launch agents: maturana setup firecracker-harnesses"
echo "       (set credentials under .maturana/host-auth/<harness>/ first)"
echo "    3. Drive agents: codex in the repo, or the web cockpit at :47836"
