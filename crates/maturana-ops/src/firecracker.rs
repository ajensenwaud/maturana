use crate::{
    guest_worker::{install_guest_worker, GuestWorkerInstall},
    runtime_plane::{
        ensure_graph_token, ensure_sessiond_token, start_linux_graph, start_linux_sessiond,
        GRAPH_BIND,
    },
    ssh::{wait_for_guest_ssh, GuestHostKey},
};
use anyhow::Context;
use maturana_core::{
    materialize_agent,
    pipelock_proxy::ensure_mitm_ca_cert,
    spec::{AgentSpec, HarnessRuntime},
    ssh_pin,
    state::MaturanaHome,
    stop_agent, validate_spec,
    worker::{
        read_graph_token, render_firecracker_bootstrap, render_firecracker_cloud_cfg,
        render_firecracker_netplan, render_firecracker_proxy_env, render_harness_install,
        render_harness_install_service, render_run_agent, render_session_env,
        render_systemd_service, GuestWorkerConfig,
    },
    LaunchMode,
};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs,
    io::Read,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct FirecrackerHarnessProfile {
    pub agent_id: String,
    pub image_name: String,
    pub harness_arg: String,
    pub host_ip: String,
    pub guest_ip: String,
    pub cidr: String,
    pub tap_name: String,
    pub guest_mac: String,
    pub session_id: String,
    /// Pipelock source for this agent's own Telegram bot token, so `maturana up`
    /// supervises each fleet channel with the right bot.
    pub telegram_token_source: String,
    pub auth_source: String,
    pub auth_guest_path: String,
    pub spec_path: String,
}

#[derive(Debug, Clone)]
pub struct FirecrackerHarnessRepair {
    pub agent_ids: Vec<String>,
    pub ssh_key: PathBuf,
    pub sessiond_bind: String,
    pub sessiond_token_path: PathBuf,
    pub skip_assets: bool,
    pub skip_net: bool,
    pub skip_launch: bool,
    pub skip_worker_refresh: bool,
    pub skip_services: bool,
    pub install_harness: bool,
    pub ssh_wait_seconds: u64,
    pub force_reseed_auth: bool,
}

#[derive(Debug)]
pub struct FirecrackerGuestArtifacts {
    pub sessiond_env: PathBuf,
    pub runner: PathBuf,
    pub service: PathBuf,
    pub harness_install: PathBuf,
    pub harness_install_service: PathBuf,
    pub firecracker_bootstrap: PathBuf,
    pub netplan: PathBuf,
    pub cloud_cfg: PathBuf,
    pub proxy_env: Option<PathBuf>,
}

#[derive(Debug, serde::Deserialize)]
struct FirecrackerAssetManifest {
    agent_id: String,
    kernel: PathBuf,
    rootfs: PathBuf,
    ssh_key: PathBuf,
    guest_ip: String,
    host_ip: String,
    guest_mac: String,
    tap_name: String,
    kernel_sha256: String,
    rootfs_sha256: String,
    kernel_bytes: u64,
    rootfs_bytes: u64,
    /// The baked guest SSH host public key, pinned per agent. Optional so images
    /// built before host-key pinning still parse.
    #[serde(default)]
    ssh_host_ed25519_pub: Option<String>,
}

/// The three example harnesses bundled in the repo. They bootstrap a fresh
/// install without limiting the fleet: every materialized Firecracker agent can
/// resolve its own profile from its spec.
pub fn builtin_firecracker_profiles() -> Vec<FirecrackerHarnessProfile> {
    vec![
        FirecrackerHarnessProfile {
            agent_id: "codex-firecracker".to_string(),
            image_name: "codex".to_string(),
            harness_arg: "codex".to_string(),
            host_ip: "172.30.10.1".to_string(),
            guest_ip: "172.30.10.2".to_string(),
            cidr: "172.30.10.0/30".to_string(),
            tap_name: "tap-mat-codex".to_string(),
            guest_mac: "AA:FC:00:00:10:01".to_string(),
            session_id: "codex-main".to_string(),
            telegram_token_source: "pipelock:telegram/bot-token".to_string(),
            auth_source: ".maturana/host-auth/codex".to_string(),
            auth_guest_path: "/home/ubuntu/.codex".to_string(),
            spec_path: "examples/MATURANA.codex-firecracker.md".to_string(),
        },
        FirecrackerHarnessProfile {
            agent_id: "opencode-firecracker".to_string(),
            image_name: "opencode".to_string(),
            harness_arg: "opencode".to_string(),
            host_ip: "172.30.10.5".to_string(),
            guest_ip: "172.30.10.6".to_string(),
            cidr: "172.30.10.4/30".to_string(),
            tap_name: "tap-mat-open".to_string(),
            guest_mac: "AA:FC:00:00:10:02".to_string(),
            session_id: "opencode-main".to_string(),
            telegram_token_source: "pipelock:telegram/opencode-bot-token".to_string(),
            auth_source: ".maturana/host-auth/opencode".to_string(),
            auth_guest_path: "/home/ubuntu".to_string(),
            spec_path: "examples/MATURANA.opencode-firecracker.md".to_string(),
        },
        FirecrackerHarnessProfile {
            agent_id: "claude-firecracker".to_string(),
            image_name: "claude".to_string(),
            harness_arg: "claude-code".to_string(),
            host_ip: "172.30.10.9".to_string(),
            guest_ip: "172.30.10.10".to_string(),
            cidr: "172.30.10.8/30".to_string(),
            tap_name: "tap-mat-claude".to_string(),
            guest_mac: "AA:FC:00:00:10:03".to_string(),
            session_id: "claude-main".to_string(),
            telegram_token_source: "pipelock:telegram/claude-bot-token".to_string(),
            auth_source: ".maturana/host-auth/claude-code".to_string(),
            auth_guest_path: "/home/ubuntu/.claude".to_string(),
            spec_path: "examples/MATURANA.claude-firecracker.md".to_string(),
        },
    ]
}

