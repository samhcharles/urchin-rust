//! Claude CLI collector — reads `~/.claude/history.jsonl` and emits Events.
//!
//! history.jsonl records every user input to Claude Code: plain prompts, pasted
//! blocks, and slash commands. We skip slash commands and preserve the original
//! timestamp from each record. Checkpoint is a byte offset so we never re-emit
//! entries across runs, even if a record has no timestamp.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use anyhow::Result;
use chrono::DateTime;
use serde::Deserialize;

use urchin_core::{
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

pub struct ClaudeOpts {
    pub history_path: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl ClaudeOpts {
    pub fn defaults() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            history_path: home.join(".claude").join("history.jsonl"),
            checkpoint_path: state_dir().join("claude.checkpoint"),
        }
    }
}

/// One line from ~/.claude/history.jsonl.
#[derive(Debug, Deserialize)]
struct ClaudeHistoryLine {
    display: String,
    /// Pasted blocks keyed by numeric string ("1", "2", …).
    #[serde(rename = "pastedContents", default)]
    pasted_contents: HashMap<String, PastedEntry>,
    /// Unix timestamp in milliseconds. Missing on some early records.
    timestamp: Option<i64>,
    /// Working directory when the prompt was entered.
    project: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// One pasted block within a history line.
/// `content` is the full text; absent when the block was too large and only
/// a `contentHash` was written instead.
#[derive(Debug, Deserialize)]
struct PastedEntry {
    content: Option<String>,
}

/// Read new entries from the Claude CLI history file and append them as events.
/// Returns the number of events appended.
pub fn collect(journal: &Journal, identity: &Identity, opts: &ClaudeOpts) -> Result<usize> {
    if !opts.history_path.exists() {
        return Ok(0);
    }

    let file_size = fs::metadata(&opts.history_path)?.len();
    let checkpoint = read_checkpoint(&opts.checkpoint_path);

    // If the file shrank (cleared/rotated), start over.
    let start = if checkpoint > file_size {
        0
    } else {
        checkpoint
    };

    let mut file = File::open(&opts.history_path)?;
    file.seek(SeekFrom::Start(start))?;

    let mut count = 0;
    for line in BufReader::new(file).lines() {
        let raw = line?;
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }

        let parsed: ClaudeHistoryLine = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("claude collector: skipping malformed line: {}", e);
                continue;
            }
        };

        let content = match extract_content(&parsed) {
            Some(c) => c,
            None => continue,
        };

        let kind = infer_kind(&parsed.display);
        let mut event = Event::new("claude-cli", kind, content);

        // Preserve the original timestamp from the history record.
        if let Some(ts_ms) = parsed.timestamp {
            if let Some(ts) = DateTime::from_timestamp_millis(ts_ms) {
                event.timestamp = ts;
            }
        }

        event.workspace = parsed.project.clone();
        event.session = parsed.session_id.clone();
        event.title = Some(truncate(&parsed.display, 80));
        event.tags = vec!["auto-collected".to_string(), "claude".to_string()];
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: parsed.project,
            node_id: Some(identity.node_id.clone()),
        });

        journal.append(&event)?;
        count += 1;
    }

    write_checkpoint(&opts.checkpoint_path, file_size)?;
    journal.flush()?;
    Ok(count)
}

/// Return the content string, or `None` if this entry should be skipped.
/// Slash commands (/clear, /resume, /compact, …) are skipped entirely.
/// Pasted block full text takes precedence over the display summary.
fn extract_content(line: &ClaudeHistoryLine) -> Option<String> {
    let display = line.display.trim();

    if display.is_empty() || display.starts_with('/') {
        return None;
    }

    // Assemble full text from pasted blocks in insertion order (numeric key sort).
    if !line.pasted_contents.is_empty() {
        let mut keys: Vec<u64> = line
            .pasted_contents
            .keys()
            .filter_map(|k| k.parse().ok())
            .collect();
        keys.sort_unstable();

        let parts: Vec<&str> = keys
            .iter()
            .filter_map(|k| line.pasted_contents.get(&k.to_string()))
            .filter_map(|e| e.content.as_deref())
            .filter(|s| !s.trim().is_empty())
            .collect();

        if !parts.is_empty() {
            return Some(parts.join("\n\n"));
        }
    }

    Some(display.to_string())
}

/// Map display text to EventKind.
/// SYSTEM DIRECTIVE / agent-style pastes → Agent; everything else → Conversation.
fn infer_kind(display: &str) -> EventKind {
    let upper = display.to_uppercase();
    if upper.contains("SYSTEM DIRECTIVE") || upper.contains("SYSTEM PROMPT") {
        EventKind::Agent
    } else {
        EventKind::Conversation
    }
}

pub(crate) fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n).collect();
        out.push('…');
        out
    }
}

