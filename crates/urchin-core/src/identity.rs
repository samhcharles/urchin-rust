use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Cryptographic identity for this Urchin node.
/// `node_id` is the hex-encoded Ed25519 public key — stable across restarts,
/// unique per machine, and verifiable during peer sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub account: String,
    pub device: String,
    /// 64-character hex string (32-byte Ed25519 public key).
    /// Empty string in records pre-dating node identity.
    #[serde(default)]
    pub node_id: String,
}

impl Identity {
    /// Resolve identity for the running node. Loads the Ed25519 keypair from
    /// `~/.local/share/urchin/node.key`, generating and persisting it on first run.
    pub fn resolve() -> Self {
        let account = std::env::var("URCHIN_ACCOUNT").unwrap_or_else(|_| whoami_account());
        let device = std::env::var("URCHIN_DEVICE").unwrap_or_else(|_| hostname());
        let node_id = load_or_generate_node_id();
        Self {
            account,
            device,
            node_id,
        }
    }

    /// Deterministic identity for use in tests. No disk I/O, no keypair generation.
    pub fn for_test(account: impl Into<String>, device: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            device: device.into(),
            node_id: "0".repeat(64),
        }
    }
}

fn load_or_generate_node_id() -> String {
    let key_path = node_key_path();
    let signing_key = if key_path.exists() {
        load_signing_key(&key_path).unwrap_or_else(|_| create_and_save_signing_key(&key_path))
    } else {
        create_and_save_signing_key(&key_path)
    };
    hex::encode(signing_key.verifying_key().as_bytes())
}

fn node_key_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("urchin")
        .join("node.key")
}

fn load_signing_key(path: &Path) -> anyhow::Result<SigningKey> {
    let bytes = std::fs::read(path)?;
    let seed: [u8; 32] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("node.key corrupt: expected 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

// Private key retained for future peer authentication handshake (see peers.rs roadmap).
fn create_and_save_signing_key(path: &Path) -> SigningKey {
    let signing_key = SigningKey::generate(&mut OsRng);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(path, signing_key.to_bytes()) {
        tracing::warn!("failed to persist node.key to {}: {}", path.display(), e);
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    signing_key
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn for_test_node_id_is_64_chars() {
        let id = Identity::for_test("alice", "macbook");
        assert_eq!(id.node_id.len(), 64);
        assert_eq!(id.account, "alice");
        assert_eq!(id.device, "macbook");
    }

    #[test]
    fn generate_and_reload_gives_same_node_id() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("node.key");

        let k1 = create_and_save_signing_key(&key_path);
        let id1 = hex::encode(k1.verifying_key().as_bytes());

        let k2 = load_signing_key(&key_path).unwrap();
        let id2 = hex::encode(k2.verifying_key().as_bytes());

        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 64);
    }

    #[test]
    fn corrupt_key_returns_error() {
        let tmp = TempDir::new().unwrap();
        let key_path = tmp.path().join("node.key");
        std::fs::write(&key_path, b"bad data").unwrap();
        assert!(load_signing_key(&key_path).is_err());
    }

    #[test]
    fn node_ids_are_unique_across_generates() {
        let tmp = TempDir::new().unwrap();
        let k1 = create_and_save_signing_key(&tmp.path().join("k1.key"));
        let k2 = create_and_save_signing_key(&tmp.path().join("k2.key"));
        assert_ne!(
            hex::encode(k1.verifying_key().as_bytes()),
            hex::encode(k2.verifying_key().as_bytes())
        );
    }
}
