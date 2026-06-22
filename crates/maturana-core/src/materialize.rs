use crate::{
    audit::{append_event, AuditEvent},
    providers::{
        firecracker::FirecrackerProvider, hyperv::HyperVProvider, LiveAgentStatus, Provider,
        ProviderCommand,
    },
    spec::{AgentSpec, HostProvider},
    state::MaturanaHome,
    validation::{validate_spec, ValidationReport},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    DryRun,
    Apply,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedAgent {
    pub agent_id: String,
    pub agent_dir: PathBuf,
    pub validation: ValidationReport,
    pub provider_commands: Vec<ProviderCommand>,
}

pub fn materialize_agent(
    spec: &AgentSpec,
    source_markdown: &str,
    home: &MaturanaHome,
    mode: LaunchMode,
) -> anyhow::Result<MaterializedAgent> {
    let validation = validate_spec(spec);
    if !validation.valid {
        anyhow::bail!("spec validation failed: {}", validation.errors.join("; "));
    }

    let agent_dir = home.agent_dir(&spec.identity.id);
    fs::create_dir_all(agent_dir.join("state"))?;
    fs::create_dir_all(agent_dir.join("workspace"))?;
    fs::create_dir_all(agent_dir.join("memory"))?;
    fs::create_dir_all(agent_dir.join("snapshots"))?;

    fs::write(agent_dir.join("MATURANA.md"), source_markdown)?;
    fs::write(agent_dir.join("AGENTS.md"), render_guest_agents(spec))?;
    // IDENTITY.md (who the agent is + who its owner is) and SOUL.md (voice,
    // values, behavior) are authored personality files. Scaffold them only when
    // absent so the setup wizard's / user's authored versions are never
    // clobbered on re-materialize.
    write_if_absent(&agent_dir.join("IDENTITY.md"), || render_identity(spec))?;
    write_if_absent(&agent_dir.join("SOUL.md"), || render_soul(spec))?;

    let provider: Box<dyn Provider> = match spec.vm.provider {
        HostProvider::HyperV => Box::new(HyperVProvider),
        HostProvider::Firecracker => Box::new(FirecrackerProvider),
    };

    let commands = provider.plan_launch(spec, &agent_dir)?;
    fs::write(
        agent_dir.join("launch-plan.json"),
        serde_json::to_string_pretty(&commands)?,
    )?;

    append_event(
        home.audit_dir().join(format!("{}.jsonl", spec.identity.id)),
        &AuditEvent {
            at: Utc::now(),
            agent_id: spec.identity.id.clone(),
            action: match mode {
                LaunchMode::DryRun => "launch.dry-run".to_string(),
                LaunchMode::Apply => "launch.apply".to_string(),
            },
            message: format!("materialized {}", agent_dir.display()),
        },
    )?;

    if mode == LaunchMode::Apply {
        provider.launch(spec, &agent_dir)?;
    }

    Ok(MaterializedAgent {
        agent_id: spec.identity.id.clone(),
        agent_dir,
        validation,
        provider_commands: commands,
    })
}

pub fn stop_agent(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<()> {
    let agent_dir = home.agent_dir(agent_id);
    let spec_path = agent_dir.join("MATURANA.md");
    if !spec_path.exists() {
        anyhow::bail!("agent does not exist or has no MATURANA.md: {agent_id}");
    }
    let spec = AgentSpec::from_maturana_markdown(&spec_path)?;
    let provider: Box<dyn Provider> = match spec.vm.provider {
        HostProvider::HyperV => Box::new(HyperVProvider),
        HostProvider::Firecracker => Box::new(FirecrackerProvider),
    };
    provider.stop(&spec, &agent_dir)?;
    append_event(
        home.audit_dir().join(format!("{agent_id}.jsonl")),
        &AuditEvent {
            at: Utc::now(),
            agent_id: agent_id.to_string(),
            action: "agent.stop.live".to_string(),
            message: format!(
                "stopped {} provider agent",
                provider_name(&spec.vm.provider)
            ),
        },
    )?;
    Ok(())
}

pub fn inspect_agent(home: &MaturanaHome, agent_id: &str) -> anyhow::Result<LiveAgentStatus> {
    let agent_dir = home.agent_dir(agent_id);
    let spec_path = agent_dir.join("MATURANA.md");
    if !spec_path.exists() {
        anyhow::bail!("agent does not exist or has no MATURANA.md: {agent_id}");
    }
    let spec = AgentSpec::from_maturana_markdown(&spec_path)?;
    let provider: Box<dyn Provider> = match spec.vm.provider {
        HostProvider::HyperV => Box::new(HyperVProvider),
        HostProvider::Firecracker => Box::new(FirecrackerProvider),
    };
    provider.inspect(&spec, &agent_dir)
}

fn provider_name(provider: &HostProvider) -> &'static str {
    match provider {
        HostProvider::HyperV => "Hyper-V",
        HostProvider::Firecracker => "Firecracker",
    }
}

fn render_guest_agents(spec: &AgentSpec) -> String {
    let mut out = format!(
        "# {}\n\nYou are a Maturana worker agent.\n\nPurpose: {}\n\nOperate only inside the mounted workspace and obey the MATURANA.md contract.\n",
        spec.identity.name, spec.identity.purpose
    );

    // The in-VM agent only knows a capability exists if its recipe is here — the
    // skills/ library is installed on the host, not in the guest. So inline a
    // concise, accurate invocation for each capability this agent actually has.
    let mut recipes: Vec<String> = Vec::new();

    if spec.knowledge_graph.enabled {
        recipes.push(
            "### Memory (MaturanaGraph)\nYou have a private knowledge graph + GraphRAG (service \
             URL + token are in your worker env). Use the `maturana-graph` skill to store durable \
             facts and recall them across turns instead of relying on the chat window."
                .to_string(),
        );
    }

    let egress = &spec.network.egress_allowlist;
    let allows = |needle: &str| egress.iter().any(|h| h.contains(needle));
    if allows("brave") || allows("tavily") {
        let mut s = String::from(
            "### Web search\nFor live web facts, curl the allowlisted search API through the proxy \
             — send NO key header, the pipelock proxy injects it:\n\
             \x20   curl -fsS \"https://api.search.brave.com/res/v1/web/search?q=<terms>&count=5\" -H \"Accept: application/json\"\n\
             \x20   # or Tavily:\n\
             \x20   curl -fsS -X POST \"https://api.tavily.com/search\" -H \"content-type: application/json\" --data '{\"query\":\"<terms>\",\"max_results\":5}'\n\
             Read web.results[].{title,url,description} (Brave) or results[].{title,url,content} (Tavily) and cite the URLs.",
        );
        if allows("duckduckgo") {
            s.push_str(
                "\nIf the API errors or is rate-limited, fall back to the no-JS DuckDuckGo HTML \
                 endpoint and parse result links from the HTML:\n\
                 \x20   curl -fsS \"https://html.duckduckgo.com/html/?q=<terms>\"",
            );
        }
        s.push_str(
            "\nOnly the hosts on your egress allowlist are reachable — do NOT try google.com, news \
             sites, or arbitrary pages; they are blocked at the proxy.",
        );
        recipes.push(s);
    }

    if spec.browser.headless_chrome {
        recipes.push(
            "### Browse live pages\nA headless-Chrome driver is installed (one JSON arg in, one \
             JSON line out). The target host must be in your egress allowlist:\n\
             \x20   node /opt/maturana/bin/browse.js '{\"cmd\":\"text\",\"url\":\"https://example.com\",\"selector\":\"main\"}'\n\
             \x20   node /opt/maturana/bin/browse.js '{\"cmd\":\"screenshot\",\"url\":\"https://example.com\",\"out\":\"/workspace/page.png\"}'\n\
             Read ok, status, url, title, text from the JSON result."
                .to_string(),
        );
    }

    if spec.capabilities.image_gen {
        recipes.push(
            "### Generate images\nPOST the OpenAI images API through the proxy (key injected; send \
             NO Authorization header). Save PNGs under /workspace:\n\
             \x20   curl -fsS https://api.openai.com/v1/images/generations -H \"content-type: application/json\" --data '{\"model\":\"gpt-image-1\",\"prompt\":\"<prompt>\",\"size\":\"1024x1024\",\"n\":1}' | python3 -c 'import sys,json,base64; d=json.load(sys.stdin); open(\"/workspace/out.png\",\"wb\").write(base64.b64decode(d[\"data\"][0][\"b64_json\"]))'\n\
             Report the saved path."
                .to_string(),
        );
    }

    if spec.capabilities.self_forge {
        recipes.push(
            "### Self-forge — build a tool on the fly (self-mutation)\nWhen a task needs computation \
             or transformation you don't already have, author a small WebAssembly capability and run \
             it immediately, the same turn, in a fuel/memory/timeout sandbox — no host rebuild. Use \
             the `maturana-forge` helper (WAT on stdin, or `--wasm <base64>`):\n\
             \x20   maturana-forge <name> --input '{\"n\":7}' <<'WAT'\n\
             \x20   (module ;; ... compute, write the result to stdout (fd 1) via wasi fd_write ...\n\
             \x20     (func (export \"_start\") ...))\n\
             \x20   WAT\n\
             It returns the module's stdout (also a 🔨/⚙️ animation on the channel). The sandbox has \
             NO network or filesystem — it is for pure computation/transforms only, so it cannot \
             fetch URLs, read email, or call APIs. Forge sparingly; then say what you built and what \
             it returned."
                .to_string(),
        );
    }

    if !spec.mcp_servers.is_empty() {
        let names = spec
            .mcp_servers
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        recipes.push(format!(
            "### MCP tools\nYour harness is wired (with host-resolved auth) to these \
             Model-Context-Protocol servers: {names}. Use their tools natively through the harness."
        ));
    }

    if !recipes.is_empty() {
        out.push_str("\n## Capabilities available to you\n\n");
        out.push_str(&recipes.join("\n\n"));
        out.push('\n');
    }

    // How to delegate a sub-task to a PEER agent over A2A (Agent2Agent). The host
    // runs an A2A server reachable over the TAP at the sessiond host, port 47837.
    out.push_str(
        r#"
## Delegating to another agent (A2A)

When a sub-task is better handled by a different agent (a coding task while you
reason; research while you write; a second opinion), delegate it to a peer over
the Agent2Agent (A2A) protocol and use the result, instead of doing everything
yourself. The host runs an A2A server reachable from here at your sessiond host on
port 47837. Send a JSON-RPC `message/send`:

```bash
A2A="$(printf '%s' "$MATURANA_SESSIOND_URL" | sed 's/:47834/:47837/')"
curl -s -X POST "$A2A/a2a/<PEER_AGENT_ID>" \
  -H "x-maturana-session-token: $MATURANA_SESSIOND_TOKEN" \
  -d '{"jsonrpc":"2.0","id":1,"method":"message/send","params":{"message":{"role":"user","parts":[{"kind":"text","text":"<THE TASK FOR THE PEER>"}],"messageId":"d1","kind":"message","metadata":{"maturana_caller":"'"$MATURANA_AGENT_ID"'","maturana_depth":1}}}}'
```

The reply is an A2A Task: read `result.artifacts[0].parts[0].text` for the peer's
answer (`result.status.state` is `completed`, or `failed` with a reason in
`result.status.message`). A peer's id is another agent in your fleet (e.g.
`claude-firecracker`, `codex-firecracker`, `opencode-firecracker`); GET
`$A2A/a2a/<peer>/.well-known/agent-card.json` to see what it offers. Keep
delegations shallow and few — the host caps nesting depth and refuses an agent
delegating to itself.
"#,
    );

    // Be explicit about the locked boundary so the agent fails fast + honestly on
    // impossible asks (e.g. "check my iCloud email") instead of looping on blocked
    // hosts or writing code that can't run here.
    out.push_str(&format!(
        "\n## Limits — be honest, don't stall\nYour network egress is locked to: {}. No other host \
         is reachable (no arbitrary web pages, and no email/IMAP/SMTP unless a tool above provides \
         it). You also cannot create a new *loadable skill* yourself — skills are installed \
         host-side. When a request needs a capability or host you don't have, say so plainly in your \
         first reply and offer the closest thing you can do; do NOT loop on blocked actions or \
         produce code that can't run here.\n",
        if egress.is_empty() {
            "(nothing — fully offline)".to_string()
        } else {
            egress.join(", ")
        }
    ));

    out
}

