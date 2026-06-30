use maturana_core::roles::marker;

use crate::planner::Plan;

/// The outcome of running produced files before calling an orchestration run done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// An agent ran the deliverable and it works.
    Passed,
    /// It still failed after the bounded repair attempts, with the last reason.
    Failed(String),
    /// Verification could not be completed, e.g. no builder or no verdict.
    Inconclusive(String),
    /// Verification was explicitly skipped or there was nothing runnable.
    Skipped,
}

impl VerifyOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            VerifyOutcome::Passed => "verified: runs",
            VerifyOutcome::Failed(_) => "NOT verified",
            VerifyOutcome::Inconclusive(_) => "unverified",
            VerifyOutcome::Skipped => "verification skipped",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            VerifyOutcome::Passed => {
                "verification: an agent ran the deliverable and confirmed it works.".to_string()
            }
            VerifyOutcome::Failed(why) => {
                format!(
                    "verification: FAILED after repair attempts — {}",
                    why.trim()
                )
            }
            VerifyOutcome::Inconclusive(why) => {
                format!("verification: inconclusive — {}", why.trim())
            }
            VerifyOutcome::Skipped => "verification: skipped.".to_string(),
        }
    }
}

/// A short human summary placed beside real files: goal, files, verification
/// verdict, and each step's own brief report. It is not a rewrite of the files.
pub fn build_run_summary(
    goal: &str,
    plan: &Plan,
    files: &[String],
    verdict: &VerifyOutcome,
) -> String {
    let mut out = format!("# {goal}\n\n## Verification\n{}\n", verdict.detail().trim());
    out.push_str("\n## Files produced\n");
    if files.is_empty() {
        out.push_str("- (none)\n");
    }
    for file in files {
        out.push_str(&format!("- {file}\n"));
    }
    out.push_str("\n## How it was built\n");
    for step in &plan.steps {
        if let Some(result) = &step.result {
            out.push_str(&format!(
                "\n### {} ({})\n{}\n",
                step.id,
                step.role,
                result.trim()
            ));
        }
    }
    out
}

/// The verifier's task: exercise whatever was built, and fix it in place if it
/// does not work. Deliberately type-agnostic; the agent decides how to run it.
pub fn verify_task(goal: &str, out_remote: &str) -> String {
    format!(
        "You are the VERIFIER. The files produced for this goal are in {out_remote} in your \
         workspace.\n\nGoal: {goal}\n\n\
         ACTUALLY EXERCISE the deliverable the way a user would: run the program, execute the \
         script, open the page in a headless browser, call the endpoint — whatever fits what was \
         built. Decide whether it genuinely works and meets the goal.\n\
         - If it works, reply with {pass} on its own line plus ONE line on what you checked.\n\
         - If it does NOT work, FIX the files in place inside {out_remote}, then reply with {fail} \
         on its own line followed by what was broken and what you changed.\n\
         Keep your reply brief; do not paste full file contents.",
        pass = marker::VERIFY_PASS,
        fail = marker::VERIFY_FAIL,
    )
}

/// `Some(true)` on PASS, `Some(false)` on FAIL, `None` if neither marker is present.
pub fn parse_verify(reply: &str) -> Option<bool> {
    if reply.contains(marker::VERIFY_PASS) {
        Some(true)
    } else if reply.contains(marker::VERIFY_FAIL) {
        Some(false)
    } else {
        None
    }
}

/// The text after the FAIL marker, trimmed, flattened, and capped for logs.
pub fn verify_detail(reply: &str) -> String {
    let tail = reply.split(marker::VERIFY_FAIL).nth(1).unwrap_or("").trim();
    let one_line = tail.replace('\n', " ");
    if one_line.is_empty() {
        "no detail given".to_string()
    } else if one_line.chars().count() > 200 {
        format!("{}…", one_line.chars().take(200).collect::<String>())
    } else {
        one_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{Step, StepStatus};

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
    fn run_summary_lists_files_and_step_reports() {
        let mut dev = step("s1", &[], StepStatus::Done);
        dev.role = "developer".to_string();
        dev.result = Some("Wrote index.html with the board and win logic.".to_string());
        let plan = Plan {
            goal: "build a game".to_string(),
            steps: vec![dev],
        };
        let md = build_run_summary(
            "build a game",
            &plan,
            &["index.html".to_string()],
            &VerifyOutcome::Passed,
        );
        assert!(md.contains("## Files produced"));
        assert!(md.contains("- index.html"));
        assert!(md.contains("Wrote index.html"));
        assert!(md.contains("## Verification"));
        assert!(md.contains("confirmed it works"));
    }

    #[test]
    fn parse_verify_reads_the_markers() {
        assert_eq!(
            parse_verify("all good\n[[VERIFY: PASS]]\nchecked the board"),
            Some(true)
        );
        assert_eq!(
            parse_verify("[[VERIFY: FAIL]] the reset button throws"),
            Some(false)
        );
        assert_eq!(parse_verify("I think it's probably fine"), None);
    }

    #[test]
    fn verify_detail_extracts_the_failure_reason() {
        let reply = "[[VERIFY: FAIL]] script.js referenced a missing id; added it.";
        assert_eq!(
            verify_detail(reply),
            "script.js referenced a missing id; added it."
        );
        assert_eq!(verify_detail("[[VERIFY: FAIL]]"), "no detail given");
    }

    #[test]
    fn verify_task_names_the_dir_goal_and_markers() {
        let task = verify_task("build a game", "/workspace/maturana-out-run-1");
        assert!(task.contains("/workspace/maturana-out-run-1"));
        assert!(task.contains("build a game"));
        assert!(task.contains(marker::VERIFY_PASS));
        assert!(task.contains(marker::VERIFY_FAIL));
        assert!(task.contains("EXERCISE"));
        assert!(task.contains("FIX the files in place"));
    }
}
