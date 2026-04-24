use std::path::PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub vault_root: PathBuf,
    pub journal_path: PathBuf,
    pub cache_path: PathBuf,
    pub intake_port: u16,
    pub remote_host: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("urchin");

        Self {
            vault_root: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join("brain"),
            journal_path: data_dir.join("journal").join("events.jsonl"),
            cache_path: data_dir.join("event-cache.jsonl"),
            intake_port: 18799,
            remote_host: None,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        // TODO: load from ~/.config/urchin/config.toml, override with env vars
        let mut cfg = Self::default();
        if let Ok(vault) = std::env::var("URCHIN_VAULT_ROOT") {
            cfg.vault_root = PathBuf::from(vault);
        }
        if let Ok(port) = std::env::var("URCHIN_INTAKE_PORT") {
            cfg.intake_port = port.parse().unwrap_or(18799);
        }
        cfg
    }
}
