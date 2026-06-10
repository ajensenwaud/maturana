# WASM Tool Framework

Agents author, build, register, and run their own tools at runtime without
rebuilding the host — the Maturana analogue of OpenClaw's on-the-fly tool
creation, with the same animated feedback in the Telegram interface.

A tool is one WebAssembly module plus a declarative manifest. The host runs it
in a sandbox with **no ambient authority**: only the capabilities the manifest
opts into are granted, resource use is bounded, and the module talks to the
world through a narrow stdin/stdout JSON contract.

## Why WASM

Maturana is zero-trust by default. An agent-authored tool is untrusted code, so
it runs where untrusted code belongs: a capability-gated sandbox with hard
limits. WebAssembly + WASI gives portable modules, deny-by-default
authority, and deterministic resource metering — without the weight of a second
VM per tool.

## Anatomy of a tool

```
<home>/tools/<name>/
  tool.json     # ToolManifest: name, version, capabilities, limits, schemas
  module.wasm   # the compiled WASI command module
```

Manifest ([`crate::tools`](../crates/maturana-core/src/tools.rs)):

```jsonc
{
  "name": "weather-fetch",
  "version": "0.1.0",
  "description": "Look up current weather for a city",
  "wasm": "module.wasm",
  "capabilities": { "fs_read": [], "fs_write": [], "env": [], "net": ["api.open-meteo.com"] },
  "limits": { "fuel": 2000000000, "memory_mb": 256, "timeout_ms": 30000 }
}
```

`Capabilities::default()` is the **empty set** — a fresh tool is pure compute
(stdin → stdout) until the author opts into filesystem, env, or network access.
Network is allow-listed at the egress proxy, since WASI preview1 has no sockets.

## Sandbox guarantees

The engine ([`crate::tools::wasm`](../crates/maturana-core/src/tools/wasm.rs),
built with `--features wasm-runtime`) gives each invocation a fresh wasmtime
store with:

- **Fuel metering** — bounds total executed instructions.
- **Epoch interruption** — a watchdog thread enforces the wall-clock timeout, so
  an infinite loop traps instead of hanging the host.
- **Linear-memory ceiling** — via `StoreLimits`.
- **WASI preview1** wired to in-memory stdin/stdout/stderr and only the declared
  filesystem preopens and env vars.

The control plane (manifests, capability policy, the on-disk registry, the
animation) is dependency-light and always compiled; the engine is behind a
feature so default builds stay fast. Without the feature, `tool run` returns a
clear "engine not built in" error rather than silently doing nothing.

## Lifecycle

```
# Author + compile to a WASI module (Rust example):
cargo build --target wasm32-wasip1 --release

# Register the artifact:
maturana tool register weather-fetch --wasm target/.../weather_fetch.wasm \
  --manifest tool.json

maturana tool list
maturana tool inspect weather-fetch
maturana tool run weather-fetch --input '{"city":"oslo"}'   # needs wasm-runtime build
```

Deploy into a guest with the existing `maturana deploy tool <agent> <dir>`.

## Telegram animation

`/tool <name> [json]` in a paired chat runs a registered tool with an
OpenClaw-style animated status message: a single message is posted and edited in
place through a braille spinner — `⠋ ⚙️ Running \`weather\`…` → `✅ Done —
\`weather\` in 412ms` — then the output is delivered. Frames are the pure,
tested [`crate::animation`](../crates/maturana-core/src/animation.rs) core; the
channel owns only the `editMessageText` side effect.

Every `/tool` run is captured as a self-improvement trajectory, so `/good` /
`/bad` can reward it (see `self-improvement-rl.md`).
