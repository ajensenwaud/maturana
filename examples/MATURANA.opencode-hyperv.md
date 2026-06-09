---
identity:
  id: opencode-demo
  name: OpenCode Hyper-V Demo Agent
  purpose: A real Hyper-V Ubuntu worker running the OpenCode harness with directly injected provider state.
runtime:
  harness: opencode
vm:
  provider: hyper-v
  guest_os: linux
  vcpu: 2
  memory_mib: 4096
  switch_name: Default Switch
  boot_image: .maturana/agents/opencode-demo/state/maturana-opencode-demo-os.vhdx
harness_auth:
  - runtime: opencode
    source_path: .maturana/host-auth/opencode
    guest_path: /home/ubuntu
agent_run:
  install_harness: true
  start_on_boot: false
filesystem:
  mounts:
    - host_path: .maturana/agents/opencode-demo/workspace
      guest_path: /workspace
      writable: true
network:
  egress_allowlist:
    - api.anthropic.com
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
  agent_memory_path: .maturana/agents/opencode-demo/memory
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

# OpenCode Hyper-V Demo Agent

This spec targets a real Windows Hyper-V Ubuntu VM running OpenCode.
