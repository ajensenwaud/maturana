use super::*;

#[test]
fn discord_extract_message_keeps_file_only_messages() {
    // A message with an attachment but NO text must still be surfaced (with
    // its attachments) instead of dropped — the bug behind "file upload
    // doesn't work via Discord".
    let ev = serde_json::json!({
        "d": {
            "channel_id": "123",
            "content": "",
            "author": { "id": "user1" },
            "attachments": [
                { "filename": "report.pdf", "url": "https://cdn.discordapp.com/x/report.pdf" }
            ]
        }
    });
    let (chan, content, atts) = discord_extract_message(&ev, Some("bot9")).unwrap();
    assert_eq!(chan, "123");
    assert!(content.is_empty());
    assert_eq!(
        atts,
        vec![(
            "report.pdf".to_string(),
            "https://cdn.discordapp.com/x/report.pdf".to_string()
        )]
    );
    // The bot's own messages are still ignored (no echo loop).
    let own = serde_json::json!({
        "d": { "channel_id": "1", "content": "hi", "author": { "id": "bot9" } }
    });
    assert!(discord_extract_message(&own, Some("bot9")).is_none());
    // A plain text message with no attachments still parses.
    let txt = serde_json::json!({
        "d": { "channel_id": "9", "content": "  hello  ", "author": { "id": "u" } }
    });
    let (_, c, a) = discord_extract_message(&txt, Some("bot9")).unwrap();
    assert_eq!(c, "hello");
    assert!(a.is_empty());
}

#[test]
fn recent_models_are_newest_first_and_filter_non_chat() {
    let m = |id: &str, created: i64, text_output: bool, supports_tools: bool| OpenRouterModel {
        id: id.to_string(),
        created,
        text_output,
        supports_tools,
    };
    // Mixed catalog: chat models of varying age, an image model, a safety
    // classifier, and a newest-but-toolless model (like openrouter/fusion) —
    // none of the last three may appear in a chat picker opencode can use.
    let catalog = vec![
        m("deepseek/deepseek-chat-v3.1", 100, true, true),
        m("google/gemini-3.5-flash", 300, true, true),
        m("anthropic/claude-opus-4.8", 250, true, true),
        m("z-ai/glm-5.2", 280, true, true),
        m("google/gemini-3-pro-image", 999, false, true), // newest, image-only
        m("nvidia/nemotron-3.5-content-safety", 998, true, true), // classifier
        m("openrouter/fusion", 997, true, false),         // newest, but no tool support
    ];
    let picked = recent_openrouter_models(&catalog, 3);
    // Newest tool-capable chat models first; the image, safety, and toolless
    // models are excluded even though they have the largest `created` stamps.
    assert_eq!(
        picked,
        vec![
            "google/gemini-3.5-flash".to_string(),
            "z-ai/glm-5.2".to_string(),
            "anthropic/claude-opus-4.8".to_string(),
        ]
    );
    assert!(!picked.iter().any(|id| {
        id.contains("image") || id.contains("content-safety") || id.contains("fusion")
    }));
}

#[test]
fn classifies_pair_before_authorization() {
    let update = text_update(7, "/pair ABC123");
    assert_eq!(
        classify_telegram_update(&update, None, Some("ABC123")),
        InboundAction::Pair { chat_id: 7 }
    );
}

#[test]
fn denies_unpaired_chat() {
    let update = text_update(9, "hello");
    assert_eq!(
        classify_telegram_update(&update, Some(7), None),
        InboundAction::Deny { chat_id: 9 }
    );
}

#[test]
fn classifies_onboard_as_its_own_action() {
    // /onboard must route to its own action (not the generic Command path,
    // which only returns a text reply) so the onboarding turn is enqueued with
    // THIS chat's routing and the agent's greeting actually comes back.
    // Regression: it used to enqueue with channel/platform_id "onboard" and the
    // greeting was silently dropped.
    let update = text_update(7, "/onboard");
    assert_eq!(
        classify_telegram_update(&update, Some(7), None),
        InboundAction::Onboard { chat_id: 7 }
    );
}

#[test]
fn routes_paired_prompt_and_status() {
    assert_eq!(
        classify_telegram_update(&text_update(7, "/status"), Some(7), None),
        InboundAction::Status { chat_id: 7 }
    );
    assert_eq!(
        classify_telegram_update(&text_update(7, "/new"), Some(7), None),
        InboundAction::New { chat_id: 7 }
    );
    assert_eq!(
        classify_telegram_update(&text_update(7, "hello"), Some(7), None),
        InboundAction::Prompt {
            chat_id: 7,
            text: "hello".to_string()
        }
    );
}

#[test]
fn routes_tool_and_feedback_commands() {
    assert_eq!(
        classify_telegram_update(
            &text_update(7, "/tool weather {\"city\":\"oslo\"}"),
            Some(7),
            None
        ),
        InboundAction::Tool {
            chat_id: 7,
            name: "weather".to_string(),
            input: "{\"city\":\"oslo\"}".to_string(),
        }
    );
    assert_eq!(
        classify_telegram_update(&text_update(7, "/tool weather"), Some(7), None),
        InboundAction::Tool {
            chat_id: 7,
            name: "weather".to_string(),
            input: "{}".to_string(),
        }
    );
    assert_eq!(
        classify_telegram_update(&text_update(7, "/good"), Some(7), None),
        InboundAction::Feedback {
            chat_id: 7,
            value: signals::THUMBS_UP,
        }
    );
    assert_eq!(
        classify_telegram_update(&text_update(7, "/bad"), Some(7), None),
        InboundAction::Feedback {
            chat_id: 7,
            value: signals::THUMBS_DOWN,
        }
    );
    // An invalid tool name falls back to help rather than crashing.
    assert_eq!(
        classify_telegram_update(&text_update(7, "/tool Bad_Name"), Some(7), None),
        InboundAction::Help { chat_id: 7 }
    );
}

#[test]
fn routes_spawn_command() {
    assert_eq!(
        classify_telegram_update(
            &text_update(7, "/spawn persistent Researcher -- find context"),
            Some(7),
            None
        ),
        InboundAction::Spawn {
            chat_id: 7,
            mode: SpawnMode::Persistent,
            name: "researcher".to_string(),
            prompt: "find context".to_string(),
        }
    );
}

#[test]
fn routes_new_slash_commands() {
    assert_eq!(
        classify_telegram_update(&text_update(7, "/models"), Some(7), None),
        InboundAction::Command {
            chat_id: 7,
            name: "models".to_string(),
            args: String::new()
        }
    );
    assert_eq!(
        classify_telegram_update(&text_update(7, "/model openai/gpt-5"), Some(7), None),
        InboundAction::Command {
            chat_id: 7,
            name: "model".to_string(),
            args: "openai/gpt-5".to_string()
        }
    );
    assert_eq!(
        classify_telegram_update(&text_update(7, "/graph-query roadmap q3"), Some(7), None),
        InboundAction::Command {
            chat_id: 7,
            name: "graph-query".to_string(),
            args: "roadmap q3".to_string()
        }
    );
    // /emerge spawns a sub-agent; /skill <name> becomes a prompt.
    assert_eq!(
        classify_telegram_update(&text_update(7, "/emerge summarize my inbox"), Some(7), None),
        InboundAction::Spawn {
            chat_id: 7,
            mode: SpawnMode::Ephemeral,
            name: "summarize-my-inbox".to_string(),
            prompt: "summarize my inbox".to_string(),
        }
    );
    assert_eq!(
        classify_telegram_update(
            &text_update(7, "/skill maturana-pipelock list"),
            Some(7),
            None
        ),
        InboundAction::Prompt {
            chat_id: 7,
            text: "Use the `maturana-pipelock` skill. list".to_string(),
        }
    );
    // Unknown slash command routes to the command handler (replies via /help).
    assert_eq!(
        classify_telegram_update(&text_update(7, "/wat"), Some(7), None),
        InboundAction::Command {
            chat_id: 7,
            name: "unknown".to_string(),
            args: "/wat".to_string()
        }
    );
}

