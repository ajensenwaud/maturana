#!/usr/bin/env bash
# Runs ON the claude guest. Captures claude-code's real MCP error for notion.
set -a
. /agent/proxy.env 2>/dev/null
export HTTP_PROXY="http://172.30.10.9:47833"; export HTTPS_PROXY="$HTTP_PROXY"
export http_proxy="$HTTP_PROXY"; export https_proxy="$HTTP_PROXY"
set +a
cd /workspace 2>/dev/null || cd /home/ubuntu

timeout 90 claude -p --permission-mode bypassPermissions --debug --output-format stream-json --verbose \
  "Call the Notion search tool with an empty/short query and report the raw result or the exact error text." \
  </dev/null >/tmp/claude-out.txt 2>/tmp/claude-mcp.err
echo "=== claude exit: $? ==="
echo "=== last stdout ==="
tail -c 500 /tmp/claude-out.txt
echo
echo "=== stderr: mcp/notion/proxy/error lines ==="
grep -iE "notion|mcp|socket|hang|proxy|ECONN|ENOTFOUND|undici|fetch|error|timeout|spawn" /tmp/claude-mcp.err | tail -30
