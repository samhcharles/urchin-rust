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
    /// Run a collector once and append new events to the journal
    Collect {
        #[command(subcommand)]
        which: CollectKind,
    },
}

#[derive(Subcommand)]
enum CollectKind {
    /// Tail ~/.bash_history for new commands
    Shell,
    /// Ingest commits from one or more git repos.
    /// Repos can be passed via --repo (repeatable) or via URCHIN_REPO_ROOTS (colon-separated).
    Git {
        #[arg(short, long)]
        repo: Vec<String>,
    },
    /// Run every collector that has a default path (currently: shell, git via URCHIN_REPO_ROOTS)
    All,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
        Commands::Collect { which } => collect(which),
    }
}

async fn serve() -> Result<()> {
    let cfg = urchin_core::config::Config::load();
    urchin_intake::server::serve(&cfg).await
}

async fn mcp() -> Result<()> {
    let cfg = urchin_core::config::Config::load();
    urchin_mcp::server::run(cfg).await
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

fn collect(which: CollectKind) -> Result<()> {
    use urchin_collectors::{git as git_col, shell as shell_col};
    use urchin_core::{config::Config, identity::Identity, journal::Journal};

    let cfg = Config::load();
    let identity = Identity::resolve();
    let journal = Journal::new(cfg.journal_path.clone());

    match which {
        CollectKind::Shell => {
            let opts = shell_col::ShellOpts::defaults();
            let n = shell_col::collect(&journal, &identity, &opts)?;
            println!("shell: {} new events", n);
        }
        CollectKind::Git { repo } => {
            let repos = resolve_repos(repo);
            if repos.is_empty() {
                eprintln!("no repos given. Pass --repo <path> or set URCHIN_REPO_ROOTS.");
                return Ok(());
            }
            let mut total = 0;
            for r in &repos {
                let opts = git_col::GitOpts::defaults_for(r.clone());
                match git_col::collect_repo(&journal, &identity, &opts) {
                    Ok(n) => {
                        println!("git {}: {} new commits", r.display(), n);
                        total += n;
                    }
                    Err(e) => eprintln!("git {} skipped: {}", r.display(), e),
                }
            }
            println!("git total: {}", total);
        }
        CollectKind::All => {
            let opts = shell_col::ShellOpts::defaults();
            match shell_col::collect(&journal, &identity, &opts) {
                Ok(n)  => println!("shell: {} new events", n),
                Err(e) => eprintln!("shell skipped: {}", e),
            }
            for r in &resolve_repos(vec![]) {
                let opts = git_col::GitOpts::defaults_for(r.clone());
                match git_col::collect_repo(&journal, &identity, &opts) {
                    Ok(n)  => println!("git {}: {} new commits", r.display(), n),
                    Err(e) => eprintln!("git {} skipped: {}", r.display(), e),
                }
            }
        }
    }

    Ok(())
}

fn resolve_repos(from_args: Vec<String>) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut out: Vec<PathBuf> = from_args.into_iter().map(PathBuf::from).collect();
    if out.is_empty() {
        if let Ok(env) = std::env::var("URCHIN_REPO_ROOTS") {
            out.extend(env.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));
        }
    }
    out
}
