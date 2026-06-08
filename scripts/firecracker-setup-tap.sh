#!/usr/bin/env bash
set -euo pipefail

tap_name="${1:-tap-maturana0}"
host_cidr="${2:-172.30.0.1/30}"
nat_cidr="${3:-172.30.0.0/30}"

if [[ "$(id -u)" -ne 0 ]]; then
  if ! sudo -n true 2>/dev/null; then
    echo "passwordless sudo is required for automated tap setup." >&2
    echo "Ensure the user has a NOPASSWD rule that wins sudoers ordering, for example:" >&2
    echo "  aj ALL=(ALL) NOPASSWD: ALL" >&2
    echo "Place it after any broader passworded sudo rule, or in a late /etc/sudoers.d file." >&2
    exit 1
  fi
  exec sudo -n "$0" "$tap_name" "$host_cidr"
fi

if ! ip link show "$tap_name" >/dev/null 2>&1; then
  ip tuntap add dev "$tap_name" mode tap
fi

if ! ip addr show "$tap_name" | grep -q "$host_cidr"; then
  ip addr flush dev "$tap_name"
  ip addr add "$host_cidr" dev "$tap_name"
fi

ip link set "$tap_name" up

sysctl -w net.ipv4.ip_forward=1 >/dev/null
if command -v iptables >/dev/null 2>&1; then
  iptables -t nat -C POSTROUTING -s "$nat_cidr" -j MASQUERADE 2>/dev/null ||
    iptables -t nat -A POSTROUTING -s "$nat_cidr" -j MASQUERADE
  iptables -C FORWARD -i "$tap_name" -j ACCEPT 2>/dev/null ||
    iptables -A FORWARD -i "$tap_name" -j ACCEPT
  iptables -C FORWARD -o "$tap_name" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null ||
    iptables -A FORWARD -o "$tap_name" -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT
fi

echo "tap ready: $tap_name $host_cidr nat=$nat_cidr"
