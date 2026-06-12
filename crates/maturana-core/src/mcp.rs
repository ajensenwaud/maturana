//! Render in-guest, harness-native MCP (Model Context Protocol) config from the
//! agent spec's `mcp_servers`, so the guest harness connects to the declared
//! servers on its own. Secrets in `env` are resolved **host-side** at render
//! time and written as literal values into the config the guest reads — they
//! never live in the spec, and the server's outbound traffic is still bounded
//! by the egress allowlist (the proxy auto-allows each server's `egress_hosts`).
//!
//! Per harness (formats verified against codex 0.139 `mcp add`):
//! - codex      → `$CODEX_HOME/config.toml`, `[mcp_servers.<name>]` tables,
//!   MERGED into the operator's existing config.toml (preserves model/settings).
//! - claude-code → `~/.claude.json` `mcpServers` map (user scope; what
//!   `claude -p` reads), merged with any existing file.
//! - opencode   → `~/.config/opencode/opencode.json` `mcp` map (best-effort).

use std::path::Path;

use crate::secrets::resolve_secret_source_with_home;
use crate::spec::{HarnessRuntime, McpServer, McpTransport};

/// A rendered config file plus the absolute guest path it must be written to.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedMcpConfig {
    pub guest_path: String,
    pub contents: String,
}

/// Render the MCP config for `harness`. `host_auth_dir` is the host-side
/// `.maturana/host-auth/<harness>/` directory whose existing config (if any) is
/// merged so operator settings survive. Returns `None` when there are no
/// servers (so callers can skip the push entirely).
pub fn render_mcp_config(
    harness: &HarnessRuntime,
    servers: &[McpServer],
    home_root: &Path,
    host_auth_dir: &Path,
) -> anyhow::Result<Option<RenderedMcpConfig>> {
    if servers.is_empty() {
        return Ok(None);
    }
    let rendered = match harness {
        HarnessRuntime::Codex => render_codex(servers, home_root, host_auth_dir)?,
        HarnessRuntime::ClaudeCode => render_claude(servers, home_root, host_auth_dir)?,
        HarnessRuntime::Opencode => render_opencode(servers, home_root)?,
    };
    Ok(Some(rendered))
}

/// Resolve each `env` source to a literal `{NAME: value}` map, host-side.
fn resolve_env(
    server: &McpServer,
    home_root: &Path,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let mut env = serde_json::Map::new();
    for var in &server.env {
        let value = resolve_secret_source_with_home(&var.source, home_root)
            .map_err(|e| anyhow::anyhow!("mcp server '{}' env {}: {e}", server.name, var.name))?;
        env.insert(
            var.name.clone(),
            serde_json::Value::String(value.expose_for_runtime().to_string()),
        );
    }
    Ok(env)
}

fn render_codex(
    servers: &[McpServer],
    home_root: &Path,
    host_auth_dir: &Path,
) -> anyhow::Result<RenderedMcpConfig> {
    // Merge into the operator's config.toml so model/sandbox/etc. survive.
    let base = std::fs::read_to_string(host_auth_dir.join("config.toml")).unwrap_or_default();
    let mut doc: toml::Value = if base.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        base.parse()
            .map_err(|e| anyhow::anyhow!("existing codex config.toml is not valid TOML: {e}"))?
    };
    let root = doc
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("codex config.toml root is not a table"))?;
    let mcp = root
        .entry("mcp_servers".to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("codex mcp_servers is not a table"))?;
    for server in servers {
        let mut entry = toml::map::Map::new();
        match server.transport {
            McpTransport::Stdio => {
                entry.insert(
                    "command".into(),
                    toml::Value::String(server.command.clone().unwrap_or_default()),
                );
                entry.insert(
                    "args".into(),
                    toml::Value::Array(
                        server.args.iter().cloned().map(toml::Value::String).collect(),
                    ),
                );
            }
            McpTransport::Http => {
                entry.insert(
                    "url".into(),
                    toml::Value::String(server.url.clone().unwrap_or_default()),
                );
            }
        }
        if !server.env.is_empty() {
            let mut env_tbl = toml::map::Map::new();
            for (k, v) in resolve_env(server, home_root)? {
                env_tbl.insert(k, toml::Value::String(v.as_str().unwrap_or("").to_string()));
            }
            entry.insert("env".into(), toml::Value::Table(env_tbl));
        }
        mcp.insert(server.name.clone(), toml::Value::Table(entry));
    }
    Ok(RenderedMcpConfig {
        guest_path: "/home/ubuntu/.codex/config.toml".to_string(),
        contents: toml::to_string_pretty(&doc)?,
    })
}

fn render_claude(
    servers: &[McpServer],
    home_root: &Path,
    host_auth_dir: &Path,
) -> anyhow::Result<RenderedMcpConfig> {
    // ~/.claude.json is what `claude -p` reads for user-scoped MCP servers.
    let base = std::fs::read_to_string(host_auth_dir.join(".claude.json")).unwrap_or_default();
    let mut doc: serde_json::Value = if base.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&base)
            .map_err(|e| anyhow::anyhow!("existing .claude.json is not valid JSON: {e}"))?
    };
    let map = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!(".claude.json root is not an object"))?;
    let servers_obj = map
        .entry("mcpServers".to_string())
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("mcpServers is not an object"))?;
    for server in servers {
        let mut entry = serde_json::Map::new();
        match server.transport {
            McpTransport::Stdio => {
                entry.insert("type".into(), "stdio".into());
                entry.insert(
                    "command".into(),
                    server.command.clone().unwrap_or_default().into(),
                );
                entry.insert("args".into(), serde_json::json!(server.args));
                let env = resolve_env(server, home_root)?;
                if !env.is_empty() {
                    entry.insert("env".into(), serde_json::Value::Object(env));
                }
            }
            McpTransport::Http => {
                entry.insert("type".into(), "http".into());
                entry.insert("url".into(), server.url.clone().unwrap_or_default().into());
            }
        }
        servers_obj.insert(server.name.clone(), serde_json::Value::Object(entry));
    }
    Ok(RenderedMcpConfig {
        guest_path: "/home/ubuntu/.claude.json".to_string(),
        contents: serde_json::to_string_pretty(&doc)?,
    })
}

