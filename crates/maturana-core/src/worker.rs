use crate::spec::HarnessRuntime;
use anyhow::Context;

/// Sentinel for the graph URL, resolved in the guest to `http://<gateway>:47835`
/// just like the sessiond URL.
pub const DEFAULT_GRAPH_URL_SENTINEL: &str = "__MATURANA_DEFAULT_GRAPH_URL__";

/// Read the host MaturanaGraph token (`<home>/graph/token`) if the graph service
/// is set up. Returns `None` when absent/empty, which keeps graph env out of the
/// guest entirely.
pub fn read_graph_token(home_root: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(home_root.join("graph").join("token"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|token| !token.is_empty())
}

#[derive(Debug, Clone)]
pub struct GuestWorkerConfig {
    pub agent_id: String,
    pub session_id: String,
    pub sessiond_url: String,
    pub sessiond_token: String,
    pub harness: HarnessRuntime,
    pub harness_auth_guest_path: String,
    pub headless_chrome: bool,
    /// Host MaturanaGraph token, present when the graph service is set up.
    pub graph_token: Option<String>,
    /// The named graph this agent connects to, present only when
    /// `knowledge_graph.enabled`. Both this and the token gate graph access.
    pub graph_name: Option<String>,
}

pub fn render_session_env(config: &GuestWorkerConfig) -> String {
    let mut env = format!(
        "MATURANA_AGENT_ID={}\nMATURANA_SESSION_ID={}\nMATURANA_SESSIOND_URL={}\nMATURANA_SESSIOND_TOKEN={}\nMATURANA_HARNESS={}\nCODEX_HOME={}\nMATURANA_HEADLESS_CHROME={}\nPLAYWRIGHT_BROWSERS_PATH={}\n",
        shell_env_value(&config.agent_id),
        shell_env_value(&config.session_id),
        shell_env_value(&config.sessiond_url),
        shell_env_value(&config.sessiond_token),
        shell_env_value(harness_name(&config.harness)),
        shell_env_value(&config.harness_auth_guest_path),
        shell_env_value(if config.headless_chrome { "1" } else { "0" }),
        shell_env_value("/opt/maturana/browsers"),
    );
    // MaturanaGraph access: only when the agent opted in (graph_name) and the
    // host has a graph token. The URL is a sentinel resolved at runtime.
    if let (Some(token), Some(name)) = (&config.graph_token, &config.graph_name) {
        env.push_str(&format!(
            "MATURANA_GRAPH_URL={}\nMATURANA_GRAPH_TOKEN={}\nMATURANA_GRAPH_NAME={}\n",
            shell_env_value(DEFAULT_GRAPH_URL_SENTINEL),
            shell_env_value(token),
            shell_env_value(name),
        ));
    }
    env
}

pub fn render_run_agent() -> &'static str {
    RUN_AGENT
}

pub fn render_guest_bootstrap() -> &'static str {
    GUEST_BOOTSTRAP
}

pub fn render_firecracker_bootstrap() -> &'static str {
    FIRECRACKER_BOOTSTRAP
}

pub fn render_firecracker_netplan(guest_mac: &str, guest_ip: &str, host_ip: &str) -> String {
    format!(
        "network:\n  version: 2\n  ethernets:\n    eth0:\n      match:\n        macaddress: \"{guest_mac}\"\n      set-name: eth0\n      dhcp4: false\n      addresses:\n        - {guest_ip}/30\n      routes:\n        - to: default\n          via: {host_ip}\n      nameservers:\n        addresses:\n          - 1.1.1.1\n          - 8.8.8.8\n"
    )
}

pub fn render_firecracker_cloud_cfg() -> &'static str {
    "network: {config: disabled}\n"
}

pub fn render_firecracker_proxy_env(
    proxy_enabled: bool,
    proxy_bind: Option<&str>,
    host_ip: &str,
) -> anyhow::Result<Option<String>> {
    if !proxy_enabled {
        return Ok(None);
    }
    let bind = proxy_bind.ok_or_else(|| anyhow::anyhow!("proxy bind is required"))?;
    let port = bind
        .rsplit_once(':')
        .map(|(_, port)| port)
        .ok_or_else(|| anyhow::anyhow!("proxy bind must include a port: {bind}"))?;
    port.parse::<u16>()
        .with_context(|| format!("invalid proxy bind port in {bind}"))?;
    Ok(Some(format!(
        "MATURANA_USE_HOST_PROXY=1\nMATURANA_PROXY_HOST={host_ip}\nMATURANA_PROXY_PORT={port}\nMATURANA_PROXY_HTTPS=1\nNO_PROXY=localhost,127.0.0.1,::1\n"
    )))
}

pub fn render_harness_install(harness: &HarnessRuntime, headless_chrome: bool) -> String {
    let npm_package = match harness {
        HarnessRuntime::Codex => "@openai/codex",
        HarnessRuntime::ClaudeCode => "@anthropic-ai/claude-code",
        HarnessRuntime::Opencode => "opencode-ai",
    };
    let binary = harness_binary(harness);
    let browser_install = if headless_chrome {
        r#"
$SUDO mkdir -p /opt/maturana/browsers /opt/maturana/bin
$SUDO chown -R "${USER}:${USER}" /opt/maturana/browsers
$SUDO npm install -g playwright
PLAYWRIGHT_BROWSERS_PATH=/opt/maturana/browsers npx --yes playwright install --with-deps chromium || PLAYWRIGHT_BROWSERS_PATH=/opt/maturana/browsers npx --yes playwright install chromium
cat > /tmp/maturana-browser-smoke.js <<'JS'
const { chromium } = require('playwright');
(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  await page.setContent('<title>maturana-browser-ok</title>');
  console.log(await page.title());
  await browser.close();
})();
JS
$SUDO mv /tmp/maturana-browser-smoke.js /opt/maturana/bin/browser-smoke.js
$SUDO chmod 0755 /opt/maturana/bin/browser-smoke.js
cat > /tmp/maturana-browse.js <<'JS'
// Maturana browse driver: stateless single-shot Playwright commands.
// Usage: node /opt/maturana/bin/browse.js '{"cmd":"text","url":"https://..."}'
// Commands:
//   {"cmd":"navigate","url"}                       -> {ok,status,url,title}
//   {"cmd":"text","url","selector"?}               -> {ok,...,text}
//   {"cmd":"screenshot","url","out"?}              -> {ok,...,screenshot}
//   {"cmd":"click","url","selector"}               -> {ok,...,text} (after click)
const { chromium } = require('playwright');
(async () => {
  const cmd = JSON.parse(process.argv[2] || '{}');
  const out = { ok: false };
  if (!cmd.url) {
    console.log(JSON.stringify({ ok: false, error: 'missing url' }));
    process.exit(1);
  }
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage();
    const response = await page.goto(cmd.url, { waitUntil: 'domcontentloaded', timeout: 30000 });
    out.status = response ? response.status() : null;
    if (cmd.cmd === 'click' && cmd.selector) {
      await page.click(cmd.selector, { timeout: 10000 });
      await page.waitForLoadState('domcontentloaded');
    }
    out.url = page.url();
    out.title = await page.title();
    if (cmd.cmd === 'screenshot') {
      const dest = cmd.out || '/workspace/screenshot.png';
      await page.screenshot({ path: dest, fullPage: true });
      out.screenshot = dest;
    }
    if (cmd.cmd === 'text' || cmd.cmd === 'click') {
      const target = cmd.cmd === 'text' && cmd.selector
        ? page.locator(cmd.selector).first()
        : page.locator('body');
      out.text = (await target.innerText()).slice(0, 20000);
    }
    out.ok = true;
  } catch (error) {
    out.error = String(error);
  } finally {
    await browser.close();
  }
  console.log(JSON.stringify(out));
  process.exit(out.ok ? 0 : 1);
})();
JS
$SUDO mv /tmp/maturana-browse.js /opt/maturana/bin/browse.js
$SUDO chmod 0755 /opt/maturana/bin/browse.js
"#
    } else {
        ""
    };
    format!(
        r#"#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
else
  SUDO="sudo"
fi
# Idempotent: nothing to do if the harness is already present. This lets the
# script run as a first-boot systemd one-shot that no-ops on later boots.
if command -v {binary} >/dev/null 2>&1; then
  echo "maturana: {binary} already installed"
  exit 0
fi
if command -v cloud-init >/dev/null 2>&1; then
  $SUDO cloud-init status --wait || true
fi
while $SUDO fuser /var/lib/dpkg/lock-frontend /var/lib/dpkg/lock /var/lib/apt/lists/lock >/dev/null 2>&1; do
  sleep 5
done
$SUDO dpkg --configure -a || true
$SUDO apt-get clean
$SUDO apt-get update
$SUDO apt-get install -y ca-certificates curl git nodejs npm python3 ripgrep
$SUDO npm install -g {npm_package}
{browser_install}
"#
    )
}

