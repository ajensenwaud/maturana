//! Hard safety limits for multi-agent orchestration.
//!
//! Multi-agent orchestration fans a single goal out across many worker agents
//! (each its own microVM), so both cost and the risk of never stopping
//! *multiply*. This module is the safety core, and it is deliberately the first
//! thing built and the most heavily tested: every hard limit lives here, the
//! per-run turn budget can only be spent DOWN (there is no method, anywhere, to
//! add turns or raise the cap), and operator/agent overrides may only LOWER a
//! limit — never raise the compiled ceiling.
//!
//! The contract the rest of the system relies on: **an agent can choose to stop
//! early, but can never widen its own budget.** Continuation is decided by the
//! host loop against limits the agent cannot touch. As long as a model turn can
//! only be started by spending from a [`RunBudget`], the total number of model
//! turns one run can ever cause is bounded by `max_total_turns`, full stop.

use serde::{Deserialize, Serialize};

/// Compiled absolute ceilings. Operator flags and agent requests may only lower
/// a configured value toward zero; nothing at runtime can exceed these. They are
/// the final backstop that makes a runaway impossible regardless of
/// configuration, a buggy planner, or a misbehaving agent.
pub mod ceiling {
    /// Most model turns one run may ever cause.
    pub const MAX_TOTAL_TURNS: u32 = 200;
    /// Most steps one plan may contain.
    pub const MAX_STEPS: u32 = 64;
    /// Longest a run may take on the wall clock.
    pub const MAX_WALL_SECONDS: u64 = 6 * 3600;
    /// Most scheduler ticks before a run is force-stopped (liveness backstop).
    pub const MAX_TICKS: u64 = 100_000;
    /// Deepest nesting of orchestration (an agent dispatched as a worker may
    /// itself dispatch only while below this depth).
    pub const MAX_DEPTH: u32 = 4;
    /// Most steps in flight at once.
    pub const MAX_PARALLEL: u32 = 32;
    /// Most worker VMs alive at once for one run.
    pub const MAX_CONCURRENT_VMS: u32 = 32;
    /// Most times one step may be sent back for revision.
    pub const MAX_REVIEW_CYCLES: u32 = 5;
    /// Most times the coordinator may re-plan a failed step.
    pub const MAX_REPLANS: u32 = 3;
}

/// Every hard limit for one orchestration run. Defaults are conservative; an
/// operator can lower any field. [`OrchestratorCaps::clamp_to_ceiling`] enforces
/// the upper bound, so a config or override can never push a limit past the
/// compiled ceiling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorCaps {
    /// Most model turns the whole run may spend. The real cost ceiling.
    pub max_total_turns: u32,
    /// Most steps the plan may contain (the planner is rejected past this).
    pub max_steps: u32,
    /// Longest the run may take, in seconds.
    pub max_wall_seconds: u64,
    /// Most scheduler ticks before force-stop (fires even if no turn was spent).
    pub max_ticks: u64,
    /// Deepest orchestration nesting allowed.
    pub max_depth: u32,
    /// Most steps in flight at once.
    pub max_parallel: u32,
    /// Most worker VMs alive at once for this run.
    pub max_concurrent_vms: u32,
    /// Most revision rounds per step.
    pub max_review_cycles: u32,
    /// Most re-plans per failed step.
    pub max_replans: u32,
}

impl Default for OrchestratorCaps {
    fn default() -> Self {
        Self {
            // Sized to admit a full `max_steps`-step plan's worst case
            // (`worst_case_turns(6)` = 38, plus the coordinator turn), so the
            // turn budget never contradicts the step cap. Typical runs spend far
            // less; this is the ceiling, not the expected spend.
            max_total_turns: 40,
            max_steps: 6,
            max_wall_seconds: 1800,
            max_ticks: 1200,
            max_depth: 2,
            max_parallel: 4,
            max_concurrent_vms: 4,
            max_review_cycles: 2,
            max_replans: 1,
        }
    }
}

