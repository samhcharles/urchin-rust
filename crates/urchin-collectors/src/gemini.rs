//! Gemini CLI collector — reads session JSONL files from `~/.gemini/tmp/{user}/chats/`.
//!
//! Each session is a `session-YYYY-MM-DDTHH-MM-*.jsonl` file. Lines with
//! `"type": "user"` contain the user's messages; content is an array of
//! `{"text": "..."}` objects. We extract text, strip empty/duplicate parts.
//!
//! Checkpoint strategy: a JSON file tracking which session files are fully
//! processed and the byte offset into the most-recently-seen (possibly still
//! active) session file.

use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use urchin_core::{
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

pub struct GeminiOpts {
    /// Directory containing session JSONL files, e.g. ~/.gemini/tmp/{user}/chats/
    pub chats_dir: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl GeminiOpts {
    pub fn defaults() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "unknown".to_string());
        Self {
            chats_dir: home.join(".gemini").join("tmp").join(&user).join("chats"),
            checkpoint_path: state_dir().join("gemini.checkpoint.json"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Checkpoint {
    /// Session filenames (basename) that have been fully processed.
    seen: Vec<String>,
    /// Basename of the last partially-read file (the active session).
    partial_file: Option<String>,
    partial_offset: u64,
}

pub fn collect(journal: &Journal, identity: &Identity, opts: &GeminiOpts) -> Result<usize> {
    if !opts.chats_dir.exists() {
        return Ok(0);
    }

    // Gather and sort session files.
    let mut files: Vec<PathBuf> = fs::read_dir(&opts.chats_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    files.sort();

    if files.is_empty() {
        return Ok(0);
    }

    let mut ckpt = load_checkpoint(&opts.checkpoint_path);
    let seen_set: std::collections::HashSet<String> = ckpt.seen.iter().cloned().collect();

    let mut total = 0usize;
    let newest = files.last().map(|p| basename(p));

    for file in &files {
        let base = basename(file);

        // Skip fully-processed files.
        if seen_set.contains(&base) {
            continue;
        }

        let is_newest = Some(&base) == newest.as_ref();

        // Use saved partial offset for any file we've partially read before (not just the newest).
        // This lets a file that was the active session last run get drained from its
        // last-seen byte rather than reprocessed from the beginning.
        let start = if ckpt.partial_file.as_deref() == Some(&base) {
            ckpt.partial_offset
        } else {
            0
        };

        let n = process_file(file, start, journal, identity, &mut |end_offset| {
            if is_newest {
                ckpt.partial_file = Some(base.clone());
                ckpt.partial_offset = end_offset;
            }
        })?;
        total += n;

        // Mark older (non-newest) files as done.
        if !is_newest {
            ckpt.seen.push(base);
        }
    }

    save_checkpoint(&opts.checkpoint_path, &ckpt)?;
    Ok(total)
}

/// Read user messages from a session JSONL file starting at `start` byte offset.
/// Calls `on_end(end_offset)` once with the final EOF position so the caller
/// can persist the partial offset.
fn process_file<F>(
    path: &PathBuf,
    start: u64,
    journal: &Journal,
    identity: &Identity,
    on_end: &mut F,
) -> Result<usize>
where
    F: FnMut(u64),
{
    let mut f = fs::File::open(path)?;
    let file_len = f.metadata()?.len();

    // If start > file_len the file must have been rotated; reset to 0.
    let start = if start > file_len { 0 } else { start };
    f.seek(SeekFrom::Start(start))?;

    let mut reader = BufReader::new(f);
    let mut count = 0usize;
    let mut pos = start;

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        pos += bytes as u64;

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(obj) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        if obj.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }

        let Some(content_arr) = obj.get("content").and_then(|v| v.as_array()) else {
            continue;
        };

        // Concatenate all text parts in the content array.
        let text: String = content_arr
            .iter()
            .filter_map(|part| part.get("text")?.as_str())
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");

        if text.trim().is_empty() {
            continue;
        }

        // Use the event's own timestamp if present, else now.
        let timestamp: DateTime<Utc> = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(Utc::now);

        let mut event = Event::new("gemini", EventKind::Conversation, text.trim().to_string());
        event.timestamp = timestamp;
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: None,
            node_id: Some(identity.node_id.clone()),
        });

        journal.append(&event)?;
        count += 1;
    }

    on_end(pos);
    journal.flush()?;
    Ok(count)
}

fn basename(p: &std::path::Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn load_checkpoint(path: &PathBuf) -> Checkpoint {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_checkpoint(path: &PathBuf, ckpt: &Checkpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(ckpt)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::Path;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, GeminiOpts, Journal, Identity) {
        let dir = tempfile::tempdir().unwrap();
        let chats = dir.path().join("chats");
        fs::create_dir_all(&chats).unwrap();
        let opts = GeminiOpts {
            chats_dir: chats,
            checkpoint_path: dir.path().join("gemini.checkpoint.json"),
        };
        let journal = Journal::new(dir.path().join("events.jsonl"));
        let identity = Identity::for_test("test", "test");
        (dir, opts, journal, identity)
    }

    fn write_session(chats_dir: &Path, name: &str, lines: &[&str]) {
        let path = chats_dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{}", line).unwrap();
        }
    }

    fn user_msg(ts: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{}","content":[{{"text":"{}"}}]}}"#,
            ts, text
        )
    }

    #[test]
    fn no_chats_dir_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let opts = GeminiOpts {
            chats_dir: dir.path().join("nonexistent"),
            checkpoint_path: dir.path().join("ckpt.json"),
        };
        let journal = Journal::new(dir.path().join("events.jsonl"));
        let identity = Identity::for_test("t", "t");
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 0);
    }

    #[test]
    fn empty_chats_dir_returns_zero() {
        let (_dir, opts, journal, identity) = fixture();
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 0);
    }

    #[test]
    fn first_run_collects_user_messages() {
        let (_dir, opts, journal, identity) = fixture();
        write_session(
            &opts.chats_dir,
            "session-001.jsonl",
            &[
                &user_msg("2026-05-01T10:00:00Z", "hello gemini"),
                &user_msg("2026-05-01T10:01:00Z", "do a thing"),
            ],
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 2);
        let events = journal.read_all().unwrap();
        assert_eq!(events[0].content, "hello gemini");
        assert_eq!(events[1].content, "do a thing");
    }

    #[test]
    fn second_run_emits_nothing_without_new_content() {
        let (_dir, opts, journal, identity) = fixture();
        write_session(
            &opts.chats_dir,
            "session-001.jsonl",
            &[&user_msg("2026-05-01T10:00:00Z", "hi")],
        );
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 1);
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 0);
    }

    #[test]
    fn new_session_file_picked_up_on_next_run() {
        let (_dir, opts, journal, identity) = fixture();
        write_session(
            &opts.chats_dir,
            "session-001.jsonl",
            &[&user_msg("2026-05-01T10:00:00Z", "first")],
        );
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 1);

        write_session(
            &opts.chats_dir,
            "session-002.jsonl",
            &[&user_msg("2026-05-02T10:00:00Z", "second")],
        );
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 1);

        let events = journal.read_all().unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn non_user_lines_are_skipped() {
        let (_dir, opts, journal, identity) = fixture();
        write_session(
            &opts.chats_dir,
            "session-001.jsonl",
            &[
                r#"{"type":"gemini","content":[{"text":"response"}]}"#,
                &user_msg("2026-05-01T10:00:00Z", "my prompt"),
                r#"{"type":"info","content":"something"}"#,
            ],
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 1);
        let events = journal.read_all().unwrap();
        assert_eq!(events[0].content, "my prompt");
    }

    #[test]
    fn event_source_is_gemini() {
        let (_dir, opts, journal, identity) = fixture();
        write_session(
            &opts.chats_dir,
            "session-001.jsonl",
            &[&user_msg("2026-05-01T10:00:00Z", "test")],
        );
        collect(&journal, &identity, &opts).unwrap();
        let events = journal.read_all().unwrap();
        assert_eq!(events[0].source, "gemini");
        assert_eq!(events[0].kind, EventKind::Conversation);
    }

    #[test]
    fn timestamp_preserved_from_session_record() {
        let (_dir, opts, journal, identity) = fixture();
        write_session(
            &opts.chats_dir,
            "session-001.jsonl",
            &[&user_msg("2026-05-01T15:30:00Z", "test ts")],
        );
        collect(&journal, &identity, &opts).unwrap();
        let events = journal.read_all().unwrap();
        assert!(
            events[0]
                .timestamp
                .to_rfc3339()
                .starts_with("2026-05-01T15:30:00")
        );
    }

    #[test]
    fn empty_text_parts_skipped() {
        let (_dir, opts, journal, identity) = fixture();
        write_session(
            &opts.chats_dir,
            "session-001.jsonl",
            &[
                r#"{"type":"user","content":[{"text":""},{"text":"   "}]}"#,
                &user_msg("2026-05-01T10:00:00Z", "real message"),
            ],
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 1);
    }
}
