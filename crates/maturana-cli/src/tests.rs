use super::*;
use std::fs;

#[test]
fn web_bind_resolves_tailnet_and_explicit() {
    // Explicit --bind always wins, Tailscale or not.
    assert_eq!(
        commands::web::web_bind_for(Some("0.0.0.0:9000"), false, Some("100.1.2.3"), None).unwrap(),
        "0.0.0.0:9000"
    );
    // --bind + --tailnet together is a conflict, not a silent ignore.
    assert!(
        commands::web::web_bind_for(Some("0.0.0.0:9000"), true, Some("100.1.2.3"), None).is_err()
    );
    // --tailnet uses the tailscale ip; errors without one.
    assert_eq!(
        commands::web::web_bind_for(None, true, Some("100.1.2.3"), None).unwrap(),
        "100.1.2.3:47836"
    );
    assert!(commands::web::web_bind_for(None, true, None, None).is_err());
    // Interactive choices map to tailnet / all / localhost.
    assert_eq!(
        commands::web::web_bind_for(None, false, Some("100.1.2.3"), Some("1")).unwrap(),
        "100.1.2.3:47836"
    );
    assert_eq!(
        commands::web::web_bind_for(None, false, Some("100.1.2.3"), Some("2")).unwrap(),
        "0.0.0.0:47836"
    );
    assert_eq!(
        commands::web::web_bind_for(None, false, Some("100.1.2.3"), Some("3")).unwrap(),
        "127.0.0.1:47836"
    );
    // Non-interactive (no choice) keeps the historical all-interfaces default,
    // even with Tailscale present.
    assert_eq!(
        commands::web::web_bind_for(None, false, Some("100.1.2.3"), None).unwrap(),
        "0.0.0.0:47836"
    );
    assert_eq!(
        commands::web::web_bind_for(None, false, None, None).unwrap(),
        "0.0.0.0:47836"
    );
}

#[test]
fn repair_windows_config_uses_three_harness_defaults() {
    let config =
        repair_windows_config(Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()).unwrap();

    assert_eq!(
        config.agent_ids,
        vec!["codex-demo", "opencode-demo", "claude-demo"]
    );
    assert_eq!(
        config.session_ids,
        vec!["codex-main", "opencode-main", "claude-main"]
    );
    assert_eq!(config.harnesses, vec!["codex", "opencode", "claude-code"]);
    assert_eq!(
        config.telegram_token_sources,
        vec![
            "pipelock:telegram/bot-token",
            "pipelock:telegram/opencode-bot-token",
            "pipelock:telegram/claude-bot-token",
        ]
    );
}

#[test]
fn repair_windows_config_rejects_uneven_lists() {
    let error = repair_windows_config(
        vec!["codex-demo".to_string(), "opencode-demo".to_string()],
        vec!["codex-main".to_string()],
        vec!["codex".to_string(), "opencode".to_string()],
        vec![
            "/home/ubuntu/.codex".to_string(),
            "/home/ubuntu".to_string(),
        ],
        vec![
            "pipelock:telegram/bot-token".to_string(),
            "pipelock:telegram/opencode-bot-token".to_string(),
        ],
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("--session-id count"));
}

