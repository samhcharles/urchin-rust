//! Codex CLI collector — reads `~/.codex/history.jsonl` and `state_5.sqlite`.
//!
//! The history file carries the live user prompt stream; the SQLite `threads`
//! table carries session metadata and serves as a fallback if history is absent.
//!
//! Checkpoint: JSON `{ "last_ts_ms": <unix_ms>, "history_offset": <bytes> }`
//! stored in the state dir.

use crate::claude::truncate;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use urchin_core::{
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

// ─── Options ─────────────────────────────────────────────────────────────────

pub struct CodexOpts {
    pub db_path: PathBuf,
    pub history_path: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl CodexOpts {
    pub fn defaults() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            db_path: home.join(".codex").join("state_5.sqlite"),
            history_path: home.join(".codex").join("history.jsonl"),
            checkpoint_path: state_dir().join("codex.json"),
        }
    }
}

// ─── Checkpoint ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct Checkpoint {
    /// Watermark: only emit sessions with created_at_ms > this value.
    last_ts_ms: i64,
    /// Byte offset into ~/.codex/history.jsonl for incremental prompt ingest.
    history_offset: u64,
}

fn load_checkpoint(path: &PathBuf) -> Checkpoint {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_checkpoint(path: &PathBuf, ckpt: &Checkpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(ckpt)?)?;
    Ok(())
}

// ─── Collector function ───────────────────────────────────────────────────────

/// Read new Codex sessions and append them to the journal.
/// Returns the number of events appended.
pub fn collect(journal: &Journal, identity: &Identity, opts: &CodexOpts) -> Result<usize> {
    if !opts.db_path.exists() && !opts.history_path.exists() {
        return Ok(0);
    }

    let mut ckpt = load_checkpoint(&opts.checkpoint_path);
    let mut count = 0usize;

    if opts.history_path.exists() {
        count += collect_history(journal, identity, opts, &mut ckpt)?;
    }

    if opts.db_path.exists() {
        count += collect_threads(journal, identity, opts, &mut ckpt)?;
    }

    if count > 0 {
        save_checkpoint(&opts.checkpoint_path, &ckpt)?;
        journal.flush()?;
    }
    Ok(count)
}

#[derive(Debug, Deserialize)]
struct CodexHistoryLine {
    session_id: Option<String>,
    ts: Option<i64>,
    text: Option<String>,
}

fn collect_history(
    journal: &Journal,
    identity: &Identity,
    opts: &CodexOpts,
    ckpt: &mut Checkpoint,
) -> Result<usize> {
    let file_size = fs::metadata(&opts.history_path)?.len();
    let start = if ckpt.history_offset > file_size {
        0
    } else {
        ckpt.history_offset
    };
    let mut file = File::open(&opts.history_path)?;
    file.seek(SeekFrom::Start(start))?;

    let mut count = 0usize;
    for line in BufReader::new(file).lines() {
        let raw = line?;
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let parsed: CodexHistoryLine = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("codex collector: skipping malformed history line: {}", e);
                continue;
            }
        };
        let content = parsed.text.unwrap_or_default().trim().to_string();
        if content.is_empty() || content.starts_with('/') {
            continue;
        }

        let mut event = Event::new("codex", EventKind::Conversation, content.clone());
        if let Some(ts_s) = parsed.ts {
            if let Some(ts) = Utc.timestamp_opt(ts_s, 0).single() {
                event.timestamp = ts;
            }
        }
        event.session = parsed.session_id.clone();
        event.title = Some(truncate(&content, 80));
        event.tags = vec!["auto-collected".to_string(), "codex".to_string()];
        if let Some(sid) = parsed.session_id {
            event.tags.push(format!("session:{}", sid));
        }
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: None,
            node_id: Some(identity.node_id.clone()),
        });
        journal.append(&event)?;
        count += 1;
    }

    ckpt.history_offset = file_size;
    Ok(count)
}

