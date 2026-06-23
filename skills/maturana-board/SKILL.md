# maturana-board

Use this skill when work should be coordinated as a **persistent, shared board of
tasks across multiple agents** — you want to write down a set of cards (each with
an owner and dependencies), leave them, edit them, and have a dispatcher run every
ready card on its assignee, in parallel, until the board is drained. It is
Maturana's Kanban for multi-agent work: the durable cousin of
`maturana-orchestrator-loop` (which is a one-shot goal→plan→done run).

A card has a title, optional detail, an assignee (a role like `developer` or a
concrete agent id like `codex-firecracker`), a status (`todo`/`doing`/`done`/
`blocked`), and dependencies on other cards. `maturana board run` claims every
ready card (its deps are `done`) and runs it on its assignee over the A2A layer —
concurrently, up to the host-enforced `max_parallel` — collecting any files the
card produced. Every agent still runs in its own VM; the board never becomes a
new, weaker place to run code.

## Grounding

1. Read `AGENTS.md` first.
2. The board model lives in `crates/maturana-core/src/board.rs` (Card/Board, JSON
   store, dep validation, ready selection). The dispatcher lives in
   `crates/maturana-cli/src/orchestrate.rs` (`handle_board` + `run_board`), reusing
   the orchestrator's A2A dispatch, host-enforced budgets, and artifact collection.
3. The same budget ceilings as the orchestrator apply (see
   `maturana-orchestrator-loop` and `crates/maturana-core/src/orchestrator_budget.rs`).
4. A card dispatched over A2A is the same wire path as everything else — read
   `maturana-a2a` if a card fails to dispatch (vs the card's work failing).

## Preflight

- Confirm the host plane is up (`maturana status`) and the `a2a` process is running.
- Confirm the agents your cards name (or the running agents, for role assignees)
  are up — a card whose assignee isn't claiming turns will stall.
- Decide assignees: a role (`developer`/`researcher`/`reviewer`/`synthesizer`) maps
  to an agent via reuse/placement; a concrete agent id pins the card to that agent.
- Set dependencies so independent cards can run in parallel and dependent cards wait.

## Decision Path

- One-shot goal, no need to keep the plan around: use `maturana-orchestrator-loop`,
  not the board.
- Ongoing, editable, multi-owner task list: use the board.
- A card should run only after others: give it `--needs c1,c2`; the dispatcher
  holds it until those are `done`.
- A card errored and downstream cards won't run: it's `blocked` — read its result,
  fix it, `board move <id> todo`, and re-run.
- A dispatch errors rather than the card's work failing: the A2A layer, not the
  board — read `maturana-a2a`.

## Actions

Add cards (independent c1, c2; c3 depends on both), then run the board:

```bash
maturana board add "Research Rust web frameworks" --assignee researcher
maturana board add "Research Python web frameworks" --assignee claude-firecracker
maturana board add "Write a comparison" --assignee developer --needs c1,c2
maturana board list
maturana board run --max-parallel 2 --output ./out
```

Inspect / nudge a board between runs:

```bash
maturana board status                 # counts per column
maturana board move c4 todo           # requeue a blocked/edited card
maturana board run --board myboard    # named boards are independent
```

Tighten the limits (same caps as the orchestrator, all host-enforced):

```bash
maturana board run --max-turns 12 --max-parallel 3
```

## Evidence

Before claiming a board run succeeded, collect:

- The dispatch lines: multiple `-> card cN (…) -> <agent>` printed together before
  any `<- card done` proves cards ran in **parallel** across agents.
- `maturana board list` (or `status`) showing the cards moved `todo → done` (or
  `blocked` with a reason in the card's result).
- For file-producing cards: the printed `wrote N file(s) to <dir>` and the real
  files under `--output` (collected from the agents' VMs, not retyped).
- That the run drained rather than stopping on a limit — a `[turn budget exhausted]`
  / `[wall-clock budget reached]` line means it stopped early; simplify or raise the
  relevant cap within reason.

## Recovery

- "board is empty": add cards first with `maturana board add …`.
- "card depends on unknown card": add the dependency card first, or fix the
  `--needs` ids; the board validates deps and rejects cycles up front.
- A card is `blocked`: read its result (`board list`), fix the cause, then
  `board move <id> todo` and re-run — done cards are not re-run.
- "no running agents to reuse" / a card stalls: launch the assignee agent (or pass
  a concrete `--assignee <agent>` that is running); a role assignee needs at least
  one running agent.
- The run stopped on a budget line: the board was bigger than the budget — raise
  `--max-turns` within reason or split the work; don't try to widen caps from inside.

## Boundaries

- Do not treat the board as a new execution backend — a card always runs in its
  assignee's VM over A2A; there is no local/Docker/SSH shortcut (that is the
  zero-trust line, see `docs/multi-agent-orchestration.md`).
- Do not raise a cap to get past a budget stop — the ceilings are host-enforced and
  an agent cannot widen them.
- Do not hand a card work that rewrites another agent's identity files; a card is a
  task for an agent, not a way to reconfigure the fleet.
- Do not point a card's assignee at an agent that isn't running and expect it to
  proceed — it will stall waiting for that agent to claim the turn.
