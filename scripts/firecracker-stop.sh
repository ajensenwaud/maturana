#!/usr/bin/env bash
set -euo pipefail

agent_dir="${1:?agent directory required}"
pid_path="$agent_dir/state/firecracker.pid"
socket_path="$agent_dir/state/firecracker.socket"

if [[ -f "$pid_path" ]] && kill -0 "$(cat "$pid_path")" 2>/dev/null; then
  kill "$(cat "$pid_path")"
  rm -f "$pid_path"
fi

rm -f "$socket_path"
echo "Firecracker stopped for $agent_dir"
