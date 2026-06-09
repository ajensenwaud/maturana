# maturana-agent-validate

Use this skill when a user wants to validate a `MATURANA.md` agent spec before
launch or update.

Validation is a design review plus a compiler check. The CLI is the authority,
but the skill should also catch obvious security or product-shape drift before
launch.

## Grounding

1. Read `AGENTS.md` first.
2. Read the candidate `MATURANA.md`.
3. Identify the intended provider, harness, mounts, egress allowlist,
   credentials, channels, schedules, memory/wiki paths, and snapshot policy.

## Preflight

- Confirm the file exists and is the spec the user intends to launch or update.
- Check for raw secret-looking strings before running commands.
- Confirm the target host matches the declared provider: Hyper-V on Windows,
  Firecracker on Linux.
- If validating a materialized agent, compare the candidate with
  `.maturana/agents/<agent-id>/MATURANA.md` before changing anything.

## Decision Path

- Run the validator before launch or update.
- If validation fails, fix the spec rather than bypassing the validator.
- Confirm the launch target is explicit:
  - Hyper-V on Windows
  - Firecracker on Linux
- Confirm capabilities match the agent purpose and are not broader than needed.
- Confirm declared mounts are bounded and intentional.
- Confirm egress policy is allowlist-based for governed agents.
- Confirm credentials use references, not literal secret values.
- Confirm Codex and Claude Code OAuth credentials are direct guest auth
  injection, not pipelock secrets.

## Actions

Run the validator:

   ```powershell
   maturana spec validate MATURANA.md
   ```

Use JSON output when the result will feed another tool or CI step:

```powershell
maturana spec validate MATURANA.md --json
```

## Evidence

Successful validation requires:

- clean `maturana spec validate` output
- no raw secret material in the spec
- clear provider and harness choice
- bounded mounts and egress
- snapshot/channel/schedule fields either explicitly configured or intentionally
  absent

## Recovery

- Missing required field: add the smallest explicit spec field.
- Unknown harness or provider: use `codex`, `claude-code`, `opencode`,
  `hyperv`, or `firecracker`.
- Overbroad mount: narrow it to the needed host and guest paths.
- Literal secret: move it to `env:`, `file:`, or `pipelock:`.
- OAuth credential declared as pipelock: replace it with the harness auth
  injection path.

## Boundaries

- Do not launch a spec that fails validation.
- Do not weaken validation to make a demo pass.
- Do not infer broad filesystem or network permissions from a vague user goal.
- Do not paste or commit raw secrets.