impl OrchestratorCaps {
    /// Clamp every field DOWN to the compiled ceiling. An override can lower a
    /// limit but can never raise it past the ceiling — the load-bearing safety
    /// invariant. Call this on any caps that came from config or CLI flags
    /// before using them. Returns the clamped caps for chaining.
    #[must_use]
    pub fn clamp_to_ceiling(mut self) -> Self {
        self.max_total_turns = self.max_total_turns.min(ceiling::MAX_TOTAL_TURNS);
        self.max_steps = self.max_steps.min(ceiling::MAX_STEPS);
        self.max_wall_seconds = self.max_wall_seconds.min(ceiling::MAX_WALL_SECONDS);
        self.max_ticks = self.max_ticks.min(ceiling::MAX_TICKS);
        self.max_depth = self.max_depth.min(ceiling::MAX_DEPTH);
        self.max_parallel = self.max_parallel.min(ceiling::MAX_PARALLEL);
        self.max_concurrent_vms = self.max_concurrent_vms.min(ceiling::MAX_CONCURRENT_VMS);
        self.max_review_cycles = self.max_review_cycles.min(ceiling::MAX_REVIEW_CYCLES);
        self.max_replans = self.max_replans.min(ceiling::MAX_REPLANS);
        self
    }

    /// Apply a caller's requested overrides, taking the MINIMUM of the current
    /// value and the request for each field — so an override can only tighten a
    /// limit, never loosen it. `None` fields are left unchanged. The result is
    /// still clamped to the compiled ceiling.
    #[must_use]
    pub fn tighten_with(mut self, overrides: &CapsOverride) -> Self {
        if let Some(v) = overrides.max_total_turns {
            self.max_total_turns = self.max_total_turns.min(v);
        }
        if let Some(v) = overrides.max_wall_seconds {
            self.max_wall_seconds = self.max_wall_seconds.min(v);
        }
        if let Some(v) = overrides.max_parallel {
            self.max_parallel = self.max_parallel.min(v);
        }
        if let Some(v) = overrides.max_concurrent_vms {
            self.max_concurrent_vms = self.max_concurrent_vms.min(v);
        }
        if let Some(v) = overrides.max_steps {
            self.max_steps = self.max_steps.min(v);
        }
        self.clamp_to_ceiling()
    }

    /// The worst-case number of model turns a fully-materialized plan of
    /// `step_count` steps could consume: each step runs once and may be revised
    /// up to `max_review_cycles` times, and that whole step may be re-planned up
    /// to `max_replans` times, plus one synthesis turn and one planning turn.
    /// The planner admission check uses this to reject — before any step runs —
    /// a plan that could not finish within the remaining budget, so a run ends
    /// by completing rather than truncating mid-plan.
    pub fn worst_case_turns(&self, step_count: u32) -> u32 {
        let per_step = (1 + self.max_review_cycles).saturating_mul(1 + self.max_replans);
        step_count.saturating_mul(per_step).saturating_add(2)
    }
}

/// A caller-supplied request to tighten the caps for one run (e.g. CLI flags).
/// Every field is optional and can only LOWER the corresponding cap.
#[derive(Debug, Clone, Default)]
pub struct CapsOverride {
    pub max_total_turns: Option<u32>,
    pub max_wall_seconds: Option<u64>,
    pub max_parallel: Option<u32>,
    pub max_concurrent_vms: Option<u32>,
    pub max_steps: Option<u32>,
}

/// Why a run can no longer proceed. Each variant maps to a stop-ladder reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetExhausted {
    /// The per-run model-turn budget is spent.
    Turns,
    /// The scheduler-tick ceiling was reached (liveness backstop).
    Ticks,
}

/// The per-run turn budget. Turns are only ever SPENT — counted down — and the
/// type exposes no way to add turns or reset the cap, so by construction a run
/// can finish early but can never widen its own budget. Tick consumption is the
/// same: a separate down-only counter that force-stops a run which is somehow
/// looping without spending turns (e.g. a scheduler bug that never finds a
/// runnable step).
#[derive(Debug, Clone)]
pub struct RunBudget {
    turns_remaining: u32,
    ticks_remaining: u64,
    caps: OrchestratorCaps,
}

impl RunBudget {
    /// Start a fresh budget from already-clamped caps.
    pub fn new(caps: OrchestratorCaps) -> Self {
        let caps = caps.clamp_to_ceiling();
        Self {
            turns_remaining: caps.max_total_turns,
            ticks_remaining: caps.max_ticks,
            caps,
        }
    }

