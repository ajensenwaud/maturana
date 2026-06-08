param(
    [Parameter(Mandatory=$true)]
    [string]$AgentId,
    [Parameter(Mandatory=$true)]
    [string]$SessionId,
    [ValidateSet("codex", "claude-code", "opencode")]
    [string]$Harness = "codex",
    [string]$HostdUrl = "http://127.0.0.1:47832",
    [string]$SshUser = "ubuntu",
    [string]$SshKeyPath = ".\.maturana\keys\maturana-agent-ed25519",
    [string]$HarnessAuthGuestPath = "/home/ubuntu/.codex",
    [string]$SessiondUrl = "",
    [string]$SessiondTokenPath = ".\.maturana\sessiond\token"
)

$ErrorActionPreference = "Stop"
$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$stateDir = Join-Path $repoRoot ".maturana\agents\$AgentId\state"
New-Item -ItemType Directory -Force -Path $stateDir | Out-Null

function Resolve-AgentIp {
    $headers = @{}
    $tokenPath = Join-Path $repoRoot ".maturana\hostd\token"
    if (Test-Path -LiteralPath $tokenPath) {
        $headers["X-Maturana-Hostd-Token"] = (Get-Content -LiteralPath $tokenPath -Raw).Trim()
    }
    $response = Invoke-RestMethod -Method Get -Uri "$($HostdUrl.TrimEnd('/'))/vms" -Headers $headers
    $vm = @($response.vms | Where-Object { $_.name -eq "maturana-$AgentId" } | Select-Object -First 1)
    if (!$vm -or [string]::IsNullOrWhiteSpace([string]$vm.ipv4)) {
        throw "Could not discover IPv4 for maturana-$AgentId from hostd."
    }
    [string]$vm.ipv4
}

function Invoke-Guest {
    param([string]$Ip, [string]$Command)
    ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=10 -i $SshKeyPath "$SshUser@$Ip" $Command
    if ($LASTEXITCODE -ne 0) {
        throw "Guest command failed with exit code ${LASTEXITCODE}: $Command"
    }
}

function Copy-ToGuest {
    param([string]$Ip, [string]$Source, [string]$Destination)
    scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=NUL -o ConnectTimeout=10 -i $SshKeyPath $Source "$SshUser@$Ip`:$Destination"
    if ($LASTEXITCODE -ne 0) {
        throw "Guest copy failed with exit code ${LASTEXITCODE}: $Source -> $Destination"
    }
}

$ip = Resolve-AgentIp
$sessiondToken = ""
if (Test-Path -LiteralPath $SessiondTokenPath) {
    $sessiondToken = (Get-Content -LiteralPath $SessiondTokenPath -Raw).Trim()
}
if ([string]::IsNullOrWhiteSpace($SessiondUrl)) {
    $SessiondUrl = "__MATURANA_DEFAULT_SESSIOND_URL__"
}

$envFile = @"
MATURANA_AGENT_ID=$AgentId
MATURANA_SESSION_ID=$SessionId
MATURANA_SESSIOND_URL=$SessiondUrl
MATURANA_SESSIOND_TOKEN=$sessiondToken
MATURANA_HARNESS=$Harness
CODEX_HOME=$HarnessAuthGuestPath
"@
$envPath = Join-Path $stateDir "sessiond.env"
Set-Content -LiteralPath $envPath -Value ($envFile -replace "`r`n", "`n") -NoNewline

$runner = @'
#!/usr/bin/env bash
set -euo pipefail
if [ -f /agent/sessiond.env ]; then
  set -a
  . /agent/sessiond.env
  set +a
fi
if [ "${MATURANA_HARNESS:-}" = "opencode" ] && [ -f "$HOME/.maturana-env" ]; then
  set -a
  . "$HOME/.maturana-env"
  set +a
fi
mkdir -p /var/log/maturana /workspace
cd /workspace