/// A bundled example profile, looked up by agent id. Returns `None` for any
/// agent that should be resolved from its materialized spec instead.
pub fn firecracker_profile_for(agent_id: &str) -> Option<FirecrackerHarnessProfile> {
    builtin_firecracker_profiles()
        .into_iter()
        .find(|profile| profile.agent_id == agent_id)
}

/// Build a launch profile for any materialized Firecracker agent by reading its
/// own spec.
pub fn firecracker_profile_from_spec(
    home: &MaturanaHome,
    agent_id: &str,
) -> anyhow::Result<FirecrackerHarnessProfile> {
    let spec_path = home.agent_dir(agent_id).join("MATURANA.md");
    let spec = AgentSpec::from_maturana_markdown(&spec_path).with_context(|| {
        format!(
            "no materialized spec for Firecracker agent '{agent_id}' at {} - \
             author it and run `maturana agent launch` first",
            spec_path.display()
        )
    })?;
    let fc = spec.vm.firecracker.clone().ok_or_else(|| {
        anyhow::anyhow!("agent '{agent_id}' is not a Firecracker agent (vm.firecracker unset)")
    })?;
    let auth = spec
        .harness_auth
        .iter()
        .find(|auth| auth.runtime == spec.runtime.harness);
    Ok(FirecrackerHarnessProfile {
        image_name: firecracker_image_name(agent_id, &fc.rootfs_image),
        harness_arg: harness_runtime_arg(&spec.runtime.harness).to_string(),
        cidr: cidr_for_host_ip(&fc.host_ip)?,
        host_ip: fc.host_ip,
        guest_ip: fc.guest_ip,
        tap_name: fc.tap_name,
        guest_mac: fc.guest_mac,
        session_id: format!("{agent_id}-main"),
        telegram_token_source: spec
            .channels
            .telegram
            .as_ref()
            .map(|telegram| telegram.token_source.clone())
            .unwrap_or_default(),
        auth_source: auth
            .map(|auth| auth.source_path.clone())
            .unwrap_or_else(|| default_host_auth_source(&spec.runtime.harness)),
        auth_guest_path: auth
            .map(|auth| auth.guest_path.clone())
            .unwrap_or_else(|| default_auth_guest_path(&spec.runtime.harness)),
        spec_path: spec_path.to_string_lossy().into_owned(),
        agent_id: agent_id.to_string(),
    })
}

/// A bundled example if it exists, otherwise a profile derived from the agent's
/// own materialized spec.
pub fn resolve_firecracker_profile(
    home: &MaturanaHome,
    agent_id: &str,
) -> anyhow::Result<FirecrackerHarnessProfile> {
    match firecracker_profile_for(agent_id) {
        Some(profile) => Ok(profile),
        None => firecracker_profile_from_spec(home, agent_id),
    }
}

pub fn selected_firecracker_profiles(
    home: &MaturanaHome,
    agent_ids: &[String],
) -> anyhow::Result<Vec<FirecrackerHarnessProfile>> {
    if agent_ids.is_empty() {
        let mut profiles = builtin_firecracker_profiles();
        let mut seen: HashSet<String> = profiles
            .iter()
            .map(|profile| profile.agent_id.clone())
            .collect();
        for agent_id in crate::agents::list_agent_ids(home).unwrap_or_default() {
            if !seen.insert(agent_id.clone()) {
                continue;
            }
            if let Ok(profile) = firecracker_profile_from_spec(home, &agent_id) {
                profiles.push(profile);
            }
        }
        return Ok(profiles);
    }

    agent_ids
        .iter()
        .map(|agent_id| resolve_firecracker_profile(home, agent_id))
        .collect()
}

/// Create or repair the host TAP/NAT wiring for one Firecracker guest.
///
/// The shell script remains a leaf adapter because it owns privileged `ip`,
/// `iptables`, and `sysctl` calls. Maturana still owns the operation boundary:
/// the caller may provide only the TAP name, host IP, and /30 CIDR.
pub fn setup_firecracker_tap(tap_name: &str, host_ip: &str, cidr: &str) -> anyhow::Result<()> {
    run_checked_process(
        Command::new("bash").args(tap_setup_args(tap_name, host_ip, cidr)),
        "setup Firecracker TAP",
    )
}