#[test]
fn command_menu_underscores_map_to_hyphenated_commands() {
    // Telegram's setMyCommands can't carry hyphens, so the interactive `/`
    // menu sends `/graph_query` and `/tts_provider`; these must classify the
    // same as their canonical hyphenated forms.
    assert_eq!(
        classify_telegram_update(&text_update(7, "/graph_query roadmap q3"), Some(7), None),
        InboundAction::Command {
            chat_id: 7,
            name: "graph-query".to_string(),
            args: "roadmap q3".to_string()
        }
    );
    assert_eq!(
        classify_telegram_update(&text_update(7, "/tts_provider"), Some(7), None),
        InboundAction::Command {
            chat_id: 7,
            name: "tts-provider".to_string(),
            args: String::new()
        }
    );
}

#[test]
fn discord_extracts_prompt_and_skips_bot_and_self() {
    // A real user message: returns (channel_id, content) with the leading
    // bot mention stripped.
    let ev = serde_json::json!({
        "op": 0, "t": "MESSAGE_CREATE",
        "d": { "channel_id": "123", "content": "<@999> hello there",
               "author": { "id": "42", "bot": false } }
    });
    assert_eq!(
        discord_extract_message(&ev, Some("999")),
        Some(("123".to_string(), "hello there".to_string(), vec![]))
    );
    // Bot-authored message is ignored.
    let bot = serde_json::json!({
        "d": { "channel_id": "1", "content": "hi", "author": { "id": "7", "bot": true } }
    });
    assert_eq!(discord_extract_message(&bot, Some("999")), None);
    // Our own message (author id == self) is ignored (no echo loop).
    let own = serde_json::json!({
        "d": { "channel_id": "1", "content": "hi", "author": { "id": "999" } }
    });
    assert_eq!(discord_extract_message(&own, Some("999")), None);
    // Empty content AND no attachments is ignored.
    let empty = serde_json::json!({
        "d": { "channel_id": "1", "content": "   ", "author": { "id": "42" } }
    });
    assert_eq!(discord_extract_message(&empty, Some("999")), None);
}

#[test]
fn memory_extraction_explicit_and_heuristic() {
    // Explicit cue is stripped to the bare fact.
    assert_eq!(
        extract_memory_fact("remember that my standup is at 9am"),
        Some("my standup is at 9am".to_string())
    );
    assert_eq!(
        extract_memory_fact("/remember the API key rotates monthly"),
        Some("the API key rotates monthly".to_string())
    );
    // Heuristic captures the whole message.
    assert_eq!(
        extract_memory_fact("My name is Anders"),
        Some("My name is Anders".to_string())
    );
    assert_eq!(
        extract_memory_fact("remind me to call the bank tomorrow"),
        Some("remind me to call the bank tomorrow".to_string())
    );
    // Ordinary chatter is not remembered.
    assert_eq!(extract_memory_fact("what's the weather like?"), None);
    assert_eq!(extract_memory_fact("   "), None);
}

#[test]
fn help_and_commands_cover_the_catalog() {
    let help = help_text();
    for group in [
        "Session",
        "Options",
        "Status",
        "Management",
        "MaturanaGraph",
        "Voice",
    ] {
        assert!(help.contains(group), "help missing group {group}");
    }
    for cmd in [
        "/model",
        "/models",
        "/tools",
        "/status",
        "/subagents",
        "/graph-query",
        "/tts",
    ] {
        assert!(help.contains(cmd), "help missing {cmd}");
        assert!(commands_text().contains(cmd), "commands missing {cmd}");
    }
}

#[test]
fn codex_models_track_auth_mode() {
    let temp = temp_dir("codex-auth");
    let dir = temp.path();

    // ChatGPT (OAuth) login → only gpt-5.5 is offered (live-verified: every
    // other id 400s on a ChatGPT account).
    fs::write(
        dir.join("auth.json"),
        r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"tokens":{"access_token":"x"}}"#,
    )
    .unwrap();
    assert_eq!(codex_auth_mode_from_dir(dir), CodexAuthMode::ChatGpt);
    assert_eq!(
        codex_models_for_auth(Some(dir)),
        vec!["gpt-5.5".to_string()]
    );

    // API-key login → the wider catalog.
    fs::write(
        dir.join("auth.json"),
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test","tokens":null}"#,
    )
    .unwrap();
    assert_eq!(codex_auth_mode_from_dir(dir), CodexAuthMode::ApiKey);
    assert!(codex_models_for_auth(Some(dir)).contains(&"gpt-5".to_string()));

    // No explicit auth_mode → infer from which credential is populated.
    fs::write(
        dir.join("auth.json"),
        r#"{"OPENAI_API_KEY":null,"tokens":{"access_token":"x"}}"#,
    )
    .unwrap();
    assert_eq!(codex_auth_mode_from_dir(dir), CodexAuthMode::ChatGpt);

    // The operator's seeded default is unioned in and de-duplicated (and not
    // confused with `model_reasoning_effort`).
    fs::write(
        dir.join("config.toml"),
        "model = \"gpt-6-preview\"\nmodel_reasoning_effort = \"low\"\n[tui]\n",
    )
    .unwrap();
    let models = codex_models_for_auth(Some(dir));
    assert_eq!(models.first().map(String::as_str), Some("gpt-6-preview"));
    assert!(models.contains(&"gpt-5.5".to_string()));

    // Unreadable / unknown auth → the ChatGPT-safe default set.
    assert_eq!(
        codex_auth_mode_from_dir(temp.path().join("missing").as_path()),
        CodexAuthMode::Unknown
    );
    assert_eq!(codex_models_for_auth(None), vec!["gpt-5.5".to_string()]);
}

#[test]
fn pair_command_accepts_bot_suffix() {
    assert!(is_pair_command("/pair@LuhmannSystemsBot ABC123", "ABC123"));
    assert!(!is_pair_command("/pair@LuhmannSystemsBot WRONG", "ABC123"));
}

#[test]
fn dispatch_prompt_never_starts_with_a_dash() {
    // Regression: a leading "--" makes a clap-based harness CLI (codex exec,
    // claude -p) treat the whole PROMPT as an unknown option and fail the turn.
    let temp = temp_dir("dispatch-prompt-dash");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\nidentity\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "operator: Anders\n").unwrap();

    // With identity + memory present.
    let p = build_dispatch_prompt(&home, "agent", "do the thing");
    assert!(
        !p.starts_with('-'),
        "prompt must not start with '-': {:?}",
        &p[..20.min(p.len())]
    );
    assert!(
        p.contains("WHO YOU ARE") && p.contains("operator: Anders") && p.contains("do the thing")
    );

    // With no identity/memory files (bare task) — still safe.
    let bare = build_dispatch_prompt(&home, "missing-agent", "do the thing");
    assert!(!bare.starts_with('-'));
}

#[test]
fn channel_prompt_includes_memory_and_transcript() {
    let temp = temp_dir("channel-prompt");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::create_dir_all(agent_dir.join("context")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
    fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
    fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "likes tea\n").unwrap();
    fs::write(agent_dir.join("context/README.md"), "local context\n").unwrap();
    append_channel_turn(&home, "agent", 42, "user", "my name is Anders").unwrap();

    let prompt =
        build_channel_prompt(&home, "agent", 42, "what is my name and tea preference?").unwrap();
    assert!(prompt.contains("likes tea"));
    assert!(prompt.contains("my name is Anders"));
    assert!(prompt.contains("what is my name and tea preference?"));
    let manifest_path = channel_context_manifest_path(&home, "agent", 42);
    let manifest: ChannelContextManifest =
        serde_json::from_str(&fs::read_to_string(manifest_path).unwrap()).unwrap();
    assert_eq!(manifest.agent_id, "agent");
    assert_eq!(manifest.chat_id, 42);
    assert!(manifest.loaded_context_chars > 0);
    assert!(manifest.wiki_query_terms.contains(&"name".to_string()));
    assert_eq!(
        manifest.context_policy.strategy,
        "durable-files-plus-current-message-and-recent-transcript-graph-terms"
    );
    assert!(manifest.context_policy.excludes_reset_marker);
    // Query-term extraction (now feeding only the graph) still picks up message terms.
    assert!(manifest
        .wiki_term_sources
        .iter()
        .any(|term| term.term == "tea" && term.sources.contains(&"current_message".to_string())));
    assert!(manifest
        .source_files
        .iter()
        .any(|file| file.label == "memory/MEMORY.md" && !file.missing));
}

