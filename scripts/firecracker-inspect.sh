#!/usr/bin/env bash
set -euo pipefail

agent_dir="${1:?agent directory required}"
state_dir="$agent_dir/state"
pid_path="$state_dir/firecracker.pid"
socket_path="$state_dir/firecracker.socket"

echo "agent_dir: $agent_dir"
echo "config: $state_dir/firecracker-config.json"
echo "metadata: $state_dir/firecracker-metadata.json"
echo "socket: $socket_path"

if [[ -f "$pid_path" ]]; then
  pid="$(cat "$pid_path")"
  if kill -0 "$pid" 2>/dev/null; then
    echo "state: running"
    echo "pid: $pid"
  else
    echo "state: stale-pid"
    echo "pid: $pid"
  fi
else
  echo "state: stopped"
fi

if [[ -f "$state_dir/firecracker-metrics.json" ]]; then
  echo "--- metrics tail ---"
  tail -n 5 "$state_dir/firecracker-metrics.json"
fi
