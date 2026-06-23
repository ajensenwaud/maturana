# maturana-orchestrator-loop

Use this skill when a goal is too big or too varied for one agent to do well in a
single turn — when it naturally splits into parts that different agents should do
(gather facts, write code, check the result, write it up), some of them at the
same time, and then be combined into one answer.

It runs `maturana orchestrator loop "<goal>"`: a host program that asks one agent
to break the goal into a small list of steps, runs those steps across several
worker agents, feeds each result into the steps that depend on it, and combines
everything into a final answer. When the goal produces files, the result is the
agents' REAL files (copied out of their VMs), and before the goal is called done
an agent actually RUNS them — fixing and re-checking within the cap. Hard limits
live in the host program, not in any agent, so a run always stops and never costs
without bound. This is the primary way an agent (e.g. Codex) takes on work that is
bigger than itself.

## Grounding

1. Read `AGENTS.md` first.
2. The loop command lives in `crates/maturana-cli/src/orchestrate.rs`; the
   host-enforced limits in `crates/maturana-core/src/orchestrator_budget.rs`; the
   roles in `crates/maturana-core/src/roles.rs`; on-demand VM spawning in
   `crates/maturana-core/src/orchestrator_spawn.rs`.
3. Every step is sent over the Agent2Agent (A2A) layer — read `maturana-a2a` if a
   dispatch is failing rather than the goal.

## When to use it

- The goal has parts that don't all depend on each other (research two topics at
  once, then write a summary).
- Different parts suit different agents (one is good at code, another at writing).
- You want a result checked before it counts as done.

## When NOT to use it

- A single, short task one agent can finish in one turn — just do it directly.
- Anything where the cost of several agents working isn't worth it.

## How the work is split (roles)

A run uses named roles. The defaults, which you can override in a `roles.toml`:

- **coordinator** — breaks the goal into steps with dependencies and assigns each
  to a role.
- **researcher** — gathers facts and source material.
- **developer** — writes code or a concrete artifact.
- **reviewer** — checks a result against the step's goal; approves it or sends it
  back for one more pass (only for steps marked for review).
- **synthesizer** — combines the step results into the final answer.

**By default the loop reuses the agents you already have running** — no config,
no VM spawning. It discovers them, puts the heavy roles (developer, coordinator)
on the strongest coder, and runs. The three ways to choose workers, in order:

- **Nothing (default):** reuse every running agent. Just works.
- `--agents codex-firecracker,claude-firecracker`: reuse a specific list
  (strongest-coder first; roles are assigned across them).
- `--base-spec <agent-or-spec>`: opt into the on-demand specialized VMs — spawn a
  fresh dedicated VM per role by cloning that base (slow, ~minutes per VM).
- `--roles-file ./roles.toml`: full per-role control (custom prompt/model/placement).

You only reach for a `roles.toml` when you want to hand-tune a role; the common
case needs none.

## How it always stops (the limits)

Every limit can be tightened but never raised past a compiled ceiling, and the
agents cannot change them — only the host program enforces them:

- `--max-turns` (default 40): most model turns the whole run may spend. The real
  cost ceiling.
- `--max-wall-seconds` (default 1800): longest the run may take.
- `--max-parallel` (default 4): most steps running at once.
- `--max-vms` (default 4): most worker VMs alive at once.
- Depth: an agent run as a worker may start helpers only one level down, never
  deeper, and a plan with a dependency cycle is rejected before any step runs.

A plan whose worst case wouldn't fit in the turn budget is rejected up front, so a
run ends by finishing, not by running out mid-way.

## Preflight

- Confirm the host plane is up (`maturana status`) and the `a2a` process is
  running — every step travels over A2A.
- Placement is reuse-by-default; confirm agents are running (`maturana list`). Only
  pass `--base-spec` if you actually want to spawn dedicated VMs (slow).
- Size the budget to the goal before running, not after — a plan that can't fit
  the turn budget is rejected up front.
- Pick a `run_id` you can follow with `status` / `abort`, or let one be assigned.

## Decision Path

- Goal fits in one turn for one agent: don't use this — just answer directly.
- Want it to just work: run with no placement flags — it reuses running agents.
  Reach for `--base-spec` (spawn) or `--roles-file` only when you specifically need
  isolation or per-role tuning.
- Goal produces files (a game, a webpage, a script): pass `--output <dir>` and the
  files land there; don't expect a single `answer.md`.
- A step dispatch errors rather than the step's work failing: the A2A layer, not
  the goal — read `maturana-a2a`.