#[test]
fn console_turns_feed_the_next_prompt_so_the_tui_has_memory() {
    // Regression: the TUI (agent_chat_turn) sent the bare prompt, so the agent
    // "started fresh" every turn. It now injects the console transcript via
    // build_channel_prompt(console_chat_key()). Record a couple of console
    // turns and assert the next prompt carries them — the exact path the TUI
    // uses, keyed the same way the TUI records (console_chat_key).
    let temp = temp_dir("tui-memory");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
    fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
    fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();
    record_console_turn(&home, "agent", "user", "My name is Anders.").unwrap();
    record_console_turn(&home, "agent", "assistant", "Hi Anders! Nice to meet you.").unwrap();

    let prompt =
        build_channel_prompt(&home, "agent", console_chat_key(), "what's my name?").unwrap();
    assert!(
        prompt.contains("My name is Anders."),
        "prompt is missing the prior user turn → no memory: {prompt}"
    );
    assert!(
        prompt.contains("Hi Anders!"),
        "prompt is missing the prior assistant turn"
    );
    assert!(prompt.contains("what's my name?"));
}

#[test]
fn enqueue_turn_is_the_single_front_door() {
    // Every chat surface goes through enqueue_turn. It must, for ANY channel:
    // record the user turn, inject the recent transcript (memory), attach
    // model+reasoning, and enqueue tagged with the real channel/platform_id.
    let temp = temp_dir("front-door");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
    fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
    fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();

    // First turn establishes history; second must see it in its enriched prompt.
    enqueue_turn(
        &home,
        "agent",
        "s",
        "telegram",
        "555",
        555,
        None,
        "remember the blue door",
        serde_json::json!({"telegram_reply_to": 9}),
    )
    .unwrap();
    let id = enqueue_turn(
        &home,
        "agent",
        "s",
        "telegram",
        "555",
        555,
        None,
        "what did I say?",
        serde_json::json!({}),
    )
    .unwrap();
    assert!(!id.is_empty());

    let paths = session_paths(&home.agent_dir("agent"), "s");
    let pending = maturana_core::session_db::claim_pending_inbound(&paths, 10).unwrap();
    let msg = pending
        .iter()
        .find(|m| m.id == id)
        .expect("enqueued message present");
    let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
    let prompt = content["prompt"].as_str().unwrap();
    assert!(
        prompt.contains("remember the blue door"),
        "front door must inject the prior turn (memory): {prompt}"
    );
    assert!(
        content.get("model").is_some(),
        "front door must attach model"
    );
    assert!(
        content.get("reasoning").is_some(),
        "front door must attach reasoning"
    );
    // The transcript is recorded under the channel chat key (555), not a sentinel.
    let transcript = fs::read_to_string(channel_transcript_path(&home, "agent", 555)).unwrap();
    assert!(transcript.contains("remember the blue door"));
    assert!(transcript.contains("what did I say?"));
}

#[test]
fn outreach_turn_is_tagged_for_the_real_chat_and_keeps_the_directive_invisible() {
    // The proactive-outreach bug: a turn tagged "proactive" had its reply
    // filtered out by deliver_outbox (channel mismatch) and never reached the
    // user. enqueue_outreach_turn must tag the turn for the REAL telegram chat
    // so the reply delivers — while NOT recording the system directive as a
    // user turn (which would pollute the transcript + every future prompt).
    let temp = temp_dir("outreach-turn");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
    fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
    fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();

    let id = enqueue_outreach_turn(
        &home,
        "agent",
        "s",
        8566198884,
        "[PROACTIVE CHECK] anything worth saying?",
        "proactive",
        serde_json::json!({}),
    )
    .unwrap();

    let paths = session_paths(&home.agent_dir("agent"), "s");
    let pending = maturana_core::session_db::claim_pending_inbound(&paths, 10).unwrap();
    let msg = pending.iter().find(|m| m.id == id).expect("enqueued");
    // Tagged for the real telegram chat => the telegram delivery loop (which
    // matches channel=="telegram" && platform_id==chat_id) WILL pick the reply up.
    assert_eq!(
        msg.channel, "telegram",
        "must route via the telegram channel"
    );
    assert_eq!(
        msg.platform_id, "8566198884",
        "must target the paired chat id"
    );
    let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
    assert!(content.get("prompt").is_some(), "context prompt attached");
    assert!(content.get("model").is_some(), "model override attached");
    assert!(
        content.get("reasoning").is_some(),
        "reasoning override attached"
    );

    // The directive must NOT appear as a user turn in the visible transcript.
    let recorded =
        fs::read_to_string(channel_transcript_path(&home, "agent", 8566198884)).unwrap_or_default();
    assert!(
        !recorded.contains("PROACTIVE CHECK"),
        "the system directive must not pollute the transcript as a fake user turn"
    );
}

#[test]
fn deliver_outbox_is_the_single_delivery_loop() {
    // Telegram and the generic Discord/Slack/AgentMail path share this loop.
    // It must: filter by channel/platform, drop the silence sentinel to
    // on_silence (never send it), record the assistant turn under chat_key,
    // mark delivered, and on a send FAILURE release the claim so the row is
    // retried (not wedged claimed-and-undelivered).
    struct MockSink {
        sent: Vec<String>,
        silenced: usize,
        fail: bool,
    }
    impl OutboundSink for MockSink {
        fn send(
            &mut self,
            _inbound: Option<&str>,
            text: &str,
            _reply: Option<&str>,
        ) -> anyhow::Result<Option<String>> {
            if self.fail {
                anyhow::bail!("send boom");
            }
            self.sent.push(text.to_string());
            Ok(Some("pmid".to_string()))
        }
        fn on_silence(&mut self, _inbound: Option<&str>) {
            self.silenced += 1;
        }
    }
    use maturana_core::session_db::write_outbound;
    let temp = temp_dir("deliver-outbox");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let paths = session_paths(&home.agent_dir("agent"), "s");
    ensure_session(&paths).unwrap();
    let body = |t: &str| serde_json::json!({ "text": t }).to_string();
    write_outbound(
        &paths,
        None,
        "chat",
        "tg",
        "111",
        None,
        &body("hello there"),
    )
    .unwrap();
    write_outbound(
        &paths,
        None,
        "chat",
        "tg",
        "111",
        None,
        &body(crate::proactive::SILENCE_SENTINEL),
    )
    .unwrap();
    write_outbound(
        &paths,
        None,
        "chat",
        "other",
        "111",
        None,
        &body("wrong channel"),
    )
    .unwrap();

    let mut sink = MockSink {
        sent: vec![],
        silenced: 0,
        fail: false,
    };
    let delivered =
        deliver_outbox(&home, "agent", &paths, "tg", "111", 111, None, &mut sink).unwrap();
    assert_eq!(delivered, 1, "only the matching non-silence row delivers");
    assert_eq!(sink.sent, vec!["hello there".to_string()]);
    assert_eq!(
        sink.silenced, 1,
        "silence sentinel routes to on_silence, never sent"
    );
    let transcript = fs::read_to_string(channel_transcript_path(&home, "agent", 111)).unwrap();
    assert!(
        transcript.contains("hello there"),
        "assistant turn recorded under chat_key"
    );

    // A failing send must RELEASE the claim so the row is retried next pass.
    write_outbound(&paths, None, "chat", "tg", "111", None, &body("retry me")).unwrap();
    let mut failer = MockSink {
        sent: vec![],
        silenced: 0,
        fail: true,
    };
    let n = deliver_outbox(&home, "agent", &paths, "tg", "111", 111, None, &mut failer).unwrap();
    assert_eq!(n, 0);
    let undelivered = list_undelivered(&paths).unwrap();
    assert!(
        undelivered.iter().any(|m| m.content.contains("retry me")),
        "failed send must release the claim for retry, not wedge it"
    );
}