pub fn repair_firecracker_harnesses(
    home: &MaturanaHome,
    repair: FirecrackerHarnessRepair,
) -> anyhow::Result<()> {
    if cfg!(windows) {
        anyhow::bail!("firecracker harness repair requires a Linux host");
    }

    let selected = selected_firecracker_profiles(home, &repair.agent_ids)?;
    let sessiond_token_path = absolute_or_cwd(repair.sessiond_token_path.clone())?;
    let ssh_key = absolute_or_cwd(repair.ssh_key.clone())?;
    let sessiond_token = ensure_sessiond_token(&sessiond_token_path)?;
    // With --skip-services the plane (sessiond + graph) is owned by the systemd
    // `maturana up` service, so do not start duplicate copies on the same ports.
    if !repair.skip_services {
        let pid = start_linux_sessiond(
            home,
            &repair.sessiond_bind,
            &sessiond_token,
            &sessiond_token_path,
        )?;
        println!("sessiond pid={} bind={}", pid, repair.sessiond_bind);
    }

    let graph_opt_in = selected.iter().any(|profile| {
        AgentSpec::from_maturana_markdown(PathBuf::from(&profile.spec_path))
            .map(|spec| spec.knowledge_graph.enabled)
            .unwrap_or(false)
    });
    if graph_opt_in {
        let graph_token = ensure_graph_token(home)?;
        if !repair.skip_services {
            let pid = start_linux_graph(home, GRAPH_BIND, &graph_token)?;
            println!("graph pid={} bind={GRAPH_BIND}", pid);
        }
    }

    let mut failures: Vec<(String, anyhow::Error)> = Vec::new();
    for profile in selected {
        println!("=== {} ===", profile.agent_id);

        // Boot recovery with --skip-assets must no-op cleanly on a host that has
        // never baked this image.
        if repair.skip_assets {
            let expected_rootfs = PathBuf::from(format!(
                ".maturana/images/firecracker/{}/ubuntu-rootfs.ext4",
                profile.image_name
            ));
            if !expected_rootfs.exists() {
                println!(
                    "  no baked rootfs at {} - skipping (run without --skip-assets to build it)",
                    expected_rootfs.display()
                );
                continue;
            }
        }

        let result = (|| -> anyhow::Result<()> {
            if !repair.skip_launch {
                let _ = stop_agent(home, &profile.agent_id);
            }
            if !repair.skip_net {
                setup_firecracker_tap(&profile.tap_name, &profile.host_ip, &profile.cidr)?;
            }
            if !repair.skip_assets {
                prepare_firecracker_assets(
                    home,
                    &profile,
                    &ssh_key,
                    &sessiond_token,
                    bind_port(&repair.sessiond_bind)?,
                )?;
            }

            let spec_path = PathBuf::from(&profile.spec_path);
            validate_and_materialize_firecracker_spec(home, &spec_path, !repair.skip_launch)?;

            if !repair.skip_worker_refresh {
                pin_firecracker_host_key(home, &profile)?;
                let host_key = GuestHostKey::resolve(home, &profile.agent_id, &profile.guest_ip)?;
                wait_for_guest_ssh(
                    &profile.guest_ip,
                    "ubuntu",
                    &ssh_key,
                    &host_key,
                    Duration::from_secs(repair.ssh_wait_seconds),
                )?;
                install_guest_worker(
                    home,
                    GuestWorkerInstall {
                        agent_id: profile.agent_id.to_string(),
                        session_id: profile.session_id.to_string(),
                        harness: parse_harness_runtime(&profile.harness_arg)?,
                        guest_ip: profile.guest_ip.to_string(),
                        ssh_user: "ubuntu".to_string(),
                        ssh_key: ssh_key.clone(),
                        harness_auth_guest_path: profile.auth_guest_path.to_string(),
                        sessiond_url: format!(
                            "http://{}:{}",
                            profile.host_ip,
                            bind_port(&repair.sessiond_bind)?
                        ),
                        sessiond_token_path: sessiond_token_path.clone(),
                        auth_source: Some(PathBuf::from(&profile.auth_source)),
                        install_harness: repair.install_harness,
                        force_reseed_auth: repair.force_reseed_auth,
                    },
                )?;
            }
            Ok(())
        })();
        match result {
            Err(err) => {
                eprintln!("  {} failed: {err:#}", profile.agent_id);
                failures.push((profile.agent_id.to_string(), err));
            }
            Ok(()) => {
                if !repair.skip_services && !repair.skip_launch {
                    if let Err(err) = start_linux_agent_proxy(home, &profile) {
                        eprintln!("  {} egress proxy not started: {err:#}", profile.agent_id);
                    }
                }
            }
        }
    }

    if !failures.is_empty() {
        let names = failures
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!("Firecracker harness repair failed for: {names}");
    }
    println!("Firecracker harness repair complete.");
    Ok(())
}

