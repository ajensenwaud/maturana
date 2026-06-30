use clap::{Args, Subcommand};
use maturana_core::state::MaturanaHome;
use std::{
    io::{IsTerminal, Write},
    process::Command as ProcessCommand,
    sync::Arc,
};

/// Web cockpit server. Access is gated by the token at `<home>/web/token`
/// exchanged for a session cookie at /login. If `--bind` is omitted and
/// Tailscale is up, an interactive run asks whether to bind to your tailnet
/// (recommended), all interfaces, or localhost.
#[derive(Debug, Args)]
pub(crate) struct WebCommand {
    #[command(subcommand)]
    pub(crate) command: Option<WebSubcommand>,
    /// Bind address (host:port). Omit to be asked (interactive) or default to
    /// 0.0.0.0:47836 (non-interactive).
    #[arg(long)]
    pub(crate) bind: Option<String>,
    /// Bind only to this host's Tailscale (tailnet) IP -- reachable from your
    /// tailnet, not the LAN/internet. Errors if Tailscale isn't up.
    #[arg(long)]
    pub(crate) tailnet: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum WebSubcommand {
    /// Print the cockpit login token (creating it if absent).
    Token,
}

pub(crate) fn run_web_command(home: &MaturanaHome, command: WebCommand) -> anyhow::Result<()> {
    match command.command {
        Some(WebSubcommand::Token) => {
            println!("{}", maturana_web::login_token(home.root())?);
            Ok(())
        }
        None => {
            let bind = resolve_web_bind(command.bind.clone(), command.tailnet)?;
            println!("Maturana web cockpit → http://{bind}");
            println!(
                "  login token: `maturana web token` (stored at {}/web/token)",
                home.root().display()
            );
            let enqueue = web_enqueue_adapter();
            let ingest = web_ingest_adapter();
            maturana_web::run_web(home.root().to_path_buf(), &bind, enqueue, Some(ingest))
        }
    }
}

fn web_enqueue_adapter() -> maturana_web::EnqueueTurnFn {
    // Inject the web-specific slash-command adapter. Plain chat turns go
    // through maturana-ops' shared conversation front door; slash commands
    // still reuse the CLI channel catalog.
    Arc::new(
        |home_root: &std::path::Path, agent_id: &str, session_id: &str, text: &str| {
            let home = MaturanaHome::new(home_root.to_path_buf());
            let chat_key =
                maturana_ops::conversation::stable_chat_key(&format!("web:{session_id}"));
            // A leading `/` is a slash command -- dispatch it through the SAME
            // shared handler as every other channel, so `/model`, `/status`,
            // `/skill`, etc. behave identically in the cockpit instead of reaching
            // the agent as a literal user message.
            if text.trim_start().starts_with('/') {
                let cmd = crate::channels::dispatch_slash_command(
                    &home,
                    agent_id,
                    session_id,
                    chat_key,
                    "web",
                    "web",
                    text.trim_start(),
                );
                return crate::channels::apply_web_console_command(
                    &home, agent_id, session_id, chat_key, cmd,
                );
            }
            maturana_ops::conversation::enqueue_turn(
                &home,
                agent_id,
                session_id,
                "web",
                "web",
                chat_key,
                None,
                text,
                serde_json::json!({}),
            )
        },
    )
}

fn web_ingest_adapter() -> maturana_web::IngestFileFn {
    // Inject the knowledge-graph ingest hook so a file uploaded in the chat
    // window becomes retrievable by the VM-isolated agent, matching the path a
    // Telegram document upload takes.
    Arc::new(
        |home_root: &std::path::Path, agent_id: &str, file_path: &std::path::Path| {
            let home = MaturanaHome::new(home_root.to_path_buf());
            let kg = crate::channels::agent_knowledge_graph(&home, agent_id);
            if !kg.enabled {
                anyhow::bail!("knowledge graph is not enabled for this agent");
            }
            let token = maturana_core::worker::read_graph_token(home.root())
                .ok_or_else(|| anyhow::anyhow!("graph service token is missing"))?;
            let supported = file_path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| crate::graph::SUPPORTED_EXTS.contains(&e.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if !supported {
                anyhow::bail!(
                    "unsupported file type for graph ingest (supported: {})",
                    crate::graph::SUPPORTED_EXTS.join(", ")
                );
            }
            let name = crate::graph::agent_graph_name(agent_id);
            crate::graph::ingest_file_into_service(
                crate::graph::DEFAULT_LOCAL_URL,
                &token,
                &name,
                file_path,
                1800,
            )
        },
    )
}

const WEB_PORT: u16 = 47836;

/// This host's Tailscale IPv4 (100.64.0.0/10), if Tailscale is installed + up.
fn tailscale_ipv4() -> Option<String> {
    let out = ProcessCommand::new("tailscale")
        .arg("ip")
        .arg("-4")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ip = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    if ip.is_empty() {
        None
    } else {
        Some(ip)
    }
}

/// Pure bind resolver (testable). Explicit `--bind` always wins. `--tailnet`
/// forces the tailnet IP (error if absent). With Tailscale present, an
/// interactive `choice` ("1"=tailnet, "2"=all, "3"=localhost) picks; a `None`
/// choice (non-interactive) keeps the historical all-interfaces default rather
/// than silently scoping to the tailnet.
pub(crate) fn web_bind_for(
    explicit: Option<&str>,
    tailnet: bool,
    tailscale_ip: Option<&str>,
    choice: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(b) = explicit {
        if tailnet {
            anyhow::bail!("pass either --bind or --tailnet, not both");
        }
        return Ok(b.to_string());
    }
    if tailnet {
        let ip = tailscale_ip.ok_or_else(|| {
            anyhow::anyhow!("--tailnet requested but Tailscale isn't up (no `tailscale ip -4`)")
        })?;
        return Ok(format!("{ip}:{WEB_PORT}"));
    }
    match (tailscale_ip, choice) {
        (Some(ip), Some("1")) => Ok(format!("{ip}:{WEB_PORT}")),
        (Some(_), Some("2")) => Ok(format!("0.0.0.0:{WEB_PORT}")),
        (Some(_), Some("3")) => Ok(format!("127.0.0.1:{WEB_PORT}")),
        (Some(ip), Some(_)) => Ok(format!("{ip}:{WEB_PORT}")),
        _ => Ok(format!("0.0.0.0:{WEB_PORT}")),
    }
}

/// Resolve where `maturana web` should bind: explicit/--tailnet honored; else,
/// on an interactive terminal with Tailscale up, ASK (defaulting to tailnet);
/// otherwise 0.0.0.0.
fn resolve_web_bind(explicit: Option<String>, tailnet: bool) -> anyhow::Result<String> {
    let ip = if explicit.is_some() {
        None
    } else {
        tailscale_ipv4()
    };
    let mut choice: Option<String> = None;
    if explicit.is_none() && !tailnet {
        if let Some(ip) = ip.as_deref() {
            if std::io::stdin().is_terminal() {
                println!("Tailscale detected at {ip}. Where should the web cockpit listen?");
                println!(
                    "  [1] tailnet only   {ip}:{WEB_PORT}   (recommended -- your tailnet only)"
                );
                println!(
                    "  [2] all interfaces 0.0.0.0:{WEB_PORT}  (LAN + tailnet; put TLS in front)"
                );
                println!("  [3] localhost only 127.0.0.1:{WEB_PORT}");
                print!("Choose [1]: ");
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                let c = line.trim();
                choice = Some(if c.is_empty() {
                    "1".to_string()
                } else {
                    c.to_string()
                });
            }
        }
    }
    web_bind_for(
        explicit.as_deref(),
        tailnet,
        ip.as_deref(),
        choice.as_deref(),
    )
}