pub fn render_systemd_service(description: &str, user: &str) -> String {
    format!(
        "[Unit]\nDescription={}\nAfter=network-online.target maturana-harness-install.service\nWants=network-online.target\n\n[Service]\nUser={}\nWorkingDirectory=/workspace\nExecStart=/opt/maturana/bin/run-agent.sh\nRestart=on-failure\nRestartSec=10\nStandardOutput=append:/var/log/maturana/agent.log\nStandardError=append:/var/log/maturana/agent.err.log\n\n[Install]\nWantedBy=multi-user.target\n",
        systemd_value(description),
        systemd_value(user),
    )
}

/// First-boot one-shot that installs the harness inside the guest over its own
/// network. Used by Firecracker, where running the install in the offline
/// libguestfs build appliance is unreliable (no/blocked network on some hosts);
/// the booted guest always has working egress. Ordered before the agent so the
/// first turn has its harness, and bounded so a stuck install can't wedge boot.
pub fn render_harness_install_service() -> &'static str {
    "[Unit]\nDescription=Maturana harness install (first boot)\nAfter=network-online.target\nWants=network-online.target\nBefore=maturana-agent.service\nConditionPathExists=/opt/maturana/bin/install-harness.sh\n\n[Service]\nType=oneshot\nRemainAfterExit=yes\nTimeoutStartSec=900\nExecStart=/opt/maturana/bin/install-harness.sh\n\n[Install]\nWantedBy=multi-user.target\n"
}

pub fn harness_name(harness: &HarnessRuntime) -> &'static str {
    match harness {
        HarnessRuntime::Codex => "codex",
        HarnessRuntime::ClaudeCode => "claude-code",
        HarnessRuntime::Opencode => "opencode",
    }
}

/// The executable name each harness installs on PATH.
pub fn harness_binary(harness: &HarnessRuntime) -> &'static str {
    match harness {
        HarnessRuntime::Codex => "codex",
        HarnessRuntime::ClaudeCode => "claude",
        HarnessRuntime::Opencode => "opencode",
    }
}

fn shell_env_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn systemd_value(value: &str) -> String {
    value
        .chars()
        .filter(|ch| *ch != '\n' && *ch != '\r')
        .collect()
}

const RUN_AGENT: &str = r#"#!/usr/bin/env bash
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
if [ -f /agent/proxy.env ]; then
  set -a
  . /agent/proxy.env
  set +a
  if [ "${MATURANA_USE_HOST_PROXY:-0}" = "1" ] && [ -n "${MATURANA_PROXY_PORT:-}" ]; then
    proxy_host="${MATURANA_PROXY_HOST:-}"
    if [ -z "$proxy_host" ]; then
      proxy_host="$(ip route | awk '/default/ {print $3; exit}')"
    fi
    export HTTP_PROXY="http://$proxy_host:$MATURANA_PROXY_PORT"
    export http_proxy="$HTTP_PROXY"
    if [ "${MATURANA_PROXY_HTTPS:-0}" = "1" ]; then
      export HTTPS_PROXY="$HTTP_PROXY"
      export https_proxy="$HTTP_PROXY"
    fi
    export NO_PROXY="${NO_PROXY:-localhost,127.0.0.1,::1}"
    export no_proxy="$NO_PROXY"
  fi
fi
mkdir -p /var/log/maturana /workspace /memory /wiki
cd /workspace

sessiond_url="${MATURANA_SESSIOND_URL:-__MATURANA_DEFAULT_SESSIOND_URL__}"
if [ "$sessiond_url" = "__MATURANA_DEFAULT_SESSIOND_URL__" ]; then
  host_gateway="$(ip route | awk '/default/ {print $3; exit}')"
  sessiond_url="http://$host_gateway:47834"
fi
sessiond_host="${sessiond_url#http://}"
sessiond_host="${sessiond_host#https://}"
sessiond_host="${sessiond_host%%/*}"
sessiond_host="${sessiond_host%%:*}"
if [ -n "$sessiond_host" ]; then
  export NO_PROXY="${NO_PROXY:-localhost,127.0.0.1,::1},$sessiond_host"
  export no_proxy="$NO_PROXY"
fi

# Resolve the MaturanaGraph URL sentinel to the host gateway, like sessiond, so
# the agent (and the maturana-graph skill) can reach the graph service.
if [ "${MATURANA_GRAPH_URL:-}" = "__MATURANA_DEFAULT_GRAPH_URL__" ]; then
  host_gateway="${host_gateway:-$(ip route | awk '/default/ {print $3; exit}')}"
  export MATURANA_GRAPH_URL="http://$host_gateway:47835"
fi

headers=(-H "content-type: application/json")
if [ -n "${MATURANA_SESSIOND_TOKEN:-}" ]; then
  headers+=(-H "x-maturana-session-token: ${MATURANA_SESSIOND_TOKEN}")
fi