/// Spawn a dedicated worker VM for an orchestration role by cloning a base
/// Firecracker agent: create a fresh TAP on an allocated address, copy the base
/// rootfs, materialize + launch the VM, and install the guest worker reusing the
/// base agent's harness auth.
pub fn orchestrator_spawn_worker(
    home: &MaturanaHome,
    base_agent_id: &str,
    new_id: &str,
    session_id: &str,
    net: &maturana_core::orchestrator_spawn::FirecrackerNet,
) -> anyhow::Result<()> {
    let base_profile = resolve_firecracker_profile(home, base_agent_id).with_context(|| {
        format!(
            "--base-spec '{base_agent_id}' is not a launchable Firecracker agent to clone; \
             pass a bundled example (e.g. codex-firecracker) or any materialized agent id"
        )
    })?;
    let base_spec =
        AgentSpec::from_maturana_markdown(home.agent_dir(base_agent_id).join("MATURANA.md"))?;
    let base_rootfs = {
        let fc = base_spec
            .vm
            .firecracker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("base agent {base_agent_id} is not Firecracker"))?;
        absolute_or_cwd(PathBuf::from(&fc.rootfs_image))?
    };

    setup_firecracker_tap(&net.tap_name, &net.host_ip, &net.cidr)?;

    let rootfs_dir = home.agent_dir(new_id);
    fs::create_dir_all(&rootfs_dir)?;
    let new_rootfs = rootfs_dir.join("ubuntu-rootfs.ext4");
    let cow = maturana_core::cow::is_cow(&rootfs_dir);
    println!(
        "  spawn {new_id}: provisioning rootfs ({})...",
        if cow {
            "copy-on-write clone - instant"
        } else {
            "full copy, a few GB"
        }
    );
    let kind = maturana_core::cow::provision_clone(&base_rootfs, &new_rootfs)
        .with_context(|| format!("failed to provision rootfs for {new_id}"))?;
    println!("  spawn {new_id}: rootfs ready ({})", kind.label());

    let netplan = render_firecracker_netplan(&net.guest_mac, &net.guest_ip, &net.host_ip);
    let netplan_file = rootfs_dir.join("50-maturana-firecracker.yaml");
    fs::write(&netplan_file, &netplan)?;
    println!(
        "  spawn {new_id}: rewriting guest netplan to {}",
        net.guest_ip
    );
    run_checked_process(
        Command::new("virt-copy-in")
            .arg("-a")
            .arg(&new_rootfs)
            .arg(&netplan_file)
            .arg("/etc/netplan"),
        "rewrite spawned guest netplan",
    )?;

    let mut spec = maturana_core::orchestrator_spawn::derive_role_spec(&base_spec, new_id, net);
    if let Some(fc) = spec.vm.firecracker.as_mut() {
        fc.rootfs_image = new_rootfs.display().to_string();
    }
    let markdown = spec.to_maturana_markdown()?;
    materialize_agent(&spec, &markdown, home, LaunchMode::Apply)?;

    let ssh_key = absolute_or_cwd(PathBuf::from(
        ".maturana/images/firecracker/maturana-firecracker.id_rsa",
    ))?;
    let host_key = GuestHostKey::resolve(home, new_id, &net.guest_ip)?;
    println!(
        "  spawn {new_id}: waiting for guest SSH at {}...",
        net.guest_ip
    );
    wait_for_guest_ssh(
        &net.guest_ip,
        "ubuntu",
        &ssh_key,
        &host_key,
        Duration::from_secs(180),
    )?;
    install_guest_worker(
        home,
        GuestWorkerInstall {
            agent_id: new_id.to_string(),
            session_id: session_id.to_string(),
            harness: base_spec.runtime.harness.clone(),
            guest_ip: net.guest_ip.clone(),
            ssh_user: "ubuntu".to_string(),
            ssh_key,
            harness_auth_guest_path: base_profile.auth_guest_path.to_string(),
            sessiond_url: format!("http://{}:47834", net.host_ip),
            sessiond_token_path: home.root().join("sessiond/token"),
            auth_source: Some(PathBuf::from(&base_profile.auth_source)),
            // The cloned rootfs already has the harness baked in (and its auth);
            // just re-point the worker at this VM's session.
            install_harness: false,
            force_reseed_auth: false,
        },
    )?;
    println!("  spawn {new_id}: worker provisioned on {}", net.guest_ip);
    Ok(())
}

/// Tear down a spawned worker VM. Best-effort: stop it, remove its TAP, and
/// delete its per-run rootfs copy.
pub fn orchestrator_teardown_worker(
    home: &MaturanaHome,
    agent_id: &str,
    tap_name: &str,
) -> anyhow::Result<()> {
    let _ = stop_agent(home, agent_id);
    let _ = Command::new("sudo")
        .args(["-n", "ip", "link", "del", tap_name])
        .status();
    let _ = fs::remove_dir_all(home.agent_dir(agent_id));
    Ok(())
}

