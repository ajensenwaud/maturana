#!/usr/bin/env bash
set -euo pipefail

agent_dir="${1:?agent directory required}"
tap_name="${2:?tap name required}"
socket_path="${3:?socket path required}"
config_path="${4:?config path required}"
pid_path="${5:?pid path required}"

state_dir="$(dirname "$socket_path")"
mkdir -p "$state_dir"

if [[ ! -f "$config_path" ]]; then
  echo "Firecracker config not found: $config_path" >&2
  exit 1
fi

if [[ -S "$socket_path" ]]; then
  rm -f "$socket_path"
fi

if [[ -f "$pid_path" ]] && kill -0 "$(cat "$pid_path")" 2>/dev/null; then
  echo "Firecracker already running with pid $(cat "$pid_path")"
  exit 0
fi

if ! command -v firecracker >/dev/null 2>&1; then
  echo "firecracker binary not found on PATH" >&2
  exit 1
fi

if ! ip link show "$tap_name" >/dev/null 2>&1; then
  echo "tap device not found: $tap_name" >&2
  echo "Create it first, for example:" >&2
  echo "  sudo ip tuntap add dev $tap_name mode tap" >&2
  echo "  sudo ip addr add 172.30.0.1/30 dev $tap_name" >&2
  echo "  sudo ip link set $tap_name up" >&2
  exit 1
fi

nohup firecracker --api-sock "$socket_path" --config-file "$config_path" \
  > "$state_dir/firecracker.stdout.log" \
  2> "$state_dir/firecracker.stderr.log" &
echo "$!" > "$pid_path"

echo "Firecracker launched for $agent_dir"
echo "pid: $(cat "$pid_path")"
echo "socket: $socket_path"