# Self-forge helper, installed on PATH: lets the in-guest agent build + run a
# sandboxed WebAssembly capability on the fly via sessiond's /session/forge. The
# host enforces the `self_forge` capability and the fuel/memory/timeout sandbox.
forge_bin="${HOME:-/home/ubuntu}/.local/bin"
mkdir -p "$forge_bin"
cat > "$forge_bin/maturana-forge" <<'FORGEEOF'
#!/usr/bin/env bash
set -euo pipefail
name="${1:-}"; shift || true
if [ -z "$name" ]; then
  echo "usage: maturana-forge <name> [--input JSON] [--wasm BASE64] [--desc TEXT]   (WAT on stdin)" >&2
  exit 2
fi
input='{}'; desc='forged on the fly'; src=''; fmt='wat'
while [ "$#" -gt 0 ]; do
  case "$1" in
    --input) input="${2:-}"; shift 2;;
    --wasm) src="${2:-}"; fmt='wasm'; shift 2;;
    --desc) desc="${2:-}"; shift 2;;
    *) echo "maturana-forge: unknown arg '$1'" >&2; exit 2;;
  esac
done
if [ -f /agent/sessiond.env ]; then set -a; . /agent/sessiond.env; set +a; fi
url="${MATURANA_SESSIOND_URL:-}"
case "$url" in
  ''|*DEFAULT*) gw="$(ip route | awk '/default/ {print $3; exit}')"; url="http://${gw}:47834";;
esac
if [ "$fmt" = "wat" ]; then src="$(cat)"; fi
msg="$(cat /tmp/maturana-current-msg 2>/dev/null || true)"
body="$(MF_NAME="$name" MF_DESC="$desc" MF_FMT="$fmt" MF_SRC="$src" MF_INPUT="$input" MF_MSG="$msg" python3 - <<'PY'
import json, os
print(json.dumps({
  "agent_id": os.environ["MATURANA_AGENT_ID"],
  "session_id": os.environ["MATURANA_SESSION_ID"],
  "message_id": os.environ.get("MF_MSG") or None,
  "name": os.environ["MF_NAME"],
  "description": os.environ["MF_DESC"],
  "format": os.environ["MF_FMT"],
  "source": os.environ["MF_SRC"],
  "input": os.environ["MF_INPUT"],
}))
PY
)"
h=(-H "content-type: application/json")
if [ -n "${MATURANA_SESSIOND_TOKEN:-}" ]; then h+=(-H "x-maturana-session-token: ${MATURANA_SESSIOND_TOKEN}"); fi
curl -sS -X POST "${url}/session/forge" "${h[@]}" --data "$body"
echo
FORGEEOF
chmod 0755 "$forge_bin/maturana-forge" 2>/dev/null || true
export PATH="$forge_bin:${PATH}"

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

# Progress streamer: reads `codex exec --json` JSONL on stdin, POSTs distilled
# live-progress events to sessiond (tool calls, the agent message), and prints
# the final agent text to stdout (captured as the reply). Self-contained and
# crash-proof so a parse hiccup never drops the turn.
cat > /tmp/maturana-stream-progress.py <<'PYEOF'
import json, os, subprocess, sys

url = os.environ.get("SESSIOND_URL", "")
token = os.environ.get("MATURANA_SESSIOND_TOKEN", "")
agent = os.environ.get("MATURANA_AGENT_ID", "")
session = os.environ.get("MATURANA_SESSION_ID", "")
msg = os.environ.get("MSG_ID", "")
seq = 0
final = []

def post(kind, text):
    global seq
    if not url or not msg:
        return
    body = json.dumps({"agent_id": agent, "session_id": session,
                       "message_id": msg, "seq": seq, "kind": kind, "text": text})
    seq += 1
    args = ["curl", "-fsS", "-X", "POST", url + "/session/progress",
            "-H", "content-type: application/json"]
    if token:
        args += ["-H", "x-maturana-session-token: " + token]
    args += ["--data", body]
    try:
        subprocess.run(args, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=5)
    except Exception:
        pass

US = "\x1f"  # separates the tool key from its detail in a "tool" progress event

def first_line(s, n):
    return (s or "").strip().split("\n", 1)[0][:n]

def detail_of(item, *keys):
    for k in keys:
        v = item.get(k)
        if isinstance(v, str) and v.strip():
            return first_line(v, 200)
        if isinstance(v, (int, float)):
            return str(v)
        if isinstance(v, dict):
            inner = detail_of(v, "query", "command", "path", "url", "name")
            if inner:
                return inner
    return ""

def files_of(item):
    changes = item.get("changes") or item.get("files") or []
    out = []
    if isinstance(changes, list):
        for c in changes:
            if isinstance(c, dict):
                p = c.get("path") or c.get("file")
                if p:
                    out.append(p)
            elif isinstance(c, str):
                out.append(c)
    return ", ".join(out)[:200]

def emit_tool(key, detail):
    # Structured tool line: the host maps the key to an icon + title and renders
    # the detail in monospace (OpenClaw-style rich progress).
    post("tool", key + US + (detail or ""))

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        ev = json.loads(line)
    except Exception:
        continue
    t = ev.get("type", "")
    item = ev.get("item", {}) if isinstance(ev.get("item"), dict) else {}
    it = item.get("type", "")
    if t == "item.started":
        if it == "command_execution":
            emit_tool("bash", detail_of(item, "command"))
        elif it == "web_search":
            emit_tool("web_search", detail_of(item, "query", "search_query", "action"))
        elif it == "file_change":
            emit_tool("edit", detail_of(item, "path") or files_of(item))
        elif it in ("mcp_tool_call", "tool_call", "function_call"):
            emit_tool("tool_call", detail_of(item, "tool", "name", "server", "invocation"))
        elif it in ("patch_apply", "apply_patch"):
            emit_tool("apply_patch", detail_of(item, "path") or files_of(item))
        elif it in ("agent_message", "reasoning"):
            pass  # surfaced on completion below
        elif it:
            emit_tool(it, detail_of(item, "command", "query", "path", "url", "name", "tool"))
    elif t == "item.completed":
        if it == "command_execution":
            code = item.get("exit_code")
            if code not in (0, None):
                emit_tool("bash", "exit %s: %s" % (code, detail_of(item, "command")))
        elif it == "agent_message":
            txt = item.get("text") or ""
            final.append(txt)
            post("text", txt[:3500])
        elif it == "reasoning":
            txt = item.get("text") or item.get("summary") or ""
            if isinstance(txt, list):
                txt = " ".join(str(x) for x in txt)
            if (txt or "").strip():
                post("thinking", first_line(txt, 240))
    elif t in ("turn.failed", "error"):
        post("status", "error")

post("status", "done")
sys.stdout.write("\n".join(final))
PYEOF

