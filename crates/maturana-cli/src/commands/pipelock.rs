use anyhow::Context;
use clap::{Args, Subcommand};
use maturana_core::{
    pipelock::PipelockVault,
    pipelock_proxy::{ensure_mitm_ca_cert, run_proxy, HeaderInjection, ProxyConfig},
    spec::AgentSpec,
    state::MaturanaHome,
    validate_spec,
};
use std::{fs, io::Read, path::PathBuf};

#[derive(Debug, Args)]
pub(crate) struct PipelockCommand {
    #[command(subcommand)]
    pub(crate) command: PipelockSubcommand,
}

#[derive(Debug, Subcommand)]
pub(crate) enum PipelockSubcommand {
    Init,
    Set {
        name: String,
        #[arg(long, conflicts_with = "value_file")]
        value: Option<String>,
        #[arg(long)]
        value_file: Option<PathBuf>,
    },
    Get {
        name: String,
    },
    List,
    Delete {
        name: String,
    },
    CaCert,
    Proxy {
        /// Resolve the agent's spec from `--home` (the agent's MATURANA.md).
        /// Preferred over `--spec` for supervised runs: it is independent of the
        /// process working directory, which `maturana up` does not set.
        #[arg(long)]
        agent_id: Option<String>,
        #[arg(long)]
        spec: Option<PathBuf>,
        #[arg(long)]
        bind: Option<String>,
        #[arg(long = "allow")]
        allowlist: Vec<String>,
        /// Permit ANY host (egress governance off). Traffic still flows through the
        /// proxy -- header injection + audit keep working -- it is just never denied.
        /// Equivalent to `network.egress_allow_all: true` in the spec.
        #[arg(long = "allow-all")]
        allow_all: bool,
        #[arg(long = "inject-header")]
        inject_headers: Vec<HeaderInjectionArg>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct HeaderInjectionArg(HeaderInjection);

impl std::str::FromStr for HeaderInjectionArg {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(HeaderInjection::parse(value)?))
    }
}

pub(crate) fn handle_pipelock(command: PipelockCommand, home: &MaturanaHome) -> anyhow::Result<()> {
    let vault = PipelockVault::new(home.pipelock_dir());
    match command.command {
        PipelockSubcommand::Init => {
            vault.init()?;
            println!("pipelock vault: {}", vault.vault_path().display());
            println!("pipelock key: {}", vault.key_path().display());
        }
        PipelockSubcommand::Set {
            name,
            value,
            value_file,
        } => {
            let value = read_pipelock_value(value, value_file)?;
            vault.set(&name, &value)?;
            println!("pipelock secret stored: {name}");
        }
        PipelockSubcommand::Get { name } => {
            println!("{}", vault.get(&name)?);
        }
        PipelockSubcommand::List => {
            for name in vault.list()? {
                println!("{name}");
            }
        }
        PipelockSubcommand::Delete { name } => {
            if vault.delete(&name)? {
                println!("pipelock secret deleted: {name}");
            } else {
                println!("pipelock secret not found: {name}");
            }
        }
        PipelockSubcommand::CaCert => {
            let path = ensure_mitm_ca_cert(home.root())?;
            println!("{}", path.display());
        }
        PipelockSubcommand::Proxy {
            agent_id,
            spec,
            bind,
            allowlist,
            allow_all,
            inject_headers,
        } => {
            run_pipelock_proxy(
                home,
                agent_id,
                spec,
                bind,
                allowlist,
                allow_all,
                inject_headers,
            )?;
        }
    }
    Ok(())
}

fn read_pipelock_value(
    value: Option<String>,
    value_file: Option<PathBuf>,
) -> anyhow::Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    if let Some(path) = value_file {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        return Ok(trim_trailing_newlines(raw));
    }

    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    if raw.is_empty() {
        anyhow::bail!("pipelock set requires --value, --value-file, or stdin");
    }
    Ok(trim_trailing_newlines(raw))
}

fn trim_trailing_newlines(mut value: String) -> String {
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
    value
}

fn run_pipelock_proxy(
    home: &MaturanaHome,
    agent_id: Option<String>,
    spec: Option<PathBuf>,
    bind: Option<String>,
    allowlist: Vec<String>,
    allow_all: bool,
    inject_headers: Vec<HeaderInjectionArg>,
) -> anyhow::Result<()> {
    // `--agent-id` resolves the spec from `--home` so supervised runs (where the
    // working directory is unset) work; an explicit `--spec` still takes
    // precedence when both are given.
    let spec = spec.or_else(|| agent_id.map(|id| home.agent_dir(&id).join("MATURANA.md")));
    let (bind, mut config) = match spec {
        Some(spec_path) => {
            let spec = AgentSpec::from_maturana_markdown(&spec_path)
                .with_context(|| format!("failed to read {}", spec_path.display()))?;
            let report = validate_spec(&spec);
            if !report.valid {
                anyhow::bail!(
                    "spec is invalid; run `maturana spec validate {}`",
                    spec_path.display()
                );
            }
            let proxy = spec.network.proxy.as_ref().ok_or_else(|| {
                anyhow::anyhow!("{} does not declare network.proxy", spec_path.display())
            })?;
            let bind = bind.unwrap_or_else(|| proxy.bind.clone());
            let audit_path = home
                .audit_dir()
                .join(format!("{}-pipelock-proxy.jsonl", spec.identity.id));
            (
                bind,
                ProxyConfig::from_spec(home.root().to_path_buf(), &spec, audit_path)?,
            )
        }
        None => {
            let audit_path = home.audit_dir().join("pipelock-proxy.jsonl");
            (
                bind.unwrap_or_else(|| "127.0.0.1:47833".to_string()),
                ProxyConfig {
                    home_root: home.root().to_path_buf(),
                    allowlist: Vec::new(),
                    allow_all: false,
                    injections: Vec::new(),
                    audit_path,
                    runtime_allow: Default::default(),
                },
            )
        }
    };
    config.allowlist.extend(allowlist);
    config.allow_all = config.allow_all || allow_all;
    config
        .injections
        .extend(inject_headers.into_iter().map(|injection| injection.0));
    if config.allowlist.is_empty() && !config.allow_all {
        anyhow::bail!(
            "pipelock proxy requires network.egress_allowlist, --allow <host>, or --allow-all"
        );
    }
    println!("pipelock proxy listening on {bind}");
    if config.allow_all {
        println!("pipelock proxy allowlist: ALLOW-ALL (egress governance off; still audited)");
    } else {
        println!("pipelock proxy allowlist: {}", config.allowlist.join(", "));
    }
    println!("pipelock proxy audit: {}", config.audit_path.display());
    run_proxy(&bind, config)?;
    Ok(())
}
