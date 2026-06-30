use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context;
use chrono::Utc;
use maturana_core::state::MaturanaHome;
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct LoopStartRequest<'a> {
    pub goal: &'a str,
    pub chat_id: i64,
    pub channel: &'a str,
    pub platform_id: &'a str,
    pub agent_id: &'a str,
    pub session_id: &'a str,
    pub no_verify: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LoopRunSummary {
    pub run_id: String,
    pub state: String,
    pub goal: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RunTally {
    pub total: usize,
    pub done: usize,
    pub running: usize,
    pub failed: usize,
    pub waiting: usize,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OrchestrationRunListItem {
    pub run_id: String,
    pub goal: serde_json::Value,
    pub tally: RunTally,
    pub modified: Option<u64>,
    pub has_output: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OrchestrationRunDetail {
    pub run_id: String,
    pub tally: RunTally,
    pub goal: serde_json::Value,
    pub steps: serde_json::Value,
    pub files: Vec<String>,
}

pub fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 128
        && !run_id.contains("..")
        && run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn run_dir(home: &MaturanaHome, run_id: &str) -> anyhow::Result<PathBuf> {
    if !valid_run_id(run_id) {
        anyhow::bail!("run id must be a simple path segment: {run_id}");
    }
    Ok(home.root().join("orchestration").join(run_id))
}

pub fn abort_marker(home: &MaturanaHome, run_id: &str) -> anyhow::Result<PathBuf> {
    Ok(run_dir(home, run_id)?.join("abort"))
}

pub fn is_aborted(home: &MaturanaHome, run_id: &str) -> anyhow::Result<bool> {
    Ok(abort_marker(home, run_id)?.exists())
}

pub fn save_plan<T: Serialize>(home: &MaturanaHome, run_id: &str, plan: &T) -> anyhow::Result<()> {
    let dir = run_dir(home, run_id)?;
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("plan.json"), serde_json::to_string_pretty(plan)?)?;
    Ok(())
}

pub fn request_abort(home: &MaturanaHome, run_id: &str) -> anyhow::Result<()> {
    let dir = run_dir(home, run_id)?;
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("abort"), "aborted")?;
    Ok(())
}

/// Start a detached `maturana orchestrator loop` run that reports back to the
/// provided chat target. The returned run id is safe to use under `orchestration/`.
pub fn start_detached_loop(
    home: &MaturanaHome,
    request: &LoopStartRequest<'_>,
) -> anyhow::Result<String> {
    let goal = request.goal.trim();
    if goal.is_empty() {
        anyhow::bail!("loop goal is required");
    }
    let run_id = format!(
        "loop-{}-{}",
        request.chat_id.unsigned_abs(),
        Utc::now().timestamp()
    );
    let exe = std::env::current_exe().context("locate the maturana binary")?;
    let dir = run_dir(home, &run_id)?;
    fs::create_dir_all(&dir)?;
    let log = fs::File::create(dir.join("loop.log"))?;
    let mut command = Command::new(exe);
    command
        .arg("--home")
        .arg(home.root())
        .arg("orchestrator")
        .arg("loop")
        .arg(goal)
        .arg("--run-id")
        .arg(&run_id)
        .arg("--chat-channel")
        .arg(request.channel)
        .arg("--chat-platform-id")
        .arg(request.platform_id)
        .arg("--chat-agent")
        .arg(request.agent_id)
        .arg("--chat-session")
        .arg(request.session_id)
        .stdin(Stdio::null())
        .stderr(log.try_clone()?)
        .stdout(log);
    if request.no_verify {
        command.arg("--no-verify");
    }
    let mut child = command.spawn().context("spawn orchestrator loop")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(run_id)
}

pub fn describe_loop(home: &MaturanaHome, run_id: &str) -> anyhow::Result<Option<LoopRunSummary>> {
    let dir = run_dir(home, run_id)?;
    if !dir.exists() {
        return Ok(None);
    }
    Ok(Some(loop_summary(run_id.to_string(), &dir)))
}

pub fn list_loops(home: &MaturanaHome) -> anyhow::Result<Vec<LoopRunSummary>> {
    let dir = home.root().join("orchestration");
    let mut runs = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("loop-") {
                runs.push(loop_summary(name, &entry.path()));
            }
        }
    }
    runs.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    Ok(runs)
}