fn write_if_absent<F: FnOnce() -> String>(path: &std::path::Path, content: F) -> std::io::Result<()> {
    if path.exists() {
        Ok(())
    } else {
        fs::write(path, content())
    }
}

/// Rich scaffold for IDENTITY.md: who the agent is and who its owner is. The
/// setup wizard fills the angle-bracket prompts from the interview; left as-is it
/// still reads as a usable template.
fn render_identity(spec: &AgentSpec) -> String {
    format!(
        "# Identity — {name}\n\
         <!-- id: {id} -->\n\n\
         ## Who I am\n\
         {name} — {purpose}\n\n\
         <Expand: my role, what I help with, and why I exist.>\n\n\
         ## Who you are to me\n\
         <Your owner: name, how to address you, timezone, working hours, and what\n\
         you rely on me for.>\n\n\
         ## Scope & boundaries\n\
         - In scope: <what I should do>\n\
         - Out of scope: <what I must not do without asking>\n\n\
         ## How we work together\n\
         <Channels you reach me on, when to ping you, response expectations.>\n",
        name = spec.identity.name,
        id = spec.identity.id,
        purpose = spec.identity.purpose,
    )
}

/// Rich scaffold for SOUL.md: the durable personality + operating posture.
fn render_soul(spec: &AgentSpec) -> String {
    format!(
        "# Soul — {name}\n\n\
         My durable personality and posture across every conversation. Edit freely.\n\n\
         ## Voice\n\
         <Tone, formality, brevity, humor — how I should sound.>\n\n\
         ## Values\n\
         - Secure, bounded, inspectable, and reversible by default.\n\
         - <Your values…>\n\n\
         ## Behavior\n\
         - Do: <…>\n\
         - Don't: <…>\n\
         - Never request credentials directly; use declared credential sources only.\n\n\
         ## Memory & continuity\n\
         I persist durable facts to memory and shared context to the wiki; I do not\n\
         rely on the chat window to remember.\n",
        name = spec.identity.name,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::AgentSpec;

    fn spec_from_yaml(yaml: &str) -> AgentSpec {
        serde_yaml::from_str(yaml).expect("spec parses")
    }

    #[test]
    fn guest_agents_emits_search_forge_and_limits_when_granted() {
        let spec = spec_from_yaml(
            "identity:\n  id: oc\n  name: OC Agent\n  purpose: test\nruntime:\n  harness: opencode\nvm:\n  provider: firecracker\nnetwork:\n  egress_allowlist:\n    - api.search.brave.com\n    - duckduckgo.com\n    - en.wikipedia.org\ncapabilities:\n  self_forge: true\n",
        );
        let out = render_guest_agents(&spec);
        // web-search recipe + DDG fallback + allowlist-only honesty
        assert!(out.contains("### Web search"), "missing web search recipe:\n{out}");
        assert!(out.contains("html.duckduckgo.com"), "missing DDG fallback:\n{out}");
        assert!(out.contains("Only the hosts on your egress allowlist"), "missing allowlist-only note:\n{out}");
        // self-forge recipe is emitted when the capability is granted
        assert!(out.contains("### Self-forge"), "missing self-forge recipe:\n{out}");
        assert!(out.contains("maturana-forge"), "missing forge helper:\n{out}");
        // honest limits block lists the locked egress
        assert!(out.contains("## Limits"), "missing limits block:\n{out}");
        assert!(out.contains("api.search.brave.com"), "limits should list egress:\n{out}");
    }

    #[test]
    fn guest_agents_gates_forge_and_ddg_but_always_has_limits() {
        let spec = spec_from_yaml(
            "identity:\n  id: oc\n  name: OC\n  purpose: test\nruntime:\n  harness: opencode\nvm:\n  provider: firecracker\nnetwork:\n  egress_allowlist:\n    - github.com\n",
        );
        let out = render_guest_agents(&spec);
        // forge recipe is gated on self_forge; DDG line gated on duckduckgo in egress
        assert!(!out.contains("### Self-forge"), "forge recipe must be gated on self_forge:\n{out}");
        assert!(!out.contains("html.duckduckgo.com"), "DDG fallback only when duckduckgo allowlisted:\n{out}");
        // the honest limits block is unconditional
        assert!(out.contains("## Limits"), "limits block must always be present:\n{out}");
    }
}
