---
identity:
  id: codex-demo
  name: Codex Hyper-V Demo Agent
  purpose: A real Hyper-V Ubuntu worker running the Codex harness with directly injected OAuth state.
runtime:
  harness: codex
vm:
  provider: hyper-v
  guest_os: linux
  vcpu: 2
  memory_mib: 2048
  switch_name: Default Switch
  boot_image: .maturana/agents/codex-demo/state/maturana-codex-demo-os.vhdx
harness_auth:
  - runtime: codex
    source_path: .maturana/host-auth/codex
    guest_path: /home/ubuntu/.codex
agent_run:
  install_harness: true
  start_on_boot: false
  prompt: Inspect /agent/MATURANA.md and report that the Codex guest harness is ready.
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
  - name: telegram-bot-token
    source: env:MATURANA_TELEGRAM_BOT_TOKEN
memory:
  wiki_path: .maturana/wiki
  agent_memory_path: .maturana/agents/codex-demo/memory
browser:
  headless_chrome: true
channels:
  tui: true
  telegram:
    token_source: env:MATURANA_TELEGRAM_BOT_TOKEN
    chat_id_source: env:MATURANA_TELEGRAM_CHAT_ID
snapshots:
  on_launch: true
  retain: 5
---

# Codex Hyper-V Demo Agent

This spec targets a real Windows Hyper-V Ubuntu VM running Codex.
