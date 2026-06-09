# maturana-self-improve

Use this skill when running the self-improvement flywheel: capturing agent
trajectories, attaching reward signals, and curating datasets for offline
training and safe redeploy.

It owns the host side of the loop (capture, reward, curate, gate). Training and
evaluation happen off-host on the exported dataset.

## Grounding

1. Read `AGENTS.md` first.
2. Read `docs/self-improvement-rl.md` for the full loop and reward table.
3. Read the target agent `MATURANA.md`, `SOUL.md`, and recent memory.
4. Inspect the corpus with `maturana improve report` before curating.
5. Confirm reward sources are trustworthy (user feedback, tests, rollbacks).

## Preflight

- Confirm trajectories are being captured for the target agent and session.
- Confirm reward signs follow `improvement::signals` (rollback dominates 👎).
- Confirm no raw secrets or private data will enter the exported dataset.
- Confirm a snapshot exists before any model/prompt/skill change is rolled out.

## Decision Path

- Few or unrewarded trajectories: collect more signal before curating.
- Strong recurring high-reward pattern: fold it into `SOUL.md`/skills first
  (cheapest improvement, no training).
- Enough high/low pairs: export preference data for DPO.
- Enough high-reward turns: export SFT JSONL for fine-tuning or distillation.
- Candidate ready: gate on an eval win-rate, then redeploy behind a snapshot.

## Actions

1. Record turns: `maturana improve record <agent> --input ... --output ...`
   (Telegram `/tool`, `/good`, `/bad` already feed this automatically).
2. Reward: `maturana improve reward --agent-id <agent> --value <signal>`.
3. Curate: `maturana improve curate --min-reward 1 --jsonl > dataset.jsonl`.
4. Hand the dataset to the external trainer and the eval harness.
5. Redeploy only after the win-rate gate, wrapped in a snapshot.

## Evidence

Before claiming success, collect:

- The `maturana improve report` counts before and after the iteration.
- The curated example list and the exported JSONL line count.
- The reward rows proving signal provenance (user, task, rollback).
- The eval win-rate of the candidate versus the current agent.
- The snapshot id taken immediately before redeploy.

## Recovery

- No turns to reward: confirm capture is wired before blaming curation.
- Reward attached to the wrong turn: reward by explicit `--trajectory-id`.
- Dataset leaked sensitive data: purge it and tighten capture redaction.
- Candidate regresses: restore the pre-improve snapshot (logged as a strong
  negative) and feed the regression back as labeled data.
- Eval gate fails: do not redeploy; iterate on data or approach instead.

## Boundaries

- Do not train models in-process on the host.
- Do not redeploy a candidate that has not passed the eval win-rate gate.
- Do not roll out an improvement without a pre-improve snapshot.
- Do not export trajectories containing raw secrets or private user data.
- Do not invent reward signs; use the canonical `improvement::signals` values.
