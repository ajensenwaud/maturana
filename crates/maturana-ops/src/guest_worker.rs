use crate::ssh::{
    copy_path_to_guest, run_ssh_with_stdin, shell_quote, GuestHostKey, SSH_TIMEOUT_PROVISION,
    SSH_TIMEOUT_QUICK,
};
use maturana_core::{
    spec::{AgentSpec, HarnessRuntime},
    state::MaturanaHome,
    worker::{
        read_graph_token, render_harness_install, render_run_agent, render_session_env,
        GuestWorkerConfig,
    },
};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone)]
pub struct GuestWorkerInstall {
    pub agent_id: String,
    pub session_id: String,
    pub harness: HarnessRuntime,
    pub guest_ip: String,
    pub ssh_user: String,
    pub ssh_key: PathBuf,
    pub harness_auth_guest_path: String,
    pub sessiond_url: String,
    pub sessiond_token_path: PathBuf,
    pub auth_source: Option<PathBuf>,
    pub install_harness: bool,
    /// Override the re-seed guard: push host auth even if the guest already has a
    /// live `.credentials.json`. Only for recovering a genuinely dead guest.
    pub force_reseed_auth: bool,
}

pub fn install_guest_worker(
    home: &MaturanaHome,
    install: GuestWorkerInstall,
) -> anyhow::Result<()> {
    let ssh_key = absolute_or_cwd(install.ssh_key)?;
    let sessiond_token = read_optional_trimmed(absolute_or_cwd(install.sessiond_token_path)?)?;

    let state_dir = home.agent_dir(&install.agent_id).join("state");
    fs::create_dir_all(&state_dir)?;
    let env_path = state_dir.join("sessiond.env");
    let runner_path = state_dir.join("run-agent.sh");
    // The post-boot re-render also carries the graph env so it isn't lost when a
    // worker is refreshed. Read the materialized spec for graph opt-in and MCP.
    let agent_spec =
        AgentSpec::from_maturana_markdown(&home.agent_dir(&install.agent_id).join("MATURANA.md"))
            .ok();
    let knowledge_graph = agent_spec
        .as_ref()
        .map(|spec| spec.knowledge_graph.clone())
        .unwrap_or_default();
    fs::write(
        &env_path,
        render_session_env(&GuestWorkerConfig {
            agent_id: install.agent_id.clone(),
            session_id: install.session_id.clone(),
            sessiond_url: install.sessiond_url.clone(),
            sessiond_token,
            harness: install.harness.clone(),
            harness_auth_guest_path: install.harness_auth_guest_path.clone(),
            headless_chrome: false,
            graph_token: read_graph_token(home.root()),
            graph_name: knowledge_graph
                .enabled
                .then(|| knowledge_graph.graph_name(&install.agent_id)),
        }),
    )?;
    fs::write(&runner_path, render_run_agent())?;

    // Verify the guest's host key before pushing sessiond tokens and harness
    // credentials over these connections.
    let host_key = GuestHostKey::resolve(home, &install.agent_id, &install.guest_ip)?;

    copy_path_to_guest(
        &install.guest_ip,
        &install.ssh_user,
        &ssh_key,
        &host_key,
        &env_path,
        "/tmp/sessiond.env",
        false,
    )?;
    copy_path_to_guest(
        &install.guest_ip,
        &install.ssh_user,
        &ssh_key,
        &host_key,
        &runner_path,
        "/tmp/run-agent.sh",
        false,
    )?;
    // Resolve + guard auth re-seed exactly once. Pushing host creds over a guest
    // that already self-refreshes its own single-use token can log it out.
    let auth_push_path: Option<PathBuf> = match install.auth_source.as_ref() {
        None => None,
        Some(src) => {
            let resolved = absolute_or_cwd(src.clone())?;
            if !resolved.exists() {
                None
            } else if install.harness == HarnessRuntime::ClaudeCode
                && !install.force_reseed_auth
                && guest_has_live_harness_creds(
                    &install.guest_ip,
                    &install.ssh_user,
                    &ssh_key,
                    &host_key,
                    &install.harness_auth_guest_path,
                )
            {
                eprintln!(
                    "guest-worker: NOT re-seeding claude auth for {} - guest {} already \
                     has a live {}/.credentials.json (it self-refreshes its own OAuth \
                     lineage). Overwriting it would consume a single-use refresh token \
                     and log the agent out. Pass --force-reseed-auth to override (only \
                     to recover a dead guest).",
                    install.agent_id, install.guest_ip, install.harness_auth_guest_path,
                );
                None
            } else {
                Some(resolved)
            }
        }
    };
    if let Some(auth_source) = auth_push_path.as_ref() {
        copy_path_to_guest(
            &install.guest_ip,
            &install.ssh_user,
            &ssh_key,
            &host_key,
            auth_source,
            "/tmp/maturana-harness-auth",
            true,
        )?;
    }
    if install.install_harness {
        run_ssh_with_stdin(
            &install.guest_ip,
            &install.ssh_user,
            &ssh_key,
            &host_key,
            &render_harness_install(&install.harness, false),
            None,
            SSH_TIMEOUT_PROVISION,
        )?;
    }
    if auth_push_path.is_some() {
        run_ssh_with_stdin(
            &install.guest_ip,
            &install.ssh_user,
            &ssh_key,
            &host_key,
            &render_auth_install_command(
                &install.harness,
                &install.ssh_user,
                &install.harness_auth_guest_path,
            ),
            None,
            SSH_TIMEOUT_PROVISION,
        )?;
    }
    // MCP config: render the harness-native file with host-side secret
    // resolution and place it where the in-guest harness reads it.
    if let Some(spec) = agent_spec.as_ref() {
        if !spec.mcp_servers.is_empty() {
            let host_auth_dir = install
                .auth_source
                .as_ref()
                .map(|p| absolute_or_cwd(p.clone()))
                .transpose()?
                .unwrap_or_else(|| {
                    home.root()
                        .join("host-auth")
                        .join(maturana_core::worker::harness_name(&install.harness))
                });
            if let Some(rendered) = maturana_core::mcp::render_mcp_config(
                &install.harness,
                &spec.mcp_servers,
                home.root(),
                &host_auth_dir,
            )? {
                let mcp_path = state_dir.join("mcp-config");
                fs::write(&mcp_path, &rendered.contents)?;
                copy_path_to_guest(
                    &install.guest_ip,
                    &install.ssh_user,
                    &ssh_key,
                    &host_key,
                    &mcp_path,
                    "/tmp/maturana-mcp-config",
                    false,
                )?;
                let guest_path = &rendered.guest_path;
                let parent = posix_parent(guest_path);
                run_ssh_with_stdin(
                    &install.guest_ip,
                    &install.ssh_user,
                    &ssh_key,
                    &host_key,
                    &format!(
                        "sudo mkdir -p {parent} && sudo mv /tmp/maturana-mcp-config {path} && sudo chown -R {user}:{user} {parent} && sudo chmod 0600 {path}",
                        parent = shell_quote(parent),
                        path = shell_quote(guest_path),
                        user = shell_quote(&install.ssh_user),
                    ),
                    None,
                    SSH_TIMEOUT_QUICK,
                )?;
                println!(
                    "installed MCP config ({} servers) at {guest_path}",
                    spec.mcp_servers.len()
                );
            }
            // Pre-install npx-launched MCP servers globally so the harness runs
            // the resident binary the config now points at. Idempotent; tolerate
            // transient npm failures rather than abort the agent.
            let npm_pkgs: Vec<String> = spec
                .mcp_servers
                .iter()
                .filter_map(|s| maturana_core::mcp::npx_package(s.command.as_deref(), &s.args))
                .collect();
            if !npm_pkgs.is_empty() {
                let quoted = npm_pkgs
                    .iter()
                    .map(|p| shell_quote(p))
                    .collect::<Vec<_>>()
                    .join(" ");
                match run_ssh_with_stdin(
                    &install.guest_ip,
                    &install.ssh_user,
                    &ssh_key,
                    &host_key,
                    &format!("sudo npm install -g {quoted}"),
                    None,
                    SSH_TIMEOUT_PROVISION,
                ) {
                    Ok(_) => println!(
                        "pre-installed {} resident MCP server(s): {}",
                        npm_pkgs.len(),
                        npm_pkgs.join(", ")
                    ),
                    Err(e) => eprintln!(
                        "warning: failed to pre-install resident MCP server(s) [{}]: {e} - \
                         the harness may be slow or the server unavailable until re-provisioned",
                        npm_pkgs.join(", ")
                    ),
                }
            }
        }
    }

    run_ssh_with_stdin(
        &install.guest_ip,
        &install.ssh_user,
        &ssh_key,
        &host_key,
        &format!(
            "sudo mkdir -p /agent /opt/maturana/bin /var/log/maturana /workspace /memory /wiki && sudo mv /tmp/sessiond.env /agent/sessiond.env && sudo mv /tmp/run-agent.sh /opt/maturana/bin/run-agent.sh && sudo chown {user}:{user} /agent/sessiond.env /opt/maturana/bin/run-agent.sh && sudo chmod 0600 /agent/sessiond.env && sudo chmod 0755 /opt/maturana/bin/run-agent.sh && sudo systemctl restart maturana-agent.service",
            user = shell_quote(&install.ssh_user)
        ),
        None,
        SSH_TIMEOUT_PROVISION,
    )?;
    println!(
        "refreshed {} worker at {}",
        install.agent_id, install.guest_ip
    );
    Ok(())
}

