use std::time::Duration;

use maturana_core::{
    improvement::TrajectoryStore, pipelock::PipelockVault, session_db::session_paths,
    state::MaturanaHome,
};

use crate::session::{run_session_once, RunnerOptions};

use super::{
    apply_channel_selection, audit_channel_event,
    command_catalog::{command_selector_buttons, help_text},
    command_handler::{handle_channel_command, status_text},
    delivery::deliver_telegram_outbox,
    enqueue_turn,
    media::{handle_telegram_document, handle_telegram_photo},
    onboarding::{enqueue_onboarding, is_onboarded, mark_onboarded},
    reset_channel_context,
    subagents::{create_subagent, frame_subtask},
    telegram::{TelegramCallbackQuery, TelegramUpdate},
    telegram_api::{
        answer_callback_query, edit_telegram_message, send_telegram, send_telegram_chat_action,
        send_telegram_keyboard,
    },
    telegram_live::{run_tool_with_animation, stream_turn_to_telegram},
    telegram_routing::{classify_telegram_update, telegram_message, InboundAction},
    telegram_state::{telegram_chat_id_key, telegram_pair_code_key},
    voice::handle_telegram_voice,
    TelegramServe,
};

const STREAM_TURN_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) fn handle_telegram_update(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    paired_chat_id: Option<i64>,
    pair_code: Option<&str>,
    update: &TelegramUpdate,
) -> anyhow::Result<()> {
    if let Some(callback) = &update.callback_query {
        return handle_telegram_callback(home, token, config, paired_chat_id, callback);
    }
    let reply_to_message_id = telegram_message(update).map(|message| message.message_id);
    match classify_telegram_update(update, paired_chat_id, pair_code) {
        InboundAction::Ignore => Ok(()),
        InboundAction::Deny { chat_id } => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.denied",
                "ignored unpaired chat",
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                "This Maturana agent is not paired with this chat.",
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Pair { chat_id } => {
            let vault = PipelockVault::new(home.pipelock_dir());
            vault.set(
                &telegram_chat_id_key(&config.agent_id),
                &chat_id.to_string(),
            )?;
            let _ = vault.delete(&telegram_pair_code_key(&config.agent_id))?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.paired",
                "paired telegram chat",
            )?;
            if !is_onboarded(home, &config.agent_id) {
                enqueue_onboarding(home, &config.agent_id, &config.session_id, chat_id)?;
                let _ = mark_onboarded(home, &config.agent_id);
                send_telegram(
                    token,
                    &chat_id.to_string(),
                    "Paired! One moment — let me introduce myself.",
                    reply_to_message_id,
                )?;
                send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
            } else {
                send_telegram(
                    token,
                    &chat_id.to_string(),
                    "Paired. Welcome back — message me any time.",
                    reply_to_message_id,
                )?;
            }
            Ok(())
        }
        InboundAction::Onboard { chat_id } => {
            enqueue_onboarding(home, &config.agent_id, &config.session_id, chat_id)?;
            let _ = mark_onboarded(home, &config.agent_id);
            send_telegram(
                token,
                &chat_id.to_string(),
                "Starting onboarding — one moment while I introduce myself.",
                reply_to_message_id,
            )?;
            send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
            Ok(())
        }
        InboundAction::Help { chat_id } => {
            send_telegram(
                token,
                &chat_id.to_string(),
                &help_text(),
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Status { chat_id } => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.status",
                "status requested",
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                &status_text(home, &config.agent_id, &config.session_id, "telegram"),
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Command {
            chat_id,
            name,
            args,
        } => {
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.command",
                &format!("/{name}"),
            )?;
            let selector = if args.trim().is_empty() {
                command_selector_buttons(home, &config.agent_id, &name)
            } else {
                None
            };
            if let Some((prompt, buttons, columns)) = selector {
                send_telegram_keyboard(
                    token,
                    &chat_id.to_string(),
                    &prompt,
                    &buttons,
                    columns,
                    reply_to_message_id,
                )?;
                return Ok(());
            }
            let reply = handle_channel_command(
                home,
                &config.agent_id,
                &config.session_id,
                chat_id,
                "telegram",
                &chat_id.to_string(),
                &name,
                &args,
            )
            .unwrap_or_else(|error| format!("Command failed: {error:#}"));
            send_telegram(token, &chat_id.to_string(), &reply, reply_to_message_id)?;
            Ok(())
        }
        InboundAction::New { chat_id } => {
            reset_channel_context(home, &config.agent_id, chat_id)?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.new_session",
                "reset telegram context window",
            )?;
            send_telegram(
                token,
                &chat_id.to_string(),
                "New session started.",
                reply_to_message_id,
            )?;
            Ok(())
        }
        InboundAction::Spawn {
            chat_id,
            mode,
            name,
            prompt,
        } => {
            let _ = create_subagent(home, &config.agent_id, &name, mode, &prompt);
            run_channel_prompt(
                home,
                token,
                config,
                chat_id,
                &frame_subtask(&name, &prompt),
                reply_to_message_id,
            )
        }
        InboundAction::Feedback { chat_id, value } => {
            let store = TrajectoryStore::open(&TrajectoryStore::store_path(home.root()))?;
            let source = "telegram";
            let rewarded =
                store.reward_latest(&config.agent_id, &config.session_id, source, value, None)?;
            audit_channel_event(
                home,
                &config.agent_id,
                "channel.telegram.feedback",
                &format!("recorded reward {value:+} on {:?}", rewarded),
            )?;
            let reply = match rewarded {
                Some(_) if value > 0.0 => "Thanks — logged 👍 for the last reply.",
                Some(_) => "Noted — logged 👎 for the last reply.",
                None => "No recent agent turn to rate yet.",
            };
            send_telegram(token, &chat_id.to_string(), reply, reply_to_message_id)?;
            Ok(())
        }
        InboundAction::Tool {
            chat_id,
            name,
            input,
        } => {
            run_tool_with_animation(home, token, config, chat_id, &name, &input)?;
            Ok(())
        }
        InboundAction::Document {
            chat_id,
            document,
            caption,
        } => handle_telegram_document(
            home,
            token,
            config,
            chat_id,
            &document,
            caption.as_deref(),
            reply_to_message_id,
        ),
        InboundAction::Photo {
            chat_id,
            file_id,
            caption,
        } => handle_telegram_photo(
            home,
            token,
            config,
            chat_id,
            &file_id,
            caption.as_deref(),
            reply_to_message_id,
        ),
        InboundAction::Voice {
            chat_id,
            file_id,
            filename,
        } => handle_telegram_voice(
            home,
            token,
            config,
            chat_id,
            &file_id,
            &filename,
            reply_to_message_id,
        ),
        InboundAction::Prompt { chat_id, text } => {
            run_channel_prompt(home, token, config, chat_id, &text, reply_to_message_id)
        }
    }
}

