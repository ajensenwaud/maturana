# maturana-self-forge

Use this skill when an agent should be allowed to extend itself at runtime —
author a small WebAssembly capability and run it the same turn, in a sandbox,
without a host rebuild. This is Maturana's self-mutation primitive (the pi.dev
"ask it to build, then use it live" idea adapted to WASM), gated by the
`self_forge` capability.

Self-forge lets an agent execute code it just wrote. That is a real privilege, so
it is off by default and granted per agent; the skill explains when to grant it
and how the agent and host cooperate.

## Grounding

1. Read `AGENTS.md` first.
2. Read `docs/wasm-tools.md` for the manifest, capability, and sandbox model the
   forge reuses.
3. Read the target agent `MATURANA.md` and `maturana-security-review` before
   granting a new privilege.
4. Note the runtime ships by default (the `maturana` binary's `wasm-runtime`
   feature); no extra install is needed on the host.

## Preflight

- Confirm the agent genuinely benefits from building capabilities at runtime, not
  a fixed pre-built tool (`maturana-wasm-tool`) that you could deploy once.
- Confirm the owner accepts that the agent may run agent-authored code (sandboxed,
  no ambient filesystem or network unless the request declares it).
- Confirm the agent has a channel so the forge animation (`Building`/`Running`)
  is visible.

## Decision Path

- One-off computation the agent needs this turn: self-forge it (this skill).
- A capability you want every agent to share, built once: author a fixed WASM
  tool instead (`maturana-wasm-tool`) and deploy it.
- Host lifecycle or a provider operation: not a forge — use a Rust command.
- Untrusted owner or sensitive data: do not grant `self_forge`; keep the agent to
  pre-reviewed tools only.

## Actions

1. Grant the capability in the agent `MATURANA.md`:

   ```yaml
   capabilities:
     self_forge: true
   ```

2. Re-apply the spec (`maturana agent update` / launch). The host then injects a
   forge-awareness block into the agent's prompt every turn and installs the
   `maturana-forge` helper on the guest PATH.
3. The in-guest agent forges from a shell — WAT on stdin, JSON on `--input`:

   ```
   maturana-forge adder --input '{"a":2,"b":3}' <<'WAT'
   (module
     (import "wasi_snapshot_preview1" "fd_write"
       (func $fd_write (param i32 i32 i32 i32) (result i32)))
     (memory (export "memory") 1)
     (func (export "_start") ... ))
   WAT
   ```

4. The host (`POST /session/forge`) checks `self_forge`, assembles the WAT (or
   base64 `--wasm`), registers it in the agent's private forge registry, runs it
   in the wasmtime sandbox, and returns `{ok, stdout, stderr, fuel_used,
   duration_ms}` while streaming the build/run animation to the channel.

## Evidence

Before claiming success, collect:

- The agent spec diff showing `capabilities.self_forge: true`.
- A forged capability's name plus its `/session/forge` response (stdout and
  non-zero `fuel_used` proving it executed).
- The channel transcript showing the `Building`/`Running`/`Forged` animation.
- A denied case: an agent without the grant gets `403` from `/session/forge`.

## Recovery

- `403` from the forge: the agent is not granted `self_forge`; grant it and
  re-apply, or decline if the privilege is not warranted.
- WAT fails to assemble: the response carries the assembler error; fix the module.
- Module traps on fuel/timeout: tighten the algorithm, or raise limits only if the
  work genuinely needs it.
- Needs filesystem/network: declare the narrowest `capabilities` in the request,
  and allow-list any host at the egress proxy too.

## Boundaries

- Do not grant `self_forge` to an agent handling sensitive data without an
  explicit owner decision and a `maturana-security-review`.
- Do not widen a forged module's capabilities beyond what it proves it needs.
- Do not use the forge to smuggle credentials or OAuth state into a module.
- Do not treat the forge as a general shell: it runs sandboxed WASM, not host
  commands.
