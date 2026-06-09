# maturana-wasm-tool

Use this skill when an agent needs a new executable capability built on the fly
as a sandboxed WebAssembly tool, registered and run without rebuilding the host.

WASM tools own untrusted, agent-authored side effects under capability gating
and hard resource limits. Skills explain when and why to invoke them.

## Grounding

1. Read `AGENTS.md` first.
2. Read `docs/wasm-tools.md` for the manifest, capability, and sandbox model.
3. Read the calling skill and the target agent `MATURANA.md`.
4. Inspect existing tools with `maturana tool list` to avoid duplicates.
5. Identify the minimum capabilities (fs/env/net) and resource limits needed.

## Preflight

- Confirm this is executable behavior with a narrow JSON input/output contract.
- Confirm the tool starts from `Capabilities::default()` (no authority) and only
  adds what it proves it needs.
- Confirm network hosts are also allow-listed at the egress proxy.
- Confirm a local `maturana tool run` smoke succeeds before any deployment.

## Decision Path

- Pure compute (parse, transform, compute): keep capabilities empty.
- Needs to read inputs: add a single `fs_read` preopen, nothing broader.
- Needs the network: add specific `net` hosts and the proxy allowlist entry.
- Host lifecycle or provider operation: this is not a tool — use a Rust command.
- Reusable policy or playbook: put the decision logic in a skill, not the tool.

## Actions

1. Author the module and compile it to a WASI command (`wasm32-wasip1`).
2. Write `tool.json` with name, version, capabilities, and limits.
3. Register: `maturana tool register <name> --wasm <path> --manifest tool.json`.
4. Smoke test: `maturana tool run <name> --input '<json>'` and inspect output.
5. Deploy to a guest with `maturana deploy tool <agent> <dir>` after it passes.

## Evidence

Before claiming success, collect:

- The registered tool path and its normalized `tool.json`.
- The input/output contract and the declared capabilities and limits.
- The `maturana tool run` stdout plus non-zero fuel used as proof it executed.
- A failing-case result showing fuel/timeout stops a runaway module.
- Guest deploy evidence and a guest-side smoke result when deployed.

## Recovery

- Module is not valid WASM: rebuild for `wasm32-wasip1` and re-register.
- Tool traps on fuel/timeout: raise limits only if the work genuinely needs it,
  otherwise fix the loop.
- Needs authority it was denied: add the narrowest capability and re-register.
- Engine not built in: rebuild the binary with `--features wasm-runtime`.
- Local smoke fails: fix before deploying; never ship an unverified module.

## Boundaries

- Do not grant capabilities the tool has not proven it needs.
- Do not embed credentials or OAuth state in the module or manifest.
- Do not create generic shell or command runners as tools.
- Do not deploy a tool that has not passed a local `maturana tool run` smoke.
- Do not put human decision logic inside a tool.