fn collect_threads(
    journal: &Journal,
    identity: &Identity,
    opts: &CodexOpts,
    ckpt: &mut Checkpoint,
) -> Result<usize> {
    let conn = rusqlite::Connection::open_with_flags(
        &opts.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;

    let mut stmt = conn.prepare(
        "SELECT id,
                COALESCE(created_at_ms, created_at * 1000) AS ts_ms,
                first_user_message,
                title,
                cwd,
                model_provider
         FROM   threads
         WHERE  COALESCE(created_at_ms, created_at * 1000) > ?1
           AND  archived = 0
         ORDER  BY ts_ms ASC",
    )?;

    let mut max_ts = ckpt.last_ts_ms;
    let mut count = 0usize;

    let rows = stmt.query_map([ckpt.last_ts_ms], |row| {
        Ok((
            row.get::<_, String>(0)?, // id
            row.get::<_, i64>(1)?,    // ts_ms
            row.get::<_, String>(2)?, // first_user_message
            row.get::<_, String>(3)?, // title
            row.get::<_, String>(4)?, // cwd
            row.get::<_, String>(5)?, // model_provider
        ))
    })?;

    for row in rows {
        let (id, ts_ms, first_msg, title, cwd, provider) = row?;

        // Prefer first_user_message; fall back to title. Skip if both are empty
        // or look like internal slash commands.
        let content = if !first_msg.trim().is_empty() {
            first_msg.trim().to_string()
        } else if !title.trim().is_empty() {
            title.trim().to_string()
        } else {
            continue;
        };

        if content.starts_with('/') {
            continue;
        }

        let ts: DateTime<Utc> = Utc
            .timestamp_millis_opt(ts_ms)
            .single()
            .unwrap_or_else(Utc::now);

        let mut event = Event::new("codex", EventKind::Conversation, content.clone());
        event.timestamp = ts;
        event.workspace = if cwd.is_empty() {
            None
        } else {
            Some(cwd.clone())
        };
        event.tags = vec![
            "auto-collected".to_string(),
            "codex".to_string(),
            format!("session:{}", id),
            format!("model:{}", provider),
        ];
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: if cwd.is_empty() { None } else { Some(cwd) },
            node_id: Some(identity.node_id.clone()),
        });

        journal.append(&event)?;
        count += 1;

        if ts_ms > max_ts {
            max_ts = ts_ms;
        }
    }

    if count > 0 {
        ckpt.last_ts_ms = max_ts;
    }
    Ok(count)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_db(dir: &std::path::Path) -> PathBuf {
        let db_path = dir.join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL DEFAULT 0,
                created_at_ms INTEGER,
                first_user_message TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT '',
                cwd TEXT NOT NULL DEFAULT '',
                model_provider TEXT NOT NULL DEFAULT 'openai',
                archived INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        db_path
    }

    fn make_opts(tmp: &TempDir, db_path: PathBuf) -> CodexOpts {
        CodexOpts {
            db_path,
            history_path: tmp.path().join("history.jsonl"),
            checkpoint_path: tmp.path().join("codex.json"),
        }
    }

    fn dummy_journal(tmp: &TempDir) -> (Journal, Identity) {
        let j = Journal::new(tmp.path().join("journal.jsonl"));
        let id = Identity::for_test("test", "test");
        (j, id)
    }

    #[test]
    fn no_db_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let opts = CodexOpts {
            db_path: tmp.path().join("missing.sqlite"),
            history_path: tmp.path().join("history.jsonl"),
            checkpoint_path: tmp.path().join("ckpt.json"),
        };
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn empty_db_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        let opts = make_opts(&tmp, db);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn collects_new_sessions() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO threads (id, created_at_ms, first_user_message, cwd, model_provider)
                 VALUES ('a1', 1700000000000, 'refactor auth module', '/home/sam', 'openai')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO threads (id, created_at_ms, first_user_message, cwd, model_provider)
                 VALUES ('a2', 1700000001000, 'add retry logic', '/home/sam', 'anthropic')",
                [],
            )
            .unwrap();
        }
        let opts = make_opts(&tmp, db);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 2);
    }

    #[test]
    fn watermark_prevents_reprocessing() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO threads (id, created_at_ms, first_user_message, cwd, model_provider)
                 VALUES ('b1', 1700000000000, 'first session', '/home/sam', 'openai')",
                [],
            )
            .unwrap();
        }
        let opts = make_opts(&tmp, db.clone());
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
        // Second run: no new rows → 0 events
        let opts2 = make_opts(&tmp, db);
        assert_eq!(collect(&j, &id, &opts2).unwrap(), 0);
    }

    #[test]
    fn skips_slash_commands_and_empty() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            // slash command title
            conn.execute(
                "INSERT INTO threads (id, created_at_ms, first_user_message, title, cwd, model_provider)
                 VALUES ('c1', 1700000000000, '', '/clear', '/home/sam', 'openai')",
                [],
            ).unwrap();
            // empty both
            conn.execute(
                "INSERT INTO threads (id, created_at_ms, first_user_message, title, cwd, model_provider)
                 VALUES ('c2', 1700000001000, '', '', '/home/sam', 'openai')",
                [],
            ).unwrap();
            // valid
            conn.execute(
                "INSERT INTO threads (id, created_at_ms, first_user_message, cwd, model_provider)
                 VALUES ('c3', 1700000002000, 'build the codex collector', '/home/sam', 'openai')",
                [],
            )
            .unwrap();
        }
        let opts = make_opts(&tmp, db);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
    }

    #[test]
    fn archived_sessions_skipped() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO threads (id, created_at_ms, first_user_message, cwd, model_provider, archived)
                 VALUES ('d1', 1700000000000, 'archived session', '/home/sam', 'openai', 1)",
                [],
            ).unwrap();
        }
        let opts = make_opts(&tmp, db);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }
}
