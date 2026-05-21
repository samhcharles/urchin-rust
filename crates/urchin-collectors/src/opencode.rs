//! OpenCode collector — reads `~/.local/share/opencode/opencode.db` and emits Events.
//!
//! OpenCode stores AI coding sessions in a SQLite DB. We join `message` with
//! `session` to get the workspace directory, filter for user-role messages
//! only, and extract text content from the `data` JSON blob.
//!
//! The `data` JSON may contain content as:
//!   - `data.parts[].text` (AI SDK streaming format)
//!   - `data.content` as a plain string (compact format)
//!   - `data.content[].text` (array of blocks)
//!
//! Checkpoint: JSON `{ "last_ts_ms": <i64> }` in the state dir.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use urchin_core::{
    event::{Actor, Event, EventKind},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

// ─── Options ─────────────────────────────────────────────────────────────────

pub struct OpenCodeOpts {
    pub db_path: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl OpenCodeOpts {
    pub fn defaults() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            db_path: home
                .join(".local")
                .join("share")
                .join("opencode")
                .join("opencode.db"),
            checkpoint_path: state_dir().join("opencode.json"),
        }
    }
}

// ─── Checkpoint ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct Checkpoint {
    last_ts_ms: i64,
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

// ─── Content extraction ───────────────────────────────────────────────────────

/// Pull the best text string out of an OpenCode message `data` blob.
///
/// Tries (in order):
/// 1. `data.parts[].text` — AI SDK streaming format
/// 2. `data.content` as a string — compact format  
/// 3. `data.content[].text` — block array format
fn extract_text(data: &Value) -> Option<String> {
    // 1. parts[].text
    if let Some(parts) = data.get("parts").and_then(|p| p.as_array()) {
        let text: String = parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" ");
        if !text.trim().is_empty() {
            return Some(text.trim().to_string());
        }
    }

    // 2. content as string
    if let Some(s) = data.get("content").and_then(|c| c.as_str()) {
        let t = s.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }

    // 3. content as array of blocks
    if let Some(blocks) = data.get("content").and_then(|c| c.as_array()) {
        let text: String = blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" ");
        if !text.trim().is_empty() {
            return Some(text.trim().to_string());
        }
    }

    None
}

// ─── Collector function ───────────────────────────────────────────────────────