pub fn list_orchestration_runs(
    home: &MaturanaHome,
) -> anyhow::Result<Vec<OrchestrationRunListItem>> {
    let mut runs = Vec::new();
    let dir = home.root().join("orchestration");
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(plan) = read_plan(&path) else {
                continue;
            };
            let run_id = entry.file_name().to_string_lossy().to_string();
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());
            runs.push(OrchestrationRunListItem {
                run_id,
                goal: plan.get("goal").cloned().unwrap_or(serde_json::Value::Null),
                tally: tally_plan(&plan),
                modified,
                has_output: path.join("output").is_dir() || path.join("answer.md").exists(),
            });
        }
    }
    runs.sort_by(|a, b| b.modified.unwrap_or(0).cmp(&a.modified.unwrap_or(0)));
    Ok(runs)
}

pub fn orchestration_run_detail(
    home: &MaturanaHome,
    run_id: &str,
) -> anyhow::Result<OrchestrationRunDetail> {
    let dir = run_dir(home, run_id)?;
    let plan = read_plan(&dir).ok_or_else(|| anyhow::anyhow!("no such run"))?;
    Ok(OrchestrationRunDetail {
        run_id: run_id.to_string(),
        tally: tally_plan(&plan),
        goal: plan.get("goal").cloned().unwrap_or(serde_json::Value::Null),
        steps: plan
            .get("steps")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
        files: list_run_files(&dir),
    })
}

