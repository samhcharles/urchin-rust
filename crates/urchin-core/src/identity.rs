use serde::{Deserialize, Serialize};

/// Runtime identity — who is running this instance of Urchin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub account: String,
    pub device: String,
}

impl Identity {
    pub fn resolve() -> Self {
        let account = std::env::var("URCHIN_ACCOUNT")
            .unwrap_or_else(|_| whoami_account());
        let device = std::env::var("URCHIN_DEVICE")
            .unwrap_or_else(|_| hostname());
        Self { account, device }
    }
}

fn whoami_account() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}
