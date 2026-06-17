# The `MATURANA.md` spec

`MATURANA.md` is the durable, human-readable contract for one agent. Codex usually writes it,
but you can read and edit it. It is a Markdown file whose **YAML front matter** (between the
opening and closing `---`) is the spec; the Markdown body below is free-form notes.

Validate before you launch:

```sh
maturana spec validate examples/MATURANA.codex-firecracker.md
```

The parser uses **`deny_unknown_fields`** — a misspelled or misplaced key is an error, not a
silent no-op. Enum values are kebab-case (`codex`, `claude-code`, `opencode`; `firecracker`,
`hyper-v`; `linux`, `windows`). The authoritative definition is
[`crates/maturana-core/src/spec.rs`](../crates/maturana-core/src/spec.rs).

---

## A minimal spec

Only `identity`, `runtime`, and `vm` are required; everything else has a default.

```yaml
---
identity:
  id: codex-firecracker
  name: Codex Firecracker Agent
  purpose: Linux Firecracker Codex harness.
runtime:
  harness: codex
vm:
  provider: firecracker
---
# Codex Firecracker Agent
```

## A worked example

```yaml
---
identity:
  id: codex-firecracker
  name: Codex Firecracker Agent
  purpose: Linux Firecracker Codex harness.
runtime:
  harness: codex                     # codex | claude-code | opencode
vm:
  provider: firecracker              # firecracker | hyper-v
  guest_os: linux
  vcpu: 2
  memory_mib: 2048
  firecracker:
    kernel_image: .maturana/images/firecracker/codex/vmlinux.bin
    rootfs_image: .maturana/images/firecracker/codex/ubuntu-rootfs.ext4
    tap_name: tap-mat-codex
    host_ip: 172.30.10.1
    guest_ip: 172.30.10.2
    guest_mac: AA:FC:00:00:10:01
    kernel_args: console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw virtio_mmio.device=4K@0xd0000000:5
harness_auth:
  - runtime: codex
    source_path: .maturana/host-auth/codex      # host-side OAuth, git-ignored
    guest_path: /home/ubuntu/.codex             # injected into the VM at launch
filesystem:
  mounts:
    - host_path: .maturana/agents/codex-firecracker/workspace
      guest_path: /workspace
      writable: true
network:
  egress_allowlist:
    - api.openai.com
    - github.com
memory:
  wiki_path: .maturana/wiki
  agent_memory_path: .maturana/agents/codex-firecracker/memory
channels:
  tui: true
snapshots:
  on_launch: false
  retain: 3
---

# Codex Firecracker Agent
Free-form notes for humans live here, below the front matter.
```

---

## Field reference

Top-level blocks (only `identity`, `runtime`, `vm` are required):

| Block | Required | Purpose |
| --- | --- | --- |
| `identity` | yes | Stable id, display name, one-line purpose. |
| `runtime` | yes | Which harness runs inside the VM. |
| `vm` | yes | Hypervisor provider + per-provider VM settings. |
| `harness_auth` | — | Where the host-side OAuth lives and where to inject it in the guest. |
| `filesystem` | — | Governed host↔guest mounts. |
| `network` | — | Egress allowlist + optional pipelock proxy policy. |
| `credentials` | — | Non-OAuth secrets resolved from `pipelock:` / `env:` / file. |
| `agent_run` | — | Whether to install the harness and start the worker on boot. |
| `memory` | — | LLM-wiki + per-agent memory paths. |
| `knowledge_graph` | — | MaturanaGraph (graph + GraphRAG). **On by default.** |
| `browser` | — | Headless Chrome in the guest. |
| `mcp_servers` | — | MCP servers the guest harness connects to. |
| `capabilities` | — | Opt-in gates: image-gen, voice, self-forge. |
| `skills` / `tools` | — | Skills/tools to deploy into the guest. |
| `schedules` | — | Named cron schedules. |
| `channels` | — | TUI / Telegram / Discord / Slack / AgentMail. |
| `snapshots` | — | Snapshot-on-launch + retention. |

### `identity` (required)

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | Stable agent id; names the `.maturana/agents/<id>/` directory. |
| `name` | string | Human-friendly display name. |
| `purpose` | string | One line on what the agent is for. |

### `runtime` (required)

| Field | Type | Notes |
| --- | --- | --- |
| `harness` | enum | `codex`, `claude-code`, or `opencode`. |

### `vm` (required)

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `provider` | enum | — | `firecracker` (Linux) or `hyper-v` (Windows). |
| `guest_os` | enum | `linux` | `linux` or `windows`. |
| `vcpu` | int | `2` | Guest vCPUs. |
| `memory_mib` | int | `2048` | Guest RAM (MiB). |
| `firecracker` | block | — | Required for the `firecracker` provider (see below). |
| `boot_image` | string | — | Hyper-V: bootable image path. |
| `switch_name` | string | — | Hyper-V: virtual switch. |
| `cloud_init` | block | — | Hyper-V: `{ username, ssh_public_key }`. |

