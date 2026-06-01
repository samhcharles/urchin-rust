//! Copilot CLI collector — reads `~/.copilot/command-history-state.json`.
//!
//! The history file contains `{"commandHistory": ["prompt1", "prompt2", ...]}` newest-first,
//! capped at a rolling window (currently 50). Because the whole array is rewritten on each
//! session there is no stable byte-offset anchor. Instead we track a bounded seen-set: every
//! prompt we have already emitted is stored (one per line) in a checkpoint file. On each run
//! we emit prompts that are not in the seen-set, then persist the merged set.
//!
//! The seen-set is capped at MAX_SEEN entries to keep the checkpoint file bounded. Old entries
//! fall off the front when the cap is hit; that can cause rare re-emission of very old prompts
//! but in practice the rolling window means they won't appear in the source file anyway.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use chrono::Utc;
use serde::Deserialize;

use urchin_core::{
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

const MAX_SEEN: usize = 2000;

pub struct CopilotOpts {
    pub history_path: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl CopilotOpts {
    pub fn defaults() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            history_path: home.join(".copilot").join("command-history-state.json"),
            checkpoint_path: state_dir().join("copilot.checkpoint"),
        }
    }
}

#[derive(Deserialize)]
struct CopilotHistory {
    #[serde(rename = "commandHistory", default)]
    command_history: Vec<String>,
}

/// Read new prompts from the Copilot CLI history file and append them as events.
/// Returns the number of events appended.
pub fn collect(journal: &Journal, identity: &Identity, opts: &CopilotOpts) -> Result<usize> {
    if !opts.history_path.exists() {
        return Ok(0);
    }

    let raw = fs::read_to_string(&opts.history_path)?;
    let history: CopilotHistory = serde_json::from_str(&raw)?;

    let seen = load_seen(&opts.checkpoint_path);
    let mut new_seen: Vec<String> = Vec::new();
    let mut count = 0;

    // The array is newest-first; iterate reversed to emit oldest-to-newest.
    for prompt in history.command_history.iter().rev() {
        let trimmed = prompt.trim();
        if trimmed.is_empty() || seen.contains(trimmed) {
            continue;
        }

        let mut event = Event::new("copilot", EventKind::Conversation, trimmed.to_string());
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: None,
            node_id: Some(identity.node_id.clone()),
        });
        // No per-entry timestamp in source; use current time.
        event.timestamp = Utc::now();

        journal.append(&event)?;
        new_seen.push(trimmed.to_string());
        count += 1;
    }

    if !new_seen.is_empty() {
        save_seen(&opts.checkpoint_path, seen, new_seen)?;
    }

    journal.flush()?;
    Ok(count)
}

fn load_seen(path: &PathBuf) -> HashSet<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

fn save_seen(
    path: &PathBuf,
    mut existing: HashSet<String>,
    new_entries: Vec<String>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    for e in &new_entries {
        existing.insert(e.clone());
    }

    // Convert to a vec so we can cap it. When over cap, keep the entries that
    // are most likely to still appear in the rolling source window — there's no
    // ordering guarantee, so we just keep the last MAX_SEEN lexicographically.
    let mut all: Vec<String> = existing.into_iter().collect();
    if all.len() > MAX_SEEN {
        all.sort();
        all.drain(..all.len() - MAX_SEEN);
    }

    fs::write(path, all.join("\n") + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, CopilotOpts, Journal, Identity) {
        let dir = tempfile::tempdir().unwrap();
        let opts = CopilotOpts {
            history_path: dir.path().join("command-history-state.json"),
            checkpoint_path: dir.path().join("copilot.checkpoint"),
        };
        let journal = Journal::new(dir.path().join("events.jsonl"));
        let identity = Identity::for_test("test", "test");
        (dir, opts, journal, identity)
    }

    fn write_history(path: &PathBuf, prompts: &[&str]) {
        let arr: Vec<serde_json::Value> = prompts.iter().map(|p| serde_json::json!(p)).collect();
        let obj = serde_json::json!({ "commandHistory": arr });
        fs::write(path, serde_json::to_string(&obj).unwrap()).unwrap();
    }

    #[test]
    fn no_history_file_returns_zero() {
        let (_dir, opts, journal, identity) = fixture();
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 0);
    }

    #[test]
    fn first_run_collects_all_entries() {
        let (_dir, opts, journal, identity) = fixture();
        // newest-first in file
        write_history(&opts.history_path, &["third", "second", "first"]);

        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 3);

        let events = journal.read_all().unwrap();
        assert_eq!(events.len(), 3);
        // emitted oldest-first
        assert_eq!(events[0].content, "first");
        assert_eq!(events[2].content, "third");
    }

    #[test]
    fn second_run_emits_nothing_without_new_entries() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(&opts.history_path, &["b", "a"]);

        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 2);
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 0);
    }

    #[test]
    fn new_entries_picked_up_on_next_run() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(&opts.history_path, &["b", "a"]);
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 2);

        // rolling window: "c" is new, "a" and "b" already seen
        write_history(&opts.history_path, &["c", "b", "a"]);
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 1);

        let events = journal.read_all().unwrap();
        assert_eq!(events[2].content, "c");
    }

    #[test]
    fn event_source_is_copilot() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(&opts.history_path, &["test prompt"]);
        collect(&journal, &identity, &opts).unwrap();

        let events = journal.read_all().unwrap();
        assert_eq!(events[0].source, "copilot");
        assert_eq!(events[0].kind, EventKind::Conversation);
    }

    #[test]
    fn empty_prompts_skipped() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(&opts.history_path, &["valid", "  ", ""]);
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 1);
    }
}
