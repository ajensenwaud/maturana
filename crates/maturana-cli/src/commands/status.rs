use std::fs;

use clap::Args;
use maturana_core::state::MaturanaHome;

#[derive(Debug, Args)]
pub struct ListCommand {
    /// Emit JSON instead of the table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StatusCommand {
    /// Emit JSON instead of the dashboard.
    #[arg(long)]
    pub json: bool,
}

pub fn run_list(home: &MaturanaHome, command: ListCommand) -> anyhow::Result<()> {
    let rows = maturana_ops::agents::collect_agent_rows(home);
    if command.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "No agents found under {}.\nIf your agents live elsewhere, pass --home (or set \
             MATURANA_HOME); otherwise create one with `maturana agent launch`.",
            home.agents_dir().display()
        );
        return Ok(());
    }
    print_agent_table(&rows, "");
    Ok(())
}

pub fn run_status(home: &MaturanaHome, command: StatusCommand) -> anyhow::Result<()> {
    let rows = maturana_ops::agents::collect_agent_rows(home);
    let up_state = fs::read_to_string(home.root().join("up").join("state.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok());
    let sessiond = maturana_ops::health::http_health("http://127.0.0.1:47834/health");
    let graph = maturana_ops::health::http_health("http://127.0.0.1:47835/health");

    if command.json {
        let out = serde_json::json!({
            "plane": up_state,
            "sessiond_ok": sessiond.ok,
            "graph_ok": graph.ok,
            "agents": rows,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("PLANE");
    match &up_state {
        Some(state) => {
            let pid = state.get("pid").and_then(|v| v.as_u64());
            println!(
                "  supervisor                          running{}",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            );
            println!(
                "  sessiond :47834                     {}",
                if sessiond.ok { "ok" } else { "DOWN" }
            );
            println!(
                "  graph    :47835                     {}",
                if graph.ok { "ok" } else { "not running" }
            );
            if let Some(procs) = state.get("processes").and_then(|v| v.as_array()) {
                for p in procs {
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    if name == "sessiond" || name == "graph" {
                        continue;
                    }
                    let restarts = p.get("restarts").and_then(|v| v.as_u64()).unwrap_or(0);
                    let up = p
                        .get("uptime_seconds")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    println!(
                        "  {name:<34}  running  restarts={restarts}  up={}",
                        humanize_uptime(up)
                    );
                }
            }
        }
        None => {
            println!(
                "  plane NOT running - start it with `maturana up` (or `maturana service install up`)."
            );
        }
    }
    println!();
    println!("AGENTS");
    if rows.is_empty() {
        println!("  (none under {})", home.agents_dir().display());
    } else {
        print_agent_table(&rows, "  ");
    }
    Ok(())
}

fn print_agent_table(rows: &[maturana_ops::agents::AgentRow], indent: &str) {
    let headers = ["AGENT", "HARNESS", "VM", "QUEUE", "LAST TURN"];
    let mut w: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for r in rows {
        for (i, cell) in [&r.agent, &r.harness, &r.vm, &r.queue].iter().enumerate() {
            w[i] = w[i].max(cell.chars().count());
        }
    }
    let line = |c: [&str; 5]| {
        format!(
            "{indent}{:<aw$}  {:<hw$}  {:<vw$}  {:<qw$}  {}",
            c[0],
            c[1],
            c[2],
            c[3],
            c[4],
            aw = w[0],
            hw = w[1],
            vw = w[2],
            qw = w[3],
        )
    };
    println!("{}", line(headers));
    for r in rows {
        println!(
            "{}",
            line([&r.agent, &r.harness, &r.vm, &r.queue, &r.last_turn])
        );
    }
}

fn humanize_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}
