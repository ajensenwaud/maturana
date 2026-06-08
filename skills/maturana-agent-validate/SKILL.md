# maturana-agent-validate

Use this skill when a user wants to validate a `MATURANA.md` agent spec before
launch or update.

## Procedure

1. Read `AGENTS.md` first.
2. Run:

   ```powershell
   maturana spec validate MATURANA.md
   ```

3. If validation fails, fix the spec rather than bypassing the validator.
4. Never paste or check in raw secrets. For the MVP, use `env:`, `file:`, or
   `pipelock:` references. Do not use pipelock for Codex or Claude OAuth
   state; those harnesses need direct guest auth injection.