sessiond_url="${MATURANA_SESSIOND_URL:-__MATURANA_DEFAULT_SESSIOND_URL__}"
if [ "$sessiond_url" = "__MATURANA_DEFAULT_SESSIOND_URL__" ]; then
  host_gateway="$(ip route | awk '/default/ {print $3; exit}')"
  sessiond_url="http://$host_gateway:47834"
fi
headers=(-H "content-type: application/json")
if [ -n "${MATURANA_SESSIOND_TOKEN:-}" ]; then
  headers+=(-H "x-maturana-session-token: ${MATURANA_SESSIOND_TOKEN}")
fi

heartbeat() {
  status="$1"
  message_id="${2:-}"
  error="${3:-}"
  heartbeat_body="$(MATURANA_WORKER_STATUS="$status" MATURANA_WORKER_MESSAGE_ID="$message_id" MATURANA_WORKER_ERROR="$error" python3 - <<'PY'
import json, os
print(json.dumps({
  "agent_id": os.environ["MATURANA_AGENT_ID"],
  "session_id": os.environ["MATURANA_SESSION_ID"],
  "status": os.environ["MATURANA_WORKER_STATUS"],
  "message_id": os.environ.get("MATURANA_WORKER_MESSAGE_ID") or None,
  "error": os.environ.get("MATURANA_WORKER_ERROR") or None,
}))
PY
)"
  curl -fsS -X POST "$sessiond_url/session/heartbeat" "${headers[@]}" --data "$heartbeat_body" >/dev/null 2>>/var/log/maturana/worker.err.log || true
}

while true; do
  date -Is > /var/log/maturana/heartbeat
  heartbeat idle
  claim_body="$(python3 - <<'PY'
import json, os
print(json.dumps({"agent_id": os.environ["MATURANA_AGENT_ID"], "session_id": os.environ["MATURANA_SESSION_ID"], "limit": 1}))
PY
)"
  claim="$(curl -fsS -X POST "$sessiond_url/session/claim" "${headers[@]}" --data "$claim_body" 2>>/var/log/maturana/worker.err.log || true)"
  count="$(printf '%s' "$claim" | python3 -c 'import json,sys; print(len(json.loads(sys.stdin.read() or "{\"messages\":[]}").get("messages", [])))' 2>/dev/null || echo 0)"
  if [ "$count" = "0" ]; then
    sleep 2
    continue
  fi
  printf '%s' "$claim" > /tmp/maturana-session-claim.json
  msg_id="$(python3 - <<'PY'
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
print(d["messages"][0]["id"])
PY
)"
  heartbeat claimed "$msg_id"
  channel="$(python3 - <<'PY'
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
print(d["messages"][0]["channel"])
PY
)"
  platform_id="$(python3 - <<'PY'
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
print(d["messages"][0]["platform_id"])
PY
)"
  thread_id="$(python3 - <<'PY'
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
print(d["messages"][0].get("thread_id") or "")
PY
)"
  python3 - <<'PY' >/tmp/maturana-session-prompt.txt
import json
d=json.load(open("/tmp/maturana-session-claim.json"))
c=json.loads(d["messages"][0]["content"])
print(c.get("prompt") or c.get("text") or "")
PY

  response=""
  if [ "${MATURANA_HARNESS}" = "codex" ]; then
    if codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -C /workspace -o /tmp/maturana-session-response.txt "$(cat /tmp/maturana-session-prompt.txt)" >>/var/log/maturana/worker.out.log 2>>/var/log/maturana/worker.err.log; then
      response="$(cat /tmp/maturana-session-response.txt)"
    else
      response="I hit an error while processing that message."
    fi
  elif [ "${MATURANA_HARNESS}" = "claude-code" ]; then
    if claude -p "$(cat /tmp/maturana-session-prompt.txt)" >/tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log; then
      response="$(cat /tmp/maturana-session-response.txt)"
    else
      response="I hit an error while processing that message."
    fi
  elif [ "${MATURANA_HARNESS}" = "opencode" ]; then
    opencode_args=(run)
    if [ -n "${OPENROUTER_API_KEY:-}" ]; then
      opencode_args+=(-m openrouter/anthropic/claude-sonnet-4.5)
    fi
    opencode_args+=("$(cat /tmp/maturana-session-prompt.txt)")
    if opencode "${opencode_args[@]}" >/tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log; then
      response="$(cat /tmp/maturana-session-response.txt)"
      if [ -z "$response" ] && [ -f "$HOME/.local/share/opencode/opencode.db" ]; then
        response="$(python3 - <<'PY'
