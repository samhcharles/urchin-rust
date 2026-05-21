use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub vault_root: PathBuf,
    pub journal_path: PathBuf,
    pub cache_path: PathBuf,
    pub intake_port: u16,
    pub cloud_url: Option<String>,
    pub cloud_token: Option<String>,
    /// Bearer token required on POST /ingest. If None, auth is disabled (loopback-only default).
    pub intake_token: Option<String>,
}

/// The on-disk TOML representation — all fields optional so partial files work.
#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    vault_root: Option<PathBuf>,
    journal_path: Option<PathBuf>,
    cache_path: Option<PathBuf>,
    intake_port: Option<u16>,
    cloud_url: Option<String>,
    cloud_token: Option<String>,
    intake_token: Option<String>,
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
            cloud_url: None,
            cloud_token: None,
            intake_token: None,
        }
    }
}

impl Config {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("urchin")
            .join("config.toml")
    }

    pub fn load() -> Self {
        let mut cfg = Self::default();

        // Layer 1: config file
        let config_path = Self::config_path();
        if config_path.exists() {
            if let Ok(raw) = std::fs::read_to_string(&config_path) {
                if let Ok(file_cfg) = toml::from_str::<FileConfig>(&raw) {
                    if let Some(v) = file_cfg.vault_root {
                        cfg.vault_root = v;
                    }
                    if let Some(v) = file_cfg.journal_path {
                        cfg.journal_path = v;
                    }
                    if let Some(v) = file_cfg.cache_path {
                        cfg.cache_path = v;
                    }
                    if let Some(v) = file_cfg.intake_port {
                        cfg.intake_port = v;
                    }
                    if let Some(v) = file_cfg.cloud_url {
                        cfg.cloud_url = Some(v);
                    }
                    if let Some(v) = file_cfg.cloud_token {
                        cfg.cloud_token = Some(v);
                    }
                    if let Some(v) = file_cfg.intake_token {
                        cfg.intake_token = Some(v);
                    }
                }
            }
        }

        // Layer 2: env var overrides
        if let Ok(v) = std::env::var("URCHIN_VAULT_ROOT") {
            cfg.vault_root = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("URCHIN_JOURNAL_PATH") {
            cfg.journal_path = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("URCHIN_INTAKE_PORT") {
            cfg.intake_port = v.parse().unwrap_or(18799);
        }
        if let Ok(v) = std::env::var("URCHIN_CLOUD_URL") {
            cfg.cloud_url = Some(v);
        }
        if let Ok(v) = std::env::var("URCHIN_CLOUD_TOKEN") {
            cfg.cloud_token = Some(v);
        }
        if let Ok(v) = std::env::var("URCHIN_INTAKE_TOKEN") {
            cfg.intake_token = Some(v);
        }

        cfg
    }

    /// Set a single key in the config file (creates the file if missing).
    /// Keys: vault_root, journal_path, cache_path, intake_port, cloud_url, cloud_token
    pub fn set_field(key: &str, value: &str) -> anyhow::Result<()> {
        use anyhow::Context;
        let path = Self::config_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut table: toml::value::Table = if path.exists() {
            toml::from_str(
                &std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
            )
            .with_context(|| "parsing config.toml")?
        } else {
            toml::value::Table::new()
        };
        if value.is_empty() {
            table.remove(key);
        } else {
            table.insert(key.to_string(), toml::Value::String(value.to_string()));
        }
        std::fs::write(&path, toml::to_string(&table)?)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}