fn render_opencode(servers: &[McpServer], home_root: &Path) -> anyhow::Result<RenderedMcpConfig> {
    let mut mcp = serde_json::Map::new();
    for server in servers {
        let mut entry = serde_json::Map::new();
        match server.transport {
            McpTransport::Stdio => {
                entry.insert("type".into(), "local".into());
                let mut command = vec![server.command.clone().unwrap_or_default()];
                command.extend(server.args.iter().cloned());
                entry.insert("command".into(), serde_json::json!(command));
                let env = resolve_env(server, home_root)?;
                if !env.is_empty() {
                    entry.insert("environment".into(), serde_json::Value::Object(env));
                }
            }
            McpTransport::Http => {
                entry.insert("type".into(), "remote".into());
                entry.insert("url".into(), server.url.clone().unwrap_or_default().into());
            }
        }
        entry.insert("enabled".into(), serde_json::Value::Bool(true));
        mcp.insert(server.name.clone(), serde_json::Value::Object(entry));
    }
    let doc = serde_json::json!({ "$schema": "https://opencode.ai/config.json", "mcp": mcp });
    Ok(RenderedMcpConfig {
        guest_path: "/home/ubuntu/.config/opencode/opencode.json".to_string(),
        contents: serde_json::to_string_pretty(&doc)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{McpEnvVar, McpServer, McpTransport};
    use uuid::Uuid;

    fn home_with_secret(name: &str, value: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("mcp-test-{}", Uuid::new_v4()));
        let vault = crate::pipelock::PipelockVault::new(home.join("pipelock"));
        vault.set(name, value).unwrap();
        home
    }

    fn notion() -> McpServer {
        McpServer {
            name: "notion".into(),
            transport: McpTransport::Stdio,
            command: Some("npx".into()),
            args: vec!["-y".into(), "@notionhq/notion-mcp-server".into()],
            url: None,
            env: vec![McpEnvVar {
                name: "NOTION_TOKEN".into(),
                source: "pipelock:notion/integration-token".into(),
            }],
            egress_hosts: vec!["api.notion.com".into()],
        }
    }

    #[test]
    fn no_servers_renders_none() {
        let home = std::env::temp_dir();
        assert!(render_mcp_config(&HarnessRuntime::Codex, &[], &home, &home)
            .unwrap()
            .is_none());
    }

    #[test]
    fn renders_claude_mcp_json_with_resolved_env() {
        let home = home_with_secret("notion/integration-token", "ntn_secret");
        let rendered = render_mcp_config(&HarnessRuntime::ClaudeCode, &[notion()], &home, &home)
            .unwrap()
            .unwrap();
        assert_eq!(rendered.guest_path, "/home/ubuntu/.claude.json");
        let v: serde_json::Value = serde_json::from_str(&rendered.contents).unwrap();
        let n = &v["mcpServers"]["notion"];
        assert_eq!(n["type"], "stdio");
        assert_eq!(n["command"], "npx");
        assert_eq!(n["env"]["NOTION_TOKEN"], "ntn_secret");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn merges_codex_toml_preserving_existing() {
        let home = home_with_secret("notion/integration-token", "ntn_secret");
        let auth_dir = home.join("host-auth-codex");
        std::fs::create_dir_all(&auth_dir).unwrap();
        std::fs::write(
            auth_dir.join("config.toml"),
            "model = \"gpt-5.5\"\n[mcp_servers.existing]\ncommand = \"keep\"\n",
        )
        .unwrap();
        let rendered = render_mcp_config(&HarnessRuntime::Codex, &[notion()], &home, &auth_dir)
            .unwrap()
            .unwrap();
        assert_eq!(rendered.guest_path, "/home/ubuntu/.codex/config.toml");
        let doc: toml::Value = rendered.contents.parse().unwrap();
        // Operator settings and the pre-existing server survive.
        assert_eq!(doc["model"].as_str(), Some("gpt-5.5"));
        assert_eq!(doc["mcp_servers"]["existing"]["command"].as_str(), Some("keep"));
        // Ours is added with resolved env.
        assert_eq!(doc["mcp_servers"]["notion"]["command"].as_str(), Some("npx"));
        assert_eq!(
            doc["mcp_servers"]["notion"]["env"]["NOTION_TOKEN"].as_str(),
            Some("ntn_secret")
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn renders_http_transport() {
        let home = std::env::temp_dir();
        let server = McpServer {
            name: "remote".into(),
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            url: Some("https://mcp.example.com/sse".into()),
            env: vec![],
            egress_hosts: vec![],
        };
        let claude = render_mcp_config(&HarnessRuntime::ClaudeCode, &[server.clone()], &home, &home)
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&claude.contents).unwrap();
        assert_eq!(v["mcpServers"]["remote"]["type"], "http");
        assert_eq!(v["mcpServers"]["remote"]["url"], "https://mcp.example.com/sse");
    }
}