# Claude streamer: reads `claude --output-format stream-json --include-partial-messages`
# and POSTs *real* text deltas (token streaming) + tool-use to the progress lane,
# throttled so we don't curl per token. Prints the final text to stdout.
cat > /tmp/maturana-stream-claude.py <<'PYEOF'
import json, os, subprocess, sys, time

url = os.environ.get("SESSIOND_URL", "")
token = os.environ.get("MATURANA_SESSIOND_TOKEN", "")
agent = os.environ.get("MATURANA_AGENT_ID", "")
session = os.environ.get("MATURANA_SESSION_ID", "")
msg = os.environ.get("MSG_ID", "")
seq = 0
text = []
last_post = 0.0
final = None

def post(kind, body_text):
    global seq
    if not url or not msg:
        return
    body = json.dumps({"agent_id": agent, "session_id": session,
                       "message_id": msg, "seq": seq, "kind": kind, "text": body_text})
    seq += 1
    args = ["curl", "-fsS", "-X", "POST", url + "/session/progress",
            "-H", "content-type: application/json"]
    if token:
        args += ["-H", "x-maturana-session-token: " + token]
    args += ["--data", body]
    try:
        subprocess.run(args, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=5)
    except Exception:
        pass

def post_text(force=False):
    global last_post
    now = time.time()
    if text and (force or now - last_post >= 0.4):
        post("text", "".join(text)[:3500])
        last_post = now

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        ev = json.loads(line)
    except Exception:
        continue
    t = ev.get("type", "")
    if t == "stream_event":
        e = ev.get("event", {}) or {}
        et = e.get("type", "")
        if et == "content_block_delta":
            d = e.get("delta", {}) or {}
            if d.get("type") == "text_delta":
                text.append(d.get("text", ""))
                post_text()
        elif et == "content_block_start":
            cb = e.get("content_block", {}) or {}
            if cb.get("type") == "tool_use":
                post("tool", "using: " + (cb.get("name") or "tool"))
    elif t == "assistant":
        # Non-partial fallback: a whole assistant message in one event.
        for block in (ev.get("message", {}) or {}).get("content", []) or []:
            if block.get("type") == "text" and not text:
                text.append(block.get("text", ""))
            elif block.get("type") == "tool_use":
                post("tool", "using: " + (block.get("name") or "tool"))
        post_text(force=True)
    elif t == "result":
        r = ev.get("result")
        if isinstance(r, str):
            final = r

post_text(force=True)
post("status", "done")
sys.stdout.write(final if final is not None else "".join(text))
PYEOF

# opencode collapser: reads `opencode run --format json` JSONL on stdin (one
# event per line, discriminated by `type`), POSTs distilled live progress to
# sessiond, and on EOF prints the FINAL assistant text. NanoClaw model: process
# exit (EOF) is the terminal signal (opencode exits at session.status=idle), the
# final text is read WHOLE as the LAST complete `text` part — never accumulated
# from deltas, never an earlier per-step preamble. reasoning/tool/step events are
# ACTIVITY ONLY (liveness + progress lane) and contribute nothing to the answer.
# The opencode sessionID is the opaque continuation token: captured the first
# time we see it and persisted IMMEDIATELY so a mid-turn crash can still resume.
cat > /tmp/maturana-stream-opencode.py <<'PYEOF'
import json, os, subprocess, sys

url = os.environ.get("SESSIOND_URL", "")
token = os.environ.get("MATURANA_SESSIOND_TOKEN", "")
agent = os.environ.get("MATURANA_AGENT_ID", "")
session = os.environ.get("MATURANA_SESSION_ID", "")
msg = os.environ.get("MSG_ID", "")
session_file = os.environ.get("MATURANA_OC_SESSION_FILE", "")
seq = 0
texts = []          # one entry per completed text part, in stream order
current = None       # id of the text part currently streaming, if any
saved_sid = False

def post(kind, body_text):
    global seq
    if not url or not msg:
        return
    body = json.dumps({"agent_id": agent, "session_id": session,
                       "message_id": msg, "seq": seq, "kind": kind, "text": body_text})
    seq += 1
    args = ["curl", "-fsS", "-X", "POST", url + "/session/progress",
            "-H", "content-type: application/json"]
    if token:
        args += ["-H", "x-maturana-session-token: " + token]
    args += ["--data", body]
    try:
        subprocess.run(args, stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=5)
    except Exception:
        pass

def first_line(s, n):
    return (s or "").strip().split("\n", 1)[0][:n]

def save_session(sid):
    # Persist the opaque continuation token immediately (NanoClaw init.continuation).
    global saved_sid
    if saved_sid or not sid or not session_file:
        return
    try:
        tmp = session_file + ".tmp"
        with open(tmp, "w") as fh:
            fh.write(sid)
        os.replace(tmp, session_file)
        saved_sid = True
    except Exception:
        pass

def sid_of(ev, part):
    for src in (ev, part, ev.get("part") or {}, ev.get("properties") or {}):
        if isinstance(src, dict):
            v = src.get("sessionID") or src.get("session_id") or src.get("sessionId")
            if v:
                return v
    return None

# iter(readline, "") reads one line at a time WITHOUT CPython's stdin read-ahead
# buffer, so each event's progress POST fires as opencode emits it (live drafts)
# rather than batched at EOF.
for line in iter(sys.stdin.readline, ""):
    line = line.strip()
    if not line:
        continue
    try:
        ev = json.loads(line)
    except Exception:
        continue
    t = ev.get("type", "")
    part = ev.get("part") if isinstance(ev.get("part"), dict) else {}
    pt = part.get("type", "")
    sid = sid_of(ev, part)
    if sid:
        save_session(sid)

    if pt == "text" or t == "text":
        # A text part. opencode streams it incrementally then marks it complete
        # with part.time.end. We REPLACE (never concatenate) the running value
        # for this part id, so the slot always holds the whole part, and only
        # commit it as a finished step-answer once part.time.end is set. The LAST
        # committed text part is the final answer; earlier ones are per-step
        # preambles ("I'll search…") that we deliberately discard.
        body = part.get("text", "") if part else ev.get("text", "")
        pid = part.get("id") if part else ev.get("id")
        ended = bool(((part.get("time") or {}) if part else {}).get("end"))
        if pid is not None and pid == current and texts:
            texts[-1] = (pid, body, ended)
        else:
            texts.append((pid, body, ended))
            current = pid
        post("text", (body or "")[:3500])
    elif pt == "reasoning" or t == "reasoning":
        rt = (part.get("text") if part else ev.get("text")) or ""
        if rt.strip():
            post("thinking", first_line(rt, 240))
    elif pt in ("tool", "tool_use") or t in ("tool", "tool_use", "tool_call"):
        tname = (part.get("tool") if part else None) or ev.get("tool") or ev.get("name") or "tool"
        post("tool", "using: " + str(tname))
    elif t in ("error", "session.error") or pt == "error":
        # An error event is still terminal-ish; surface it and let EOF end us.
        emsg = ""
        if isinstance(ev.get("error"), dict):
            emsg = ev["error"].get("message") or ev["error"].get("name") or ""
        elif isinstance(ev.get("error"), str):
            emsg = ev["error"]
        post("status", "error")
        if emsg.strip() and not [x for x in texts if (x[1] or "").strip()]:
            texts.append((None, emsg, True))
    # step_start / step_finish / message.* etc. are activity-only: opencode has
    # already exited at session idle when we hit EOF, so there is no separate
    # terminal event to wait for — EOF is it. (step_finish can race out per
    # opencode#26855; we do not depend on it.)

