#!/usr/bin/env bash
set -euo pipefail

tap_name="${1:-tap-maturana0}"
host_cidr="${2:-172.30.0.1/30}"
nat_cidr="${3:-172.30.0.0/30}"

# Run a single privileged net command: directly when root, else via passwordless
# sudo. The firecracker host installer grants a SCOPED NOPASSWD rule for exactly
# ip / iptables / sysctl (scripts/install-firecracker-host.sh writes
# /etc/sudoers.d/90-maturana-net), so this needs neither an interactive password
# nor a blanket "NOPASSWD: ALL". Running per-command (rather than re-exec'ing the
# whole script under sudo) is what lets that narrow rule match.
priv() {
  if [[ "$(id -u)" -eq 0 ]]; then
    "$@"
  else
    sudo -n "$@"
  fi
}

if [[ "$(id -u)" -ne 0 ]] && ! sudo -n true 2>/dev/null; then
  echo "passwordless sudo for the TAP/NAT setup is not available." >&2
  echo "Run scripts/install-firecracker-host.sh (it installs a scoped" >&2
  echo "/etc/sudoers.d/90-maturana-net rule for ip/iptables/sysctl), or add an" >&2
  echo "equivalent NOPASSWD rule for those commands yourself, then re-run." >&2
  exit 1
fi

# Read-only probes don't need privilege; only the mutating ops go through priv().
if ! ip link show "$tap_name" >/dev/null 2>&1; then
  priv ip tuntap add dev "$tap_name" mode tap
fi

if ! ip addr show "$tap_name" | grep -q "$host_cidr"; then
  priv ip addr flush dev "$tap_name"
  priv ip addr add "$host_cidr" dev "$tap_name"
fi

priv ip link set "$tap_name" up

priv sysctl -w net.ipv4.ip_forward=1 >/dev/null
if command -v iptables >/dev/null 2>&1; then
  priv iptables -t nat -C POSTROUTING -s "$nat_cidr" -j MASQUERADE 2>/dev/null ||
    priv iptables -t nat -A POSTROUTING -s "$nat_cidr" -j MASQUERADE
  priv iptables -C FORWARD -i "$tap_name" -j ACCEPT 2>/dev/null ||
    priv iptables -A FORWARD -i "$tap_name" -j ACCEPT
  priv iptables -C FORWARD -o "$tap_name" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null ||
    priv iptables -A FORWARD -o "$tap_name" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
fi

echo "tap ready: $tap_name $host_cidr nat=$nat_cidr"
