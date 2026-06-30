use serde::{Deserialize, Serialize};

use maturana_core::{
    orchestrator_budget::OrchestratorCaps,
    roles::{marker, RoleRegistry},
};

/// One step's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Not yet runnable, or runnable but not yet sent.
    Waiting,
    /// Sent to a worker, awaiting the reply.
    Running,
    /// Completed with a result.
    Done,
    /// Failed after exhausting retries.
    Failed,
}

fn default_status() -> StepStatus {
    StepStatus::Waiting
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Stable id within the plan, e.g. "s1".
    pub id: String,
    /// Which role does this step (must exist in the role registry).
    pub role: String,
    /// What to do, in plain English.
    pub task: String,
    /// Ids of steps whose results this step needs first.
    #[serde(default)]
    pub deps: Vec<String>,
    /// Whether this step's result must pass a reviewer before it counts as done.
    #[serde(default)]
    pub review: bool,
    #[serde(default = "default_status")]
    pub status: StepStatus,
    /// The worker's result, once Done.
    #[serde(default)]
    pub result: Option<String>,
    /// How many times this step has been sent to a worker.
    #[serde(default)]
    pub attempts: u32,
    /// How many revise rounds the reviewer has asked for.
    #[serde(default)]
    pub review_cycles: u32,
}

/// A plan: the goal plus its steps. Persisted to the run directory and used as
/// the live task board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub goal: String,
    pub steps: Vec<Step>,
}

/// What the coordinator returns (no statuses yet; the host fills those in).
#[derive(Debug, Clone, Deserialize)]
struct PlanSpec {
    steps: Vec<PlanStepSpec>,
}

#[derive(Debug, Clone, Deserialize)]
struct PlanStepSpec {
    id: String,
    role: String,
    task: String,
    #[serde(default)]
    deps: Vec<String>,
    #[serde(default)]
    review: bool,
}

impl Plan {
    fn from_spec(goal: &str, spec: PlanSpec) -> Self {
        let steps = spec
            .steps
            .into_iter()
            .map(|s| Step {
                id: s.id,
                role: s.role,
                task: s.task,
                deps: s.deps,
                review: s.review,
                status: StepStatus::Waiting,
                result: None,
                attempts: 0,
                review_cycles: 0,
            })
            .collect();
        Self {
            goal: goal.to_string(),
            steps,
        }
    }

    fn status_of(&self, id: &str) -> Option<StepStatus> {
        self.steps.iter().find(|s| s.id == id).map(|s| s.status)
    }

    /// Steps that are waiting and whose every dependency is already Done.
    pub fn ready_steps(&self) -> Vec<&Step> {
        self.steps
            .iter()
            .filter(|s| s.status == StepStatus::Waiting)
            .filter(|s| {
                s.deps
                    .iter()
                    .all(|d| self.status_of(d) == Some(StepStatus::Done))
            })
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.steps.iter().all(|s| s.status == StepStatus::Done)
    }

    pub fn has_failure(&self) -> bool {
        self.steps.iter().any(|s| s.status == StepStatus::Failed)
    }

    pub fn step_mut(&mut self, id: &str) -> Option<&mut Step> {
        self.steps.iter_mut().find(|s| s.id == id)
    }

    /// The concatenated results of a step's dependencies, to hand the worker as
    /// context. Empty when the step has no dependencies.
    pub fn dependency_context(&self, step: &Step) -> String {
        let mut out = String::new();
        for dep_id in &step.deps {
            if let Some(dep) = self.steps.iter().find(|s| &s.id == dep_id) {
                if let Some(result) = &dep.result {
                    out.push_str(&format!(
                        "\n### Result of {} ({})\n{}\n",
                        dep.id, dep.role, result
                    ));
                }
            }
        }
        out
    }

    /// Reject a plan that references unknown roles or dependencies, or that
    /// contains a dependency cycle, before any step runs.
    pub fn validate(&self, registry: &RoleRegistry) -> Result<(), String> {
        if self.steps.is_empty() {
            return Err("plan has no steps".to_string());
        }
        let ids: std::collections::HashSet<&str> =
            self.steps.iter().map(|s| s.id.as_str()).collect();
        if ids.len() != self.steps.len() {
            return Err("duplicate step ids".to_string());
        }
        for step in &self.steps {
            if registry.get(&step.role).is_none() {
                return Err(format!(
                    "step {} uses unknown role '{}'",
                    step.id, step.role
                ));
            }
            for dep in &step.deps {
                if dep == &step.id {
                    return Err(format!("step {} depends on itself", step.id));
                }
                if !ids.contains(dep.as_str()) {
                    return Err(format!(
                        "step {} depends on unknown step '{}'",
                        step.id, dep
                    ));
                }
            }
        }
        self.check_acyclic()
    }

