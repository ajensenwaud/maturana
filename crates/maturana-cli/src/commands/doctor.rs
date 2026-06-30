use clap::Args;
use maturana_core::state::MaturanaHome;

#[derive(Debug, Args)]
pub(crate) struct DoctorCommand {
    #[arg(long = "agent-id")]
    pub(crate) agent_ids: Vec<String>,
    #[arg(long)]
    pub(crate) json: bool,
    #[arg(long, default_value = "http://127.0.0.1:47834")]
    pub(crate) sessiond_url: String,
}

pub(crate) fn run_doctor(home: &MaturanaHome, command: DoctorCommand) -> anyhow::Result<()> {
    let report =
        maturana_ops::doctor::build_report(home, &command.agent_ids, &command.sessiond_url);

    if command.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("maturana.ok: {}", report.ok);
        println!("home: {}", report.home);
        print_doctor_check("hostd", &report.hostd);
        print_doctor_check("sessiond", &report.sessiond);
        for agent in &report.agents {
            println!("agent: {}", agent.agent_id);
            print_doctor_check("  vm", &agent.vm);
            print_doctor_check("  telegram", &agent.telegram);
            print_doctor_check("  guest_worker", &agent.guest_worker);
        }
    }
    if !report.ok {
        anyhow::bail!("maturana doctor found unhealthy components");
    }
    Ok(())
}

fn print_doctor_check(label: &str, check: &maturana_ops::doctor::DoctorCheck) {
    println!("{label}.ok: {}", check.ok);
    if !check.message.is_empty() {
        println!("{label}.message: {}", check.message);
    }
}
