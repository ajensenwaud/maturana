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
  harness_timeout="${MATURANA_HARNESS_TIMEOUT_SECONDS:-240}"
  run_harness() {
    timeout --kill-after=10s "${harness_timeout}s" "$@"
  }
  if [ "${MATURANA_HARNESS}" = "codex" ]; then
    if run_harness codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -C /workspace -o /tmp/maturana-session-response.txt "$(cat /tmp/maturana-session-prompt.txt)" >>/var/log/maturana/worker.out.log 2>>/var/log/maturana/worker.err.log; then
      response="$(cat /tmp/maturana-session-response.txt)"
    else
      response="I hit an error while processing that message."
    fi
  elif [ "${MATURANA_HARNESS}" = "claude-code" ]; then
    if run_harness claude -p "$(cat /tmp/maturana-session-prompt.txt)" >/tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log; then
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
    if run_harness opencode "${opencode_args[@]}" >/tmp/maturana-session-response.txt 2>>/var/log/maturana/worker.err.log; then
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
        assert!(runner.contains("opencode"));
        assert!(runner.contains("/session/heartbeat"));
        assert!(runner.contains("/session/outbound"));
        assert!(runner.contains("/agent/proxy.env"));
        assert!(runner.contains("MATURANA_PROXY_PORT"));
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