#[test]
fn deliver_outbox_does_not_make_unstreamed_replies_wait_the_backstop() {
    // The "Paired! …silence" bug: the onboarding greeting (enqueued by pairing,
    // no streamer) was deliverable only by the 6-min backstop. The age gate must
    // ONLY defer a young reply whose streamer might still be live.
    use maturana_core::session_db::write_outbound;
    let backstop = std::time::Duration::from_secs(3600); // huge, like the real one

    struct NoStream(usize);
    impl OutboundSink for NoStream {
        fn send(
            &mut self,
            _i: Option<&str>,
            _t: &str,
            _r: Option<&str>,
        ) -> anyhow::Result<Option<String>> {
            self.0 += 1;
            Ok(None)
        }
        // has_pending_stream defaults false (no streamer).
    }
    struct Streaming(usize);
    impl OutboundSink for Streaming {
        fn send(
            &mut self,
            _i: Option<&str>,
            _t: &str,
            _r: Option<&str>,
        ) -> anyhow::Result<Option<String>> {
            self.0 += 1;
            Ok(None)
        }
        fn has_pending_stream(&self, _i: Option<&str>) -> bool {
            true
        }
    }
    let body = serde_json::json!({ "text": "greeting" }).to_string();

    // No streamer → a brand-new reply delivers immediately despite the backstop.
    let temp = temp_dir("deliver-no-stream");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let paths = session_paths(&home.agent_dir("agent"), "s");
    ensure_session(&paths).unwrap();
    write_outbound(&paths, Some("inb-1"), "chat", "tg", "111", None, &body).unwrap();
    let mut sink = NoStream(0);
    let n = deliver_outbox(
        &home,
        "agent",
        &paths,
        "tg",
        "111",
        111,
        Some(backstop),
        &mut sink,
    )
    .unwrap();
    assert_eq!(
        n, 1,
        "a no-streamer reply must deliver now, not wait the backstop"
    );

    // A live streamer → the same young reply is deferred (streamer owns it).
    let temp2 = temp_dir("deliver-streaming");
    let home2 = MaturanaHome::new(temp2.path().join(".maturana"));
    let paths2 = session_paths(&home2.agent_dir("agent"), "s");
    ensure_session(&paths2).unwrap();
    write_outbound(&paths2, Some("inb-1"), "chat", "tg", "111", None, &body).unwrap();
    let mut sink2 = Streaming(0);
    let n2 = deliver_outbox(
        &home2,
        "agent",
        &paths2,
        "tg",
        "111",
        111,
        Some(backstop),
        &mut sink2,
    )
    .unwrap();
    assert_eq!(n2, 0, "a young reply with a live streamer must be deferred");
    assert_eq!(sink2.0, 0);
}

#[test]
fn onboarding_interview_persists_every_turn_until_the_completion_sentinel() {
    // The bug: the onboarding directive only reached turn 1, so the agent
    // answered once and stopped asking. Now an active marker re-injects the
    // "keep interviewing" directive on EVERY turn until the agent signals done.
    let temp = temp_dir("onboarding-interview");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
    fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
    fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();

    // Not onboarding → no directive.
    let p = build_channel_prompt(&home, "agent", 7, "hi").unwrap();
    assert!(!p.contains("KEEP THE INTERVIEW GOING"));

    // Active → a LATER turn (not turn 1) still carries the directive.
    set_onboarding_active(&home, "agent");
    assert!(is_onboarding_active(&home, "agent"));
    let p2 = build_channel_prompt(&home, "agent", 7, "My name is Anders").unwrap();
    assert!(
        p2.contains("KEEP THE INTERVIEW GOING"),
        "onboarding directive must persist into follow-up turns"
    );

    // The completion sentinel ends the interview and is stripped from the reply.
    let shown =
        finalize_onboarding_reply(&home, "agent", "Great, all set!\n[[ONBOARDING_COMPLETE]]");
    assert_eq!(
        shown, "Great, all set!",
        "sentinel stripped from the user-facing reply"
    );
    assert!(
        !is_onboarding_active(&home, "agent"),
        "sentinel clears the active state"
    );

    // Cleared → directive no longer injected.
    let p3 = build_channel_prompt(&home, "agent", 7, "thanks").unwrap();
    assert!(!p3.contains("KEEP THE INTERVIEW GOING"));
}

