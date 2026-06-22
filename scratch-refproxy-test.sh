#!/usr/bin/env bash
# Runs ON a guest. Starts a minimal, textbook-correct CONNECT proxy and routes
# the notion-mcp-server through it. Decides: maturana-proxy bug vs notion-client bug.
set -uo pipefail

cat > /tmp/refproxy.cjs <<'JS'
const net = require('net');
net.createServer((client) => {
  client.once('data', (data) => {
    const line = data.toString('latin1').split('\r\n')[0];
    const m = line.match(/^CONNECT (\S+):(\d+)/);
    if (!m) return client.end();
    const upstream = net.connect(parseInt(m[2]), m[1], () => {
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      const i = data.indexOf('\r\n\r\n');
      const leftover = data.subarray(i + 4);
      if (leftover.length) upstream.write(leftover);
      client.pipe(upstream); upstream.pipe(client);
    });
    upstream.on('error', () => client.destroy());
    client.on('error', () => upstream.destroy());
  });
  client.on('error', () => {});
}).listen(48833, '127.0.0.1', () => console.log('refproxy on 127.0.0.1:48833'));
JS
node /tmp/refproxy.cjs >/tmp/refproxy.log 2>&1 &
RP=$!
sleep 1

TOK=$(python3 -c "import json;print(json.load(open('/home/ubuntu/.claude.json'))['mcpServers']['notion']['env']['NOTION_TOKEN'])")
echo "=== notion-mcp-server search THROUGH the reference proxy ==="
( printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"API-post-search","arguments":{"query":""}}}'; sleep 7 ) \
| env HTTP_PROXY=http://127.0.0.1:48833 HTTPS_PROXY=http://127.0.0.1:48833 NOTION_TOKEN="$TOK" timeout 18 /usr/local/bin/notion-mcp-server 2>/dev/null \
| python3 -c "import sys,json
for l in sys.stdin:
  l=l.strip()
  if not l: continue
  try: o=json.loads(l)
  except: continue
  if o.get('id')==2: print('REF-PROXY:', 'SEARCH_OK' if 'result' in o else ('SEARCH_ERR '+str(o.get('error'))))"
kill $RP 2>/dev/null
