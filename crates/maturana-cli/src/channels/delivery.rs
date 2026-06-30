use std::{path::Path, time::Duration};

use chrono::Utc;
use maturana_core::{
    session_db::{
        claim_delivery, ensure_session, list_undelivered, mark_delivered, session_paths,
        unclaim_delivery, SessionPaths,
    },
    state::MaturanaHome,
};

use crate::session::{message_files, message_text};

use super::state::stable_chat_key;
use super::telegram_api::{delete_telegram_message, send_telegram_document};
use super::telegram_live::{
    clear_telegram_status, finalize_reply, peek_telegram_status, telegram_active_exists,
};
use super::voice::maybe_send_tts;
use super::{
    append_channel_turn, audit_channel_event, finalize_onboarding_reply, truncate_for_telegram,
};

/// Per-channel send behavior for the shared [`deliver_outbox`] loop. The loop owns
/// claiming, dropping unparseable rows, the silence-sentinel filter, transcript
/// recording, mark-delivered, audit, and release-on-failure (a failed send is
/// retried, never wedged). Each channel's sink supplies only HOW to send plus any
/// extras (Telegram's live-message edit + TTS).
pub(crate) trait OutboundSink {
    /// Send the final reply; return the platform message id (if any). `inbound_id`
    /// is the originating inbound row (Telegram looks up its live "working…"
    /// message by it); `reply_to` is the outbound row's thread id.
    fn send(
        &mut self,
        inbound_id: Option<&str>,
        text: &str,
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>>;

    /// Deliver a reply that carries one or more host-side files. The default, for
    /// channels with no native upload, sends the text plus the file NAMES so the
    /// user at least sees what was produced; channels that can upload override
    /// this to send the bytes. `files` are absolute host paths.
    fn send_files(
        &mut self,
        inbound_id: Option<&str>,
        text: &str,
        files: &[String],
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                Path::new(f)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| f.clone())
            })
            .collect();
        let combined = if text.trim().is_empty() {
            format!("📎 Files produced: {}", names.join(", "))
        } else {
            format!("{text}\n📎 Files produced: {}", names.join(", "))
        };
        self.send(inbound_id, &combined, reply_to)
    }

    /// The reply was the silence sentinel — clean up any live placeholder. No-op by default.
    fn on_silence(&mut self, _inbound_id: Option<&str>) {}

    /// After a successful send (e.g. speak it via TTS). No-op by default.
    fn after_delivered(&mut self, _text: &str, _reply_to: Option<&str>) {}

    /// Is an inline streamer still potentially animating this turn's live message?
    /// The backstop only defers young replies when true; replies with no streamer
    /// (e.g. onboarding) deliver immediately. Default: false (no streaming).
    fn has_pending_stream(&self, _inbound_id: Option<&str>) -> bool {
        false
    }
}

/// The ONE outbound delivery loop every async channel bridge shares — Telegram and
/// the generic Discord/Slack/AgentMail path.
pub(crate) fn deliver_outbox(
    home: &MaturanaHome,
    agent_id: &str,
    paths: &SessionPaths,
    channel: &str,
    platform_id: &str,
    chat_key: i64,
    min_age: Option<Duration>,
    sink: &mut dyn OutboundSink,
) -> anyhow::Result<usize> {
    let mut delivered = 0;
    for message in list_undelivered(paths)? {
        if message.channel != channel || message.platform_id != platform_id {
            continue;
        }
        let inbound_id = message.in_reply_to.as_deref();
        if let Some(min_age) = min_age {
            let too_young = (Utc::now() - message.created_at)
                .to_std()
                .map(|age| age < min_age)
                .unwrap_or(false);
            if too_young && sink.has_pending_stream(inbound_id) {
                continue;
            }
        }
        if !claim_delivery(paths, &message.id)? {
            continue;
        }
        let response = match message_text(&message.content) {
            Ok(text) => truncate_for_telegram(&finalize_onboarding_reply(home, agent_id, &text)),
            Err(error) => {
                eprintln!(
                    "{channel}: dropping unparseable outbound {}: {error:#}",
                    message.id
                );
                let _ = mark_delivered(paths, &message.id, None);
                continue;
            }
        };
        let reply_to = message.thread_id.as_deref();
        if response.trim() == crate::proactive::SILENCE_SENTINEL {
            sink.on_silence(inbound_id);
            let _ = mark_delivered(paths, &message.id, None);
            continue;
        }
        let files = message_files(&message.content);
        let send_result = if files.is_empty() {
            sink.send(inbound_id, &response, reply_to)
        } else {
            sink.send_files(inbound_id, &response, &files, reply_to)
        };
        match send_result {
            Ok(platform_message_id) => {
                let _ = mark_delivered(paths, &message.id, platform_message_id.as_deref());
                let _ = append_channel_turn(home, agent_id, chat_key, "assistant", &response);
                sink.after_delivered(&response, reply_to);
                let _ = audit_channel_event(
                    home,
                    agent_id,
                    &format!("channel.{channel}.outbound"),
                    "sent channel response",
                );
                delivered += 1;
            }
            Err(error) => {
                eprintln!("{channel} delivery failed, will retry: {error:#}");
                unclaim_delivery(paths, &message.id)?;
            }
        }
    }
    Ok(delivered)
}