/// Read new user messages from OpenCode and append them to the journal.
/// Returns the number of events appended.
pub fn collect(journal: &Journal, identity: &Identity, opts: &OpenCodeOpts) -> Result<usize> {
    if !opts.db_path.exists() {
        return Ok(0);
    }

    let conn = rusqlite::Connection::open_with_flags(
        &opts.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;

    let mut ckpt = load_checkpoint(&opts.checkpoint_path);

    let mut stmt = conn.prepare(
        "SELECT m.id,
                m.time_created,
                m.data,
                s.directory,
                s.title
         FROM   message m
         JOIN   session s ON m.session_id = s.id
         WHERE  m.time_created > ?1
         ORDER  BY m.time_created ASC",
    )?;

    let mut count = 0usize;
    let mut max_ts = ckpt.last_ts_ms;

    let rows = stmt.query_map([ckpt.last_ts_ms], |row| {
        Ok((
            row.get::<_, String>(0)?, // id
            row.get::<_, i64>(1)?,    // time_created (ms)
            row.get::<_, String>(2)?, // data JSON
            row.get::<_, String>(3)?, // directory
            row.get::<_, String>(4)?, // title
        ))
    })?;

    for row in rows {
        let (id, ts_ms, data_str, directory, title) = row?;

        let data: Value = match serde_json::from_str(&data_str) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "opencode collector: skipping malformed message {}: {}",
                    id,
                    e
                );
                continue;
            }
        };

        // Only capture user-role messages.
        if data.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }

        let content = match extract_text(&data) {
            Some(c) => c,
            None => continue,
        };

        let ts: DateTime<Utc> = Utc
            .timestamp_millis_opt(ts_ms)
            .single()
            .unwrap_or_else(Utc::now);

        let mut event = Event::new("opencode", EventKind::Conversation, content);
        event.timestamp = ts;
        event.workspace = if directory.is_empty() {
            None
        } else {
            Some(directory.clone())
        };
        event.title = if title.is_empty() {
            None
        } else {
            Some(title.clone())
        };
        event.tags = vec!["auto-collected".to_string(), "opencode".to_string()];
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: if directory.is_empty() {
                None
            } else {
                Some(directory)
            },
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
        save_checkpoint(&opts.checkpoint_path, &ckpt)?;
    }

    journal.flush()?;
    Ok(count)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_db(dir: &std::path::Path) -> PathBuf {
        let db_path = dir.join("opencode.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL DEFAULT '',
                slug TEXT NOT NULL DEFAULT '',
                directory TEXT NOT NULL DEFAULT '',
                title TEXT NOT NULL DEFAULT '',
                version TEXT NOT NULL DEFAULT '1',
                time_created INTEGER NOT NULL DEFAULT 0,
                time_updated INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL DEFAULT 0,
                data TEXT NOT NULL,
                FOREIGN KEY (session_id) REFERENCES session(id)
            );",
        )
        .unwrap();
        db_path
    }

    fn make_opts(tmp: &TempDir, db_path: PathBuf) -> OpenCodeOpts {
        OpenCodeOpts {
            db_path,
            checkpoint_path: tmp.path().join("opencode.json"),
        }
    }

    fn dummy_journal(tmp: &TempDir) -> (Journal, Identity) {
        let j = Journal::new(tmp.path().join("journal.jsonl"));
        let id = Identity::for_test("test", "test");
        (j, id)
    }

    fn insert_session(conn: &rusqlite::Connection, id: &str, directory: &str) {
        conn.execute(
            "INSERT INTO session (id, directory, title, time_created, time_updated)
             VALUES (?1, ?2, 'test session', 1000, 1000)",
            rusqlite::params![id, directory],
        )
        .unwrap();
    }

    #[test]
    fn no_db_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let opts = OpenCodeOpts {
            db_path: tmp.path().join("missing.db"),
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
    fn collects_user_messages_parts_format() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        let conn = rusqlite::Connection::open(&db).unwrap();
        insert_session(&conn, "s1", "/home/sam/project");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('m1', 's1', 1700000001000, '{\"role\":\"user\",\"parts\":[{\"text\":\"fix the memory leak\"}]}')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('m2', 's1', 1700000002000, '{\"role\":\"assistant\",\"parts\":[{\"text\":\"Sure, let me help\"}]}')",
            [],
        ).unwrap();
        let opts = make_opts(&tmp, db);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
    }

    #[test]
    fn collects_user_messages_content_string_format() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        let conn = rusqlite::Connection::open(&db).unwrap();
        insert_session(&conn, "s2", "/home/sam/project");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('m3', 's2', 1700000003000, '{\"role\":\"user\",\"content\":\"add retry logic\"}')",
            [],
        ).unwrap();
        let opts = make_opts(&tmp, db);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
    }

    #[test]
    fn watermark_prevents_reprocessing() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        let conn = rusqlite::Connection::open(&db).unwrap();
        insert_session(&conn, "s3", "/home/sam/project");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('m4', 's3', 1700000004000, '{\"role\":\"user\",\"content\":\"first run\"}')",
            [],
        )
        .unwrap();
        let opts = make_opts(&tmp, db.clone());
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
        let opts2 = make_opts(&tmp, db);
        assert_eq!(collect(&j, &id, &opts2).unwrap(), 0);
    }

    #[test]
    fn skips_non_user_roles() {
        let tmp = TempDir::new().unwrap();
        let db = make_db(tmp.path());
        let conn = rusqlite::Connection::open(&db).unwrap();
        insert_session(&conn, "s4", "/home/sam");
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('m5', 's4', 1700000005000, '{\"role\":\"tool\",\"content\":\"tool result\"}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('m6', 's4', 1700000006000, '{\"role\":\"user\",\"content\":\"valid user prompt\"}')",
            [],
        ).unwrap();
        let opts = make_opts(&tmp, db);
        let (j, id) = dummy_journal(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
    }
}