    /// Spend exactly one model turn. Charge this BEFORE a step is dispatched, so
    /// an in-flight turn is always already paid for and a stuck VM cannot run
    /// for free. Returns the turns left on success, or [`BudgetExhausted::Turns`]
    /// if there was nothing left to spend (the caller must NOT dispatch). This
    /// is the only mutator of the turn count and it only ever decreases it.
    pub fn spend_turn(&mut self) -> Result<u32, BudgetExhausted> {
        if self.turns_remaining == 0 {
            return Err(BudgetExhausted::Turns);
        }
        self.turns_remaining -= 1;
        Ok(self.turns_remaining)
    }

    /// Consume one scheduler tick. Call once per loop iteration, FIRST, before
    /// any other stop check, so a run that never makes progress still dies at
    /// the tick ceiling. Only ever decreases the tick count.
    pub fn tick(&mut self) -> Result<u64, BudgetExhausted> {
        if self.ticks_remaining == 0 {
            return Err(BudgetExhausted::Ticks);
        }
        self.ticks_remaining -= 1;
        Ok(self.ticks_remaining)
    }

    pub fn turns_remaining(&self) -> u32 {
        self.turns_remaining
    }

    pub fn ticks_remaining(&self) -> u64 {
        self.ticks_remaining
    }

    pub fn caps(&self) -> &OrchestratorCaps {
        &self.caps
    }

    /// Admission check, run BEFORE any step of a freshly-decomposed plan starts:
    /// does the plan have a legal size and could its worst case finish within
    /// the turns left? A plan that fails this must be bounced back to the
    /// coordinator to simplify (or failed), never started — that is what keeps
    /// "ran out of budget mid-plan" from being the normal ending.
    pub fn admits_plan(&self, step_count: u32) -> bool {
        step_count > 0
            && step_count <= self.caps.max_steps
            && self.caps.worst_case_turns(step_count) <= self.turns_remaining
    }
}

/// A bounded count of currently-held slots — steps in flight, or worker VMs
/// alive. Unlike the turn budget this is REVERSIBLE: a finished step or a torn-
/// down VM frees its slot with [`SlotCounter::release`]. But the count can never
/// exceed `limit`, so peak concurrent load (and therefore peak burn rate and VM
/// memory) is bounded. [`SlotCounter::try_acquire`] returns `false` when full;
/// the caller must `release` exactly once per successful acquire.
#[derive(Debug, Clone)]
pub struct SlotCounter {
    held: u32,
    limit: u32,
}

impl SlotCounter {
    pub fn new(limit: u32) -> Self {
        Self { held: 0, limit }
    }

    /// Take a slot if one is free. Returns `false` (acquire nothing) when at the
    /// limit — the caller must then wait, not proceed.
    pub fn try_acquire(&mut self) -> bool {
        if self.held >= self.limit {
            return false;
        }
        self.held += 1;
        true
    }

    /// Free a previously-acquired slot. Saturating, so an extra release can
    /// never underflow the count below zero.
    pub fn release(&mut self) {
        self.held = self.held.saturating_sub(1);
    }

    pub fn held(&self) -> u32 {
        self.held
    }