# EOF == turn done (opencode exited at session.status=idle). Take the LAST
# non-empty text part WHOLE — stream order is authoritative, so the final part is
# the answer and earlier parts are per-step preambles. We do NOT gate on
# part.time.end: under opencode#26855 the closing event can race out and leave the
# real final part unmarked, so requiring `end` would wrongly fall back to an
# earlier (ended) preamble. `end` is informational only.
post("status", "done")
final = ""
for (pid, body, ended) in texts:
    if (body or "").strip():
        final = body
sys.stdout.write(final)
PYEOF

# Claude OAuth keep-alive: claude-code's access token expires ~8h and refresh is
# single-use (rotates on every use). The guest runs `claude -p` one-shot PER TURN
# (above), so claude-code only ever refreshes the token while a turn is RUNNING —
# an idle agent (e.g. overnight) never fires a turn, the token expires, and the
# user is logged out. This resident loop is the only thing always running in the
# guest, so it owns the refresh during idle: the guest stays the SOLE refresher of
# its lineage (the host daemon excludes firecracker claude precisely so there is
# no second party to race on the single-use token). Mirrors maturana-core's
# claude_refresh.rs (same endpoint/client_id/skew) so it can be tested against the
# real endpoint. Silent unless it actually rotates or hits an error.
cat > /tmp/maturana-claude-keepalive.py <<'PYEOF'
import json, os, sys, time, urllib.request, urllib.error

CLIENT_ID = "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
TOKEN_URL = "https://platform.claude.com/v1/oauth/token"
skew_s = int(os.environ.get("MATURANA_CLAUDE_REFRESH_SKEW_SECONDS") or "900")

home = os.environ.get("HOME") or "/home/ubuntu"
path = os.path.join(home, ".claude", ".credentials.json")
try:
    with open(path) as f:
        data = json.load(f)
except Exception:
    sys.exit(0)  # no creds yet — nothing to keep alive
oauth = data.get("claudeAiOauth")
if not isinstance(oauth, dict):
    sys.exit(0)
refresh_token = oauth.get("refreshToken")
expires_at = oauth.get("expiresAt")
if not refresh_token or not isinstance(expires_at, (int, float)):
    sys.exit(0)
now_ms = int(time.time() * 1000)
if expires_at - now_ms > skew_s * 1000:
    sys.exit(0)  # not near expiry — stay silent

body = json.dumps({
    "grant_type": "refresh_token",
    "refresh_token": refresh_token,
    "client_id": CLIENT_ID,
}).encode()
req = urllib.request.Request(TOKEN_URL, data=body, method="POST")
req.add_header("content-type", "application/json")
req.add_header("accept", "application/json")
req.add_header("anthropic-beta", "oauth-2025-04-20")
# An explicit User-Agent is REQUIRED: the default "Python-urllib/x.y" signature
# is blocked by Cloudflare (error 1010) before it reaches the OAuth handler.
req.add_header("user-agent", "maturana-claude-keepalive/1.0")
try:
    with urllib.request.urlopen(req, timeout=30) as resp:
        raw = resp.read().decode()
except urllib.error.HTTPError as e:
    detail = ""
    try:
        detail = e.read().decode()[:200]
    except Exception:
        pass
    print("claude-keepalive: refresh failed HTTP %s: %s" % (e.code, detail), flush=True)
    sys.exit(1)
except Exception as e:
    print("claude-keepalive: refresh transport error: %s" % e, flush=True)
    sys.exit(1)

try:
    parsed = json.loads(raw)
except Exception as e:
    print("claude-keepalive: response not JSON: %s" % e, flush=True)
    sys.exit(1)
access_token = parsed.get("access_token")
if not access_token:
    print("claude-keepalive: response missing access_token", flush=True)
    sys.exit(1)
# Some responses omit a rotated refresh_token; then the old one stays valid.
new_refresh = parsed.get("refresh_token") or refresh_token
expires_in = parsed.get("expires_in") or 8 * 3600
oauth["accessToken"] = access_token
oauth["refreshToken"] = new_refresh
oauth["expiresAt"] = now_ms + int(expires_in) * 1000
data["claudeAiOauth"] = oauth
# Atomic + 0600 so a crash can't leave a partial or world-readable token.
tmp = path + ".tmp"
with open(tmp, "w") as f:
    json.dump(data, f, indent=2)
os.chmod(tmp, 0o600)
os.replace(tmp, path)
print("claude-keepalive: refreshed; expires in %d min"
      % int((oauth["expiresAt"] - now_ms) / 60000), flush=True)
PYEOF

# Throttle the keep-alive: the script only POSTs when within skew of expiry, but
# we still re-check periodically (cheap) rather than every ~1s idle iteration.
claude_keepalive_last=0
claude_keepalive_interval="${MATURANA_CLAUDE_REFRESH_INTERVAL_SECONDS:-60}"

while true; do
  date -Is > /var/log/maturana/heartbeat
  heartbeat idle
  if [ "${MATURANA_HARNESS}" = "claude-code" ]; then
    keepalive_now="$(date +%s)"
    if [ "$(( keepalive_now - claude_keepalive_last ))" -ge "$claude_keepalive_interval" ]; then
      claude_keepalive_last="$keepalive_now"
      # Inherit the runner's stdout/stderr (systemd append:agent.log, opened by
      # root) rather than self-opening the log: agent.log is root-owned, so a
      # ">> /var/log/maturana/agent.log" as the ubuntu service user fails EACCES
      # and `|| true` would silently skip the whole keep-alive.
      python3 /tmp/maturana-claude-keepalive.py 2>&1 || true
    fi
  fi
  claim_body="$(python3 - <<'PY'
import json, os
print(json.dumps({"agent_id": os.environ["MATURANA_AGENT_ID"], "session_id": os.environ["MATURANA_SESSION_ID"], "limit": 1}))
PY
)"
  claim="$(curl -fsS -X POST "$sessiond_url/session/claim" "${headers[@]}" --data "$claim_body" 2>>/var/log/maturana/worker.err.log || true)"
  count="$(printf '%s' "$claim" | python3 -c 'import json,sys; print(len(json.loads(sys.stdin.read() or "{\"messages\":[]}").get("messages", [])))' 2>/dev/null || echo 0)"
  if [ "$count" = "0" ]; then
    sleep 1
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
  # Expose the in-flight message id so `maturana-forge` can stream forge progress
  # onto this turn's lane (the channel animates it).
  printf '%s' "$msg_id" > /tmp/maturana-current-msg 2>/dev/null || true
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

  # Per-turn model override from the `/model` channel command: the host attaches
  # the agent's current model to the inbound message. Empty => harness default.
  model="$(python3 - <<'PY'