pub fn prepare_firecracker_assets(
    home: &MaturanaHome,
    profile: &FirecrackerHarnessProfile,
    ssh_key: &Path,
    sessiond_token: &str,
    sessiond_port: &str,
) -> anyhow::Result<()> {
    let image_dir = PathBuf::from(format!(
        ".maturana/images/firecracker/{}",
        profile.image_name
    ));
    let asset_manifest_path = image_dir.join("asset-manifest.json");
    let spec = AgentSpec::from_maturana_markdown(PathBuf::from(&profile.spec_path))
        .with_context(|| format!("failed to parse {}", profile.spec_path))?;
    let artifacts = render_firecracker_guest_artifacts(
        home,
        profile,
        &spec,
        sessiond_token,
        &format!("http://{}:{}", profile.host_ip, sessiond_port),
    )?;

    let mut command = Command::new("sudo");
    command
        .arg("env")
        .arg(format!("MATURANA_AGENT_ID={}", profile.agent_id))
        .arg(format!(
            "MATURANA_SESSIOND_ENV_PATH={}",
            artifacts.sessiond_env.display()
        ))
        .arg(format!(
            "MATURANA_RUN_AGENT_PATH={}",
            artifacts.runner.display()
        ))
        .arg(format!(
            "MATURANA_AGENT_SERVICE_PATH={}",
            artifacts.service.display()
        ))
        .arg(format!(
            "MATURANA_HARNESS_INSTALL_PATH={}",
            artifacts.harness_install.display()
        ))
        .arg(format!(
            "MATURANA_HARNESS_INSTALL_SERVICE_PATH={}",
            artifacts.harness_install_service.display()
        ))
        .arg(format!(
            "MATURANA_FIRECRACKER_BOOTSTRAP_PATH={}",
            artifacts.firecracker_bootstrap.display()
        ))
        .arg(format!(
            "MATURANA_NETPLAN_PATH={}",
            artifacts.netplan.display()
        ))
        .arg(format!(
            "MATURANA_CLOUD_CFG_PATH={}",
            artifacts.cloud_cfg.display()
        ))
        .arg(format!("MATURANA_FIRECRACKER_HOST_IP={}", profile.host_ip))
        .arg(format!(
            "MATURANA_FIRECRACKER_GUEST_IP={}",
            profile.guest_ip
        ))
        .arg(format!(
            "MATURANA_FIRECRACKER_GUEST_MAC={}",
            profile.guest_mac
        ))
        .arg(format!(
            "MATURANA_FIRECRACKER_TAP_NAME={}",
            profile.tap_name
        ))
        .arg(format!(
            "MATURANA_FIRECRACKER_ASSET_MANIFEST_PATH={}",
            asset_manifest_path.display()
        ));
    if let Some(proxy_env) = artifacts.proxy_env.as_ref() {
        command.arg(format!("MATURANA_PROXY_ENV_PATH={}", proxy_env.display()));
        let ca_cert = ensure_mitm_ca_cert(home.root())?;
        command.arg(format!("MATURANA_PROXY_CA_CERT_PATH={}", ca_cert.display()));
    }
    command
        .arg("bash")
        .arg("./scripts/firecracker-prepare-assets.sh")
        .arg(&image_dir)
        .arg(ssh_key)
        .arg(&profile.auth_source);
    run_checked_process(&mut command, "prepare Firecracker assets")?;
    validate_firecracker_asset_manifest(profile, &asset_manifest_path, &image_dir, ssh_key)
}

/// Copy the image's baked SSH host public key from the asset manifest into the
/// agent state dir so SSH connections to the guest can pin it.
pub fn pin_firecracker_host_key(
    home: &MaturanaHome,
    profile: &FirecrackerHarnessProfile,
) -> anyhow::Result<()> {
    let manifest_path = PathBuf::from(format!(
        ".maturana/images/firecracker/{}/asset-manifest.json",
        profile.image_name
    ));
    if !manifest_path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: FirecrackerAssetManifest =
        serde_json::from_str(&raw).context("failed to parse Firecracker asset manifest")?;
    if let Some(public_line) = manifest.ssh_host_ed25519_pub.as_deref() {
        let state_dir = home.agent_dir(&profile.agent_id).join("state");
        fs::create_dir_all(&state_dir)?;
        fs::write(
            state_dir.join(ssh_pin::HOST_PUBLIC_KEY_FILE),
            format!("{}\n", public_line.trim()),
        )?;
    }
    Ok(())
}

