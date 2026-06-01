use anyhow::Result;
use pipeline::PassConfig;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

mod cluster;
mod cursor;
mod filter;
mod pipeline;
mod store;

const POLL_INTERVAL: Duration = Duration::from_secs(30);

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let journal_path = urchin_core::journal::Journal::default_path();
    let config = PassConfig::with_defaults(journal_path);

    tracing::info!("sleipnir: starting — journal={}", config.journal_path.display());

    loop {
        match pipeline::run_pass(&config) {
            Ok(r) if r.events_read > 0 => {
                tracing::info!(
                    events_read = r.events_read,
                    events_kept = r.events_kept,
                    clusters = r.clusters_produced,
                    offset = r.new_offset,
                    "pass complete",
                );
            }
            Ok(_) => {}
            Err(e) => tracing::error!("pass failed: {:#}", e),
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}
