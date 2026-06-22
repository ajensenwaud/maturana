#!/usr/bin/env bash
# Runs ON a guest. Drives the resident notion-mcp-server through a real
# API-post-search via raw JSON-RPC, with and without the maturana proxy env,
# to isolate "socket hang up".
CFG=/home/ubuntu/.claude.json
TOK=$(python3 -c "import json;print(json.load(open('$CFG'))['mcpServers']['notion']['env']['NOTION_TOKEN'])" 2>/dev/null)
[ -z "$TOK" ] && TOK=$(python3 -c "import re;print(re.search(r'NOTION_TOKEN = \"([^\"]+)\"', open('/home/ubuntu/.codex/config.toml').read()).group(1))" 2>/dev/null)

seq_jsonrpc() {
  printf '%s\n' \
    '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
    '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
    '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"API-post-search","arguments":{"query":""}}}'
}

run_case() {
  local label="$1"; shift
  echo "===== $label ====="
  ( seq_jsonrpc; sleep 8 ) | env "$@" NOTION_TOKEN="$TOK" timeout 20 /usr/local/bin/notion-mcp-server 2>/tmp/nmcp.err \
    | python3 -c "import sys,json
for l in sys.stdin:
  l=l.strip()
  if not l: continue
  try: o=json.loads(l)
  except: continue
  if o.get('id')==2:
    if 'result' in o: print('SEARCH_OK', str(o['result'])[:200])
    else: print('SEARCH_ERR', str(o.get('error'))[:200])"
  echo "  (server stderr tail:)"; tail -3 /tmp/nmcp.err 2>/dev/null | sed 's/^/    /'
}

run_case "DIRECT (no proxy)"  HTTP_PROXY= HTTPS_PROXY= http_proxy= https_proxy=
run_case "VIA maturana proxy" HTTP_PROXY=http://172.30.10.9:47833 HTTPS_PROXY=http://172.30.10.9:47833 http_proxy=http://172.30.10.9:47833 https_proxy=http://172.30.10.9:47833