import json
try:
    d=json.load(open("/tmp/maturana-session-claim.json"))
    c=json.loads(d["messages"][0]["content"])
    print((c.get("model") or "").strip())
except Exception:
    print("")
PY
)"
  model_args=()
  [ -n "$model" ] && model_args=(--model "$model")

  # Reasoning effort (codex/gpt-5). Default to `low` — it keeps turns light
  # (0 reasoning tokens on simple turns) while staying compatible with the
  # web_search/image_gen tools (`minimal` is rejected by the API when those tools
  # are enabled). The `/reasoning` channel command overrides this per agent.
  reasoning="$(python3 - <<'PY'
import json
try:
    d=json.load(open("/tmp/maturana-session-claim.json"))
    c=json.loads(d["messages"][0]["content"])
    print((c.get("reasoning") or "").strip())
except Exception:
    print("")
PY
)"
  [ -z "$reasoning" ] && reasoning="low"

  response=""
  harness_timeout="${MATURANA_HARNESS_TIMEOUT_SECONDS:-240}"
  run_harness() {
    timeout --kill-after=10s "${harness_timeout}s" "$@"
  }
  # --- /stop (in-progress cancel): a background watcher polls sessiond for a
  # cancel request for THIS turn and kills the running harness so the turn ends
  # promptly. The post-harness block then replaces the partial with "Stopped." ---
  rm -f /tmp/maturana-cancelled
  : > /tmp/maturana-turn-active
  case "${MATURANA_HARNESS}" in
    codex) cancel_pat="codex exec" ;;
    claude-code) cancel_pat="claude -p" ;;
    opencode) cancel_pat="opencode run" ;;
    *) cancel_pat="" ;;
  esac
  cancel_body="{\"agent_id\":\"$MATURANA_AGENT_ID\",\"session_id\":\"$MATURANA_SESSION_ID\",\"message_id\":\"$msg_id\"}"
  (
    while [ -f /tmp/maturana-turn-active ]; do
      sleep 2
      [ -f /tmp/maturana-turn-active ] || break
      cs="$(curl -fsS -X POST "$sessiond_url/session/cancel-status" "${headers[@]}" --data "$cancel_body" 2>/dev/null || true)"
      case "$cs" in
        *'"cancelled":true'*)
          touch /tmp/maturana-cancelled
          if [ -n "$cancel_pat" ]; then pkill -TERM -f "$cancel_pat" 2>/dev/null || true; fi
          pkill -TERM -f 'maturana-stream' 2>/dev/null || true
          sleep 2
          if [ -n "$cancel_pat" ]; then pkill -KILL -f "$cancel_pat" 2>/dev/null || true; fi
          break
          ;;
      esac
    done
  ) &
  cancel_watcher_pid=$!
  if [ "${MATURANA_HARNESS}" = "codex" ]; then
    # Stream `codex exec --json`: the streamer POSTs live progress to sessiond and
    # prints the final agent text. `set -o pipefail` makes a codex failure fail
    # the pipeline so the else branch's fallback applies.
    : > /tmp/maturana-session-response.txt
    if run_harness codex exec --json -c model_reasoning_effort="$reasoning" "${model_args[@]}" --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -C /workspace "$(cat /tmp/maturana-session-prompt.txt)" </dev/null 2>>/var/log/maturana/worker.err.log \
        | SESSIOND_URL="$sessiond_url" MSG_ID="$msg_id" python3 /tmp/maturana-stream-progress.py > /tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log; then
      response="$(cat /tmp/maturana-session-response.txt)"
    else
      response="$(cat /tmp/maturana-session-response.txt)"
      if [ -z "$response" ]; then
        response="I hit an error while processing that message."
      fi
    fi
  elif [ "${MATURANA_HARNESS}" = "claude-code" ]; then
    # Real token streaming: claude emits text deltas in stream-json, which the
    # claude streamer POSTs to the progress lane as they arrive.
    : > /tmp/maturana-session-response.txt
    if run_harness claude -p "${model_args[@]}" --permission-mode bypassPermissions --output-format stream-json --include-partial-messages --verbose "$(cat /tmp/maturana-session-prompt.txt)" </dev/null 2>>/var/log/maturana/worker.err.log \
        | SESSIOND_URL="$sessiond_url" MSG_ID="$msg_id" python3 /tmp/maturana-stream-claude.py > /tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log; then
      response="$(cat /tmp/maturana-session-response.txt)"
    else
      response="$(cat /tmp/maturana-session-response.txt)"
      if [ -z "$response" ]; then
        response="I hit an error while processing that message."
      fi
    fi
  elif [ "${MATURANA_HARNESS}" = "opencode" ]; then
    # NanoClaw model on the opencode CLI: run STANDALONE `opencode run --format
    # json --continue`, which blocks until the turn reaches `session.status=idle`
    # and then exits — process exit IS NanoClaw's terminal `result` event. We do
    # NOT use `--attach` to a warm server: the attached run returns BEFORE idle
    # with empty stdout, forcing a fragile opencode.db/SSE scrape that read the
    # mid-stream PREAMBLE ("I'll search…") instead of the final answer. Standalone
    # pays a per-turn Node boot (~5s) but reliably streams the whole turn's events
    # to stdout, including the final assistant text, so we read it the NanoClaw
    # way: collapse the JSONL event stream, take the LAST complete `text` part,
    # never accumulate deltas, never settle on an earlier per-step preamble.
    #
    # Continuity: opencode is SQLite-backed, so `--continue` resumes the last
    # session across separate `run` invocations. We capture the opencode sessionID
    # from the first turn's stream (the opaque continuation token, NanoClaw's
    # init.continuation) and persist it, then resume with `--session <id>` so a
    # parallel/other session can never hand us the wrong thread. `--continue`
    # bootstraps turn one (no id yet) and is a safe fallback if the id is lost.
    oc_session_file="$HOME/.maturana-opencode-session"
    oc_model_args=()
    if [ -n "$model" ]; then
      # OpenCode routes OpenRouter models as `openrouter/<vendor>/<model>`. The
      # /model picker stores the raw catalog id (e.g. google/gemini-2.5-pro), so
      # prefix `openrouter/` unless the user already gave a provider-qualified id.
      case "$model" in
        openrouter/*) oc_model_args=(-m "$model") ;;
        *) oc_model_args=(-m "openrouter/$model") ;;
      esac
    elif [ -n "${OPENROUTER_API_KEY:-}" ]; then
      oc_model_args=(-m openrouter/anthropic/claude-sonnet-4.5)
    fi
    oc_prompt="$(cat /tmp/maturana-session-prompt.txt)"
    # One turn = one standalone `opencode run --format json` piped through the
    # NanoClaw collapser. $1 is the resume flag-pair ("--session <id>" or
    # "--continue"). The collapser exits 0 even on harness error, so pipefail
    # surfaces an opencode non-zero exit.
    run_opencode_turn() {
      : > /tmp/maturana-session-response.txt
      set -o pipefail
      local rc=0
      run_harness opencode run --format json "$@" "${oc_model_args[@]}" "$oc_prompt" </dev/null \
          2>>/var/log/maturana/worker.err.log \
        | SESSIOND_URL="$sessiond_url" MSG_ID="$msg_id" \
          MATURANA_OC_SESSION_FILE="$oc_session_file" \
          python3 /tmp/maturana-stream-opencode.py > /tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log \
        || rc=$?
      set +o pipefail
      return $rc
    }
    if [ -s "$oc_session_file" ]; then
      run_opencode_turn --session "$(cat "$oc_session_file")" || true
      response="$(cat /tmp/maturana-session-response.txt)"
      # Self-heal a stale/invalid session id (opencode returns "Resource not
      # found"): if the resume produced no text, drop the token and retry once
      # with `--continue`, which the collapser will repopulate with a fresh id.
      if [ -z "$response" ]; then
        rm -f "$oc_session_file"
        run_opencode_turn --continue || true
        response="$(cat /tmp/maturana-session-response.txt)"
      fi
    else
      run_opencode_turn --continue || true
      response="$(cat /tmp/maturana-session-response.txt)"
    fi
    # Last-ditch fallback ONLY when the stream carried no text at all (e.g. the
    # #26855 race exited before flushing, or a crash mid-stream). Read the last
    # assistant message's last text part straight from opencode.db — the same
    # "terminal message, terminal text part" selection, just from disk.
    if [ -z "$response" ] && [ -f "$HOME/.local/share/opencode/opencode.db" ]; then
      response="$(python3 - <<'PY'
import os, sqlite3
db = os.path.expanduser("~/.local/share/opencode/opencode.db")
try:
    con = sqlite3.connect(db)
    m = con.execute(
        "select id from message "
        "where json_extract(data, '$.role') = 'assistant' "
        "order by json_extract(data, '$.time.created') desc, rowid desc limit 1"
    ).fetchone()
    if m:
        rows = con.execute(
            "select json_extract(data, '$.text') t from part "
            "where message_id = ? and json_extract(data, '$.type') = 'text' "
            "order by time_updated asc", (m[0],)
        ).fetchall()
        texts = [r[0] for r in rows if r[0] and r[0].strip()]
        if texts:
            print(texts[-1])
    con.close()
except Exception:
    pass
PY
)"
    fi
    if [ -z "$response" ]; then
      response="I hit an error while processing that message."
    fi
  else
    response="Unsupported harness: ${MATURANA_HARNESS}"
  fi
  # Turn finished (or was killed by /stop): stop the cancel watcher. If the user
  # cancelled this turn, replace whatever partial/error the killed harness left
  # with a clean acknowledgement.
  rm -f /tmp/maturana-turn-active
  kill "$cancel_watcher_pid" 2>/dev/null || true
  wait "$cancel_watcher_pid" 2>/dev/null || true
  if [ -f /tmp/maturana-cancelled ]; then
    response="🛑 Stopped."
    rm -f /tmp/maturana-cancelled
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
"#;

const GUEST_BOOTSTRAP: &str = r#"#!/usr/bin/env bash
set -euo pipefail
sudo mkdir -p /agent /workspace /memory /wiki /opt/maturana/bin /var/log/maturana
sudo chown -R "${USER}:${USER}" /agent /workspace /memory /wiki /var/log/maturana
"#;

const FIRECRACKER_BOOTSTRAP: &str = r#"#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends openssh-server curl ca-certificates nodejs npm
id ubuntu >/dev/null 2>&1 || useradd -m -s /bin/bash ubuntu
mkdir -p /etc/sudoers.d /etc/netplan /etc/cloud/cloud.cfg.d /agent /workspace /memory /wiki /opt/maturana/bin /var/log/maturana
sed -i.bak -e "\|[[:space:]]/boot[[:space:]]|d" -e "\|[[:space:]]/boot/efi[[:space:]]|d" -e "/LABEL=BOOT/d" -e "/LABEL=UEFI/d" /etc/fstab
printf "ubuntu ALL=(ALL) NOPASSWD: ALL\n" > /etc/sudoers.d/90-maturana-ubuntu
chmod 0440 /etc/sudoers.d/90-maturana-ubuntu
ssh-keygen -A
systemctl disable ssh.socket || true
systemctl enable ssh.service || systemctl enable ssh || true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_env_quotes_values() {
        let env = render_session_env(&GuestWorkerConfig {
            graph_token: None,
            graph_name: None,
            agent_id: "demo".to_string(),
            session_id: "telegram-main".to_string(),
            sessiond_url: "__MATURANA_DEFAULT_SESSIOND_URL__".to_string(),
            sessiond_token: "token'with-quote".to_string(),
            harness: HarnessRuntime::Codex,
            harness_auth_guest_path: "/home/ubuntu/.codex".to_string(),
            headless_chrome: true,
        });

        assert!(env.contains("MATURANA_AGENT_ID='demo'"));
        assert!(env.contains("MATURANA_HARNESS='codex'"));
        assert!(env.contains("MATURANA_SESSIOND_TOKEN='token'\"'\"'with-quote'"));
        assert!(env.contains("MATURANA_HEADLESS_CHROME='1'"));
        assert!(env.contains("PLAYWRIGHT_BROWSERS_PATH='/opt/maturana/browsers'"));
    }

    #[test]
    fn runner_contains_all_supported_harnesses() {
        let runner = render_run_agent();
        assert!(runner.contains("codex exec"));
        assert!(runner.contains("claude -p"));
        // Claude must run with tool autonomy in the VM sandbox, else MCP/tools
        // are permission-gated and silently denied in headless `-p` mode.
        assert!(runner.contains("--permission-mode bypassPermissions"));
        // opencode runs STANDALONE blocking `run --format json` (NanoClaw model:
        // process exit at session idle = terminal event), NOT `--attach` to a
        // warm server (which returns the mid-stream preamble). Continuity comes
        // from the persisted opencode session id (`--session`/`--continue`).
        assert!(runner.contains("opencode run --format json"));
        assert!(!runner.contains("run --attach"));
        assert!(runner.contains("maturana-stream-opencode.py"));
        assert!(runner.contains(".maturana-opencode-session"));
        assert!(runner.contains("/session/heartbeat"));
        assert!(runner.contains("/session/outbound"));
        assert!(runner.contains("/agent/proxy.env"));
        assert!(runner.contains("MATURANA_PROXY_PORT"));
    }

    #[test]
    fn runner_installs_self_forge_helper() {
        let runner = render_run_agent();
        // The guest gets the on-PATH forge helper and exposes the in-flight
        // message id so forge progress can animate on the channel.
        assert!(runner.contains("maturana-forge"));
        assert!(runner.contains("/session/forge"));
        assert!(runner.contains("/tmp/maturana-current-msg"));
        assert!(runner.contains(".local/bin"));
    }

    #[test]
    fn runner_keeps_claude_token_alive_while_idle() {
        let runner = render_run_agent();
        // The keep-alive script ships and the loop calls it (throttled) ONLY for
        // claude-code — the gap it closes is an idle guest never firing a turn.
        assert!(runner.contains("/tmp/maturana-claude-keepalive.py"));
        assert!(runner.contains(r#"if [ "${MATURANA_HARNESS}" = "claude-code" ]; then"#));
        assert!(runner.contains("claude_keepalive_interval"));
        // It mirrors claude_refresh.rs against the real OAuth endpoint, so the
        // values must stay in lock-step with the shared module constants.
        assert!(runner.contains(crate::claude_refresh::CLIENT_ID));
        assert!(runner.contains(crate::claude_refresh::TOKEN_URL));
        assert!(runner.contains("grant_type"));
        assert!(runner.contains("refresh_token"));
        // Default skew matches the host daemon's REFRESH_SKEW (15 min).
        assert_eq!(crate::claude_refresh::REFRESH_SKEW.as_secs(), 900);
        assert!(runner.contains("MATURANA_CLAUDE_REFRESH_SKEW_SECONDS") || runner.contains("\"900\""));
        // Writes back atomically + 0600 (never a partial/world-readable token).
        assert!(runner.contains("os.replace(tmp, path)"));
        assert!(runner.contains("0o600"));
    }

    #[test]
    fn harness_install_maps_supported_harnesses() {
        let codex = render_harness_install(&HarnessRuntime::Codex, false);
        assert!(codex.contains("npm install -g @openai/codex"));
        assert!(codex.contains("python3"));
        assert!(!codex.contains("playwright install"));

        let claude = render_harness_install(&HarnessRuntime::ClaudeCode, true);
        assert!(claude.contains("npm install -g @anthropic-ai/claude-code"));
        assert!(claude.contains("npm install -g playwright"));
        assert!(claude.contains("playwright install --with-deps chromium"));
        assert!(claude.contains("/opt/maturana/bin/browser-smoke.js"));
        // The browse driver ships with the browser, and only with it.
        assert!(claude.contains("/opt/maturana/bin/browse.js"));
        assert!(claude.contains(r#""cmd":"screenshot""#));
        assert!(!codex.contains("browse.js"));

        let opencode = render_harness_install(&HarnessRuntime::Opencode, false);
        assert!(opencode.contains("npm install -g opencode-ai"));
        assert!(opencode.starts_with("#!/usr/bin/env bash\nset -euo pipefail"));
    }

    #[test]
    fn harness_install_is_idempotent_and_wired_to_a_boot_service() {
        // The install script no-ops if the harness is already present, so it can
        // run as a first-boot one-shot that is harmless on later boots.
        for (harness, bin) in [
            (HarnessRuntime::Codex, "codex"),
            (HarnessRuntime::ClaudeCode, "claude"),
            (HarnessRuntime::Opencode, "opencode"),
        ] {
            let script = render_harness_install(&harness, false);
            assert!(script.contains(&format!("command -v {bin} >/dev/null")));
            assert!(script.contains("already installed"));
        }

        let unit = render_harness_install_service();
        assert!(unit.contains("Type=oneshot"));
        assert!(unit.contains("Before=maturana-agent.service"));
        assert!(unit.contains("ExecStart=/opt/maturana/bin/install-harness.sh"));
        assert!(unit.contains("ConditionPathExists=/opt/maturana/bin/install-harness.sh"));

        // The agent waits for the install one-shot before its first turn.
        let service = render_systemd_service("x", "ubuntu");
        assert!(service.contains("After=network-online.target maturana-harness-install.service"));
    }

    #[test]
    fn guest_bootstrap_is_policy_light_and_reusable() {
        let bootstrap = render_guest_bootstrap();
        assert!(bootstrap.contains("/agent /workspace /memory /wiki"));
        assert!(bootstrap.contains("/opt/maturana/bin"));
        assert!(!bootstrap.contains("apt-get"));
        assert!(!bootstrap.contains("npm install"));
    }

    #[test]
    fn firecracker_guest_network_and_bootstrap_are_core_rendered() {
        let bootstrap = render_firecracker_bootstrap();
        assert!(bootstrap.contains("apt-get install -y --no-install-recommends"));
        assert!(bootstrap.contains("openssh-server curl ca-certificates nodejs npm"));
        assert!(bootstrap.contains("/etc/sudoers.d/90-maturana-ubuntu"));
        assert!(bootstrap.contains("systemctl enable ssh.service"));

        let netplan = render_firecracker_netplan("AA:FC:00:00:10:02", "172.30.10.6", "172.30.10.5");
        assert!(netplan.contains("macaddress: \"AA:FC:00:00:10:02\""));
        assert!(netplan.contains("- 172.30.10.6/30"));
        assert!(netplan.contains("via: 172.30.10.5"));
        assert_eq!(
            render_firecracker_cloud_cfg(),
            "network: {config: disabled}\n"
        );
    }

    #[test]
    fn firecracker_proxy_env_is_core_rendered_from_explicit_policy() {
        assert!(render_firecracker_proxy_env(false, None, "172.30.0.1")
            .unwrap()
            .is_none());

        let proxy_env = render_firecracker_proxy_env(true, Some("0.0.0.0:47833"), "172.30.0.1")
            .unwrap()
            .unwrap();
        assert!(proxy_env.contains("MATURANA_USE_HOST_PROXY=1"));
        assert!(proxy_env.contains("MATURANA_PROXY_HOST=172.30.0.1"));
        assert!(proxy_env.contains("MATURANA_PROXY_PORT=47833"));
        assert!(proxy_env.contains("MATURANA_PROXY_HTTPS=1"));
        assert!(proxy_env.contains("NO_PROXY=localhost,127.0.0.1,::1"));

        let error = render_firecracker_proxy_env(true, Some("0.0.0.0"), "172.30.0.1")
            .unwrap_err()
            .to_string();
        assert!(error.contains("proxy bind must include a port"));
    }

    #[test]
    fn systemd_service_uses_fixed_worker_entrypoint() {
        let service = render_systemd_service("Maturana demo\nignored", "ubuntu");
        assert!(service.contains("Description=Maturana demoignored"));
        assert!(service.contains("User=ubuntu"));
        assert!(service.contains("ExecStart=/opt/maturana/bin/run-agent.sh"));
        assert!(service.contains("Restart=on-failure"));
    }
}