pub fn validate_firecracker_asset_manifest(
    profile: &FirecrackerHarnessProfile,
    manifest_path: &Path,
    image_dir: &Path,
    expected_ssh_key: &Path,
) -> anyhow::Result<()> {
    let manifest_path = absolute_or_cwd(manifest_path.to_path_buf())?;
    let raw = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read {}", manifest_path.display()))?;
    let manifest: FirecrackerAssetManifest =
        serde_json::from_str(&raw).context("failed to parse Firecracker asset manifest")?;
    if manifest.agent_id != profile.agent_id {
        anyhow::bail!(
            "Firecracker asset manifest agent mismatch: expected {}, got {}",
            profile.agent_id,
            manifest.agent_id
        );
    }
    if manifest.guest_ip != profile.guest_ip
        || manifest.host_ip != profile.host_ip
        || !manifest.guest_mac.eq_ignore_ascii_case(&profile.guest_mac)
        || manifest.tap_name != profile.tap_name
    {
        anyhow::bail!(
            "Firecracker asset manifest network identity does not match profile {}",
            profile.agent_id
        );
    }
    let image_dir = absolute_or_cwd(image_dir.to_path_buf())?;
    let expected_kernel = image_dir.join("vmlinux.bin");
    let expected_rootfs = image_dir.join("ubuntu-rootfs.ext4");
    assert_manifest_path("kernel", &manifest.kernel, &expected_kernel)?;
    assert_manifest_path("rootfs", &manifest.rootfs, &expected_rootfs)?;
    assert_manifest_path("ssh_key", &manifest.ssh_key, expected_ssh_key)?;
    validate_exact_size_file("kernel", &expected_kernel, manifest.kernel_bytes)?;
    validate_exact_size_file("rootfs", &expected_rootfs, manifest.rootfs_bytes)?;
    validate_nonempty_file("ssh_key", expected_ssh_key)?;
    validate_nonempty_file(
        "ssh public key",
        &PathBuf::from(format!("{}.pub", expected_ssh_key.display())),
    )?;
    validate_elf_file(&expected_kernel)?;
    validate_manifest_sha256("kernel", &expected_kernel, &manifest.kernel_sha256)?;
    validate_manifest_sha256("rootfs", &expected_rootfs, &manifest.rootfs_sha256)?;
    println!(
        "Firecracker assets verified: kernel={} rootfs={} manifest={}",
        expected_kernel.display(),
        expected_rootfs.display(),
        manifest_path.display()
    );
    Ok(())
}

pub fn render_firecracker_guest_artifacts(
    home: &MaturanaHome,
    profile: &FirecrackerHarnessProfile,
    spec: &AgentSpec,
    sessiond_token: &str,
    sessiond_url: &str,
) -> anyhow::Result<FirecrackerGuestArtifacts> {
    let artifacts_dir = home
        .root()
        .join("agents")
        .join(&profile.agent_id)
        .join("state")
        .join("firecracker-image");
    fs::create_dir_all(&artifacts_dir)?;

    let config = GuestWorkerConfig {
        agent_id: profile.agent_id.to_string(),
        session_id: profile.session_id.to_string(),
        sessiond_url: sessiond_url.to_string(),
        sessiond_token: sessiond_token.to_string(),
        harness: parse_harness_runtime(&profile.harness_arg)?,
        harness_auth_guest_path: profile.auth_guest_path.to_string(),
        headless_chrome: spec.browser.headless_chrome,
        graph_token: read_graph_token(home.root()),
        graph_name: spec
            .knowledge_graph
            .enabled
            .then(|| spec.knowledge_graph.graph_name(&profile.agent_id)),
    };

    let sessiond_env = artifacts_dir.join("sessiond.env");
    let runner = artifacts_dir.join("run-agent.sh");
    let service = artifacts_dir.join("maturana-agent.service");
    let harness_install = artifacts_dir.join("install-harness.sh");
    let harness_install_service = artifacts_dir.join("maturana-harness-install.service");
    let firecracker_bootstrap = artifacts_dir.join("firecracker-bootstrap.sh");
    let netplan = artifacts_dir.join("50-maturana-firecracker.yaml");
    let cloud_cfg = artifacts_dir.join("99-disable-network-config.cfg");
    let proxy_env_path = artifacts_dir.join("proxy.env");

    fs::write(&sessiond_env, render_session_env(&config))?;
    fs::write(&runner, render_run_agent())?;
    fs::write(
        &harness_install,
        render_harness_install(&config.harness, config.headless_chrome),
    )?;
    fs::write(&harness_install_service, render_harness_install_service())?;
    fs::write(&firecracker_bootstrap, render_firecracker_bootstrap())?;
    fs::write(
        &service,
        render_systemd_service(
            &format!(
                "Maturana {} agent {}",
                profile.harness_arg, profile.agent_id
            ),
            "ubuntu",
        ),
    )?;
    fs::write(
        &netplan,
        render_firecracker_netplan(&profile.guest_mac, &profile.guest_ip, &profile.host_ip),
    )?;
    fs::write(&cloud_cfg, render_firecracker_cloud_cfg())?;
    let proxy_env = if let Some(content) = render_firecracker_proxy_env(
        spec.network
            .proxy
            .as_ref()
            .map(|proxy| proxy.enabled)
            .unwrap_or(false),
        spec.network.proxy.as_ref().map(|proxy| proxy.bind.as_str()),
        &profile.host_ip,
    )? {
        fs::write(&proxy_env_path, content)?;
        Some(absolute_or_cwd(proxy_env_path)?)
    } else {
        None
    };

    Ok(FirecrackerGuestArtifacts {
        sessiond_env: absolute_or_cwd(sessiond_env)?,
        runner: absolute_or_cwd(runner)?,
        service: absolute_or_cwd(service)?,
        harness_install: absolute_or_cwd(harness_install)?,
        harness_install_service: absolute_or_cwd(harness_install_service)?,
        firecracker_bootstrap: absolute_or_cwd(firecracker_bootstrap)?,
        netplan: absolute_or_cwd(netplan)?,
        cloud_cfg: absolute_or_cwd(cloud_cfg)?,
        proxy_env,
    })
}