    /// How many more slots could be acquired right now.
    pub fn available(&self) -> u32 {
        self.limit.saturating_sub(self.held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_within_the_compiled_ceilings() {
        let caps = OrchestratorCaps::default();
        let clamped = caps.clone().clamp_to_ceiling();
        // Defaults are already legal, so clamping is a no-op.
        assert_eq!(caps, clamped, "defaults must already be within ceilings");
    }

    #[test]
    fn clamp_only_ever_lowers_never_raises() {
        // A config asking for the moon is clamped DOWN to the ceiling...
        let greedy = OrchestratorCaps {
            max_total_turns: u32::MAX,
            max_steps: u32::MAX,
            max_wall_seconds: u64::MAX,
            max_ticks: u64::MAX,
            max_depth: u32::MAX,
            max_parallel: u32::MAX,
            max_concurrent_vms: u32::MAX,
            max_review_cycles: u32::MAX,
            max_replans: u32::MAX,
        }
        .clamp_to_ceiling();
        assert_eq!(greedy.max_total_turns, ceiling::MAX_TOTAL_TURNS);
        assert_eq!(greedy.max_concurrent_vms, ceiling::MAX_CONCURRENT_VMS);
        assert_eq!(greedy.max_depth, ceiling::MAX_DEPTH);

        // ...while a modest config is left untouched (clamp never raises it).
        let modest = OrchestratorCaps {
            max_total_turns: 5,
            max_concurrent_vms: 1,
            ..OrchestratorCaps::default()
        }
        .clamp_to_ceiling();
        assert_eq!(modest.max_total_turns, 5);
        assert_eq!(modest.max_concurrent_vms, 1);
    }

    #[test]
    fn overrides_can_only_tighten() {
        let caps = OrchestratorCaps::default(); // 40 turns, 4 VMs
        // A request to RAISE turns to 1000 is ignored (min wins); a request to
        // LOWER VMs to 1 is applied.
        let tightened = caps.tighten_with(&CapsOverride {
            max_total_turns: Some(1000),
            max_concurrent_vms: Some(1),
            ..CapsOverride::default()
        });
        assert_eq!(tightened.max_total_turns, 40, "override cannot raise the cap");
        assert_eq!(tightened.max_concurrent_vms, 1, "override can lower the cap");
    }

    #[test]
    fn turn_budget_counts_down_and_then_refuses() {
        let mut budget = RunBudget::new(OrchestratorCaps {
            max_total_turns: 2,
            ..OrchestratorCaps::default()
        });
        assert_eq!(budget.spend_turn(), Ok(1));
        assert_eq!(budget.spend_turn(), Ok(0));
        // Exhausted: every further spend is refused, and stays refused. There is
        // no API that could bring the count back up.
        assert_eq!(budget.spend_turn(), Err(BudgetExhausted::Turns));
        assert_eq!(budget.spend_turn(), Err(BudgetExhausted::Turns));
        assert_eq!(budget.turns_remaining(), 0);
    }

    #[test]
    fn tick_ceiling_fires_even_without_spending_turns() {
        // A run that loops without ever finding a step to send (so it never
        // spends a turn) must still die — at the tick ceiling.
        let mut budget = RunBudget::new(OrchestratorCaps {
            max_ticks: 3,
            ..OrchestratorCaps::default()
        });
        assert!(budget.tick().is_ok());
        assert!(budget.tick().is_ok());
        assert!(budget.tick().is_ok());
        assert_eq!(budget.tick(), Err(BudgetExhausted::Ticks));
        // Turns were never touched.
        assert_eq!(budget.turns_remaining(), budget.caps().max_total_turns);
    }

    #[test]
    fn admission_rejects_a_plan_that_cannot_finish_in_budget() {
        // Default caps: 40 turns, 6 steps, 2 review cycles, 1 replan.
        // worst_case = steps * (1+2) * (1+1) + 2 = steps*6 + 2.
        let budget = RunBudget::new(OrchestratorCaps::default());
        // The default budget admits a FULL max_steps plan: 6 steps -> 38 worst
        // case, fits in 40. (The turn budget no longer contradicts the step cap.)
        assert!(budget.admits_plan(3)); // 20
        assert!(budget.admits_plan(6)); // 38
        // Over the step cap is rejected regardless of turn math.
        assert!(!budget.admits_plan(7));
        // Empty plans are rejected too.
        assert!(!budget.admits_plan(0));

        // A TIGHTENED budget still rejects a plan that cannot finish: with only
        // 20 turns, a 4-step plan (26 worst case) is refused before any step runs.
        let tight = RunBudget::new(OrchestratorCaps {
            max_total_turns: 20,
            ..OrchestratorCaps::default()
        });
        assert!(tight.admits_plan(3)); // 20 <= 20
        assert!(!tight.admits_plan(4)); // 26 > 20, within step cap -> budget-rejected
    }

    #[test]
    fn slot_counter_is_bounded_and_reversible() {
        let mut slots = SlotCounter::new(2);
        assert!(slots.try_acquire());
        assert!(slots.try_acquire());
        // Full: no more slots until something is released.
        assert!(!slots.try_acquire());
        assert_eq!(slots.held(), 2);
        assert_eq!(slots.available(), 0);
        slots.release();
        assert_eq!(slots.available(), 1);
        assert!(slots.try_acquire());
        // Over-release can never underflow.
        slots.release();
        slots.release();
        slots.release();
        assert_eq!(slots.held(), 0);
    }
}