fn guest_has_live_harness_creds(
    ip: &str,
    ssh_user: &str,
    ssh_key: &PathBuf,
    host_key: &GuestHostKey,
    harness_auth_guest_path: &str,
) -> bool {
    let path = format!(
        "{}/.credentials.json",
        harness_auth_guest_path.trim_end_matches('/')
    );
    // `test -s` (present + non-empty) -> LIVE; otherwise ABSENT. The `|| echo`
    // keeps the remote exit status 0 so SSH itself never errors on a missing file.
    let cmd = format!("test -s {} && echo LIVE || echo ABSENT", shell_quote(&path));
    match run_ssh_with_stdin(
        ip,
        ssh_user,
        ssh_key,
        host_key,
        &cmd,
        None,
        SSH_TIMEOUT_QUICK,
    ) {
        Ok(out) => out.trim() == "LIVE",
        Err(_) => false,
    }
}

fn render_auth_install_command(
    harness: &HarnessRuntime,
    ssh_user: &str,
    harness_auth_guest_path: &str,
) -> String {
    let user = shell_quote(ssh_user);
    let guest_path = shell_quote(harness_auth_guest_path);
    match harness {
        HarnessRuntime::Opencode => format!(
            "sudo mkdir -p {guest_path} && sudo cp -a /tmp/maturana-harness-auth/. {guest_path}/ && sudo rm -rf /tmp/maturana-harness-auth && sudo chown -R {user}:{user} {guest_path} && chmod -R go-rwx {guest_path}/.config {guest_path}/.local {guest_path}/.maturana-env 2>/dev/null || true"
        ),
        _ => {
            let parent = shell_quote(posix_parent(harness_auth_guest_path));
            format!(
                "sudo mkdir -p {parent} && sudo rm -rf {guest_path} && sudo mv /tmp/maturana-harness-auth {guest_path} && sudo chown -R {user}:{user} {guest_path} && chmod -R go-rwx {guest_path}"
            )
        }
    }
}

fn posix_parent(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((parent, _)) if !parent.is_empty() => parent,
        _ => ".",
    }
}

fn read_optional_trimmed(path: PathBuf) -> anyhow::Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    Ok(fs::read_to_string(path)?.trim().to_string())
}

fn absolute_or_cwd(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}