import json, os, sqlite3
db = os.path.expanduser("~/.local/share/opencode/opencode.db")
con = sqlite3.connect(db)
rows = con.execute("""
select part.data
from part
join message on message.id = part.message_id
where json_extract(part.data, '$.type') = 'text'
  and json_extract(message.data, '$.role') = 'assistant'
order by part.time_updated desc
limit 1
""").fetchall()
if rows:
    print(json.loads(rows[0][0]).get("text", ""))
PY
)"
      fi
    else
      response="I hit an error while processing that message."
    fi
  else
    response="Unsupported harness: ${MATURANA_HARNESS}"
  fi
  if [ -z "$response" ]; then
    response="I processed that message but did not receive a text response from the harness."
  fi

  outbound_body="$(MATURANA_MSG_ID="$msg_id" MATURANA_CHANNEL="$channel" MATURANA_PLATFORM_ID="$platform_id" MATURANA_THREAD_ID="$thread_id" MATURANA_RESPONSE="$response" python3 - <<'PY'
import json, os
print(json.dumps({
  "agent_id": os.environ["MATURANA_AGENT_ID"],
  "session_id": os.environ["MATURANA_SESSION_ID"],
  "in_reply_to": os.environ["MATURANA_MSG_ID"],
  "kind": "chat",
  "channel": os.environ["MATURANA_CHANNEL"],
  "platform_id": os.environ["MATURANA_PLATFORM_ID"],
  "thread_id": os.environ.get("MATURANA_THREAD_ID") or None,
  "content": json.dumps({"text": os.environ["MATURANA_RESPONSE"]}),
}))
PY
)"
  if ! curl -fsS -X POST "$sessiond_url/session/outbound" "${headers[@]}" --data "$outbound_body" >/dev/null 2>>/var/log/maturana/worker.err.log; then
    heartbeat error "$msg_id" "failed to post outbound"
    sleep 2
    continue
  fi
  complete_body="$(MATURANA_MSG_ID="$msg_id" python3 - <<'PY'
import json, os
print(json.dumps({"agent_id": os.environ["MATURANA_AGENT_ID"], "session_id": os.environ["MATURANA_SESSION_ID"], "message_ids": [os.environ["MATURANA_MSG_ID"]]}))
PY
)"
  if curl -fsS -X POST "$sessiond_url/session/complete" "${headers[@]}" --data "$complete_body" >/dev/null 2>>/var/log/maturana/worker.err.log; then
    heartbeat completed "$msg_id"
  else
    heartbeat error "$msg_id" "failed to mark complete"
  fi
done
'@
$runnerPath = Join-Path $stateDir "run-agent.sh"
Set-Content -LiteralPath $runnerPath -Value ($runner -replace "`r`n", "`n") -NoNewline

Copy-ToGuest -Ip $ip -Source $envPath -Destination "/tmp/sessiond.env"
Copy-ToGuest -Ip $ip -Source $runnerPath -Destination "/tmp/run-agent.sh"
Invoke-Guest -Ip $ip -Command "sudo mkdir -p /agent /opt/maturana/bin /var/log/maturana /workspace && sudo mv /tmp/sessiond.env /agent/sessiond.env && sudo mv /tmp/run-agent.sh /opt/maturana/bin/run-agent.sh && sudo chown ${SshUser}:${SshUser} /agent/sessiond.env /opt/maturana/bin/run-agent.sh && sudo chmod 0600 /agent/sessiond.env && sudo chmod 0755 /opt/maturana/bin/run-agent.sh && sudo systemctl restart maturana-agent.service"
Write-Host "Refreshed $AgentId worker at $ip"
