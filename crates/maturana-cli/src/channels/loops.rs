use maturana_core::state::MaturanaHome;
use maturana_ops::orchestration::{self, LoopRunSummary, LoopStartRequest};

use super::audit_channel_event;

/// `/loop` — start a multi-agent orchestration loop on a goal, or manage one
/// (`status` / `abort`). Available on every text channel through the shared
/// command handler.
pub(super) fn handle_loop_command(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    channel: &str,
    platform_id: &str,
    args: &str,
) -> String {
    let args = args.trim();
    let (sub, rest) = match args.split_once(char::is_whitespace) {
        Some((a, b)) => (a.to_ascii_lowercase(), b.trim()),
        None => (args.to_ascii_lowercase(), ""),
    };
    match sub.as_str() {
        "" => loop_usage_text(),
        "abort" => abort_loop(home, rest),
        "status" => {
            if rest.is_empty() {
                loop_list_text(home)
            } else if !orchestration::valid_run_id(rest) {
                "Invalid run id.".to_string()
            } else {
                loop_status_text(home, rest)
            }
        }
        "fast" => start_loop(
            home,
            agent_id,
            session_id,
            chat_id,
            channel,
            platform_id,
            rest,
            true,
        ),
        _ => start_loop(
            home,
            agent_id,
            session_id,
            chat_id,
            channel,
            platform_id,
            args,
            false,
        ),
    }
}

fn abort_loop(home: &MaturanaHome, run_id: &str) -> String {
    if run_id.is_empty() {
        return "Usage: /loop abort <run_id>".to_string();
    }
    if !orchestration::valid_run_id(run_id) {
        return "Invalid run id.".to_string();
    }
    match orchestration::describe_loop(home, run_id) {
        Ok(None) => format!("No loop `{run_id}` found."),
        Ok(Some(_)) => match orchestration::request_abort(home, run_id) {
            Ok(()) => format!(
                "🛑 Abort requested for `{run_id}` — the in-flight step finishes its lease, then it stops."
            ),
            Err(error) => format!("Couldn't abort `{run_id}`: {error}"),
        },
        Err(error) => format!("Couldn't abort `{run_id}`: {error}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn start_loop(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    channel: &str,
    platform_id: &str,
    goal: &str,
    no_verify: bool,
) -> String {
    let goal = goal.trim();
    if goal.is_empty() {
        return loop_usage_text();
    }
    let request = LoopStartRequest {
        goal,
        chat_id,
        channel,
        platform_id,
        agent_id,
        session_id,
        no_verify,
    };
    match orchestration::start_detached_loop(home, &request) {
        Ok(run_id) => {
            let _ = audit_channel_event(
                home,
                agent_id,
                "channel.loop.start",
                &format!("{run_id}{}: {goal}", if no_verify { " (fast)" } else { "" }),
            );
            let mode = if no_verify {
                " (fast — skips the run-it-and-verify pass)"
            } else {
                ""
            };
            format!(
                "🔄 Loop `{run_id}` started{mode} on:\n{goal}\n\nSeveral agents will plan it, do the parts, check the result, and combine them — I'll post the plan and each step here, then the result (files attached when produced).\nManage: /loop status {run_id} · /loop abort {run_id}"
            )
        }
        Err(error) => format!("Couldn't start the loop: {error:#}"),
    }
}

fn loop_usage_text() -> String {
    "🔄 /loop runs a multi-agent loop on a goal: several agents plan it, do the parts, \
     check the result, and combine them — progress posts here.\n\n\
     • /loop <goal> — start (e.g. /loop build a tic-tac-toe game playable in the browser)\n\
     • /loop fast <goal> — start without the run-it-and-verify pass (~half the time)\n\
     • /loop status [run_id] — list loops, or show one run's steps\n\
     • /loop abort <run_id> — stop a run"
        .to_string()
}

fn loop_status_text(home: &MaturanaHome, run_id: &str) -> String {
    match orchestration::describe_loop(home, run_id) {
        Ok(Some(summary)) => format_loop_summary(&summary),
        Ok(None) => format!("No loop `{run_id}` found."),
        Err(error) => format!("Could not read loop `{run_id}`: {error:#}"),
    }
}

fn format_loop_summary(summary: &LoopRunSummary) -> String {
    match summary.goal.as_deref().filter(|goal| !goal.is_empty()) {
        Some(goal) => format!("Loop `{}`: {}\nGoal: {goal}", summary.run_id, summary.state),
        None => format!("Loop `{}`: {}", summary.run_id, summary.state),
    }
}

fn loop_list_text(home: &MaturanaHome) -> String {
    let runs = match orchestration::list_loops(home) {
        Ok(runs) => runs,
        Err(error) => return format!("Could not list loops: {error:#}"),
    };
    if runs.is_empty() {
        "No loops yet. Start one with /loop <goal>.".to_string()
    } else {
        format!(
            "Loops:\n{}",
            runs.into_iter()
                .map(|run| format!("• `{}` — {}", run.run_id, run.state))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}