- The run stops on a limit (`wall-clock budget reached`, `turn budget exhausted`):
  the goal is too big for that budget — shrink the goal, don't reflexively raise
  the cap.
- The plan is rejected up front (`plan could exceed the budget` / a cycle): fix
  the plan/goal, not the ceiling.

## Actions

Run a goal — reuses your running agents, no flags needed:

```bash
maturana orchestrator loop "Research the top 3 Rust web frameworks and write a one-page comparison"
```

Build something with files (a game, a webpage, a script) — the deliverable is
written as **real files** into the `--output` directory, not a single markdown:

```bash
maturana orchestrator loop "Build a tic-tac-toe game playable in the browser" --output ./tictactoe
# -> ./tictactoe/index.html, ./tictactoe/game.js, ./tictactoe/SUMMARY.md
```

The files are the agents' REAL output — each worker writes its files inside its VM
and the host copies those exact bytes out (binaries included); they are not retyped
by another agent. Before delivering, an agent RUNS the files (opens the page, runs
the script, calls the endpoint) and fixes them in place if broken, re-checking up to
the review-cycle cap; the verdict (`verified: runs` / `NOT verified` / `unverified`)
prints and is recorded in `SUMMARY.md`. Pass `--no-verify` to skip that and deliver
unchecked. A short `SUMMARY.md` (verdict + goal + each step's report + the file list)
is written alongside. If the goal was prose and no worker wrote a file, the result
is a single text answer instead (`--output` file, default `<run>/answer.md`). You
just say where with `--output`; the system picks files-vs-prose from what the agents
actually produced.

Pick specific agents, or tighten the limits for a cheaper run:

```bash
maturana orchestrator loop "<goal>" --agents codex-firecracker,claude-firecracker
maturana orchestrator loop "<goal>" --max-turns 12 --max-parallel 2
```

Opt into dedicated spawned VMs, or take full per-role control:

```bash
maturana orchestrator loop "<goal>" --base-spec codex-firecracker --max-vms 2
maturana orchestrator loop "<goal>" --roles-file ./roles.toml
```

Watch a run's step list and status:

```bash
maturana orchestrator status <run_id>
```

Stop a run (takes effect between steps; a step already running finishes its turn):

```bash
maturana orchestrator abort <run_id>
```

## Evidence

Before claiming a run succeeded, collect:

- The printed plan (`N steps`) and the per-step `-> sent` / `<- done` lines.
- `maturana orchestrator status <run_id>` showing every step `Done`.
- For a file deliverable: the printed `wrote N file(s) to <dir> [<verdict>]` line,
  the real files at `--output` (default `<run>/output/`), and a `SUMMARY.md` beside
  them. The per-step `(+N file(s) collected)` lines show the bytes came off the
  workers' VMs, not a rewrite; the `-> verifying…` / `verification passed` lines and
  the `[verified: runs]` tag show it was actually run. For a prose goal: `answer.md`.
- The run directory `.maturana/orchestration/<run_id>/`: `plan.json` (the final
  step list with results) and `staging/` (what was fetched from the workers).
- That the run ended on its own (completed) rather than hitting a limit — a stop
  for `wall-clock budget reached` or `turn budget exhausted` means it did not
  finish; simplify the goal or raise the relevant cap (within reason).

## Recovery

- "planning failed: …": the coordinator did not return a usable plan. Re-run, or
  give a clearer goal.
- "plan could exceed the budget": the goal is too big for the turn budget. Make
  the goal smaller or raise `--max-turns`.
- "no running agents to reuse": launch agents first (`maturana list`), or pass
  `--agents <id,id>`, or spawn dedicated VMs with `--base-spec <agent-or-spec>`.
- A file deliverable came out as prose in `answer.md` instead of files: the
  synthesizer judged the goal as prose — rerun with a goal that clearly asks for
  files, and pass `--output <dir>`.
- Verdict is `NOT verified` / `unverified`: an agent ran the files and they still
  failed (or it couldn't finish within the cap) — read `SUMMARY.md` for what's
  broken, then re-run (optionally `--max-turns` higher to allow more repair passes).
- A step keeps failing: read its result in `plan.json`; the failure stops the run
  with the partial results preserved.

## Boundaries

- Do not raise a limit just to get past a stop — a run that hits a limit is
  telling you the goal is too big for that budget.
- Do not use this for a task one agent can do in one turn.
- Do not run several orchestrations against the same agents at once (the first
  version runs one at a time on purpose).
- Do not have a worker role rewrite another agent's identity files; a role's
  instructions are added to the task only.
