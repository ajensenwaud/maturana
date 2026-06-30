use clap::{Args, Subcommand};
use maturana_core::{secrets::resolve_secret_source_with_home, state::MaturanaHome};

use crate::channels::paired_telegram_chat_source;

#[derive(Debug, Args)]
pub(crate) struct NotifyCommand {
    #[command(subcommand)]
    pub(crate) command: NotifySubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum NotifySubcommand {
    Telegram {
        #[arg(
            long,
            env = "MATURANA_TELEGRAM_BOT_TOKEN_SOURCE",
            default_value = "pipelock:telegram/bot-token"
        )]
        token_source: String,
        #[arg(long, env = "MATURANA_TELEGRAM_CHAT_ID_SOURCE")]
        chat_id_source: Option<String>,
        #[arg(long)]
        message: String,
        #[arg(long)]
        dry_run: bool,
    },
    Discord {
        #[arg(long, env = "MATURANA_DISCORD_WEBHOOK_SOURCE")]
        webhook_source: String,
        #[arg(long)]
        message: String,
        #[arg(long)]
        dry_run: bool,
    },
}

pub(crate) fn handle_notify(command: NotifyCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    match command.command {
        NotifySubcommand::Telegram {
            token_source,
            chat_id_source,
            message,
            dry_run,
        } => {
            if dry_run {
                println!("telegram notification dry-run: {message}");
                return Ok(());
            }

            let token = resolve_secret_source_with_home(&token_source, home.root())?;
            let chat_id_source =
                chat_id_source.or_else(|| paired_telegram_chat_source(home)).ok_or_else(|| {
                    anyhow::anyhow!(
                        "Telegram chat is not paired; run `maturana channel pair telegram start`, send `/pair CODE` to the bot, then run `maturana channel pair telegram complete`"
                    )
                })?;
            let chat_id = resolve_secret_source_with_home(&chat_id_source, home.root())?;
            send_telegram(
                token.expose_for_runtime(),
                chat_id.expose_for_runtime(),
                &message,
            )?;
            println!("telegram notification sent");
        }
        NotifySubcommand::Discord {
            webhook_source,
            message,
            dry_run,
        } => {
            if dry_run {
                println!("discord notification dry-run: {message}");
                return Ok(());
            }
            let webhook = resolve_secret_source_with_home(&webhook_source, home.root())?;
            send_discord(webhook.expose_for_runtime(), &message)?;
            println!("discord notification sent");
        }
    }
    Ok(())
}

fn send_telegram(token: &str, chat_id: &str, message: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "chat_id": chat_id,
        "text": message,
    });

    let request = ureq::post(&format!("https://api.telegram.org/bot{token}/sendMessage"))
        .set("content-type", "application/json")
        .send_string(&body.to_string());

    match request {
        Ok(_) => Ok(()),
        Err(error) => Err(anyhow::anyhow!("Telegram notification failed: {error}")),
    }
}

fn send_discord(webhook: &str, message: &str) -> anyhow::Result<()> {
    let body = serde_json::json!({
        "content": message,
    });

    let request = ureq::post(webhook)
        .set("content-type", "application/json")
        .send_string(&body.to_string());

    match request {
        Ok(_) => Ok(()),
        Err(error) => Err(anyhow::anyhow!("Discord notification failed: {error}")),
    }
}
