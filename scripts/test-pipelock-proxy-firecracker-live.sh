#!/usr/bin/env bash
set -euo pipefail

repo_root="$(pwd)"
agent_root=".maturana"
guest_ip="172.30.0.2"
host_ip="172.30.0.1"
ssh_user="ubuntu"
ssh_key=""
proxy_port="47833"

usage() {
  cat <<'USAGE'
Usage:
  scripts/test-pipelock-proxy-firecracker-live.sh [options]

Options:
  --repo-root PATH     Maturana checkout on the Linux host. Default: current dir.
  --agent-root PATH    Maturana runtime dir containing images/agents. Default: .maturana
  --guest-ip IP        Firecracker guest IP. Default: 172.30.0.2
  --host-ip IP         Firecracker host/tap IP visible to guest. Default: 172.30.0.1
  --ssh-user USER      Guest SSH user. Default: ubuntu
  --ssh-key PATH       Guest SSH key. Default: <agent-root>/images/firecracker/maturana-firecracker.id_rsa
  --proxy-port PORT    Host pipelock proxy port. Default: 47833

This script proves the Linux/Firecracker pipelock path without rebuilding the
guest image. It starts a temporary HTTPS upstream and pipelock proxy on the
Linux host, calls that upstream from inside the running Firecracker guest, and
verifies allowlist, header injection, TLS interception, and audit logging.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo-root) repo_root="$2"; shift 2 ;;
    --agent-root) agent_root="$2"; shift 2 ;;
    --guest-ip) guest_ip="$2"; shift 2 ;;
    --host-ip) host_ip="$2"; shift 2 ;;
    --ssh-user) ssh_user="$2"; shift 2 ;;
    --ssh-key) ssh_key="$2"; shift 2 ;;
    --proxy-port) proxy_port="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

cd "$repo_root"

if [[ -z "$ssh_key" ]]; then
  ssh_key="$agent_root/images/firecracker/maturana-firecracker.id_rsa"
fi

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 1
  fi
}

need cargo
need curl
need openssl
need python3
need scp
need ssh
need sudo

if ! sudo -n true >/dev/null 2>&1; then
  echo "passwordless sudo is required to trust the temporary upstream CA" >&2
  exit 1
fi

if [[ ! -f "$ssh_key" ]]; then
  echo "guest SSH key not found: $ssh_key" >&2
  exit 1
fi

if [[ ! -x target/debug/maturana ]]; then
  cargo build -p maturana-cli >/dev/null
fi

run_dir="$(mktemp -d "${TMPDIR:-/tmp}/maturana-pipelock-firecracker-live.XXXXXX")"
proxy_pid=""
upstream_pid=""
host_ca_path="/usr/local/share/ca-certificates/maturana-test-upstream-ca-$(basename "$run_dir").crt"

cleanup() {
  set +e
  if [[ -n "$proxy_pid" ]]; then kill "$proxy_pid" 2>/dev/null || true; fi
  if [[ -n "$upstream_pid" ]]; then kill "$upstream_pid" 2>/dev/null || true; fi
  if [[ -f "$host_ca_path" ]]; then
    sudo rm -f "$host_ca_path" >/dev/null 2>&1 || true
    sudo update-ca-certificates >/dev/null 2>&1 || true
  fi
  rm -rf "$run_dir"
}
trap cleanup EXIT

mkdir -p "$run_dir/upstream"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
  -keyout "$run_dir/upstream-ca.key" \
  -out "$run_dir/upstream-ca.crt" \
  -subj "/CN=Maturana Test CA" >/dev/null 2>&1

cat > "$run_dir/upstream/localhost.cnf" <<'CNF'
[req]
distinguished_name=req_distinguished_name
req_extensions=v3_req
prompt=no
[req_distinguished_name]
CN=localhost
[v3_req]
subjectAltName=@alt_names
[alt_names]
DNS.1=localhost
IP.1=127.0.0.1
CNF

openssl req -new -newkey rsa:2048 -nodes \
  -keyout "$run_dir/upstream/localhost.key" \
  -out "$run_dir/upstream/localhost.csr" \
  -config "$run_dir/upstream/localhost.cnf" >/dev/null 2>&1