pub(crate) fn run_channel_prompt(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    chat_id: i64,
    text: &str,
    reply_to_message_id: Option<i64>,
) -> anyhow::Result<()> {
    audit_channel_event(
        home,
        &config.agent_id,
        "channel.telegram.inbound",
        "received telegram prompt",
    )?;
    println!("telegram inbound prompt accepted");
    let inbound_id = enqueue_turn(
        home,
        &config.agent_id,
        &config.session_id,
        "telegram",
        &chat_id.to_string(),
        chat_id,
        reply_to_message_id.map(|id| id.to_string()).as_deref(),
        text,
        serde_json::json!({ "telegram_reply_to": reply_to_message_id }),
    )?;
    let paths = session_paths(&home.agent_dir(&config.agent_id), &config.session_id);
    send_telegram_chat_action(token, &chat_id.to_string(), "typing")?;
    if let Some(provider) = &config.run_once_provider {
        let options = RunnerOptions {
            provider: provider.to_string(),
        };
        run_session_once(&paths, &options, 20)?;
    }
    if let Err(error) = stream_turn_to_telegram(
        home,
        token,
        config,
        chat_id,
        &inbound_id,
        reply_to_message_id,
        &paths,
        STREAM_TURN_TIMEOUT,
    ) {
        eprintln!("telegram streamer error (delivering anyway): {error:#}");
    }
    deliver_telegram_outbox(
        home,
        token,
        &config.agent_id,
        &config.session_id,
        chat_id,
        None,
    )?;
    Ok(())
}

fn handle_telegram_callback(
    home: &MaturanaHome,
    token: &str,
    config: &TelegramServe,
    paired_chat_id: Option<i64>,
    callback: &TelegramCallbackQuery,
) -> anyhow::Result<()> {
    let Some(message) = &callback.message else {
        return answer_callback_query(token, &callback.id, None);
    };
    let chat_id = message.chat.id;
    if paired_chat_id != Some(chat_id) {
        answer_callback_query(token, &callback.id, Some("Not paired with this chat."))?;
        return Ok(());
    }
    let data = callback.data.clone().unwrap_or_default();
    let (action, value) = data.split_once(':').unwrap_or((data.as_str(), ""));
    let toast = match action {
        "model" => format!("Model: {value}"),
        "reasoning" => format!("Reasoning: {value}"),
        "ttsprov" => format!("Provider: {value}"),
        "tts" => format!("TTS {}", if value == "on" { "ENABLED" } else { "disabled" }),
        "session" => format!("Session {value}"),
        _ => String::new(),
    };
    let updated = apply_channel_selection(home, &config.agent_id, &data);
    answer_callback_query(token, &callback.id, Some(&toast))?;
    let _ = edit_telegram_message(token, chat_id, message.message_id, &updated);
    audit_channel_event(home, &config.agent_id, "channel.telegram.callback", &data)?;
    Ok(())
}
