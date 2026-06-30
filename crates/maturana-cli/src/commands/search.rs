use clap::Args;
use maturana_core::state::MaturanaHome;

/// Host-side web search: `maturana search "query" --provider brave|tavily`.
/// Keys live in pipelock (`brave/api-key`, `tavily/api-key`). Guests use the
/// maturana-web-search skill (proxy header injection) instead.
#[derive(Debug, Args)]
pub struct SearchCommand {
    pub query: Vec<String>,
    #[arg(long, default_value = "brave")]
    pub provider: String,
    #[arg(long, default_value_t = 5)]
    pub count: usize,
    #[arg(long)]
    pub json: bool,
}

pub fn run_search(home: &MaturanaHome, command: SearchCommand) -> anyhow::Result<()> {
    let query = command.query.join(" ");
    if query.trim().is_empty() {
        anyhow::bail!("search query is empty");
    }
    let provider: maturana_core::search::SearchProviderKind = command.provider.parse()?;
    let results = maturana_core::search::search(
        home.root(),
        provider,
        &maturana_core::search::SearchRequest {
            query,
            count: command.count,
        },
    )?;
    if command.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else if results.is_empty() {
        println!("(no results)");
    } else {
        for result in &results {
            println!("{}\n  {}\n  {}\n", result.title, result.url, result.snippet);
        }
    }
    Ok(())
}