fn validate_and_materialize_firecracker_spec(
    home: &MaturanaHome,
    spec_path: &Path,
    apply: bool,
) -> anyhow::Result<()> {
    let raw = fs::read_to_string(spec_path)
        .with_context(|| format!("failed to read {}", spec_path.display()))?;
    let spec = AgentSpec::from_maturana_markdown(spec_path)
        .with_context(|| format!("failed to parse {}", spec_path.display()))?;
    let report = validate_spec(&spec);
    if !report.valid {
        anyhow::bail!("spec validation failed: {}", report.errors.join("; "));
    }
    materialize_agent(&spec, &raw, home, LaunchMode::DryRun)?;
    if apply {
        materialize_agent(&spec, &raw, home, LaunchMode::Apply)?;
    }
    Ok(())
}

/// Start an agent's per-agent egress proxy as a detached child. An agent
/// provisioned by setup firecracker-harnesses routes its egress through this
/// proxy when `maturana up` is not supervising the plane.
fn start_linux_agent_proxy(
    home: &MaturanaHome,
    profile: &FirecrackerHarnessProfile,
) -> anyhow::Result<()> {
    let spec = AgentSpec::from_maturana_markdown(PathBuf::from(&profile.spec_path))?;
    if !spec
        .network
        .proxy
        .as_ref()
        .is_some_and(|proxy| proxy.enabled)
    {
        return Ok(());
    }
    let _ = Command::new("pkill")
        .arg("-f")
        .arg(format!("pipelock proxy --agent-id {}", profile.agent_id))
        .status();
    let logs_dir = home.root().join("logs");
    fs::create_dir_all(&logs_dir)?;
    let stdout = fs::File::create(logs_dir.join(format!("proxy-{}.out.log", profile.agent_id)))?;
    let stderr = fs::File::create(logs_dir.join(format!("proxy-{}.err.log", profile.agent_id)))?;
    let child = Command::new(std::env::current_exe()?)
        .arg("--home")
        .arg(home.root())
        .arg("pipelock")
        .arg("proxy")
        .arg("--agent-id")
        .arg(&profile.agent_id)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .context("failed to start agent egress proxy")?;
    thread::sleep(Duration::from_millis(400));
    println!(
        "  egress proxy pid={} for {} (spec allowlist)",
        child.id(),
        profile.agent_id
    );
    Ok(())
}

fn tap_setup_args(tap_name: &str, host_ip: &str, cidr: &str) -> Vec<String> {
    vec![
        "./scripts/firecracker-setup-tap.sh".to_string(),
        tap_name.to_string(),
        format!("{host_ip}/30"),
        cidr.to_string(),
    ]
}

pub fn bind_port(bind: &str) -> anyhow::Result<&str> {
    bind.rsplit_once(':')
        .map(|(_, port)| port)
        .filter(|port| !port.is_empty())
        .ok_or_else(|| anyhow::anyhow!("sessiond bind must include a port: {bind}"))
}

fn assert_manifest_path(name: &str, actual: &Path, expected: &Path) -> anyhow::Result<()> {
    let actual = absolute_or_cwd(actual.to_path_buf())?;
    let expected = absolute_or_cwd(expected.to_path_buf())?;
    if actual != expected {
        anyhow::bail!(
            "Firecracker asset manifest {name} path mismatch: expected {}, got {}",
            expected.display(),
            actual.display()
        );
    }
    Ok(())
}

fn validate_exact_size_file(name: &str, path: &Path, manifest_bytes: u64) -> anyhow::Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("missing {name}: {}", path.display()))?;
    if metadata.len() == 0 {
        anyhow::bail!("{name} is empty: {}", path.display());
    }
    if metadata.len() != manifest_bytes {
        anyhow::bail!(
            "{name} size mismatch: manifest={} actual={} path={}",
            manifest_bytes,
            metadata.len(),
            path.display()
        );
    }
    Ok(())
}

fn validate_nonempty_file(name: &str, path: &Path) -> anyhow::Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("missing {name}: {}", path.display()))?;
    if metadata.len() == 0 {
        anyhow::bail!("{name} is empty: {}", path.display());
    }
    Ok(())
}

fn validate_elf_file(path: &Path) -> anyhow::Result<()> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)?;
    if magic != [0x7f, b'E', b'L', b'F'] {
        anyhow::bail!(
            "Firecracker kernel is not an ELF vmlinux: {}",
            path.display()
        );
    }
    Ok(())
}

