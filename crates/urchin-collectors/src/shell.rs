//! Shell collector: tail `~/.bash_history` and append new commands as events.
//!
//! The history file is appended to by every shell session. We track the byte offset
//! of the last line we read so we don't re-emit lines we've already seen. If the file
//! shrinks (HISTSIZE truncation, log rotation) we reset to the start.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use anyhow::Result;

use urchin_core::{
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

pub struct ShellOpts {
    pub history_path: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl ShellOpts {
    pub fn defaults() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            history_path: home.join(".bash_history"),
            checkpoint_path: state_dir().join("shell.checkpoint"),
        }
    }
}

/// Read new lines from the bash history file and append them as events.
/// Returns the number of events appended.
pub fn collect(journal: &Journal, identity: &Identity, opts: &ShellOpts) -> Result<usize> {
    if !opts.history_path.exists() {
        return Ok(0);
    }

    let file_size = fs::metadata(&opts.history_path)?.len();
    let checkpoint = read_checkpoint(&opts.checkpoint_path);

    // If checkpoint is past EOF, the file shrank — start over.
    let start = if checkpoint > file_size {
        0
    } else {
        checkpoint
    };

    let mut file = File::open(&opts.history_path)?;
    file.seek(SeekFrom::Start(start))?;

    let mut count = 0;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let cleaned = clean_history_line(&line);
        if cleaned.is_empty() {
            continue;
        }

        let mut event = Event::new("shell", EventKind::Command, cleaned);
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: None,
            node_id: Some(identity.node_id.clone()),
        });
        journal.append(&event)?;
        count += 1;
    }

    write_checkpoint(&opts.checkpoint_path, file_size)?;
    journal.flush()?;
    Ok(count)
}

/// Drop empty lines and HISTTIMEFORMAT timestamp markers (`: 1234567890:0;cmd`).
fn clean_history_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(rest) = trimmed.strip_prefix(": ") {
        if let Some(idx) = rest.find(';') {
            return rest[idx + 1..].trim().to_string();
        }
    }
    trimmed.to_string()
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

    fn fixture() -> (TempDir, ShellOpts, Journal, Identity) {
        let dir = tempfile::tempdir().unwrap();
        let opts = ShellOpts {
            history_path: dir.path().join("bash_history"),
            checkpoint_path: dir.path().join("shell.checkpoint"),
        };
        let journal = Journal::new(dir.path().join("events.jsonl"));
        let identity = Identity::for_test("test", "test");
        (dir, opts, journal, identity)
    }

    fn write_history(path: &PathBuf, content: &str) {
        std::fs::write(path, content).unwrap();
    }

    fn append_history(path: &PathBuf, content: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn no_history_file_returns_zero() {
        let (_dir, opts, journal, identity) = fixture();
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn first_run_collects_everything() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(&opts.history_path, "ls\ngit status\ncargo build\n");

        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 3);

        let events = journal.read_all().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].source, "shell");
        assert_eq!(events[0].content, "ls");
        assert_eq!(events[2].content, "cargo build");
    }

    #[test]
    fn second_run_only_picks_up_new_lines() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(&opts.history_path, "ls\ngit status\n");

        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 2);
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 0);

        append_history(&opts.history_path, "cargo test\nrm -rf target\n");
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 2);

        let events = journal.read_all().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[3].content, "rm -rf target");
    }

    #[test]
    fn truncation_resets_checkpoint() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(&opts.history_path, "old1\nold2\nold3\n");
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 3);

        write_history(&opts.history_path, "fresh\n");
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 1);

        let events = journal.read_all().unwrap();
        assert_eq!(events.last().unwrap().content, "fresh");
    }

    #[test]
    fn strips_histtimeformat_markers() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(
            &opts.history_path,
            ": 1730000000:0;ls -la\n: 1730000050:0;cargo build\nplain command\n",
        );
        let n = collect(&journal, &identity, &opts).unwrap();
        assert_eq!(n, 3);

        let events = journal.read_all().unwrap();
        assert_eq!(events[0].content, "ls -la");
        assert_eq!(events[1].content, "cargo build");
        assert_eq!(events[2].content, "plain command");
    }

    #[test]
    fn skips_empty_lines() {
        let (_dir, opts, journal, identity) = fixture();
        write_history(&opts.history_path, "ls\n\n\ngit status\n   \n");
        assert_eq!(collect(&journal, &identity, &opts).unwrap(), 2);
    }
}
