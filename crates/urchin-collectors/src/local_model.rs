//! Local model collector — reads `~/.local/share/urchin/local-model.jsonl`.
//!
//! This is an **opt-in drop file**. When an external tool (Ollama wrapper,
//! LM Studio, or any local-inference harness) wants to push events into the
//! Urchin journal, it appends newline-delimited JSON records to this file.
//! Urchin only reads; it never writes to this file.
//!
//! Each line must be valid JSON with at minimum a `prompt` field:
//! ```json
//! {"prompt":"fix the build","model":"ollama:mistral","ts":"2026-05-01T10:00:00Z","workspace":"/opt/project"}
//! ```
//!
//! Fields:
//! - `prompt`    (required)  — the user intent sent to the local model
//! - `model`     (optional)  — e.g. `"ollama:mistral"`, becomes tag `model:ollama:mistral`
//! - `ts`        (optional)  — RFC3339 timestamp; defaults to now if absent or unparseable
//! - `workspace` (optional)  — absolute path of the project directory
//!
//! Checkpoint: byte-offset (same mechanism as the shell collector), stored as
//! a single `u64` text file at `XDG_STATE_HOME/urchin/local_model.offset`.
//!
//! `is_available()` returns `false` when the drop file does not yet exist,
//! so no noise is emitted before the user sets up a local model harness.

use std::{
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::PathBuf,
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use urchin_core::{
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

// ─── Options ─────────────────────────────────────────────────────────────────

pub struct LocalModelOpts {
    pub drop_file: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl LocalModelOpts {
    pub fn defaults() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            drop_file: home
                .join(".local")
                .join("share")
                .join("urchin")
                .join("local-model.jsonl"),
            checkpoint_path: state_dir().join("local_model.offset"),
        }
    }
}

// ─── Checkpoint ───────────────────────────────────────────────────────────────

fn read_checkpoint(path: &PathBuf) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn write_checkpoint(path: &PathBuf, offset: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, offset.to_string())?;
    Ok(())
}

// ─── Drop-file record ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Record {
    prompt: String,
    model: Option<String>,
    ts: Option<String>,
    workspace: Option<String>,
}

// ─── Collector function ───────────────────────────────────────────────────────

/// Read new records from the local-model drop file and append them to the journal.
pub fn collect(journal: &Journal, identity: &Identity, opts: &LocalModelOpts) -> Result<usize> {
    if !opts.drop_file.exists() {
        return Ok(0);
    }

    let mut file = File::open(&opts.drop_file)?;
    let file_size = file.metadata()?.len();
    let offset = read_checkpoint(&opts.checkpoint_path);

    if offset >= file_size {
        return Ok(0);
    }

    file.seek(SeekFrom::Start(offset))?;

    let mut reader = BufReader::new(&file);
    let mut line = String::new();
    let mut count = 0usize;
    let mut pos = offset;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        pos += bytes_read as u64;

        let raw = line.trim();
        if raw.is_empty() {
            continue;
        }

        let record: Record = match serde_json::from_str(raw) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("local-model collector: skipping malformed line: {}", e);
                continue;
            }
        };

        if record.prompt.trim().is_empty() {
            continue;
        }

        let ts: DateTime<Utc> = record
            .ts
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);

        let mut tags = vec!["auto-collected".to_string(), "local-model".to_string()];
        if let Some(m) = &record.model {
            tags.push(format!("model:{}", m));
        }

        let mut event = Event::new("local-model", EventKind::Conversation, record.prompt.trim());
        event.timestamp = ts;
        event.workspace = record.workspace.clone();
        event.tags = tags;
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: record.workspace,
            node_id: Some(identity.node_id.clone()),
        });

        journal.append(&event)?;
        count += 1;
    }

    if count > 0 {
        write_checkpoint(&opts.checkpoint_path, pos)?;
    }

    journal.flush()?;
    Ok(count)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_opts(tmp: &TempDir, drop_file: PathBuf) -> LocalModelOpts {
        LocalModelOpts {
            drop_file,
            checkpoint_path: tmp.path().join("local_model.offset"),
        }
    }

    fn dummy_journal(tmp: &TempDir) -> (Journal, Identity) {
        let j = Journal::new(tmp.path().join("journal.jsonl"));
        let id = Identity::for_test("test", "test");
        (j, id)
    }

    #[test]
    fn no_drop_file_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let opts = LocalModelOpts {
            drop_file: tmp.path().join("missing.jsonl"),
            checkpoint_path: tmp.path().join("ckpt"),
        };
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn collects_valid_records() {
        let tmp = TempDir::new().unwrap();
        let drop = tmp.path().join("local-model.jsonl");
        let mut f = File::create(&drop).unwrap();
        writeln!(f, r#"{{"prompt":"fix the memory leak","model":"ollama:mistral","ts":"2026-01-01T00:00:00Z","workspace":"/opt/project"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"prompt":"add retry logic","model":"lmstudio:llama3"}}"#
        )
        .unwrap();
        let opts = make_opts(&tmp, drop);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 2);
    }

    #[test]
    fn watermark_prevents_reprocessing() {
        let tmp = TempDir::new().unwrap();
        let drop = tmp.path().join("local-model.jsonl");
        let mut f = File::create(&drop).unwrap();
        writeln!(f, r#"{{"prompt":"first run","model":"ollama:mistral"}}"#).unwrap();
        let opts = make_opts(&tmp, drop.clone());
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
        let opts2 = make_opts(&tmp, drop);
        assert_eq!(collect(&j, &id, &opts2).unwrap(), 0);
    }

    #[test]
    fn picks_up_new_appended_records() {
        let tmp = TempDir::new().unwrap();
        let drop = tmp.path().join("local-model.jsonl");
        let mut f = File::create(&drop).unwrap();
        writeln!(f, r#"{{"prompt":"first"}}"#).unwrap();
        let opts = make_opts(&tmp, drop.clone());
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
        // append more
        let mut f2 = std::fs::OpenOptions::new()
            .append(true)
            .open(&drop)
            .unwrap();
        writeln!(f2, r#"{{"prompt":"second","model":"ollama:phi4"}}"#).unwrap();
        let opts2 = make_opts(&tmp, drop);
        assert_eq!(collect(&j, &id, &opts2).unwrap(), 1);
    }

    #[test]
    fn skips_malformed_and_empty_lines() {
        let tmp = TempDir::new().unwrap();
        let drop = tmp.path().join("local-model.jsonl");
        let mut f = File::create(&drop).unwrap();
        writeln!(f, "{{not json}}").unwrap();
        writeln!(f).unwrap(); // empty line
        writeln!(f, r#"{{"prompt":"valid"}}"#).unwrap();
        let opts = make_opts(&tmp, drop);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
    }

    #[test]
    fn model_tag_included_when_present() {
        let tmp = TempDir::new().unwrap();
        let drop = tmp.path().join("local-model.jsonl");
        let mut f = File::create(&drop).unwrap();
        writeln!(f, r#"{{"prompt":"test","model":"ollama:mistral"}}"#).unwrap();
        let opts = make_opts(&tmp, drop);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
        // verify event has model tag by reading journal
        let content = std::fs::read_to_string(tmp.path().join("journal.jsonl")).unwrap();
        assert!(content.contains("model:ollama:mistral"));
    }
}