pub fn orchestration_run_status_lines(
    home: &MaturanaHome,
    run_id: &str,
) -> anyhow::Result<Option<Vec<String>>> {
    let dir = run_dir(home, run_id)?;
    let detail = match orchestration_run_detail(home, run_id) {
        Ok(detail) => detail,
        Err(error) => {
            if !dir.exists() {
                return Ok(None);
            }
            return Err(error);
        }
    };
    let goal = detail
        .goal
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| detail.goal.to_string());
    let mut lines = vec![format!("orchestrator run {run_id} — goal: {goal}")];
    let steps = detail.steps.as_array().cloned().unwrap_or_default();
    if steps.is_empty() {
        lines.push("  [no steps]".to_string());
    }
    for step in steps {
        let id = step
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or("?");
        let role = step
            .get("role")
            .and_then(|value| value.as_str())
            .unwrap_or("?");
        let status = step
            .get("status")
            .and_then(|value| value.as_str())
            .unwrap_or("waiting");
        let deps = step
            .get("deps")
            .and_then(|value| value.as_array())
            .map(|deps| {
                deps.iter()
                    .filter_map(|dep| dep.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        lines.push(format!(
            "  {id:<6} {role:<12} {status}{}",
            if deps.is_empty() {
                String::new()
            } else {
                format!("  (after {})", deps.join(", "))
            }
        ));
    }
    if is_aborted(home, run_id)? {
        lines.push("  [abort requested]".to_string());
    }
    if !detail.files.is_empty() {
        lines.push(format!("  files: {}", detail.files.join(", ")));
    }
    Ok(Some(lines))
}

fn loop_summary(run_id: String, dir: &Path) -> LoopRunSummary {
    let plan = read_plan(dir);
    LoopRunSummary {
        run_id,
        state: loop_state_label(dir, plan.as_ref()),
        goal: plan
            .as_ref()
            .and_then(|value| value.get("goal").and_then(|goal| goal.as_str()))
            .map(str::to_string),
    }
}

pub fn tally_plan(plan: &serde_json::Value) -> RunTally {
    let mut done = 0usize;
    let mut running = 0usize;
    let mut failed = 0usize;
    let mut waiting = 0usize;
    let steps = plan.get("steps").and_then(|steps| steps.as_array());
    let total = steps.map(|steps| steps.len()).unwrap_or(0);
    if let Some(steps) = steps {
        for step in steps {
            match step
                .get("status")
                .and_then(|status| status.as_str())
                .unwrap_or("waiting")
            {
                "done" => done += 1,
                "running" => running += 1,
                "failed" => failed += 1,
                _ => waiting += 1,
            }
        }
    }
    let state = if failed > 0 {
        "failed"
    } else if total > 0 && done == total {
        "done"
    } else if running > 0 {
        "running"
    } else {
        "waiting"
    };
    RunTally {
        total,
        done,
        running,
        failed,
        waiting,
        state: state.to_string(),
    }
}

fn list_run_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for sub in ["output", "staging"] {
        if let Ok(entries) = fs::read_dir(dir.join(sub)) {
            for entry in entries.flatten() {
                if entry.path().is_file() {
                    files.push(format!("{sub}/{}", entry.file_name().to_string_lossy()));
                }
            }
        }
    }
    files.sort();
    files
}

fn read_plan(dir: &Path) -> Option<serde_json::Value> {
    fs::read_to_string(dir.join("plan.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

fn loop_state_label(dir: &Path, plan: Option<&serde_json::Value>) -> String {
    let Some(steps) = plan
        .and_then(|value| value.get("steps"))
        .and_then(|steps| steps.as_array())
    else {
        return "planning…".to_string();
    };
    let status_is = |step: &serde_json::Value, want: &str| {
        step.get("status")
            .and_then(|status| status.as_str())
            .map(|status| status.eq_ignore_ascii_case(want))
            .unwrap_or(false)
    };
    let total = steps.len();
    let done = steps.iter().filter(|step| status_is(step, "done")).count();
    let failed = steps.iter().any(|step| status_is(step, "failed"));
    let state = if dir.join("abort").exists() {
        "aborted"
    } else if failed {
        "failed"
    } else if total > 0 && done == total {
        "complete"
    } else {
        "running"
    };
    format!("{state} — {done}/{total} steps")
}

#[cfg(test)]
mod tests {
    use super::*;
    use maturana_core::state::MaturanaHome;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn valid_run_id_allows_generated_ids() {
        assert!(valid_run_id("run-1782828077"));
        assert!(valid_run_id("board-main-1782828077"));
        assert!(valid_run_id("up-maturana.out.log"));
    }

    #[test]
    fn valid_run_id_blocks_traversal() {
        for bad in ["", "..", "../run", "a/b", "a\\b", "/tmp/run", "..%2f"] {
            assert!(!valid_run_id(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn request_abort_writes_marker_under_home() {
        let root = std::env::temp_dir().join(format!(
            "maturana-ops-orchestration-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = MaturanaHome::new(&root);

        request_abort(&home, "run-1").unwrap();

        let marker = root.join("orchestration").join("run-1").join("abort");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "aborted");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn save_plan_and_abort_status_use_validated_run_dir() {
        let root = temp_root("save-plan");
        let home = MaturanaHome::new(&root);

        save_plan(
            &home,
            "run-1",
            &serde_json::json!({"goal":"ship","steps":[]}),
        )
        .unwrap();

        let plan_path = root.join("orchestration/run-1/plan.json");
        assert!(std::fs::read_to_string(plan_path)
            .unwrap()
            .contains("\"goal\": \"ship\""));
        assert!(!is_aborted(&home, "run-1").unwrap());
        request_abort(&home, "run-1").unwrap();
        assert!(is_aborted(&home, "run-1").unwrap());
        assert!(save_plan(&home, "../escape", &serde_json::json!({})).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn loop_describe_reports_goal_and_state() {
        let root = temp_root("describe");
        let home = MaturanaHome::new(&root);
        let run = root.join("orchestration/loop-1");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(
            run.join("plan.json"),
            r#"{"goal":"ship it","steps":[{"status":"done"},{"status":"running"}]}"#,
        )
        .unwrap();

        let summary = describe_loop(&home, "loop-1").unwrap().unwrap();

        assert_eq!(summary.run_id, "loop-1");
        assert_eq!(summary.goal.as_deref(), Some("ship it"));
        assert_eq!(summary.state, "running — 1/2 steps");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn list_loops_only_lists_loop_runs() {
        let root = temp_root("list");
        let home = MaturanaHome::new(&root);
        std::fs::create_dir_all(root.join("orchestration/loop-b")).unwrap();
        std::fs::create_dir_all(root.join("orchestration/run-a")).unwrap();
        std::fs::create_dir_all(root.join("orchestration/loop-a")).unwrap();

        let runs = list_loops(&home).unwrap();

        assert_eq!(
            runs.into_iter().map(|run| run.run_id).collect::<Vec<_>>(),
            vec!["loop-a".to_string(), "loop-b".to_string()]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn orchestration_run_views_match_web_contract() {
        let root = temp_root("run-views");
        let home = MaturanaHome::new(&root);
        let run_a = root.join("orchestration/run-a");
        let run_b = root.join("orchestration/run-b");
        std::fs::create_dir_all(run_a.join("output")).unwrap();
        std::fs::create_dir_all(run_b.join("staging")).unwrap();
        std::fs::write(run_a.join("output/index.html"), "<h1>ok</h1>").unwrap();
        std::fs::write(
            run_a.join("plan.json"),
            r#"{"goal":"ship files","steps":[{"status":"done"},{"status":"done"}]}"#,
        )
        .unwrap();
        std::fs::write(
            run_b.join("plan.json"),
            r#"{"goal":"repair","steps":[{"status":"running"},{"status":"waiting"},{"status":"failed"}]}"#,
        )
        .unwrap();

        let runs = list_orchestration_runs(&home).unwrap();

        assert_eq!(runs.len(), 2);
        let a = runs.iter().find(|run| run.run_id == "run-a").unwrap();
        assert_eq!(a.goal, serde_json::json!("ship files"));
        assert_eq!(a.tally.state, "done");
        assert_eq!(a.tally.done, 2);
        assert!(a.has_output);

        let detail = orchestration_run_detail(&home, "run-b").unwrap();
        assert_eq!(detail.goal, serde_json::json!("repair"));
        assert_eq!(detail.tally.failed, 1);
        assert_eq!(detail.tally.state, "failed");
        assert_eq!(detail.steps.as_array().unwrap().len(), 3);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn orchestration_status_lines_are_cli_ready() {
        let root = temp_root("status-lines");
        let home = MaturanaHome::new(&root);
        let run = root.join("orchestration/run-1");
        std::fs::create_dir_all(run.join("output")).unwrap();
        std::fs::write(run.join("output/report.md"), "ok").unwrap();
        std::fs::write(run.join("abort"), "aborted").unwrap();
        std::fs::write(
            run.join("plan.json"),
            r#"{"goal":"ship it","steps":[{"id":"s1","role":"developer","status":"done","deps":[]},{"id":"s2","role":"reviewer","status":"waiting","deps":["s1"]}]}"#,
        )
        .unwrap();

        let lines = orchestration_run_status_lines(&home, "run-1")
            .unwrap()
            .unwrap();

        assert_eq!(lines[0], "orchestrator run run-1 — goal: ship it");
        assert!(lines.iter().any(|line| line.contains("s2")));
        assert!(lines.iter().any(|line| line.contains("after s1")));
        assert!(lines.iter().any(|line| line.contains("abort requested")));
        assert!(lines.iter().any(|line| line.contains("output/report.md")));
        assert!(orchestration_run_status_lines(&home, "missing")
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "maturana-ops-orchestration-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