fn validate_manifest_sha256(name: &str, path: &Path, expected: &str) -> anyhow::Result<()> {
    if expected.len() != 64 || !expected.chars().all(|ch| ch.is_ascii_hexdigit()) {
        anyhow::bail!("Firecracker asset manifest {name} sha256 is invalid");
    }
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected.to_ascii_lowercase() {
        anyhow::bail!(
            "Firecracker asset manifest {name} sha256 mismatch: expected {expected}, got {actual}"
        );
    }
    Ok(())
}

fn parse_harness_runtime(harness: &str) -> anyhow::Result<HarnessRuntime> {
    match harness {
        "codex" => Ok(HarnessRuntime::Codex),
        "claude-code" | "claude" => Ok(HarnessRuntime::ClaudeCode),
        "opencode" => Ok(HarnessRuntime::Opencode),
        other => anyhow::bail!("unsupported harness runtime: {other}"),
    }
}

fn absolute_or_cwd(path: PathBuf) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn run_checked_process(command: &mut Command, label: &str) -> anyhow::Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run {label}"))?;
    if !status.success() {
        anyhow::bail!("{label} failed with {status}");
    }
    Ok(())
}

fn harness_runtime_arg(harness: &HarnessRuntime) -> &'static str {
    match harness {
        HarnessRuntime::Codex => "codex",
        HarnessRuntime::ClaudeCode => "claude-code",
        HarnessRuntime::Opencode => "opencode",
    }
}

fn default_host_auth_source(harness: &HarnessRuntime) -> String {
    match harness {
        HarnessRuntime::Codex => ".maturana/host-auth/codex",
        HarnessRuntime::ClaudeCode => ".maturana/host-auth/claude-code",
        HarnessRuntime::Opencode => ".maturana/host-auth/opencode",
    }
    .to_string()
}

fn default_auth_guest_path(harness: &HarnessRuntime) -> String {
    match harness {
        HarnessRuntime::Codex => "/home/ubuntu/.codex",
        HarnessRuntime::ClaudeCode => "/home/ubuntu/.claude",
        HarnessRuntime::Opencode => "/home/ubuntu",
    }
    .to_string()
}

fn firecracker_image_name(agent_id: &str, rootfs_image: &str) -> String {
    PathBuf::from(rootfs_image)
        .parent()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| agent_id.to_string())
}

fn cidr_for_host_ip(host_ip: &str) -> anyhow::Result<String> {
    let addr: Ipv4Addr = host_ip
        .parse()
        .with_context(|| format!("invalid firecracker host_ip '{host_ip}'"))?;
    let network = u32::from(addr) & 0xFFFF_FFFC;
    Ok(format!("{}/30", Ipv4Addr::from(network)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_home(name: &str) -> MaturanaHome {
        let dir = std::env::temp_dir().join(format!(
            "maturana-ops-firecracker-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        MaturanaHome::new(dir)
    }

    #[test]
    fn builtin_profiles_carry_per_agent_session_and_tokens() {
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
        assert_eq!(
            firecracker_profile_for("claude-firecracker")
                .unwrap()
                .session_id,
            "claude-main"
        );
        assert!(firecracker_profile_for("not-a-fleet-agent").is_none());
    }

    #[test]
    fn profile_selection_defaults_to_builtins_plus_disk() {
        let home = temp_home("select");
        let dir = home.agent_dir("humberto-maturana");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("MATURANA.md"), spec_fixture()).unwrap();

        let profiles = selected_firecracker_profiles(&home, &[]).unwrap();
        assert_eq!(profiles[0].agent_id, "codex-firecracker");
        assert_eq!(profiles[1].harness_arg, "opencode");
        assert_eq!(profiles[2].auth_guest_path, "/home/ubuntu/.claude");
        assert!(profiles
            .iter()
            .any(|profile| profile.agent_id == "humberto-maturana"));

        let _ = fs::remove_dir_all(home.root());
    }

    #[test]
    fn profile_from_spec_makes_any_agent_first_class() {
        let home = temp_home("from-spec");
        let dir = home.agent_dir("humberto-maturana");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("MATURANA.md"), spec_fixture()).unwrap();

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

        let _ = fs::remove_dir_all(home.root());
    }

    #[test]
    fn named_missing_profile_points_to_launch_flow() {
        let home = temp_home("missing");
        let error = selected_firecracker_profiles(&home, &["missing".to_string()])
            .unwrap_err()
            .to_string();
        assert!(error.contains("no materialized spec for Firecracker agent 'missing'"));
        let _ = fs::remove_dir_all(home.root());
    }

    #[test]
    fn tap_setup_args_are_narrow_and_stable() {
        assert_eq!(
            tap_setup_args("tap-mat-codex", "172.30.10.1", "172.30.10.0/30"),
            vec![
                "./scripts/firecracker-setup-tap.sh",
                "tap-mat-codex",
                "172.30.10.1/30",
                "172.30.10.0/30"
            ]
        );
    }

    fn spec_fixture() -> &'static str {
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
         # Humberto\n"
    }
}
