use anyhow::Context;
use maturana_core::{
    board::{self, Board, Card, CardStatus},
    roles::RoleRegistry,
    state::MaturanaHome,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    path::Path,
    process::{Command, Stdio},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardCardJob {
    Decompose,
    Specify,
}

impl BoardCardJob {
    fn command(self) -> &'static str {
        match self {
            Self::Decompose => "decompose",
            Self::Specify => "specify",
        }
    }

    fn log_suffix(self) -> &'static str {
        self.command()
    }
}

pub fn card_out_dir(run_id: &str, card_id: &str) -> String {
    format!("{}-{}", crate::artifacts::remote_out_dir(run_id), card_id)
}

pub fn build_card_task(
    registry: &RoleRegistry,
    board: &Board,
    card: &Card,
    out_remote: &str,
) -> String {
    let mut body = format!("Task: {}\n", card.title);
    if !card.detail.is_empty() {
        body.push_str(&format!("\n{}\n", card.detail));
    }
    if card.deliver.is_some() {
        body.push_str(
            "\n[Delivery is handled for you: produce the finished result as your reply and the host will deliver it to the requested channel. Do NOT attempt to send it yourself.]\n",
        );
    }
    let ctx = board.dependency_context(card);
    if !ctx.trim().is_empty() {
        body.push_str(&format!("\n--- INPUTS FROM EARLIER CARDS ---{ctx}\n"));
    }
    if !card.comments.is_empty() {
        body.push_str("\n--- NOTES ON THIS CARD ---\n");
        for cm in &card.comments {
            let who = if cm.author.is_empty() {
                "note"
            } else {
                &cm.author
            };
            body.push_str(&format!("[{who}] {}\n", cm.body));
        }
    }
    if !card.attachments.is_empty() {
        body.push_str("\n--- ATTACHMENTS ---\n");
        for path in &card.attachments {
            let name = Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            match fs::metadata(path) {
                Ok(meta) if meta.len() <= 64 * 1024 => match fs::read_to_string(path) {
                    Ok(text) => body.push_str(&format!("\n## {name}\n{text}\n")),
                    Err(_) => body.push_str(&format!("- {name} (binary, at {path})\n")),
                },
                _ => body.push_str(&format!("- {name} (at {path})\n")),
            }
        }
    }
    body.push_str(&format!(
        "\n--- PRODUCING FILES ---\nIf this task creates any files (code, a page, a script, data), \
         WRITE them into the directory {out_remote}/ (create it). Keep your reply a brief summary; \
         do NOT paste full file contents.\n"
    ));
    match card.assignee.as_deref() {
        Some(a) if registry.get(a).is_some() => registry.frame_task(a, &body).unwrap_or(body),
        Some(_) => body,
        None => registry.frame_task("developer", &body).unwrap_or(body),
    }
}

pub fn build_goal_judge_task(title: &str, detail: &str, result: &str) -> String {
    format!(
        "Acceptance check. The goal was:\n\n{title}\n{detail}\n\nThe worker produced this result:\n\n{result}\n\n\
         Reply with EXACTLY `PASS` on the first line if the result fully meets the goal. \
         Otherwise reply `REVISE` on the first line, then say specifically what to fix."
    )
}

pub fn build_decompose_task(title: &str, detail: &str) -> String {
    format!(
        "Break this task into a small list (2-6) of concrete subtasks for a team of agents.\n\nTask:\n{}\n{}\n\n\
         Reply ONLY with JSON: {{\"steps\": [{{\"id\": \"s1\", \"role\": \"developer\", \"task\": \"...\", \"deps\": []}}]}}. \
         role is one of developer|researcher|reviewer|synthesizer; deps reference earlier step ids. No prose.",
        title, detail
    )
}

pub fn apply_decomposition(
    board: &mut Board,
    card_id: &str,
    reply: &str,
    registry: &RoleRegistry,
) -> anyhow::Result<Vec<String>> {
    let title = board
        .card(card_id)
        .ok_or_else(|| anyhow::anyhow!("no card '{card_id}'"))?
        .title
        .clone();
    let plan = crate::planner::parse_plan(&title, reply, registry)
        .map_err(|e| anyhow::anyhow!("decompose failed: {e}\n{reply}"))?;
    if plan.steps.is_empty() {
        anyhow::bail!("decompose produced no subtasks");
    }

    let mut step_to_card: HashMap<String, String> = HashMap::new();
    let mut new_ids = Vec::new();
    for step in &plan.steps {
        let deps: Vec<String> = step
            .deps
            .iter()
            .filter_map(|dep| step_to_card.get(dep).cloned())
            .collect();
        let id = board.add(&step.task, "", Some(step.role.clone()), deps);
        step_to_card.insert(step.id.clone(), id.clone());
        new_ids.push(id);
    }
    if let Some(card) = board.card_mut(card_id) {
        card.status = CardStatus::Done;
        card.result = Some(format!("decomposed into {}", new_ids.join(", ")));
    }
    board.validate().map_err(|e| anyhow::anyhow!(e))?;
    Ok(new_ids)
}