struct TelegramSink<'a> {
    token: &'a str,
    chat_id: i64,
    paths: &'a SessionPaths,
    home: &'a MaturanaHome,
    agent_id: &'a str,
}

impl OutboundSink for TelegramSink<'_> {
    fn send(
        &mut self,
        inbound_id: Option<&str>,
        text: &str,
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let live_id = inbound_id.and_then(|inbound| peek_telegram_status(self.paths, inbound));
        let reply_to = reply_to.and_then(|value| value.parse::<i64>().ok());
        let platform_message_id =
            finalize_reply(self.token, self.chat_id, live_id, text, reply_to)?;
        if let Some(inbound) = inbound_id {
            clear_telegram_status(self.paths, inbound);
        }
        Ok(platform_message_id.map(|id| id.to_string()))
    }

    fn send_files(
        &mut self,
        inbound_id: Option<&str>,
        text: &str,
        files: &[String],
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let reply_to_i = reply_to.and_then(|value| value.parse::<i64>().ok());
        let live_id = inbound_id.and_then(|inbound| peek_telegram_status(self.paths, inbound));
        let text_already_sent = if let Some(id) = live_id {
            let _ = finalize_reply(self.token, self.chat_id, Some(id), text, reply_to_i)?;
            true
        } else {
            false
        };
        if let Some(inbound) = inbound_id {
            clear_telegram_status(self.paths, inbound);
        }
        let mut last_id: Option<String> = None;
        let mut uploaded = 0usize;
        for (i, path) in files.iter().enumerate() {
            let caption = if i == 0 && !text_already_sent {
                Some(text)
            } else {
                None
            };
            match send_telegram_document(
                self.token,
                self.chat_id,
                Path::new(path),
                caption,
                reply_to_i,
            ) {
                Ok(id) => {
                    last_id = id.map(|i| i.to_string());
                    uploaded += 1;
                }
                Err(error) => eprintln!("telegram: sendDocument failed for {path}: {error:#}"),
            }
        }
        if uploaded == 0 && !text_already_sent {
            let names: Vec<String> = files
                .iter()
                .filter_map(|f| {
                    Path::new(f)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                })
                .collect();
            let msg = format!("{text}\n(couldn't attach: {})", names.join(", "));
            return Ok(
                finalize_reply(self.token, self.chat_id, None, &msg, reply_to_i)?
                    .map(|id| id.to_string()),
            );
        }
        Ok(last_id)
    }

    fn on_silence(&mut self, inbound_id: Option<&str>) {
        let live_id = inbound_id.and_then(|inbound| peek_telegram_status(self.paths, inbound));
        if let Some(id) = live_id {
            let _ = delete_telegram_message(self.token, self.chat_id, id);
        }
        if let Some(inbound) = inbound_id {
            clear_telegram_status(self.paths, inbound);
        }
    }

    fn after_delivered(&mut self, text: &str, reply_to: Option<&str>) {
        let reply_to = reply_to.and_then(|value| value.parse::<i64>().ok());
        maybe_send_tts(
            self.home,
            self.token,
            self.agent_id,
            self.chat_id,
            text,
            reply_to,
        );
    }

    fn has_pending_stream(&self, inbound_id: Option<&str>) -> bool {
        inbound_id
            .map(|inbound| {
                telegram_active_exists(self.paths, inbound)
                    || peek_telegram_status(self.paths, inbound).is_some()
            })
            .unwrap_or(false)
    }
}

pub(super) fn deliver_telegram_outbox(
    home: &MaturanaHome,
    token: &str,
    agent_id: &str,
    session_id: &str,
    chat_id: i64,
    min_age: Option<Duration>,
) -> anyhow::Result<usize> {
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let mut sink = TelegramSink {
        token,
        chat_id,
        paths: &paths,
        home,
        agent_id,
    };
    let delivered = deliver_outbox(
        home,
        agent_id,
        &paths,
        "telegram",
        &chat_id.to_string(),
        chat_id,
        min_age,
        &mut sink,
    )?;
    if delivered > 0 {
        println!("telegram outbound responses sent: {delivered}");
    }
    Ok(delivered)
}

struct ClosureSink<'a, F> {
    send: &'a mut F,
}

impl<F> OutboundSink for ClosureSink<'_, F>
where
    F: FnMut(&str, Option<&str>) -> anyhow::Result<Option<String>>,
{
    fn send(
        &mut self,
        _inbound_id: Option<&str>,
        text: &str,
        reply_to: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        (self.send)(text, reply_to)
    }
}

pub(crate) fn deliver_channel_outbox<F>(
    home: &MaturanaHome,
    agent_id: &str,
    session_id: &str,
    channel: &str,
    platform_id: &str,
    mut send: F,
) -> anyhow::Result<usize>
where
    F: FnMut(&str, Option<&str>) -> anyhow::Result<Option<String>>,
{
    let paths = session_paths(&home.agent_dir(agent_id), session_id);
    ensure_session(&paths)?;
    let key = stable_chat_key(platform_id);
    let mut sink = ClosureSink { send: &mut send };
    deliver_outbox(
        home,
        agent_id,
        &paths,
        channel,
        platform_id,
        key,
        None,
        &mut sink,
    )
}
