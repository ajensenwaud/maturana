//! Lifecycle event hooks — host-side reactions to agent events.
//!
//! When an agent event fires (a message arrives, a turn ends, a schedule runs),
//! the host runs any matching [`Hook`]s from the agent's spec: a shell command,
//! a webhook POST, or a follow-up turn. Everything runs ON THE HOST — a hook can
//! observe and react to an agent without ever entering the guest VM, so the
//! zero-trust boundary is preserved. Dispatch is best-effort and logged: a
//! failing hook never breaks the turn it reacted to.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::spec::AgentSpec;
// Re-export the spec-defined hook types so callers can use the ergonomic
// `maturana_core::hooks::{Hook, HookAction, HookEvent}` path.
pub use crate::spec::{Hook, HookAction, HookEvent};

/// The context for a fired event, surfaced to commands (as `MATURANA_HOOK_*`
/// env vars) and webhooks (as a JSON body).
#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: HookEvent,
    pub agent_id: String,
    pub channel: Option<String>,
    pub text: String,
    /// Event-specific extra fields (e.g. schedule name, error detail).
    pub fields: BTreeMap<String, String>,
}

impl HookContext {
    pub fn new(event: HookEvent, agent_id: impl Into<String>) -> Self {
        Self {
            event,
            agent_id: agent_id.into(),
            channel: None,
            text: String::new(),
            fields: BTreeMap::new(),
        }
    }
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }
    pub fn field(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }
}

/// Callback the CLI injects so an `enqueue-turn` hook can reach the channel
/// pipeline (which lives above core). `(agent_id, prompt) -> result`.
pub type EnqueueHookFn<'a> = dyn Fn(&str, &str) -> anyhow::Result<()> + 'a;

const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Fire every hook in `spec` that matches `ctx.event` (and any filter).
/// Best-effort: each hook's outcome is logged and errors never propagate. Call
/// this off the hot path (e.g. `std::thread::spawn`) — command/webhook actions
/// can block up to the per-hook timeout.
pub fn fire(spec: &AgentSpec, ctx: &HookContext, enqueue: Option<&EnqueueHookFn>) {
    fire_list(&spec.hooks.on, ctx, enqueue)
}

/// Core dispatch over a hook list — separated from [`fire`] so it is testable
/// without constructing a whole [`AgentSpec`].
pub fn fire_list(hooks: &[Hook], ctx: &HookContext, enqueue: Option<&EnqueueHookFn>) {
    for hook in hooks {
        if !hook.enabled || hook.event != ctx.event {
            continue;
        }
        if let Some(filter) = &hook.filter {
            if !ctx.text.to_lowercase().contains(&filter.to_lowercase()) {
                continue;
            }
        }
        let label = hook
            .name
            .clone()
            .unwrap_or_else(|| ctx.event.as_str().to_string());
        match run_one(hook, ctx, enqueue) {
            Ok(()) => eprintln!(
                "[hook:{}] '{}' fired on {}",
                ctx.agent_id,
                label,
                ctx.event.as_str()
            ),
            Err(error) => eprintln!("[hook:{}] '{}' failed: {error}", ctx.agent_id, label),
        }
    }
}

fn run_one(hook: &Hook, ctx: &HookContext, enqueue: Option<&EnqueueHookFn>) -> anyhow::Result<()> {
    match &hook.action {
        HookAction::Command { command } => run_command(command, ctx),
        HookAction::Webhook { url } => post_webhook(url, ctx),
        HookAction::EnqueueTurn { prompt, agent } => {
            let target = agent.clone().unwrap_or_else(|| ctx.agent_id.clone());
            match enqueue {
                Some(handler) => handler(&target, prompt),
                None => anyhow::bail!("enqueue-turn hook fired but no enqueue handler is wired"),
            }
        }
    }
}