#[test]
fn ssh_key_repair_keeps_existing_key_without_force() {
    let temp = std::env::temp_dir().join(format!(
        "maturana-ssh-key-repair-test-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp).unwrap();
    let key_path = temp.join("maturana-agent-ed25519");
    fs::write(&key_path, "existing-key").unwrap();

    ensure_agent_ssh_key(key_path.clone(), false).unwrap();

    assert_eq!(fs::read_to_string(&key_path).unwrap(), "existing-key");
    assert_eq!(
        public_key_path(&key_path),
        PathBuf::from(format!("{}.pub", key_path.display()))
    );

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn ubuntu_cloudimg_checksum_helpers_match_script_behavior() {
    let temp = std::env::temp_dir().join(format!(
        "maturana-cloudimg-repair-test-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&temp);
    fs::create_dir_all(&temp).unwrap();
    let image_path = temp.join("noble-server-cloudimg-amd64.img");
    fs::write(&image_path, b"maturana").unwrap();
    let sha_path = temp.join("SHA256SUMS");
    fs::write(
        &sha_path,
        format!(
            "{} *noble-server-cloudimg-amd64.img\n{} other.img\n",
            sha256_file_hex(&image_path).unwrap(),
            "0".repeat(64)
        ),
    )
    .unwrap();

    assert_eq!(
        expected_sha256_for_image(&sha_path, "noble-server-cloudimg-amd64.img").unwrap(),
        sha256_file_hex(&image_path).unwrap()
    );
    assert!(expected_sha256_for_image(&sha_path, "missing.img").is_err());

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn snapshot_audit_events_cover_success_and_failure() {
    assert_eq!(
        commands::snapshot::snapshot_audit_event("list", true, false),
        "snapshot.list.live"
    );
    assert_eq!(
        commands::snapshot::snapshot_audit_event("list", false, true),
        "snapshot.list.local.failed"
    );
    assert_eq!(
        commands::snapshot::snapshot_audit_event("take", false, false),
        "snapshot.take.local"
    );
    assert_eq!(
        commands::snapshot::snapshot_audit_event("take", true, true),
        "snapshot.take.live.failed"
    );
    assert_eq!(
        commands::snapshot::snapshot_audit_event("restore", true, false),
        "snapshot.restore.live"
    );
    assert_eq!(
        commands::snapshot::snapshot_audit_event("restore", true, true),
        "snapshot.restore.live.failed"
    );
}

#[test]
fn windows_runner_helpers_are_rust_owned() {
    assert_eq!(safe_windows_task_suffix("codex-demo"), "codex-demo");
    assert_eq!(safe_windows_task_suffix("../bad name"), "..-bad-name");
    assert!(quote_cmd_arg("C:/Program Files/maturana/maturana.exe").starts_with('"'));
    assert!(!quote_cmd_arg("maturana.exe").contains("powershell"));
}

fn up_command_for_test(agent_ids: Vec<&str>, session_id: Option<&str>) -> UpCommand {
    UpCommand {
        agent_ids: agent_ids.into_iter().map(String::from).collect(),
        sessiond_bind: "0.0.0.0:47834".to_string(),
        sessiond_token: Some("tok".to_string()),
        session_id: session_id.map(String::from),
        telegram_token_source: "pipelock:telegram/bot-token".to_string(),
        no_telegram: false,
        no_schedules: false,
        no_proactive: false,
        channel_poll_seconds: 5,
        schedule_poll_seconds: 60,
        dry_run: false,
    }
}

#[test]
fn build_orchestrator_config_derives_per_agent_session_ids() {
    let temp = std::env::temp_dir().join(format!(
        "maturana-up-session-derive-test-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&temp);
    let home = MaturanaHome::new(&temp);

    // codex-firecracker has a materialized sessiond.env (highest priority);
    // claude-firecracker has none, so it falls back to its profile default.
    let state_dir = home.agent_dir("codex-firecracker").join("state");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        state_dir.join("sessiond.env"),
        "MATURANA_SESSION_ID='codex-rendered'\n",
    )
    .unwrap();

    let command = up_command_for_test(vec!["codex-firecracker", "claude-firecracker"], None);
    let config = commands::up::build_orchestrator_config(&home, &command).unwrap();

    let codex = config
        .agents
        .iter()
        .find(|a| a.agent_id == "codex-firecracker")
        .unwrap();
    let claude = config
        .agents
        .iter()
        .find(|a| a.agent_id == "claude-firecracker")
        .unwrap();
    // Materialized spec wins for codex; profile default for claude. They are
    // NOT collapsed onto a single global session id.
    assert_eq!(codex.session_id, "codex-rendered");
    assert_eq!(claude.session_id, "claude-main");

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn build_orchestrator_config_skips_telegram_for_a_bot_less_agent() {
    // A Discord-only agent (no channels.telegram, not a bundled example) must
    // NOT get a Telegram poller: it would squat the command-default bot token
    // and 409-crash-loop against whichever agent owns that bot. A built-in in
    // the same fleet still gets Telegram (via its profile token).
    let temp =
        std::env::temp_dir().join(format!("maturana-up-telegram-gate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    let home = MaturanaHome::new(&temp);
    let dir = home.agent_dir("humberto-maturana");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
            dir.join("MATURANA.md"),
            "---\n\
             identity: { id: humberto-maturana, name: Humberto, purpose: support }\n\
             runtime: { harness: codex }\n\
             vm: { provider: firecracker, guest_os: linux, firecracker: { kernel_image: k, rootfs_image: r } }\n\
             channels: { discord: { bot_token_source: pipelock:discord/humberto-bot } }\n\
             ---\n# Humberto\n",
        )
        .unwrap();

    let command = up_command_for_test(vec!["humberto-maturana", "codex-firecracker"], None);
    let config = commands::up::build_orchestrator_config(&home, &command).unwrap();
    let humberto = config
        .agents
        .iter()
        .find(|a| a.agent_id == "humberto-maturana")
        .unwrap();
    let codex = config
        .agents
        .iter()
        .find(|a| a.agent_id == "codex-firecracker")
        .unwrap();
    assert!(!humberto.telegram, "bot-less agent must not poll Telegram");
    assert!(humberto.discord.is_some(), "its declared Discord stays on");
    assert!(codex.telegram, "a built-in keeps its own Telegram bot");

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn build_orchestrator_config_session_id_flag_overrides_all_agents() {
    let temp = std::env::temp_dir().join(format!(
        "maturana-up-session-override-test-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&temp);
    let home = MaturanaHome::new(&temp);

    let command = up_command_for_test(
        vec!["codex-firecracker", "claude-firecracker"],
        Some("global-session"),
    );
    let config = commands::up::build_orchestrator_config(&home, &command).unwrap();
    assert!(config
        .agents
        .iter()
        .all(|a| a.session_id == "global-session"));

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn firecracker_profiles_carry_per_agent_telegram_tokens() {
    // `maturana up` reads these so each fleet channel uses its own bot,
    // matching the per-agent session id its guest worker claims from.
    assert_eq!(
        firecracker_profile_for("codex-firecracker")
            .unwrap()
            .telegram_token_source,
        "pipelock:telegram/bot-token"
    );
    assert_eq!(
        firecracker_profile_for("claude-firecracker")
            .unwrap()
            .telegram_token_source,
        "pipelock:telegram/claude-bot-token"
    );
    assert_eq!(
        firecracker_profile_for("opencode-firecracker")
            .unwrap()
            .telegram_token_source,
        "pipelock:telegram/opencode-bot-token"
    );
    // Session id and token agree per agent (no cross-wiring).
    assert_eq!(
        firecracker_profile_for("claude-firecracker")
            .unwrap()
            .session_id,
        "claude-main"
    );
    assert!(firecracker_profile_for("not-a-fleet-agent").is_none());
}

#[test]
fn firecracker_profile_selection_defaults_to_builtins_plus_disk() {
    let temp =
        std::env::temp_dir().join(format!("maturana-fc-select-empty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    let home = MaturanaHome::new(&temp);
    // No agents on disk → exactly the three bundled examples.
    let profiles = selected_firecracker_profiles(&home, &[]).unwrap();
    assert_eq!(profiles.len(), 3);
    assert_eq!(profiles[0].agent_id, "codex-firecracker");
    assert_eq!(profiles[1].harness_arg, "opencode");
    assert_eq!(profiles[2].auth_guest_path, "/home/ubuntu/.claude");
}

#[test]
fn firecracker_profile_selection_errors_on_unmaterialized_agent() {
    let temp =
        std::env::temp_dir().join(format!("maturana-fc-select-missing-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    let home = MaturanaHome::new(&temp);
    // A named agent that isn't a built-in and has no spec on disk is a real
    // error — but it points at how to launch it, not "unknown harness".
    let error = selected_firecracker_profiles(&home, &["missing".to_string()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("no materialized spec for Firecracker agent 'missing'"));
}

#[test]
fn firecracker_profile_from_spec_makes_any_agent_first_class() {
    let temp = std::env::temp_dir().join(format!("maturana-fc-from-spec-{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    let home = MaturanaHome::new(&temp);
    let dir = home.agent_dir("humberto-maturana");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("MATURANA.md"),
        "---\n\
             identity: { id: humberto-maturana, name: Humberto, purpose: support }\n\
             runtime: { harness: codex }\n\
             harness_auth:\n\
            \x20 - runtime: codex\n\
            \x20   source_path: .maturana/host-auth/humberto\n\
            \x20   guest_path: /home/ubuntu/.codex\n\
             channels: { telegram: { token_source: pipelock:telegram/humberto-bot-token } }\n\
             vm:\n\
            \x20 provider: firecracker\n\
            \x20 guest_os: linux\n\
            \x20 firecracker:\n\
            \x20   kernel_image: .maturana/images/firecracker/humberto/vmlinux.bin\n\
            \x20   rootfs_image: .maturana/images/firecracker/humberto/ubuntu-rootfs.ext4\n\
            \x20   tap_name: tap-mat-hum\n\
            \x20   host_ip: 172.30.10.13\n\
            \x20   guest_ip: 172.30.10.14\n\
            \x20   guest_mac: \"AA:FC:00:00:10:04\"\n\
             ---\n\
             # Humberto\n",
    )
    .unwrap();

    // A brand-new agent derives a complete launch profile from its OWN spec —
    // no built-in entry, no special-casing.
    let profile = firecracker_profile_from_spec(&home, "humberto-maturana").unwrap();
    assert_eq!(profile.session_id, "humberto-maturana-main");
    assert_eq!(profile.image_name, "humberto");
    assert_eq!(profile.cidr, "172.30.10.12/30");
    assert_eq!(profile.harness_arg, "codex");
    assert_eq!(profile.tap_name, "tap-mat-hum");
    assert_eq!(
        profile.telegram_token_source,
        "pipelock:telegram/humberto-bot-token"
    );
    assert_eq!(profile.auth_source, ".maturana/host-auth/humberto");
    assert_eq!(profile.auth_guest_path, "/home/ubuntu/.codex");

    // It is relaunched on reboot like any built-in: the empty-id fleet
    // selection includes it alongside the three bundled examples.
    let fleet = selected_firecracker_profiles(&home, &[]).unwrap();
    assert!(fleet.iter().any(|p| p.agent_id == "humberto-maturana"));

    // And `agent run` enqueues to the SAME session its guest worker claims
    // from, instead of the old `telegram-main` dead-end.
    assert_eq!(
        maturana_ops::agents::infer_agent_session_id(&home, "humberto-maturana").unwrap(),
        "humberto-maturana-main"
    );
}

#[test]
fn firecracker_asset_manifest_validation_accepts_expected_assets() {
    let root = temp_test_dir("firecracker-assets-ok");
    let image_dir = root.join("images");
    fs::create_dir_all(&image_dir).unwrap();
    let kernel = image_dir.join("vmlinux.bin");
    let rootfs = image_dir.join("ubuntu-rootfs.ext4");
    let ssh_key = root.join("maturana-firecracker.id_rsa");
    fs::write(&kernel, b"\x7fELFkernel").unwrap();
    fs::write(&rootfs, b"rootfs").unwrap();
    fs::write(&ssh_key, b"private").unwrap();
    fs::write(
        PathBuf::from(format!("{}.pub", ssh_key.display())),
        b"public",
    )
    .unwrap();
    let profiles = builtin_firecracker_profiles();
    let profile = &profiles[0];
    let manifest = image_dir.join("asset-manifest.json");
    write_test_firecracker_manifest(profile, &manifest, &kernel, &rootfs, &ssh_key);

    validate_firecracker_asset_manifest(profile, &manifest, &image_dir, &ssh_key).unwrap();
}

#[test]
fn firecracker_asset_manifest_validation_rejects_identity_mismatch() {
    let root = temp_test_dir("firecracker-assets-mismatch");
    let image_dir = root.join("images");
    fs::create_dir_all(&image_dir).unwrap();
    let kernel = image_dir.join("vmlinux.bin");
    let rootfs = image_dir.join("ubuntu-rootfs.ext4");
    let ssh_key = root.join("maturana-firecracker.id_rsa");
    fs::write(&kernel, b"\x7fELFkernel").unwrap();
    fs::write(&rootfs, b"rootfs").unwrap();
    fs::write(&ssh_key, b"private").unwrap();
    fs::write(
        PathBuf::from(format!("{}.pub", ssh_key.display())),
        b"public",
    )
    .unwrap();
    let profiles = builtin_firecracker_profiles();
    let profile = &profiles[0];
    let manifest = image_dir.join("asset-manifest.json");
    write_test_firecracker_manifest(profile, &manifest, &kernel, &rootfs, &ssh_key);
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    value["agent_id"] = serde_json::json!("wrong-agent");
    fs::write(&manifest, serde_json::to_string_pretty(&value).unwrap()).unwrap();

    let error = validate_firecracker_asset_manifest(profile, &manifest, &image_dir, &ssh_key)
        .unwrap_err()
        .to_string();
    assert!(error.contains("agent mismatch"));
}

#[test]
fn firecracker_asset_manifest_validation_rejects_non_elf_kernel() {
    let root = temp_test_dir("firecracker-assets-non-elf");
    let image_dir = root.join("images");
    fs::create_dir_all(&image_dir).unwrap();
    let kernel = image_dir.join("vmlinux.bin");
    let rootfs = image_dir.join("ubuntu-rootfs.ext4");
    let ssh_key = root.join("maturana-firecracker.id_rsa");
    fs::write(&kernel, b"not-elf").unwrap();
    fs::write(&rootfs, b"rootfs").unwrap();
    fs::write(&ssh_key, b"private").unwrap();
    fs::write(
        PathBuf::from(format!("{}.pub", ssh_key.display())),
        b"public",
    )
    .unwrap();
    let profiles = builtin_firecracker_profiles();
    let profile = &profiles[0];
    let manifest = image_dir.join("asset-manifest.json");
    write_test_firecracker_manifest(profile, &manifest, &kernel, &rootfs, &ssh_key);

    let error = validate_firecracker_asset_manifest(profile, &manifest, &image_dir, &ssh_key)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not an ELF"));
}

fn temp_test_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_test_firecracker_manifest(
    profile: &FirecrackerHarnessProfile,
    manifest: &Path,
    kernel: &Path,
    rootfs: &Path,
    ssh_key: &Path,
) {
    let value = serde_json::json!({
        "agent_id": profile.agent_id,
        "kernel": kernel,
        "rootfs": rootfs,
        "ssh_key": ssh_key,
        "guest_ip": profile.guest_ip,
        "host_ip": profile.host_ip,
        "guest_mac": profile.guest_mac,
        "tap_name": profile.tap_name,
        "kernel_sha256": sha256_file_hex(kernel).unwrap(),
        "rootfs_sha256": sha256_file_hex(rootfs).unwrap(),
        "kernel_bytes": fs::metadata(kernel).unwrap().len(),
        "rootfs_bytes": fs::metadata(rootfs).unwrap().len(),
    });
    fs::write(manifest, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

fn repo_example(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

#[test]
fn firecracker_guest_artifacts_are_rust_rendered() {
    let temp = std::env::temp_dir().join(format!(
        "maturana-firecracker-artifacts-test-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&temp);
    let home = MaturanaHome::new(&temp);
    let profiles = builtin_firecracker_profiles();
    let profile = &profiles[1];
    let spec = AgentSpec::from_maturana_markdown(repo_example(&profile.spec_path)).unwrap();

    let artifacts = render_firecracker_guest_artifacts(
        &home,
        profile,
        &spec,
        "test-token",
        "http://172.30.10.5:47834",
    )
    .unwrap();

    let env = fs::read_to_string(&artifacts.sessiond_env).unwrap();
    assert!(env.contains("MATURANA_AGENT_ID='opencode-firecracker'"));
    assert!(env.contains("MATURANA_HARNESS='opencode'"));
    assert!(env.contains("MATURANA_SESSIOND_URL='http://172.30.10.5:47834'"));
    assert!(env.contains("MATURANA_SESSIOND_TOKEN='test-token'"));

    let runner = fs::read_to_string(&artifacts.runner).unwrap();
    assert!(runner.contains("/session/claim"));
    assert!(runner.contains("opencode"));

    let service = fs::read_to_string(&artifacts.service).unwrap();
    assert!(service.contains("Description=Maturana opencode agent opencode-firecracker"));
    assert!(service.contains("ExecStart=/opt/maturana/bin/run-agent.sh"));

    let harness_install = fs::read_to_string(&artifacts.harness_install).unwrap();
    assert!(harness_install.contains("npm install -g opencode-ai"));
    assert!(!harness_install.contains("@openai/codex"));

    let firecracker_bootstrap = fs::read_to_string(&artifacts.firecracker_bootstrap).unwrap();
    assert!(firecracker_bootstrap.contains("openssh-server curl ca-certificates nodejs npm"));
    assert!(firecracker_bootstrap.contains("systemctl enable ssh.service"));
    assert!(firecracker_bootstrap.contains("/etc/sudoers.d/90-maturana-ubuntu"));

    let netplan = fs::read_to_string(&artifacts.netplan).unwrap();
    assert!(netplan.contains("macaddress: \"AA:FC:00:00:10:02\""));
    assert!(netplan.contains("- 172.30.10.6/30"));
    assert!(netplan.contains("via: 172.30.10.5"));

    let cloud_cfg = fs::read_to_string(&artifacts.cloud_cfg).unwrap();
    assert_eq!(cloud_cfg, "network: {config: disabled}\n");
    // The opencode example spec declares network.proxy (Brave injection), so
    // the guest proxy.env IS rendered, pointing at the host-TAP proxy bind.
    let proxy_env_path = artifacts
        .proxy_env
        .expect("proxy.env should be rendered for a spec with network.proxy");
    let proxy_env = fs::read_to_string(&proxy_env_path).unwrap();
    assert!(proxy_env.contains("MATURANA_USE_HOST_PROXY=1"));
    assert!(proxy_env.contains("MATURANA_PROXY_HOST=172.30.10.5"));
    assert!(proxy_env.contains("MATURANA_PROXY_PORT=47833"));

    let _ = fs::remove_dir_all(&temp);
}

#[test]
fn firecracker_proxy_env_is_rust_rendered_from_spec() {
    let spec =
        AgentSpec::from_maturana_markdown(&repo_example("examples/MATURANA.firecracker-demo.md"))
            .unwrap();
    let proxy = spec.network.proxy.as_ref().unwrap();
    let proxy_env = render_firecracker_proxy_env(proxy.enabled, Some(&proxy.bind), "172.30.0.1")
        .unwrap()
        .unwrap();

    assert!(proxy_env.contains("MATURANA_USE_HOST_PROXY=1"));
    assert!(proxy_env.contains("MATURANA_PROXY_HOST=172.30.0.1"));
    assert!(proxy_env.contains("MATURANA_PROXY_PORT=47833"));
    assert!(proxy_env.contains("MATURANA_PROXY_HTTPS=1"));
    assert!(proxy_env.contains("NO_PROXY=localhost,127.0.0.1,::1"));
}

#[test]
fn bind_port_extracts_sessiond_port() {
    assert_eq!(bind_port("0.0.0.0:47834").unwrap(), "47834");
    assert_eq!(bind_port("127.0.0.1:1").unwrap(), "1");
    assert!(bind_port("0.0.0.0").is_err());
}