#[test]
fn channel_context_selects_query_terms_from_recent_transcript_for_followups() {
    let temp = temp_dir("channel-followup-context");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::create_dir_all(agent_dir.join("context")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
    fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
    fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();
    fs::write(agent_dir.join("context/README.md"), "# Context\n").unwrap();
    append_channel_turn(
        &home,
        "agent",
        42,
        "user",
        "Please remember the calendar planning context.",
    )
    .unwrap();

    let _prompt = build_channel_prompt(&home, "agent", 42, "what about that?").unwrap();
    let manifest: ChannelContextManifest = serde_json::from_str(
        &fs::read_to_string(channel_context_manifest_path(&home, "agent", 42)).unwrap(),
    )
    .unwrap();
    // A term from the RECENT TRANSCRIPT (not the bare follow-up) is selected for
    // the graph query, so follow-ups like "what about that?" still retrieve context.
    assert!(manifest.wiki_query_terms.contains(&"calendar".to_string()));
    assert!(manifest
        .wiki_term_sources
        .iter()
        .any(|term| term.term == "calendar"
            && term.sources.contains(&"recent_transcript".to_string())));
}

#[test]
fn new_session_rotates_transcript_and_reloads_context_next_turn() {
    let temp = temp_dir("channel-new-session");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::create_dir_all(agent_dir.join("context")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
    fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
    fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "prefers fresh starts\n").unwrap();
    fs::write(agent_dir.join("context/README.md"), "local context\n").unwrap();
    append_channel_turn(&home, "agent", 42, "user", "old context").unwrap();
    fs::write(
        channel_context_manifest_path(&home, "agent", 42),
        r#"{"stale":true}"#,
    )
    .unwrap();

    reset_channel_context(&home, "agent", 42).unwrap();

    let transcript = fs::read_to_string(channel_transcript_path(&home, "agent", 42)).unwrap();
    assert!(transcript.contains("Memory and wiki context will be reloaded"));
    assert!(!transcript.contains("old context"));
    let archive_dir = home.agent_dir("agent").join("channels/telegram/archive");
    let archive_files = fs::read_dir(archive_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    assert_eq!(archive_files.len(), 2);
    assert!(archive_files.iter().any(|name| name.ends_with(".md")));
    assert!(archive_files
        .iter()
        .any(|name| name.ends_with(".context.json")));
    assert!(!channel_context_manifest_path(&home, "agent", 42).exists());
    let prompt = build_channel_prompt(&home, "agent", 42, "hello again").unwrap();
    assert!(prompt.contains("prefers fresh starts"));
    assert!(!prompt.contains("old context"));
    assert!(channel_context_manifest_path(&home, "agent", 42).exists());
}

#[test]
fn new_session_does_not_use_reset_marker_or_archived_transcript_for_wiki() {
    let temp = temp_dir("channel-new-session-wiki-query");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let agent_dir = home.agent_dir("agent");
    fs::create_dir_all(agent_dir.join("memory")).unwrap();
    fs::create_dir_all(agent_dir.join("context")).unwrap();
    fs::write(agent_dir.join("AGENTS.md"), "# Agent\n").unwrap();
    fs::write(agent_dir.join("SOUL.md"), "# Soul\n").unwrap();
    fs::write(agent_dir.join("MATURANA.md"), "# Contract\n").unwrap();
    fs::write(agent_dir.join("memory/MEMORY.md"), "# Memory\n").unwrap();
    fs::write(agent_dir.join("context/README.md"), "# Context\n").unwrap();
    append_channel_turn(
        &home,
        "agent",
        42,
        "user",
        "Please use oldcontext next time.",
    )
    .unwrap();

    reset_channel_context(&home, "agent", 42).unwrap();
    let _prompt = build_channel_prompt(&home, "agent", 42, "freshnote please").unwrap();

    let manifest: ChannelContextManifest = serde_json::from_str(
        &fs::read_to_string(channel_context_manifest_path(&home, "agent", 42)).unwrap(),
    )
    .unwrap();
    // After a reset, graph query terms come from the fresh message only — the
    // archived transcript ("oldcontext") and the reset-marker text ("reloaded")
    // must not drive retrieval.
    assert!(manifest.wiki_query_terms.contains(&"freshnote".to_string()));
    assert!(!manifest
        .wiki_query_terms
        .contains(&"oldcontext".to_string()));
    assert!(!manifest.wiki_query_terms.contains(&"reloaded".to_string()));
    assert!(manifest
        .wiki_term_sources
        .iter()
        .any(|term| term.term == "freshnote"
            && term.sources.contains(&"current_message".to_string())));
}

#[test]
fn remember_message_appends_to_memory() {
    let temp = temp_dir("channel-memory");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    maybe_remember_user_message(&home, "agent", "remember that I prefer short replies").unwrap();

    let memory = fs::read_to_string(home.agent_dir("agent").join("memory/MEMORY.md")).unwrap();
    // The explicit "remember that" cue is stripped to the bare fact.
    assert!(memory.contains("I prefer short replies"));
    assert!(!memory.contains("remember that"));
}

#[test]
fn slack_extracts_user_message_and_strips_mention() {
    let envelope = serde_json::json!({
        "type": "events_api",
        "envelope_id": "env-1",
        "payload": { "event": {
            "type": "app_mention",
            "channel": "C123",
            "ts": "1700.1",
            "text": "<@U0BOT> what is the roadmap?"
        }}
    });
    let (channel, text, thread) = slack_extract_prompt(&envelope).unwrap();
    assert_eq!(channel, "C123");
    assert_eq!(text, "what is the roadmap?");
    assert_eq!(thread.as_deref(), Some("1700.1"));
}

#[test]
fn slack_ignores_bot_and_non_message_events() {
    let bot = serde_json::json!({
        "type": "events_api",
        "payload": { "event": { "type": "message", "channel": "C1", "text": "hi", "bot_id": "B1" }}
    });
    assert!(slack_extract_prompt(&bot).is_none());
    let edit = serde_json::json!({
        "type": "events_api",
        "payload": { "event": { "type": "message", "channel": "C1", "text": "hi", "subtype": "message_changed" }}
    });
    assert!(slack_extract_prompt(&edit).is_none());
    let reaction = serde_json::json!({
        "type": "events_api",
        "payload": { "event": { "type": "reaction_added" }}
    });
    assert!(slack_extract_prompt(&reaction).is_none());
}

#[test]
fn stable_chat_key_is_deterministic_and_positive() {
    let a = stable_chat_key("C123");
    assert_eq!(a, stable_chat_key("C123"));
    assert!(a >= 0);
    assert_ne!(a, stable_chat_key("C124"));
}

#[test]
fn dispatch_turn_round_trips_and_is_never_chat_deliverable() {
    use maturana_core::session_db::write_outbound;
    let temp = temp_dir("dispatch");
    let home = MaturanaHome::new(temp.path().join(".maturana"));

    // Enqueue one orchestration step to a worker; nothing to collect yet.
    let handle =
        enqueue_dispatch_turn(&home, "worker", "s", "run-7", "do the thing", None).unwrap();
    assert!(try_collect_dispatch(&home, "worker", &handle)
        .unwrap()
        .is_none());

    // The step inbound is tagged on the non-deliverable orchestrate channel,
    // not a user channel — no live delivery loop serves it.
    let paths = session_paths(&home.agent_dir("worker"), "s");
    let pending = maturana_core::session_db::claim_pending_inbound(&paths, 10).unwrap();
    let step = pending.iter().find(|m| m.id == handle.message_id).unwrap();
    assert_eq!(step.channel, "orchestrate");
    assert_eq!(step.platform_id, "run-7");
    assert_eq!(step.kind, "dispatch");

    // The worker replies; the loop collects it exactly once, then it's consumed.
    write_outbound(
        &paths,
        Some(&handle.message_id),
        "dispatch",
        "orchestrate",
        "run-7",
        None,
        &serde_json::json!({ "text": "did the thing" }).to_string(),
    )
    .unwrap();
    assert_eq!(
        try_collect_dispatch(&home, "worker", &handle)
            .unwrap()
            .as_deref(),
        Some("did the thing")
    );
    assert!(
        try_collect_dispatch(&home, "worker", &handle)
            .unwrap()
            .is_none(),
        "a collected reply is consumed and never seen again"
    );
}

#[test]
fn console_transcript_round_trips_and_is_per_agent() {
    // BUG1: TUI conversation must persist across an agent switch — record +
    // read back the same Markdown transcript Telegram uses, keyed per agent.
    let temp = temp_dir("console-transcript");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    record_console_turn(&home, "alpha", "user", "hello\nworld").unwrap();
    record_console_turn(&home, "alpha", "assistant", "hi there").unwrap();
    assert_eq!(
        read_console_transcript(&home, "alpha"),
        vec![
            ("user".to_string(), "hello\nworld".to_string()),
            ("assistant".to_string(), "hi there".to_string()),
        ]
    );
    // A different agent has its own (empty) transcript — switching can't bleed.
    assert!(read_console_transcript(&home, "beta").is_empty());
}

#[test]
fn clear_console_transcript_persists_across_reopen() {
    // /clear must wipe the stored transcript so it does NOT come back the next
    // time the TUI opens (read_console_transcript returns empty after a clear).
    let temp = temp_dir("console-clear");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    record_console_turn(&home, "alpha", "user", "old conversation").unwrap();
    assert!(!read_console_transcript(&home, "alpha").is_empty());
    clear_console_transcript(&home, "alpha").unwrap();
    assert!(read_console_transcript(&home, "alpha").is_empty());
    // Clearing an already-empty transcript is fine (no file → ok).
    clear_console_transcript(&home, "alpha").unwrap();
}

#[test]
fn dispatch_model_with_args_sets_via_text_not_picker() {
    // BUG2: bare `/model` opens a picker (Select), but `/model gpt-5` must set
    // directly via the text handler (never a picker).
    let temp = temp_dir("dispatch-model-args");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let out = dispatch_slash_command(
        &home,
        "alpha",
        "s",
        console_chat_key(),
        "console",
        &console_chat_key().to_string(),
        "/model gpt-5",
    );
    assert!(matches!(out, ConsoleCommand::Reply(_)));
}

#[test]
fn every_catalog_command_dispatches_on_all_surfaces() {
    // Anti-drift guard for slash-command parity: every command advertised in
    // COMMAND_GROUPS must be RECOGNIZED by the shared dispatcher on every text
    // surface (console TUI + Discord share `dispatch_slash_command`; Telegram
    // routes the same names to the same `handle_channel_command`). A command
    // that fell through to "Unknown command" would mean a channel lags the set.
    let temp = temp_dir("channel-command-parity");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    let names: Vec<&str> = COMMAND_GROUPS
        .iter()
        .flat_map(|(_, cmds)| cmds.iter().map(|(name, _)| *name))
        .collect();
    assert!(!names.is_empty(), "command catalog is empty");
    for name in names {
        // arg-guarded commands (/skill, /emerge, …) need a dummy arg to reach
        // the handler rather than the usage fallthrough. /loop's bare-goal form
        // would actually spawn a run, so exercise its non-spawning `status` path.
        let raw = if name == "/loop" {
            format!("{name} status")
        } else {
            format!("{name} x")
        };
        for (chat_id, channel) in [
            (console_chat_key(), "console"),
            (stable_chat_key("c1"), "discord"),
        ] {
            let outcome = dispatch_slash_command(
                &home,
                "a",
                "s",
                chat_id,
                channel,
                &chat_id.to_string(),
                &raw,
            );
            if let ConsoleCommand::Reply(text) = &outcome {
                assert!(
                    !text.starts_with("Unknown command"),
                    "catalog command {name} fell through to Unknown on {channel}: {text}"
                );
            }
        }
    }
}

fn text_update(chat_id: i64, text: &str) -> TelegramUpdate {
    TelegramUpdate {
        update_id: 1,
        message: Some(TelegramMessage {
            message_id: 1,
            text: Some(text.to_string()),
            caption: None,
            document: None,
            photo: None,
            voice: None,
            audio: None,
            chat: TelegramChat { id: chat_id },
        }),
        channel_post: None,
        callback_query: None,
    }
}

fn document_update(chat_id: i64, file_name: &str, caption: Option<&str>) -> TelegramUpdate {
    TelegramUpdate {
        update_id: 1,
        message: Some(TelegramMessage {
            message_id: 1,
            text: None,
            caption: caption.map(str::to_string),
            document: Some(TelegramDocument {
                file_id: "file-123".to_string(),
                file_name: Some(file_name.to_string()),
                file_size: Some(1024),
            }),
            photo: None,
            voice: None,
            audio: None,
            chat: TelegramChat { id: chat_id },
        }),
        channel_post: None,
        callback_query: None,
    }
}

fn photo_update(chat_id: i64, caption: Option<&str>) -> TelegramUpdate {
    TelegramUpdate {
        update_id: 1,
        message: Some(TelegramMessage {
            message_id: 1,
            text: None,
            caption: caption.map(str::to_string),
            document: None,
            photo: Some(vec![
                TelegramPhotoSize {
                    file_id: "photo-small".to_string(),
                },
                TelegramPhotoSize {
                    file_id: "photo-large".to_string(),
                },
            ]),
            voice: None,
            audio: None,
            chat: TelegramChat { id: chat_id },
        }),
        channel_post: None,
        callback_query: None,
    }
}

fn voice_update(chat_id: i64) -> TelegramUpdate {
    TelegramUpdate {
        update_id: 1,
        message: Some(TelegramMessage {
            message_id: 1,
            text: None,
            caption: None,
            document: None,
            photo: None,
            voice: Some(TelegramVoice {
                file_id: "voice-123".to_string(),
            }),
            audio: None,
            chat: TelegramChat { id: chat_id },
        }),
        channel_post: None,
        callback_query: None,
    }
}

#[test]
fn routes_voice_notes_to_transcription_from_paired_chat_only() {
    // A voice note carries no text/document/photo. Before the fix it fell
    // through to the empty-text path and was Ignored (the "doesn't even
    // register it" bug); it must now classify as Voice so it gets transcribed.
    assert_eq!(
        classify_telegram_update(&voice_update(7), Some(7), None),
        InboundAction::Voice {
            chat_id: 7,
            file_id: "voice-123".to_string(),
            filename: "voice.ogg".to_string(),
        }
    );
    // The pairing gate applies to voice exactly like documents/photos.
    assert_eq!(
        classify_telegram_update(&voice_update(9), Some(7), None),
        InboundAction::Deny { chat_id: 9 }
    );
}

#[test]
fn stt_multipart_carries_model_and_audio() {
    let (content_type, body) = multipart_audio("model_id", "scribe_v1", "voice.ogg", b"OGGDATA");
    assert!(content_type.contains("multipart/form-data; boundary="));
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("name=\"model_id\""));
    assert!(text.contains("scribe_v1"));
    assert!(text.contains("filename=\"voice.ogg\""));
    assert!(text.contains("OGGDATA"));
    assert!(text.trim_end().ends_with("--"));
}

#[test]
fn routes_photo_uploads_to_ocr_from_paired_chat_only() {
    // The largest size is OCR'd; pairing gates the upload.
    assert_eq!(
        classify_telegram_update(&photo_update(7, Some("store this")), Some(7), None),
        InboundAction::Photo {
            chat_id: 7,
            file_id: "photo-large".to_string(),
            caption: Some("store this".to_string()),
        }
    );
    assert_eq!(
        classify_telegram_update(&photo_update(9, None), Some(7), None),
        InboundAction::Deny { chat_id: 9 }
    );
}

#[test]
fn routes_document_uploads_from_paired_chat_only() {
    let update = document_update(7, "notes.pdf", Some("for the graph"));
    assert_eq!(
        classify_telegram_update(&update, Some(7), None),
        InboundAction::Document {
            chat_id: 7,
            document: TelegramDocument {
                file_id: "file-123".to_string(),
                file_name: Some("notes.pdf".to_string()),
                file_size: Some(1024),
            },
            caption: Some("for the graph".to_string()),
        }
    );
    // The pairing gate applies to documents exactly like text.
    assert_eq!(
        classify_telegram_update(&document_update(9, "notes.pdf", None), Some(7), None),
        InboundAction::Deny { chat_id: 9 }
    );
    assert_eq!(
        classify_telegram_update(&document_update(9, "notes.pdf", None), None, None),
        InboundAction::Deny { chat_id: 9 }
    );
}

#[test]
fn sanitizes_telegram_document_names() {
    assert_eq!(
        sanitize_document_name(Some("Q3 Roadmap.pdf")),
        "Q3 Roadmap.pdf"
    );
    assert_eq!(
        sanitize_document_name(Some("../../etc/passwd")),
        "-..-etc-passwd"
    );
    assert_eq!(sanitize_document_name(Some("..")), "document");
    assert_eq!(sanitize_document_name(None), "document");
    assert_eq!(sanitize_document_name(Some("a/b\\c.md")), "a-b-c.md");
}

struct TempDir {
    path: PathBuf,
}

#[test]
fn console_command_dispatch_matches_telegram_catalog() {
    let temp = temp_dir("console-commands");
    let home = MaturanaHome::new(temp.path().join(".maturana"));
    fs::create_dir_all(home.agent_dir("agent")).unwrap();

    assert!(matches!(
        run_console_command(&home, "agent", "telegram-main", "/clear"),
        ConsoleCommand::Clear
    ));
    assert!(matches!(
        run_console_command(&home, "agent", "telegram-main", "/quit"),
        ConsoleCommand::Quit
    ));
    assert!(matches!(
        run_console_command(&home, "agent", "telegram-main", "/new"),
        ConsoleCommand::NewSession
    ));
    match run_console_command(&home, "agent", "telegram-main", "/status") {
        ConsoleCommand::Reply(t) => {
            assert!(t.contains("agent: agent"));
            assert!(t.contains("console"));
        }
        _ => panic!("/status should produce a reply"),
    }
    // /skill <name> [args] runs the skill via a normal agent turn.
    match run_console_command(
        &home,
        "agent",
        "telegram-main",
        "/skill summarize the notes",
    ) {
        ConsoleCommand::Prompt(p) => assert_eq!(p, "Use the `summarize` skill. the notes"),
        _ => panic!("/skill with args should be a prompt"),
    }
    // /model persists a setting and confirms it (shared with Telegram).
    match run_console_command(&home, "agent", "telegram-main", "/model gpt-5") {
        ConsoleCommand::Reply(t) => assert!(t.contains("gpt-5")),
        _ => panic!("/model should reply"),
    }
    match run_console_command(&home, "agent", "telegram-main", "/bogus") {
        ConsoleCommand::Reply(t) => assert!(t.contains("Unknown command")),
        _ => panic!("unknown command should reply"),
    }
    // The catalog the TUI advertises includes the Telegram menu commands.
    let names: Vec<&str> = console_command_catalog()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    for cmd in [
        "/model",
        "/models",
        "/session",
        "/tools",
        "/subagents",
        "/graph-query",
        "/tts",
        "/onboard",
        "/new",
        "/good",
    ] {
        assert!(names.contains(&cmd), "catalog missing {cmd}");
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A throwaway HTTP server that records each request (first line + body) and
/// replies with a generic Telegram-OK so the real send/edit code succeeds.
fn spawn_mock_telegram() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let cap = captured.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            loop {
                let n = match s.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let cl = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let body_start = pos + 4;
                    while buf.len() < body_start + cl {
                        match s.read(&mut tmp) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let first = headers.lines().next().unwrap_or("").to_string();
                    let body =
                        String::from_utf8_lossy(&buf[body_start..(body_start + cl).min(buf.len())])
                            .to_string();
                    cap.lock().unwrap().push(format!("{first}\n{body}"));
                    break;
                }
            }
            let body = r#"{"ok":true,"result":{"message_id":4242}}"#;
            let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });
    (format!("http://127.0.0.1:{port}"), captured)
}

