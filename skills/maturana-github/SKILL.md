# maturana-github

Use this skill when a Maturana agent needs to work with GitHub repositories.

GitHub support belongs in agent tools/skills, not Maturana core. Repository
access must respect the agent contract, workspace boundaries, and credential
policy.

## Grounding

1. Read `AGENTS.md` first.
2. Read the target agent `MATURANA.md` and confirm GitHub access is in scope.
3. Identify the repo, branch, allowed operations, and whether push/PR creation
   is permitted.
4. Inspect `/workspace` policy and writable mounts.
5. Confirm GitHub credentials are stored or injected through pipelock/tool env,
   not committed files.

## Preflight

- Confirm the repo and operation are allowed by the agent contract.
- Confirm the workspace path is bounded and writable.
- Confirm GitHub credentials are scoped and referenced through pipelock or tool
  environment.
- Confirm tests or verification commands are known before committing changes.
- Confirm push/PR creation is explicitly allowed before enabling write access.

## Decision Path

- Read-only repo work: clone or fetch into `/workspace`.
- Code modification: ensure tests and commit policy are clear first.
- Push or PR: require explicit user permission or explicit agent contract
  permission.
- Token required: use pipelock for GitHub tokens and scoped tool env.
- Broad organization access requested: narrow the token/repo scope before
  deployment.

## Actions

Deploy or install `git` and any required credential helper as a guest tool.

Clone into `/workspace`:

```bash
git clone <repo-url> /workspace/<repo>
```

Make changes and run tests:

```bash
git status --short
<project test command>
```

Commit only the intended paths:

```bash
git add <paths>
git commit -m "<message>"
```

Push only when allowed:

```bash
git push <remote> <branch>
```

## Evidence

Before claiming success, collect:

- Repo path under `/workspace`.
- `git status --short` before and after changes.
- Test output for changed code.
- Commit hash when a commit is made.
- Push/PR URL when publishing is allowed.
- Audit/session transcript showing the user or contract allowed write access.

## Recovery

- Auth fails: verify pipelock secret name and scoped env injection; do not paste
  the token.
- Clone path outside `/workspace`: stop and reclone inside allowed roots.
- Dirty worktree contains unrelated changes: preserve them and ask before
  staging.
- Tests fail: fix or report failure; do not commit a broken change as success.
- Push rejected: inspect branch protection and remote state before retrying.

## Boundaries

- Do not write raw GitHub tokens into specs, docs, skills, commits, or logs.
- Do not clone outside declared workspace roots.
- Do not commit unrelated user changes.
- Do not push without explicit permission or contract authorization.
- Do not install broad credential helpers that leak access across agents.