`vm.firecracker`:

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `kernel_image` | string | — | Path to the `vmlinux` kernel. |
| `rootfs_image` | string | — | Path to the ext4 rootfs. |
| `tap_name` | string | `tap-maturana0` | Host TAP device (recreated per launch). |
| `host_ip` | string | `172.30.0.1` | Host side of the TAP. |
| `guest_ip` | string | `172.30.0.2` | Guest IP. |
| `guest_mac` | string | `AA:FC:00:00:00:01` | Guest MAC. |
| `kernel_args` | string | (serial console, `root=/dev/vda`, …) | Kernel command line. |

### `harness_auth`

A list. Each entry stages a host-side OAuth directory into the guest at launch — never baked
into the image, never a `pipelock` secret.

| Field | Type | Notes |
| --- | --- | --- |
| `runtime` | enum | `codex` / `claude-code` / `opencode`. |
| `source_path` | string | Host path (git-ignored, e.g. `.maturana/host-auth/codex`). |
| `guest_path` | string | Where to inject it (e.g. `/home/ubuntu/.codex`). |

### `filesystem.mounts`

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `host_path` | string | — | Host directory. |
| `guest_path` | string | — | Guest mount point. |
| `writable` | bool | `false` | Read-only unless set. |

Live file transfers are bounded to `/workspace`, `/memory`, `/wiki`, and declared mount roots.

### `network`

| Field | Type | Notes |
| --- | --- | --- |
| `egress_allowlist` | string[] | Hosts the guest may reach. Everything else is blocked. |
| `proxy` | block | Optional pipelock proxy policy (header injection). |

`network.proxy`: `enabled` (default `true` when the block is present), `bind`
(default `0.0.0.0:47833`), and `inject_headers[]` of `{ host, header, source, prefix? }` where
`source` is a `pipelock:`/`env:`/file reference (see [the pipelock skill](../skills/maturana-pipelock/SKILL.md)).

### `credentials`

Non-OAuth secrets. Each is `{ name, source }`, `source` being `pipelock:<path>`, `env:<VAR>`,
or a file path. Never put raw secret values in the spec.

### `agent_run`

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `install_harness` | bool | `true` | Install the selected harness in the guest. |
| `start_on_boot` | bool | `false` | Start the worker service on guest boot. |

### `memory`

| Field | Type | Notes |
| --- | --- | --- |
| `wiki_path` | string | Shared LLM-wiki store (default `.maturana/wiki`). |
| `agent_memory_path` | string | Per-agent durable memory directory. |

### `knowledge_graph` (on by default)

MaturanaGraph: a property graph + GraphRAG retrieval layer for durable, queryable memory.

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Opt out with `enabled: false`. |
| `graph` | string | agent id | Name the graph to share it across agents; omit for a private graph. |

### `browser`

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `headless_chrome` | bool | `false` | Provision Playwright Chromium in the guest for the `maturana-browse` skill. |

### `mcp_servers`

A list of MCP servers rendered into the harness's native config at install time.

| Field | Type | Notes |
| --- | --- | --- |
| `name` | string | Server name. |
| `transport` | enum | `stdio` (default) or `http`. |
| `command` / `args` | string / string[] | For `stdio` servers (e.g. `npx`). |
| `url` | string | For `http`/`sse` servers. |
| `env` | list | `{ name, source }`; secrets resolved host-side. |
| `egress_hosts` | string[] | Folded into the egress allowlist automatically. |

### `capabilities`

Opt-in gates; each enables an egress default + the relevant skill. All default `false`.

| Field | Notes |
| --- | --- |
| `image_gen` | Image generation (`maturana-image-gen`). |
| `voice` | Speech-to-text / text-to-speech (`maturana-voice`). |
| `self_forge` | Agent may build + run its own WASM capabilities (`maturana-self-forge`). |

### `skills` / `tools`

String lists naming skills/tools to deploy into the guest (see
[deploy](../skills/maturana-skill-deploy/SKILL.md)).

### `schedules`

A list of `{ name, cron }`. The prompt and delivery channel for a schedule are set with the CLI
(`maturana schedule add <id> <name> --cron … --prompt … --channel …`); see
[the schedule skill](../skills/maturana-schedule/SKILL.md).

### `channels`

| Field | Type | Notes |
| --- | --- | --- |
| `tui` | bool | Records the console TUI (`maturana agent chat <id>`) as an intended surface. |
| `telegram` | block | `{ token_source, chat_id_source? }`. |
| `discord` | block | `{ bot_token_source }` (needs the MESSAGE CONTENT intent). |
| `slack` | block | `{ bot_token_source, app_token_source }` (Socket Mode). |
| `agentmail` | block | `{ api_key_source, inbox? }`. |

All channel secrets are `*_source` references resolved host-side, never raw tokens.

### `snapshots`

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `on_launch` | bool | `true` | Take a snapshot at launch. |
| `retain` | int | `5` | How many to keep. |

See [snapshot-operations.md](snapshot-operations.md) for the snapshot/restore lifecycle.

---

## Secret handling

The only secrets injected as files are the harness OAuth directories under `harness_auth`
(Codex / Claude Code expect local subscription auth state). Everything else — bot tokens, API
keys — is a `pipelock:` / `env:` / file **reference**, resolved host-side. Never commit raw
secrets to a spec. See [the pipelock skill](../skills/maturana-pipelock/SKILL.md).