/// `MATURANA_TELEGRAM_API_BASE` is process-global, so tests that point the real
/// Telegram code at a local mock must not run concurrently — otherwise one test's
/// base URL bleeds into another's HTTP call. Serialize them all through this lock.
static TG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn with_tg_base<T>(base: &str, f: impl FnOnce() -> T) -> T {
    let _guard = TG_ENV_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    std::env::set_var("MATURANA_TELEGRAM_API_BASE", base);
    let out = f();
    std::env::remove_var("MATURANA_TELEGRAM_API_BASE");
    out
}

/// A throwaway HTTP server that replies with a FIXED status line + (optional)
/// extra headers + body, so the live-edit classifier can be exercised against
/// real 429/400 responses. `extra_headers`, if non-empty, must include its own
/// trailing CRLF (e.g. "retry-after: 3\r\n"). Returns the base URL.
fn spawn_mock_telegram_status(status_line: &str, extra_headers: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let status_line = status_line.to_string();
    let extra_headers = extra_headers.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            // Read the FULL request (headers + Content-Length body) before replying.
            // Closing the socket while the client is still writing its body makes
            // ureq surface a transport error instead of our intended status code.
            loop {
                let n = match s.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let cl = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let body_start = pos + 4;
                    while buf.len() < body_start + cl {
                        match s.read(&mut tmp) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    break;
                }
            }
            let resp = format!(
                    "{status_line}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[test]
fn live_edit_classifies_429_with_retry_after() {
    // A 429 with a retry-after header must surface as Throttled(retry_after) so
    // the loop honors Telegram's cooldown instead of hammering it (the behaviour
    // that turned per-second edits into a 15s freeze).
    let base = spawn_mock_telegram_status(
        "HTTP/1.1 429 Too Many Requests",
        "retry-after: 3\r\n",
        r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 3","parameters":{"retry_after":3}}"#,
    );
    let outcome = with_tg_base(&base, || {
        edit_telegram_live_html("TESTTOKEN", 1, 2, "<pre>x</pre>")
    });
    assert!(
        matches!(outcome, LiveEditOutcome::Throttled(Some(3))),
        "429 with retry-after must classify as Throttled(Some(3))"
    );
}

#[test]
fn live_edit_treats_not_modified_400_as_ok() {
    // Editing with identical content yields 400 "message is not modified" — that
    // is benign and must NOT trigger backoff (we already dedup on rendered text).
    let base = spawn_mock_telegram_status(
        "HTTP/1.1 400 Bad Request",
        "",
        r#"{"ok":false,"error_code":400,"description":"Bad Request: message is not modified"}"#,
    );
    let outcome = with_tg_base(&base, || {
        edit_telegram_live_html("TESTTOKEN", 1, 2, "<pre>x</pre>")
    });
    assert!(
        matches!(outcome, LiveEditOutcome::Ok),
        "benign 'message is not modified' 400 must classify as Ok, got a failure"
    );
}

#[test]
fn finalize_edits_thinking_bubble_into_answer() {
    // Reliable single-message finish: the live "Thinking…" bubble is EDITED in
    // place into the answer — exactly one message, no new send, no delete (so it
    // can never duplicate or orphan a bubble). finalize returns the bubble id.
    let (base, captured) = spawn_mock_telegram();
    let answer = "Three biggest stories: one, two, and three with a short note on each.";
    let returned = with_tg_base(&base, || {
        let id = send_telegram_html("TESTTOKEN", "123", "<pre>💭 Thinking… 0:08</pre>", None)
            .unwrap()
            .expect("draft message id");
        finalize_reply("TESTTOKEN", 123, Some(id), answer, None).unwrap()
    });

    let reqs = captured.lock().unwrap().clone();
    // sendMessage(draft) + editMessageText(answer). No second send, no delete.
    assert_eq!(
        reqs.len(),
        2,
        "unexpected sequence:\n{}",
        reqs.join("\n--\n")
    );
    assert!(
        reqs[0].contains("/sendMessage") && reqs[0].contains("Thinking"),
        "{}",
        reqs[0]
    );
    assert!(
        reqs[1].contains("/editMessageText") && reqs[1].contains("short note on each"),
        "answer must edit the bubble in place: {}",
        reqs[1]
    );
    assert!(
        reqs.iter().all(|r| !r.contains("/deleteMessage")),
        "no delete — the one bubble becomes the answer"
    );
    // finalize returns the (reused) bubble id, not a new message id.
    assert_eq!(returned, Some(4242));
}

#[test]
fn live_loop_ticks_counter_then_edits_into_answer() {
    // Drive the REAL stream_turn_to_telegram loop against the mock, with a worker
    // reply that lands after ~8s. Asserts the captured HTTP shows the counter
    // ADVANCING (≥2 distinct "💭 Thinking… 0:0X" frames — the old 10s-frozen bug
    // would emit only one) but NOT hammering Telegram (≤6 frames over the ~6s
    // pre-reply window), then a reliable finish: the live bubble is EDITED in
    // place into the answer (one message, no duplicate, no leftover bubble).
    let (base, captured) = spawn_mock_telegram();

    let tmp = std::env::temp_dir().join(format!("mat-streamloop-{}", std::process::id()));
    let home = MaturanaHome::new(tmp.clone());
    let agent = "claude";
    let session = "telegram-main";
    let chat_id = 777i64;
    std::fs::create_dir_all(home.agent_dir(agent)).unwrap();
    let paths = session_paths(&home.agent_dir(agent), session);
    ensure_session(&paths).unwrap();
    let inbound_id = insert_inbound(
        &paths,
        "chat",
        "telegram",
        &chat_id.to_string(),
        None,
        &serde_json::json!({ "text": "hi" }).to_string(),
    )
    .unwrap();

    // The worker's reply lands after ~8s so the counter advances across a few
    // cadence ticks (base 2.5s) before the answer arrives.
    let answer = "Here is a reasonably long answer that the live bubble is edited into when the turn completes.";
    let paths_w = paths.clone();
    let reply_to = inbound_id.clone();
    let config = TelegramServe {
        agent_id: agent.to_string(),
        session_id: session.to_string(),
        token_source: "x".to_string(),
        once: false,
        run_once_provider: None,
        poll_seconds: 5,
        timeout_seconds: 600,
    };
    with_tg_base(&base, || {
        // Spawn the worker INSIDE the serialized section so its reply-delay timer
        // starts at the loop's start, not before this test acquired the shared
        // TG_ENV_LOCK — otherwise a slow earlier test makes the reply land early
        // and the counter shows too few frames (flaky under test contention).
        let worker = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(8000));
            let body = serde_json::json!({ "text": answer }).to_string();
            let _ = maturana_core::session_db::write_outbound(
                &paths_w,
                Some(&reply_to),
                "chat",
                "telegram",
                &chat_id.to_string(),
                None,
                &body,
            );
        });
        stream_turn_to_telegram(
            &home,
            "TESTTOKEN",
            &config,
            chat_id,
            &inbound_id,
            None,
            &paths,
            std::time::Duration::from_secs(25),
        )
        .unwrap();
        worker.join().unwrap();
    });
    let _ = std::fs::remove_dir_all(&tmp);

    let reqs = captured.lock().unwrap().clone();
    // Counter ADVANCED smoothly: several DISTINCT "💭 Thinking… 0:0X" payloads
    // over the ~6s pre-reply window (~1/s cadence). ≥3 proves the clock isn't
    // frozen/jumpy (the old bucketed bug emitted one, the 2.5s cadence ~2–3);
    // the upper bound just guards against an unbounded edit storm.
    let thinking: std::collections::BTreeSet<String> = reqs
        .iter()
        .filter(|r| r.contains("Thinking"))
        .cloned()
        .collect();
    assert!(
            (3..=14).contains(&thinking.len()),
            "counter should advance ~1/s (expect several distinct frames over the ~6s window), got {}:\n{}",
            thinking.len(),
            thinking.into_iter().collect::<Vec<_>>().join("\n--\n")
        );
    // No dust anywhere — the simulated dots/crumble are gone for good.
    assert!(reqs.iter().all(|r| !r.contains('·')), "no dust frames");
    // The answer was delivered by EDITING the live bubble in place (one message),
    // not a second send and not a delete.
    assert!(
        reqs.iter()
            .any(|r| r.contains("/editMessageText")
                && r.contains("edited into when the turn completes")),
        "answer not delivered by editing the bubble:\n{}",
        reqs.join("\n--\n")
    );
    assert!(
        reqs.iter().all(|r| !r.contains("/deleteMessage")),
        "no delete on a normal answer — the bubble becomes the answer:\n{}",
        reqs.join("\n--\n")
    );
}

