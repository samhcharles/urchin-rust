use clap::{Parser, Subcommand};
use anyhow::Result;

#[derive(Parser)]
#[command(name = "urchin", about = "Local-first memory sync substrate for AI tools")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the background daemon (intake + sync)
    Serve,
    /// Start the MCP server (stdio)
    Mcp,
    /// Show status and health
    Doctor,
    /// Ingest an event from the command line
    Ingest {
        #[arg(short, long)]
        content: String,
        #[arg(short, long)]
        source: Option<String>,
        #[arg(short, long)]
        workspace: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("URCHIN_LOG")
                .unwrap_or_else(|_| "urchin=info".into())
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve    => serve().await,
        Commands::Mcp      => mcp().await,
        Commands::Doctor   => doctor().await,
        Commands::Ingest { content, source, workspace } => {
            ingest(content, source, workspace)
        }
    }
}

async fn serve() -> Result<()> {
    tracing::info!("urchin serve — not yet implemented");
    Ok(())
}

async fn mcp() -> Result<()> {
    tracing::info!("urchin mcp — not yet implemented");
    Ok(())
}

async fn doctor() -> Result<()> {
    let cfg = urchin_core::config::Config::load();
    println!("vault_root:   {}", cfg.vault_root.display());
    println!("journal:      {}", cfg.journal_path.display());
    println!("cache:        {}", cfg.cache_path.display());
    println!("intake_port:  {}", cfg.intake_port);
    Ok(())
}

fn ingest(content: String, source: Option<String>, workspace: Option<String>) -> Result<()> {
    use urchin_core::{event::{Event, EventKind}, journal::Journal, config::Config};

    let cfg = Config::load();
    let journal = Journal::new(cfg.journal_path);

    let mut event = Event::new(
        source.unwrap_or_else(|| "cli".into()),
        EventKind::Conversation,
        content,
    );
    event.workspace = workspace;

    journal.append(&event)?;
    println!("ingested: {}", event.id);
    Ok(())
}