pub fn build_specify_task(title: &str, detail: &str) -> String {
    format!(
        "Rewrite this rough task into a clear, actionable spec.\n\nTask:\n{}\n{}\n\n\
         Reply ONLY with JSON: {{\"title\": \"concise imperative title\", \"detail\": \"what to do, plus acceptance criteria\"}}. No prose.",
        title, detail
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CardSpecification {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub detail: String,
}

pub fn parse_card_specification(reply: &str) -> anyhow::Result<CardSpecification> {
    let json = crate::planner::extract_json_object(reply)
        .ok_or_else(|| anyhow::anyhow!("specify reply had no JSON:\n{reply}"))?;
    serde_json::from_str(json).map_err(|e| anyhow::anyhow!("bad specify JSON: {e}\n{json}"))
}

pub fn apply_card_specification(
    board: &mut Board,
    card_id: &str,
    spec: &CardSpecification,
) -> anyhow::Result<()> {
    let card = board
        .card_mut(card_id)
        .ok_or_else(|| anyhow::anyhow!("no card '{card_id}'"))?;
    let title = spec.title.trim();
    let detail = spec.detail.trim();
    if !title.is_empty() {
        card.title = title.to_string();
    }
    if !detail.is_empty() {
        card.detail = detail.to_string();
    }
    if card.status == CardStatus::Triage {
        card.status = CardStatus::Todo;
    }
    Ok(())
}

pub fn parse_goal_judge_reply(reply: &str) -> (bool, String) {
    let trimmed = reply.trim();
    if trimmed.to_ascii_uppercase().starts_with("PASS") {
        return (true, String::new());
    }
    let feedback = trimmed
        .trim_start_matches(|c: char| c.is_alphabetic())
        .trim_start_matches([':', '-', ' ', '\n'])
        .trim()
        .to_string();
    (
        false,
        if feedback.is_empty() {
            "revise".to_string()
        } else {
            feedback
        },
    )
}

/// A board is running if a card is in flight, or its event stream has not
/// reached a `run_end` marker yet.
pub fn is_board_running(home: &MaturanaHome, board: &Board) -> bool {
    if board
        .cards
        .iter()
        .any(|card| card.status == CardStatus::Doing)
    {
        return true;
    }
    let events = board::read_events(home, &board.name);
    matches!(events.last(), Some(event) if event.kind != "run_end")
}

pub fn launch_board_run(home: &MaturanaHome, name: &str) -> anyhow::Result<()> {
    ensure_safe_id(name, "board name")?;
    let board = Board::load(home, name)?;
    if board.cards.is_empty() {
        anyhow::bail!("board is empty - add cards first");
    }
    if is_board_running(home, &board) {
        anyhow::bail!("board is already running");
    }

    let log_path = Board::dir(home).join(format!("{name}.run.log"));
    spawn_maturana_detached(home.root(), &board_run_args(name), &log_path)
}

pub fn launch_board_card_job(
    home: &MaturanaHome,
    name: &str,
    card_id: &str,
    job: BoardCardJob,
) -> anyhow::Result<()> {
    ensure_safe_id(name, "board name")?;
    ensure_safe_id(card_id, "card id")?;
    let board = Board::load(home, name)?;
    if board.card(card_id).is_none() {
        anyhow::bail!("no such card");
    }

    let log_path = Board::dir(home).join(format!("{name}.{}.log", job.log_suffix()));
    spawn_maturana_detached(
        home.root(),
        &board_card_job_args(job, name, card_id),
        &log_path,
    )
}

fn spawn_maturana_detached(
    home_root: &Path,
    args: &[String],
    log_path: &Path,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    let log = fs::File::create(log_path)
        .with_context(|| format!("failed to create board job log {}", log_path.display()))?;
    let mut command = Command::new(exe);
    command
        .arg("--home")
        .arg(home_root)
        .args(args)
        .stdin(Stdio::null())
        .stderr(log.try_clone()?)
        .stdout(log);
    let mut child = command.spawn().context("failed to launch board job")?;

    // Reap off-thread so the detached CLI child does not become a zombie after
    // the caller returns.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn board_run_args(name: &str) -> Vec<String> {
    vec![
        "board".to_string(),
        "run".to_string(),
        "--board".to_string(),
        name.to_string(),
    ]
}

fn board_card_job_args(job: BoardCardJob, name: &str, card_id: &str) -> Vec<String> {
    vec![
        "board".to_string(),
        job.command().to_string(),
        card_id.to_string(),
        "--board".to_string(),
        name.to_string(),
    ]
}

fn ensure_safe_id(value: &str, label: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 128
        || value
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')))
    {
        anyhow::bail!("invalid {label}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> MaturanaHome {
        let dir = std::env::temp_dir().join(format!(
            "maturana-ops-board-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(Board::dir(&MaturanaHome::new(&dir))).unwrap();
        MaturanaHome::new(dir)
    }

    #[test]
    fn board_args_are_narrow_and_stable() {
        assert_eq!(
            board_run_args("launch-plan"),
            vec!["board", "run", "--board", "launch-plan"]
        );
        assert_eq!(
            board_card_job_args(BoardCardJob::Decompose, "launch-plan", "c1"),
            vec!["board", "decompose", "c1", "--board", "launch-plan"]
        );
        assert_eq!(
            board_card_job_args(BoardCardJob::Specify, "launch-plan", "c2"),
            vec!["board", "specify", "c2", "--board", "launch-plan"]
        );
    }

    #[test]
    fn board_running_uses_cards_and_event_tail() {
        let home = temp_home("running");
        let mut board = Board::new("demo");
        assert!(!is_board_running(&home, &board));

        let id = board.add("work", "", None, vec![]);
        assert_eq!(id, "c1");
        board.cards[0].status = CardStatus::Doing;
        assert!(is_board_running(&home, &board));

        board.cards[0].status = CardStatus::Done;
        board::clear_events(&home, "demo");
        board::log_event(&home, "demo", "run_start", None, "");
        assert!(is_board_running(&home, &board));
        board::log_event(&home, "demo", "run_end", None, "");
        assert!(!is_board_running(&home, &board));

        let _ = fs::remove_dir_all(home.root());
    }

    #[test]
    fn ids_reject_path_escape() {
        assert!(ensure_safe_id("board_1.ok", "board").is_ok());
        assert!(ensure_safe_id("../escape", "board").is_err());
        assert!(ensure_safe_id("has/slash", "board").is_err());
        assert!(ensure_safe_id("", "board").is_err());
    }

    #[test]
    fn card_out_dir_is_per_card() {
        assert_eq!(
            card_out_dir("board-x-1", "c3"),
            "/workspace/maturana-out-board-x-1-c3"
        );
    }

    #[test]
    fn build_card_task_frames_a_role_and_adds_the_file_instruction() {
        let reg = RoleRegistry::reuse_across(&["codex-firecracker".to_string()]);
        let mut board = Board::new("t");
        board.add(
            "Build the page",
            "two players",
            Some("developer".into()),
            vec![],
        );
        let card = board.card("c1").unwrap().clone();
        let task = build_card_task(&reg, &board, &card, "/workspace/out-c1");
        assert!(task.contains("Build the page"));
        assert!(task.contains("/workspace/out-c1"));
        assert!(task.contains("DEVELOPER"));
    }

    #[test]
    fn build_card_task_for_a_concrete_agent_has_no_role_prefix() {
        let reg = RoleRegistry::reuse_across(&["codex-firecracker".to_string()]);
        let mut board = Board::new("t");
        board.add("Do it", "", Some("claude-firecracker".into()), vec![]);
        let card = board.card("c1").unwrap().clone();
        let task = build_card_task(&reg, &board, &card, "/workspace/out");
        assert!(task.starts_with("Task: Do it"));
    }

    #[test]
    fn goal_judge_prompt_and_reply_parsing_are_stable() {
        let prompt = build_goal_judge_task("Ship it", "must work", "it works");
        assert!(prompt.contains("Ship it"));
        assert!(prompt.contains("EXACTLY `PASS`"));

        assert_eq!(
            parse_goal_judge_reply("PASS\nchecked"),
            (true, String::new())
        );
        assert_eq!(
            parse_goal_judge_reply("REVISE: fix the missing button"),
            (false, "fix the missing button".to_string())
        );
        assert_eq!(
            parse_goal_judge_reply("REVISE"),
            (false, "revise".to_string())
        );
    }

    #[test]
    fn decompose_prompt_and_apply_reply_add_child_cards() {
        let reg = RoleRegistry::defaults("worker-base");
        let mut board = Board::new("t");
        let root = board.add("Build feature", "ship it", None, vec![]);
        let prompt = build_decompose_task("Build feature", "ship it");
        assert!(prompt.contains("Break this task"));
        assert!(prompt.contains("Build feature"));

        let reply = r#"{
            "steps": [
                {"id": "s1", "role": "developer", "task": "Implement feature", "deps": []},
                {"id": "s2", "role": "reviewer", "task": "Review feature", "deps": ["s1"]}
            ]
        }"#;
        let ids = apply_decomposition(&mut board, &root, reply, &reg).unwrap();
        assert_eq!(ids, vec!["c2".to_string(), "c3".to_string()]);
        assert_eq!(board.card(&root).unwrap().status, CardStatus::Done);
        assert_eq!(board.card("c2").unwrap().title, "Implement feature");
        assert_eq!(board.card("c3").unwrap().deps, vec!["c2".to_string()]);
    }

    #[test]
    fn specify_prompt_parse_and_apply_reply_update_triage_card() {
        let mut board = Board::new("t");
        let id = board.add("rough", "", None, vec![]);
        board.card_mut(&id).unwrap().status = CardStatus::Triage;

        let prompt = build_specify_task("rough", "");
        assert!(prompt.contains("Rewrite this rough task"));

        let spec = parse_card_specification(
            r#"Sure: {"title": "Implement login", "detail": "Add login with tests."}"#,
        )
        .unwrap();
        apply_card_specification(&mut board, &id, &spec).unwrap();

        let card = board.card(&id).unwrap();
        assert_eq!(card.title, "Implement login");
        assert_eq!(card.detail, "Add login with tests.");
        assert_eq!(card.status, CardStatus::Todo);
    }
}