fn run_command(command: &str, ctx: &HookContext) -> anyhow::Result<()> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.env("MATURANA_HOOK_EVENT", ctx.event.as_str())
        .env("MATURANA_HOOK_AGENT", &ctx.agent_id)
        .env("MATURANA_HOOK_TEXT", &ctx.text);
    if let Some(channel) = &ctx.channel {
        cmd.env("MATURANA_HOOK_CHANNEL", channel);
    }
    for (key, value) in &ctx.fields {
        cmd.env(format!("MATURANA_HOOK_{}", env_key(key)), value);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + HOOK_TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("command timed out after {}s", HOOK_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        anyhow::bail!(
            "command exited unsuccessfully: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Uppercase a field key and replace any non-alphanumeric char with `_`, so it
/// is a safe environment-variable suffix.
fn env_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn post_webhook(url: &str, ctx: &HookContext) -> anyhow::Result<()> {
    let mut payload = serde_json::Map::new();
    payload.insert("event".into(), ctx.event.as_str().into());
    payload.insert("agent".into(), ctx.agent_id.clone().into());
    if let Some(channel) = &ctx.channel {
        payload.insert("channel".into(), channel.clone().into());
    }
    payload.insert("text".into(), ctx.text.clone().into());
    for (key, value) in &ctx.fields {
        payload
            .entry(key.clone())
            .or_insert_with(|| value.clone().into());
    }
    match ureq::post(url)
        .timeout(HOOK_TIMEOUT)
        .send_json(serde_json::Value::Object(payload))
    {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, _)) => anyhow::bail!("webhook returned HTTP {code}"),
        Err(error) => Err(anyhow::anyhow!("webhook request failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::HookAction;
    use std::sync::Mutex;

    fn hook(event: HookEvent, action: HookAction) -> Hook {
        Hook {
            name: None,
            event,
            filter: None,
            action,
            enabled: true,
        }
    }

    #[test]
    fn enqueue_hook_invokes_handler_for_matching_event_only() {
        let calls: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
        let handler = |agent: &str, prompt: &str| {
            calls.lock().unwrap().push((agent.to_string(), prompt.to_string()));
            Ok(())
        };
        let hooks = vec![
            hook(
                HookEvent::TurnEnd,
                HookAction::EnqueueTurn {
                    prompt: "summarize".into(),
                    agent: Some("scribe".into()),
                },
            ),
            hook(
                HookEvent::MessageIn,
                HookAction::EnqueueTurn {
                    prompt: "should not fire".into(),
                    agent: None,
                },
            ),
        ];
        let ctx = HookContext::new(HookEvent::TurnEnd, "codex");
        fire_list(&hooks, &ctx, Some(&handler));
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.as_slice(), &[("scribe".to_string(), "summarize".to_string())]);
    }

    #[test]
    fn enqueue_hook_defaults_to_the_firing_agent() {
        let calls: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
        let handler = |agent: &str, prompt: &str| {
            calls.lock().unwrap().push((agent.to_string(), prompt.to_string()));
            Ok(())
        };
        let hooks = vec![hook(
            HookEvent::Error,
            HookAction::EnqueueTurn {
                prompt: "self-diagnose".into(),
                agent: None,
            },
        )];
        let ctx = HookContext::new(HookEvent::Error, "humberto");
        fire_list(&hooks, &ctx, Some(&handler));
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &[("humberto".to_string(), "self-diagnose".to_string())]
        );
    }

    #[test]
    fn filter_is_a_case_insensitive_substring_gate() {
        let calls: Mutex<usize> = Mutex::new(0);
        let handler = |_a: &str, _p: &str| {
            *calls.lock().unwrap() += 1;
            Ok(())
        };
        let mut h = hook(
            HookEvent::MessageIn,
            HookAction::EnqueueTurn {
                prompt: "x".into(),
                agent: None,
            },
        );
        h.filter = Some("Deploy".into());
        let hooks = vec![h];
        fire_list(
            &hooks,
            &HookContext::new(HookEvent::MessageIn, "a").text("please DEPLOY now"),
            Some(&handler),
        );
        fire_list(
            &hooks,
            &HookContext::new(HookEvent::MessageIn, "a").text("unrelated"),
            Some(&handler),
        );
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn disabled_hook_does_not_fire() {
        let calls: Mutex<usize> = Mutex::new(0);
        let handler = |_a: &str, _p: &str| {
            *calls.lock().unwrap() += 1;
            Ok(())
        };
        let mut h = hook(
            HookEvent::TurnEnd,
            HookAction::EnqueueTurn {
                prompt: "x".into(),
                agent: None,
            },
        );
        h.enabled = false;
        fire_list(
            &[h],
            &HookContext::new(HookEvent::TurnEnd, "a"),
            Some(&handler),
        );
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[test]
    fn command_action_runs_and_reports_failure() {
        // Success: a trivial true-equivalent command on either platform.
        let ok = run_command("exit 0", &HookContext::new(HookEvent::TurnEnd, "a"));
        assert!(ok.is_ok(), "exit 0 should succeed: {ok:?}");
        // Failure: a non-zero exit is surfaced as an error.
        let bad = run_command("exit 3", &HookContext::new(HookEvent::TurnEnd, "a"));
        assert!(bad.is_err(), "exit 3 should fail");
    }

    #[test]
    fn env_key_sanitizes() {
        assert_eq!(env_key("schedule.name"), "SCHEDULE_NAME");
        assert_eq!(env_key("Error-Detail"), "ERROR_DETAIL");
    }
}