    fn check_acyclic(&self) -> Result<(), String> {
        use std::collections::HashMap;
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Visiting,
            Done,
        }
        let mut state: HashMap<&str, Mark> = HashMap::new();
        for start in self.steps.iter() {
            if state.get(start.id.as_str()).is_some() {
                continue;
            }
            let mut stack: Vec<(&str, usize)> = vec![(start.id.as_str(), 0)];
            state.insert(start.id.as_str(), Mark::Visiting);
            while let Some((id, idx)) = stack.last().copied() {
                let step = self.steps.iter().find(|s| s.id == id).unwrap();
                if idx < step.deps.len() {
                    *stack.last_mut().unwrap() = (id, idx + 1);
                    let dep = step.deps[idx].as_str();
                    match state.get(dep) {
                        Some(Mark::Visiting) => {
                            return Err(format!("dependency cycle through step '{dep}'"));
                        }
                        Some(Mark::Done) => {}
                        None => {
                            state.insert(dep, Mark::Visiting);
                            stack.push((dep, 0));
                        }
                    }
                } else {
                    state.insert(id, Mark::Done);
                    stack.pop();
                }
            }
        }
        Ok(())
    }
}

pub fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in text[start..].char_indices() {
        match ch {
            '"' if !escaped => in_string = !in_string,
            '\\' if in_string => {
                escaped = !escaped;
                continue;
            }
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + i + 1]);
                }
            }
            _ => {}
        }
        escaped = false;
    }
    None
}

/// Parse a coordinator reply into a validated plan.
pub fn parse_plan(goal: &str, reply: &str, registry: &RoleRegistry) -> Result<Plan, String> {
    let json = extract_json_object(reply).ok_or("coordinator reply had no JSON plan")?;
    let spec: PlanSpec =
        serde_json::from_str(json).map_err(|e| format!("plan JSON invalid: {e}"))?;
    let plan = Plan::from_spec(goal, spec);
    plan.validate(registry)?;
    Ok(plan)
}

/// The framing sent to the coordinator to get a machine-readable plan.
pub fn coordinator_task(goal: &str, registry: &RoleRegistry, caps: &OrchestratorCaps) -> String {
    format!(
        "Goal:\n{goal}\n\n\
         Break this into at most {} steps. Available worker roles: {}.\n\
         Reply with ONLY a JSON object of this exact shape (no prose):\n\
         {{\"steps\":[{{\"id\":\"s1\",\"role\":\"researcher\",\"task\":\"...\",\"deps\":[],\"review\":false}}]}}\n\
         - `id` is a short unique string. `deps` lists ids whose results this step needs first.\n\
         - Set `review` true for steps whose output should be checked before it counts as done.\n\
         - Keep the plan as small as will satisfy the goal.",
        caps.max_steps,
        registry
            .names()
            .into_iter()
            .filter(|n| n != "coordinator" && n != "synthesizer")
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// A stricter re-prompt after an unusable coordinator reply.
pub fn coordinator_retry_task(
    goal: &str,
    registry: &RoleRegistry,
    caps: &OrchestratorCaps,
) -> String {
    format!(
        "{}\n\nIMPORTANT: your previous reply could not be parsed. Reply with ONLY \
         the JSON object and nothing else — no prose, no markdown fences, no code \
         block. Your reply must start with {{ and end with }}.",
        coordinator_task(goal, registry, caps)
    )
}

/// A short, single-line preview of a model reply for surfacing in errors.
pub fn reply_preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "(empty reply — the coordinator agent likely timed out or returned nothing)".to_string()
    } else {
        trimmed
            .chars()
            .take(240)
            .collect::<String>()
            .replace('\n', " ")
    }
}

/// Read a reviewer reply's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    Approve,
    Revise(String),
    /// No recognizable verdict marker; counts as one failed review cycle.
    Unclear,
}

pub fn parse_review(reply: &str) -> ReviewVerdict {
    if reply.contains(marker::REVIEW_APPROVE) {
        ReviewVerdict::Approve
    } else if let Some(idx) = reply.find(marker::REVIEW_REVISE) {
        let feedback = reply[idx + marker::REVIEW_REVISE.len()..]
            .trim()
            .to_string();
        ReviewVerdict::Revise(feedback)
    } else {
        ReviewVerdict::Unclear
    }
}