fn read_checkpoint(path: &PathBuf) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_checkpoint(path: &PathBuf, offset: u64) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, offset.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, ClaudeOpts, Journal, Identity) {
        let dir = tempfile::tempdir().unwrap();
        let opts = ClaudeOpts {
            history_path: dir.path().join("history.jsonl"),
            checkpoint_path: dir.path().join("claude.checkpoint"),
        };
        let journal = Journal::new(dir.path().join("events.jsonl"));
        let identity = Identity::for_test("test", "test");
        (dir, opts, journal, identity)
    }

    fn write_history(path: &PathBuf, content: &str) {
        fs::write(path, content).unwrap();
    }

    fn append_history(path: &PathBuf, content: &str) {
        let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn no_history_file_returns_zero() {
        let (_dir, opts, journal, identity) = fixture();
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn slash_commands_are_skipped() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(
            &opts.history_path,
            r#"{"display":"/clear","pastedContents":{},"timestamp":1000,"project":"/tmp","sessionId":"s1"}
{"display":"/resume","pastedContents":{},"timestamp":1001,"project":"/tmp","sessionId":"s1"}
{"display":"/compact","pastedContents":{},"timestamp":1002,"project":"/tmp","sessionId":"s1"}
"#,
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 0);
        assert_eq!(journal.read_all().unwrap().len(), 0);
    }

    #[test]
    fn plain_text_entries_produce_events() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(
            &opts.history_path,
            r#"{"display":"build the sdk","pastedContents":{},"timestamp":1700000000000,"project":"/home/me/urchin","sessionId":"s1"}
{"display":"run the tests","pastedContents":{},"timestamp":1700000001000,"project":"/home/me/urchin","sessionId":"s1"}
"#,
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 2);

        let events = journal.read_all().unwrap();
        assert_eq!(events[0].source, "claude-cli");
        assert_eq!(events[0].content, "build the sdk");
        assert_eq!(events[0].workspace.as_deref(), Some("/home/me/urchin"));
        assert_eq!(events[0].session.as_deref(), Some("s1"));
        assert!(events[0].tags.contains(&"claude".to_string()));
        assert!(events[0].tags.contains(&"auto-collected".to_string()));
    }

    #[test]
    fn pasted_content_used_over_display() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(
            &opts.history_path,
            r#"{"display":"[Pasted text #1 +5 lines]","pastedContents":{"1":{"content":"the full pasted directive here"}},"timestamp":1700000000000,"project":"/tmp","sessionId":"s1"}
"#,
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 1);

        let events = journal.read_all().unwrap();
        assert_eq!(events[0].content, "the full pasted directive here");
        assert_eq!(
            events[0].title.as_deref(),
            Some("[Pasted text #1 +5 lines]")
        );
    }

    #[test]
    fn falls_back_to_display_when_content_hash_only() {
        let (_dir, opts, journal, identity) = fixture();
        // contentHash present but no content field — fall back to display
        write_history(
            &opts.history_path,
            r#"{"display":"[Pasted text #1 +99 lines]","pastedContents":{"1":{"contentHash":"abc123"}},"timestamp":1700000000000,"project":"/tmp","sessionId":"s1"}
"#,
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            journal.read_all().unwrap()[0].content,
            "[Pasted text #1 +99 lines]"
        );
    }

    #[test]
    fn second_run_produces_zero() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(
            &opts.history_path,
            r#"{"display":"first prompt","pastedContents":{},"timestamp":1000,"project":"/tmp","sessionId":"s1"}
"#,
        );
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 1);
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 0);
    }

    #[test]
    fn new_entries_picked_up_after_checkpoint() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(
            &opts.history_path,
            r#"{"display":"first","pastedContents":{},"timestamp":1000,"project":"/tmp","sessionId":"s1"}
"#,
        );
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 1);

        append_history(
            &opts.history_path,
            r#"{"display":"second","pastedContents":{},"timestamp":2000,"project":"/tmp","sessionId":"s1"}
{"display":"third","pastedContents":{},"timestamp":3000,"project":"/tmp","sessionId":"s1"}
"#,
        );
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 2);

        let events = journal.read_all().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[2].content, "third");
    }

    #[test]
    fn malformed_lines_are_skipped_without_panic() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(
            &opts.history_path,
            r#"not json at all
{"display":"valid entry","pastedContents":{},"timestamp":1000,"project":"/tmp","sessionId":"s1"}
{broken
"#,
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 1);
        assert_eq!(journal.read_all().unwrap()[0].content, "valid entry");
    }

    #[test]
    fn timestamp_preserved_from_history_record() {
        let (_dir, opts, journal, identity) = fixture();
        // 1700000000000 ms = 2023-11-14T22:13:20Z
        write_history(
            &opts.history_path,
            r#"{"display":"timed prompt","pastedContents":{},"timestamp":1700000000000,"project":"/tmp","sessionId":"s1"}
"#,
        );
        collect(&journal, &identity, &opts).unwrap();
        let events = journal.read_all().unwrap();
        assert_eq!(events[0].timestamp.timestamp_millis(), 1700000000000);
    }

    #[test]
    fn system_directive_display_maps_to_agent_kind() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(
            &opts.history_path,
            r#"{"display":"SYSTEM DIRECTIVE: build the thing","pastedContents":{},"timestamp":1000,"project":"/tmp","sessionId":"s1"}
"#,
        );
        collect(&journal, &identity, &opts).unwrap();
        let events = journal.read_all().unwrap();
        assert_eq!(events[0].kind, EventKind::Agent);
    }
}
