# Self-Improvement Loop (RL Data Flywheel)

Maturana improves agents with a Hermes-style data flywheel: real agent turns are
captured, scored with reward signals, curated into datasets, used to train a
candidate **offline and off-host**, then gated on evaluation and rolled out
behind a snapshot so any regression is instantly reversible.

The host never trains in-process. It owns capture, reward, curation, and the
safe-redeploy gate; the actual SFT/DPO/distillation happens in an external
trainer fed by the exported dataset.

## The loop

```
        ┌──────────── capture ───────────┐
        │  every agent turn → Trajectory  │
        ▼                                 │
   reward signals                         │
   (👍/👎, task success, rollback)        │
        ▼                                 │
   curate → SFT / preference JSONL        │
        ▼                                 │
   train candidate (OFF-HOST)             │
        ▼                                 │
   evaluate vs baseline (win-rate gate)   │
        ▼                                 │
   snapshot → redeploy → monitor ─────────┘   (rollback on regression)
```

## 1. Capture

Every turn is recorded as a `Trajectory` (agent, session, input/state, the
model's output, tool calls) in
[`crate::improvement`](../crates/maturana-core/src/improvement.rs):

```
maturana improve record personal --input "<prompt>" --output "<reply>"
```

The Telegram `/tool` path records tool runs automatically; the guest worker
records harness turns the same way.

## 2. Reward

Signals attach to trajectories with a canonical sign/magnitude
(`improvement::signals`) so every call site agrees:

| Signal | Value | Source |
| --- | --- | --- |
| 👍 / `/good` | `+1` | user feedback in Telegram |
| 👎 / `/bad` | `-1` | user feedback in Telegram |
| task success | `+1` | rule/judge: tests pass, schedule completed |
| task failure | `-1` | rule/judge |
| **snapshot rollback** | `-5` | the operator had to undo the agent — the strongest negative |

```
maturana improve reward --agent-id personal --value 1     # rewards the latest turn
```

`/good` and `/bad` in a paired Telegram chat call `reward_latest`, so feedback
lands on the turn the user just saw without the channel tracking ids. A snapshot
restore is a strong negative because it means the agent did something that had
to be reversed — exactly the behaviour the flywheel should train away from.

## 3. Curate

High-reward turns become supervised examples; high/low pairs become preference
data:

```
maturana improve curate --min-reward 1            # inspect the SFT set
maturana improve curate --min-reward 1 --jsonl    # export chat-style JSONL
maturana improve report                            # corpus health
```

Curation orders by aggregate reward and filters by threshold. The JSONL is the
hand-off artifact for the external trainer.

## 4. Improve (off-host)

The exported dataset feeds an external job. Options, cheapest first:

1. **Prompt / skill optimization** — fold recurring high-reward patterns into
   `SOUL.md`, skills, or context (no training; immediate).
2. **SFT / distillation** — fine-tune a smaller guest model on curated turns.
3. **DPO / preference tuning** — use high vs low pairs for the same prompt.

## 5. Evaluate and redeploy safely

A candidate must beat the current agent on a held-out eval set (win-rate gate)
before rollout. Redeploy is always wrapped in a snapshot:

```
maturana snapshot take <agent> pre-improve --live
# swap model/prompt/skill, then monitor reward
# if reward regresses: maturana snapshot restore <agent> pre-improve --live  (logged as -5)
```

The rollback both protects the user and feeds the flywheel: the regression
becomes labeled negative data for the next iteration.

## What is implemented vs. external

- **Implemented in-repo:** trajectory + reward store, curation, JSONL export,
  reporting, Telegram feedback wiring, snapshot-rollback as a reward signal.
- **External by design:** the trainer (SFT/DPO/distillation) and the evaluation
  harness, which run off-host on the exported dataset and report a win-rate the
  redeploy gate consumes.