pub fn build_step_task(registry: &RoleRegistry, plan: &Plan, step: &Step) -> String {
    let base = registry
        .frame_task(&step.role, &step.task)
        .unwrap_or_else(|| step.task.clone());
    let ctx = plan.dependency_context(step);
    if ctx.trim().is_empty() {
        base
    } else {
        format!("{base}\n\n--- INPUTS FROM EARLIER STEPS ---{ctx}")
    }
}

/// A compact, chat-friendly rendering of a plan, one line per step.
pub fn plan_chat_summary(plan: &Plan) -> String {
    plan.steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let task: String = step.task.chars().take(80).collect();
            format!("{}. [{}] {}", i + 1, step.role, task.trim())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> RoleRegistry {
        RoleRegistry::defaults("worker-base")
    }

    fn step(id: &str, deps: &[&str], status: StepStatus) -> Step {
        Step {
            id: id.to_string(),
            role: "researcher".to_string(),
            task: "t".to_string(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
            review: false,
            status,
            result: None,
            attempts: 0,
            review_cycles: 0,
        }
    }

    #[test]
    fn ready_steps_respects_dependencies() {
        let plan = Plan {
            goal: "g".to_string(),
            steps: vec![
                step("s1", &[], StepStatus::Done),
                step("s2", &["s1"], StepStatus::Waiting),
                step("s3", &["s2"], StepStatus::Waiting),
            ],
        };
        assert_eq!(plan.ready_steps()[0].id, "s2");
    }

    #[test]
    fn validate_rejects_cycles_unknown_roles_and_bad_deps() {
        let reg = registry();
        let cyclic = Plan {
            goal: "g".into(),
            steps: vec![
                step("a", &["b"], StepStatus::Waiting),
                step("b", &["a"], StepStatus::Waiting),
            ],
        };
        assert!(cyclic.validate(&reg).unwrap_err().contains("cycle"));

        let mut bad_role = Plan {
            goal: "g".into(),
            steps: vec![step("s1", &[], StepStatus::Waiting)],
        };
        bad_role.steps[0].role = "nope".into();
        assert!(bad_role
            .validate(&reg)
            .unwrap_err()
            .contains("unknown role"));

        let dangling = Plan {
            goal: "g".into(),
            steps: vec![step("s1", &["ghost"], StepStatus::Waiting)],
        };
        assert!(dangling
            .validate(&reg)
            .unwrap_err()
            .contains("unknown step"));

        let ok = Plan {
            goal: "g".into(),
            steps: vec![
                step("s1", &[], StepStatus::Waiting),
                step("s2", &["s1"], StepStatus::Waiting),
            ],
        };
        ok.validate(&reg).unwrap();
    }

    #[test]
    fn parse_plan_extracts_json_from_prose() {
        let reg = registry();
        let reply = "sure:\n```json\n{\"steps\":[{\"id\":\"s1\",\"role\":\"researcher\",\"task\":\"look\",\"deps\":[],\"review\":true}]}\n```";
        let plan = parse_plan("the goal", reply, &reg).expect("should parse");
        assert_eq!(plan.goal, "the goal");
        assert!(plan.steps[0].review);
        assert_eq!(plan.steps[0].status, StepStatus::Waiting);
    }

    #[test]
    fn dependency_context_includes_upstream_results() {
        let mut plan = Plan {
            goal: "g".to_string(),
            steps: vec![
                step("s1", &[], StepStatus::Done),
                step("s2", &["s1"], StepStatus::Waiting),
            ],
        };
        plan.steps[0].result = Some("the answer is 42".to_string());
        let ctx = plan.dependency_context(&plan.steps[1].clone());
        assert!(ctx.contains("the answer is 42"));
        assert!(ctx.contains("s1"));
    }

    #[test]
    fn review_verdict_is_read_from_markers() {
        assert!(matches!(
            parse_review("looks good [[REVIEW: APPROVE]]"),
            ReviewVerdict::Approve
        ));
        match parse_review("[[REVIEW: REVISE]] fix the title") {
            ReviewVerdict::Revise(fb) => assert_eq!(fb, "fix the title"),
            _ => panic!("expected revise"),
        }
        assert!(matches!(
            parse_review("no marker here"),
            ReviewVerdict::Unclear
        ));
    }

    #[test]
    fn plan_chat_summary_is_compact_and_role_tagged() {
        let mut s1 = step("s1", &[], StepStatus::Waiting);
        s1.role = "developer".to_string();
        s1.task = "build the thing with a title that is intentionally long enough to be truncated before it floods the chat".to_string();
        let plan = Plan {
            goal: "g".to_string(),
            steps: vec![s1],
        };

        let summary = plan_chat_summary(&plan);

        assert!(summary.starts_with("1. [developer] build the thing"));
        assert!(summary.len() < 110, "summary was too long: {summary}");
    }
}
