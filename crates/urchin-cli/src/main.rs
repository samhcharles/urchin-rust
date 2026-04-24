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
    /// Start the MCP server over stdio
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
        #[arg(short, long)]
        title: Option<String>,
        #[arg(short = 'T', long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Event kind: conversation | agent | command | commit | file (default: conversation)
        #[arg(short, long, default_value = "conversation")]
        kind: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("URCHIN_LOG").unwrap_or_else(|_| "urchin=info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Serve  => serve().await,
        Commands::Mcp    => mcp().await,
        Commands::Doctor => doctor().await,
        Commands::Ingest { content, source, workspace, title, tags, kind } => {
            ingest(content, source, workspace, title, tags, kind)
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
    use urchin_core::{config::Config, identity::Identity, journal::Journal};

    let cfg = Config::load();
    let identity = Identity::resolve();
    let journal = Journal::new(cfg.journal_path.clone());
    let stats = journal.stats()?;

    println!("urchin doctor");
    println!();

    println!("  identity:");
    println!("    account:  {}", identity.account);
    println!("    device:   {}", identity.device);
    println!();

    println!("  config:");
    let config_path = Config::config_path();
    let config_source = if config_path.exists() {
        config_path.display().to_string()
    } else {
        format!("{} (not found, using defaults)", config_path.display())
    };
    println!("    config:   {}", config_source);
    println!("    vault:    {}", cfg.vault_root.display());
    println!("    intake:   {}", cfg.intake_port);
    println!();

    println!("  journal:");
    if stats.event_count == 0 && !journal.exists() {
        println!("    path:     {}", cfg.journal_path.display());
        println!("    status:   not found");
    } else {
        println!("    path:     {}", cfg.journal_path.display());
        println!("    events:   {}", stats.event_count);
        println!("    size:     {} KB", stats.file_size_bytes / 1024);
        if let Some(last) = stats.last_event {
            println!("    last:     {} ({})", last.timestamp.format("%Y-%m-%dT%H:%M:%SZ"), last.source);
        }
    }

    Ok(())
}

fn ingest(
    content: String,
    source: Option<String>,
    workspace: Option<String>,
    title: Option<String>,
    tags: Vec<String>,
    kind: String,
) -> Result<()> {
    use urchin_core::{
        config::Config,
        event::{Actor, Event, EventKind},
        identity::Identity,
        journal::Journal,
    };

    let cfg = Config::load();
    let journal = Journal::new(cfg.journal_path);
    let identity = Identity::resolve();

    let event_kind = match kind.as_str() {
        "agent"        => EventKind::Agent,
        "command"      => EventKind::Command,
        "commit"       => EventKind::Commit,
        "file"         => EventKind::File,
        "conversation" => EventKind::Conversation,
        other          => EventKind::Other(other.to_string()),
    };

    let mut event = Event::new(
        source.unwrap_or_else(|| "cli".into()),
        event_kind,
        content,
    );
    event.workspace = workspace;
    event.title = title;
    event.tags = tags;
    event.actor = Some(Actor {
        account: Some(identity.account),
        device: Some(identity.device),
        workspace: event.workspace.clone(),
    });

    journal.append(&event)?;
    println!("ingested: {}", event.id);
    Ok(())
}