#[test]
fn live_loop_deletes_orphan_bubble_when_reply_already_delivered() {
    // Regression for the duplicate-message + never-ending-counter class
    // (the "Snak dansk" incident): if a backstop pass delivers the reply while
    // the streamer is mid-turn (its live bubble already on screen), the streamer
    // must DELETE its now-orphan bubble and send NO duplicate — the chat shows
    // exactly the one delivered answer, and the counter stops. The streamer
    // detects the reply by EXISTENCE (not undelivered status), so it can't tick
    // forever against a reply someone else already delivered.
    let (base, captured) = spawn_mock_telegram();

    let tmp = std::env::temp_dir().join(format!("mat-orphanbubble-{}", std::process::id()));
    let home = MaturanaHome::new(tmp.clone());
    let agent = "claude";
    let session = "telegram-main";
    let chat_id = 778i64;
    std::fs::create_dir_all(home.agent_dir(agent)).unwrap();
    let paths = session_paths(&home.agent_dir(agent), session);
    ensure_session(&paths).unwrap();
    let inbound_id = insert_inbound(
        &paths,
        "chat",
        "telegram",
        &chat_id.to_string(),
        None,
        &serde_json::json!({ "text": "hi" }).to_string(),
    )
    .unwrap();

    // After ~3.5s the streamer's live bubble exists; THEN a backstop delivers the
    // reply (write outbound + atomically claim it) out from under the streamer.
    let answer = "Already delivered by the backstop.";
    let paths_w = paths.clone();
    let reply_to = inbound_id.clone();
    let config = TelegramServe {
        agent_id: agent.to_string(),
        session_id: session.to_string(),
        token_source: "x".to_string(),
        once: false,
        run_once_provider: None,
        poll_seconds: 5,
        timeout_seconds: 600,
    };
    with_tg_base(&base, || {
        // Spawn the worker INSIDE the serialized section so its timer starts at
        // the loop's start (see the sibling live-loop test for why).
        let worker = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(3500));
            let body = serde_json::json!({ "text": answer }).to_string();
            let _ = maturana_core::session_db::write_outbound(
                &paths_w,
                Some(&reply_to),
                "chat",
                "telegram",
                &chat_id.to_string(),
                None,
                &body,
            );
            // Simulate the backstop winning the atomic claim before the streamer.
            if let Ok(Some(msg)) = find_reply_outbound(&paths_w, &reply_to) {
                let _ = claim_delivery(&paths_w, &msg.id);
            }
        });
        stream_turn_to_telegram(
            &home,
            "TESTTOKEN",
            &config,
            chat_id,
            &inbound_id,
            None,
            &paths,
            std::time::Duration::from_secs(25),
        )
        .unwrap();
        worker.join().unwrap();
    });

    // The active-streamer lock is released on every exit path.
    assert!(
        !telegram_active_exists(&paths, &inbound_id),
        "the .tgactive lock must be cleared when the streamer exits"
    );
    let _ = std::fs::remove_dir_all(&tmp);

    let reqs = captured.lock().unwrap().clone();
    // The orphan bubble was DELETED (cleanup) ...
    assert!(
        reqs.iter().any(|r| r.contains("/deleteMessage")),
        "orphan bubble should be deleted on a lost claim:\n{}",
        reqs.join("\n--\n")
    );
    // ... and the streamer did NOT re-send the answer (no duplicate).
    assert!(
        reqs.iter()
            .all(|r| !r.contains("Already delivered by the backstop")),
        "streamer must NOT send the answer the backstop already delivered:\n{}",
        reqs.join("\n--\n")
    );
}