openssl x509 -req -days 1 \
  -in "$run_dir/upstream/localhost.csr" \
  -CA "$run_dir/upstream-ca.crt" \
  -CAkey "$run_dir/upstream-ca.key" \
  -CAcreateserial \
  -out "$run_dir/upstream/localhost.crt" \
  -extensions v3_req \
  -extfile "$run_dir/upstream/localhost.cnf" >/dev/null 2>&1

sudo cp "$run_dir/upstream-ca.crt" "$host_ca_path"
sudo update-ca-certificates >/dev/null

cat > "$run_dir/upstream.py" <<'PY'
import http.server, ssl, sys

log_path, cert_path, key_path, port_path = sys.argv[1:]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        token = self.headers.get("X-Test-Token", "")
        with open(log_path, "a", encoding="utf-8") as handle:
            handle.write(f"path={self.path}\nX-Test-Token={token}\n")
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args):
        return

server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert_path, key_path)
server.socket = context.wrap_socket(server.socket, server_side=True)
with open(port_path, "w", encoding="utf-8") as handle:
    handle.write(str(server.server_port))
server.serve_forever()
PY

python3 "$run_dir/upstream.py" \
  "$run_dir/upstream.log" \
  "$run_dir/upstream/localhost.crt" \
  "$run_dir/upstream/localhost.key" \
  "$run_dir/upstream.port" &
upstream_pid=$!

for _ in $(seq 1 100); do
  if [[ -s "$run_dir/upstream.port" ]]; then break; fi
  sleep 0.1
done
upstream_port="$(cat "$run_dir/upstream.port")"

vault_home="$run_dir/home"
target/debug/maturana --home "$vault_home" pipelock init >/dev/null
target/debug/maturana --home "$vault_home" pipelock set api/token --value X-From-Firecracker-Pipelock >/dev/null
proxy_ca="$(target/debug/maturana --home "$vault_home" pipelock ca-cert | tail -n 1)"

cat > "$run_dir/MATURANA.proxy.md" <<SPEC
---
identity:
  id: firecracker-pipelock-live
  name: Firecracker Pipelock Live Test
  purpose: Verify Firecracker guest HTTPS egress through pipelock.
runtime:
  harness: codex
vm:
  provider: firecracker
  guest_os: linux
  vcpu: 1
  memory_mib: 512
  firecracker:
    kernel_image: none
    rootfs_image: none
    tap_name: tap-maturana0
network:
  egress_allowlist:
    - localhost
  proxy:
    enabled: true
    bind: 0.0.0.0:$proxy_port
    inject_headers:
      - host: localhost
        header: X-Test-Token
        source: pipelock:api/token
filesystem:
  mounts: []
---
SPEC

target/debug/maturana --home "$vault_home" pipelock proxy --spec "$run_dir/MATURANA.proxy.md" \
  > "$run_dir/proxy.log" 2>&1 &
proxy_pid=$!

for _ in $(seq 1 100); do
  if grep -q "pipelock proxy listening" "$run_dir/proxy.log" 2>/dev/null; then break; fi
  sleep 0.1
done

scp -q -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -i "$ssh_key" "$proxy_ca" "$ssh_user@$guest_ip:/tmp/maturana-pipelock-ca.crt"

response="$(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 \
  -i "$ssh_key" "$ssh_user@$guest_ip" \
  "curl -sS --max-time 20 --proxy http://$host_ip:$proxy_port --cacert /tmp/maturana-pipelock-ca.crt https://localhost:$upstream_port/test")"

if [[ "$response" != "ok" ]]; then
  echo "unexpected guest response: $response" >&2
  exit 1
fi

if ! grep -q "X-Test-Token=X-From-Firecracker-Pipelock" "$run_dir/upstream.log"; then
  echo "upstream did not receive injected header" >&2
  cat "$run_dir/upstream.log" >&2
  exit 1
fi

audit="$vault_home/audit/firecracker-pipelock-live-pipelock-proxy.jsonl"
grep -q "pipelock.proxy.allowed" "$audit"
grep -q '"injected_headers":1' "$audit"
grep -q '"tls_intercepted":true' "$audit"

echo "firecracker pipelock live test passed"
echo "guest: $ssh_user@$guest_ip"
echo "proxy: http://$host_ip:$proxy_port"
echo "upstream_port: $upstream_port"
echo "audit: $audit"
tail -n 1 "$audit"
