#!/usr/bin/env bash
# Ensure KVM is available and usable for Firecracker microVMs. Idempotent and
# safe to re-run. It will:
#   1. detect hardware virtualization (vmx/svm),
#   2. load the matching KVM module (kvm_intel / kvm_amd) and persist it,
#   3. grant the invoking user access via the kvm group,
#   4. give a clear, actionable diagnosis when it genuinely can't be enabled
#      (firmware virtualization off, or nested virtualization not enabled on a
#      VM host).
#
#   curl -fsSL https://raw.githubusercontent.com/ajensenwaud/maturana/main/scripts/kvm-enable.sh | bash
#
# Exits non-zero only when KVM cannot be enabled, so callers can treat that as a
# hard failure (Firecracker host) or a warning (control-plane-only install).
set -euo pipefail

say() { printf '\033[36m[maturana-kvm]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[maturana-kvm] %s\033[0m\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = "Linux" ] || die "KVM is Linux-only"

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null 2>&1 || die "sudo is required (or run as root)"
  SUDO="sudo"
fi

ME="${USER:-$(id -un)}"

if [ ! -e /dev/kvm ]; then
  # No device node yet. We can only proceed if the CPU exposes virt extensions.
  if ! grep -Eq '(vmx|svm)' /proc/cpuinfo; then
    die "no hardware virtualization found (no vmx/svm in /proc/cpuinfo).
  - Bare metal: enable Intel VT-x / AMD-V in BIOS/UEFI, then re-run.
  - Cloud / VM host: enable NESTED virtualization for this instance, e.g.
      GCP:     gcloud ... --enable-nested-virtualization
      Hyper-V: Set-VMProcessor <vm> -ExposeVirtualizationExtensions \$true
      KVM:     load kvm_intel/kvm_amd with nested=1 on the *host*."
  fi

  if grep -q vmx /proc/cpuinfo; then MOD=kvm_intel; else MOD=kvm_amd; fi
  say "KVM not enabled yet; loading $MOD"
  $SUDO modprobe kvm 2>/dev/null || true
  errf="$(mktemp)"
  if ! $SUDO modprobe "$MOD" 2>"$errf"; then
    err="$(tr '\n' ' ' <"$errf" 2>/dev/null || true)"
    rm -f "$errf"
    die "could not load $MOD: ${err}
  Virtualization is likely disabled in firmware, or this is a VM host without
  nested virtualization. Enable it on the host/firmware, then re-run."
  fi
  rm -f "$errf"

  # Persist across reboots.
  printf 'kvm\n%s\n' "$MOD" | $SUDO tee /etc/modules-load.d/maturana-kvm.conf >/dev/null

  # The device node can take a moment to appear after modprobe.
  for _ in 1 2 3 4 5; do [ -e /dev/kvm ] && break; sleep 1; done
  [ -e /dev/kvm ] || die "$MOD loaded but /dev/kvm is still missing.
  Nested virtualization is almost certainly not enabled on this host's
  hypervisor. Enable it there, then re-run."
  say "KVM enabled — /dev/kvm is present"
else
  say "KVM already available (/dev/kvm present)"
fi

# Access: the Firecracker launcher needs read+write on /dev/kvm.
if [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
  say "granting KVM access to $ME (kvm group)"
  $SUDO groupadd -r kvm 2>/dev/null || true
  $SUDO usermod -aG kvm "$ME" || true
  say "NOTE: log out/in (or run 'newgrp kvm') for kvm group membership to take effect"
fi

say "KVM ready"