#[test]
fn progress_html_renders_monospace_tool_block() {
    // Nothing to show yet → empty (no placeholder; caller posts no draft).
    assert_eq!(render_progress_html(&[]), "");

    // Structured tool events render as ONE monospace <pre> block: web_search
    // labelled, bash shown as just icon + command. No brain/thinking chrome.
    let events = vec![
        ProgressEvent {
            seq: 0,
            kind: "tool".into(),
            text: "web_search\u{1f}L:Ron:Harald top songs".into(),
        },
        ProgressEvent {
            seq: 1,
            kind: "tool".into(),
            text: "bash\u{1f}rg foo".into(),
        },
    ];
    let rendered = render_progress_html(&events);
    assert!(
        rendered.starts_with("<pre>") && rendered.ends_with("</pre>"),
        "{rendered}"
    );
    assert!(
        rendered.contains("🔎 Web Search: L:Ron:Harald top songs"),
        "{rendered}"
    );
    assert!(rendered.contains("🛠️ rg foo"), "{rendered}");
    assert!(!rendered.contains('🧠'), "no brain emoji: {rendered}");

    // Legacy "running: <cmd>" events still map to a bash line for back-compat.
    let legacy = vec![ProgressEvent {
        seq: 0,
        kind: "tool".into(),
        text: "running: ls -la".into(),
    }];
    assert!(render_progress_html(&legacy).contains("🛠️ ls -la"));

    // HTML-special characters in the detail are escaped inside the <pre>.
    let unsafe_detail = vec![ProgressEvent {
        seq: 0,
        kind: "tool".into(),
        text: "bash\u{1f}grep <foo> & bar".into(),
    }];
    let escaped = render_progress_html(&unsafe_detail);
    assert!(escaped.contains("grep &lt;foo&gt; &amp; bar"), "{escaped}");

    // "thinking" events are ignored (no brain-dump); final text shows plain.
    let mixed = vec![
        ProgressEvent {
            seq: 0,
            kind: "thinking".into(),
            text: "Looking it up".into(),
        },
        ProgressEvent {
            seq: 1,
            kind: "text".into(),
            text: "Here is the answer.".into(),
        },
    ];
    let r = render_progress_html(&mixed);
    assert!(r.contains("Here is the answer."));
    assert!(!r.contains("Looking it up"), "thinking suppressed: {r}");

    // Unknown tool key falls back to 🧩 + title-cased key.
    let unknown = vec![ProgressEvent {
        seq: 0,
        kind: "tool".into(),
        text: "record_voice\u{1f}cue".into(),
    }];
    assert!(render_progress_html(&unknown).contains("🧩 Record Voice: cue"));
}

impl TempDir {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn temp_dir(name: &str) -> TempDir {
    let path = std::env::temp_dir().join(format!(
        "maturana-{name}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap()
    ));
    fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
