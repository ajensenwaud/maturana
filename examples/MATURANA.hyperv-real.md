---
identity:
  id: codex-demo
  name: Codex Demo Agent
  purpose: A real Hyper-V Ubuntu worker used to prove Maturana MVP boot, network, SSH, and heartbeat flows.
runtime:
  harness: codex
vm:
  provider: hyper-v
  guest_os: linux
  vcpu: 2
  memory_mib: 2048
  switch_name: Default Switch
  boot_image: .maturana/agents/codex-demo/state/maturana-codex-demo-os.vhdx
filesystem:
  mounts:
    - host_path: .maturana/agents/codex-demo/workspace
      guest_path: /workspace
      writable: true
network:
  egress_allowlist:
    - api.openai.com
    - github.com
    - api.telegram.org
  proxy:
    enabled: true
    bind: 0.0.0.0:47833
    inject_headers: []
credentials:
  - name: codex-oauth
    source: env:CODEX_OAUTH_JSON
  - name: claude-code-oauth
    source: env:CLAUDE_CODE_OAUTH_JSON
memory:
  wiki_path: .maturana/wiki
  agent_memory_path: .maturana/agents/codex-demo/memory
browser:
  headless_chrome: true
skills:
  - maturana-agent-inspect
  - maturana-snapshot
tools:
  - git
  - rg
channels:
  tui: true
  telegram:
    token_source: env:MATURANA_TELEGRAM_BOT_TOKEN
    chat_id_source: env:MATURANA_TELEGRAM_CHAT_ID
snapshots:
  on_launch: true
  retain: 5
---

# Codex Demo Agent

This spec targets a real Windows Hyper-V Ubuntu VM created by
`scripts/launch-ubuntu-cloudimg-hyperv.ps1`.
