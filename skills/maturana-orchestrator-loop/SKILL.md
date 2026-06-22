# maturana-orchestrator-loop

Use this skill when a goal is too big or too varied for one agent to do well in a
single turn — when it naturally splits into parts that different agents should do
(gather facts, write code, check the result, write it up), some of them at the
same time, and then be combined into one answer.

It runs `maturana orchestrator loop "<goal>"`: a host program that asks one agent
to break the goal into a small list of steps, runs those steps across several
worker agents, feeds each result into the steps that depend on it, and combines
everything into a final answer. Hard limits live in the host program, not in any
agent, so a run always stops and never costs without bound. This is the primary
way an agent (e.g. Codex) takes on work that is bigger than itself.

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

By default each role runs in its own dedicated worker VM the loop brings up for
the run (set by `--base-spec`). On a small install you can instead point a role at
an existing agent in `roles.toml`:

```toml
[roles.researcher.placement]
reuse = { agent_id = "opencode-firecracker" }
```

## How it always stops (the limits)

Every limit can be tightened but never raised past a compiled ceiling, and the
agents cannot change them — only the host program enforces them:

- `--max-turns` (default 24): most model turns the whole run may spend. The real
  cost ceiling.
- `--max-wall-seconds` (default 1800): longest the run may take.
- `--max-parallel` (default 4): most steps running at once.
- `--max-vms` (default 4): most worker VMs alive at once.
- Depth: an agent run as a worker may start helpers only one level down, never
  deeper, and a plan with a dependency cycle is rejected before any step runs.

A plan whose worst case wouldn't fit in the turn budget is rejected up front, so a
run ends by finishing, not by running out mid-way.

## Actions

Run a goal:

```bash
maturana orchestrator loop "Research the top 3 Rust web frameworks and write a one-page comparison"
```

Tighten the limits for a cheaper run:

```bash
maturana orchestrator loop "<goal>" --max-turns 12 --max-parallel 2 --max-vms 2
```

Use a custom role set:

```bash
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
- The run directory `.maturana/orchestration/<run_id>/`: `plan.json` (the final
  step list with results) and `answer.md` (the final answer).
- That the run ended on its own (completed) rather than hitting a limit — a stop
  for `wall-clock budget reached` or `turn budget exhausted` means it did not
  finish; simplify the goal or raise the relevant cap (within reason).

## Recovery

- "planning failed: …": the coordinator did not return a usable plan. Re-run, or
  give a clearer goal.
- "plan could exceed the budget": the goal is too big for the turn budget. Make
  the goal smaller or raise `--max-turns`.
- A role error about "spawn placement … not wired": map that role to a standing
  agent in `roles.toml` (see above) until on-demand VM spawning is available.
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
