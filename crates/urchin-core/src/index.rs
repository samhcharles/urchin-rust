//! SQLite projection index alongside the JSONL journal.
//! JSONL is the source of truth; this index is derived and fully rebuildable.
//! SQLite errors never crash the daemon — they are logged as warnings and the
//! write path continues using only the JSONL file.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::{params, Connection};

use crate::event::Event;

pub struct Index {
    path: PathBuf,
}

pub(crate) struct IndexRow {
    pub id: String,
    pub timestamp_ms: i64,
    pub source: String,
    pub kind: String,
    pub workspace: Option<String>,
    pub tags: String,
    pub byte_offset: u64,
    pub json: String,
    pub content: String,
}

impl Index {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            path: path.to_path_buf(),
        })
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )?;
        Ok(conn)
    }

    /// Returns true if an event with the given ID is already in the index.
    pub fn exists_by_id(&self, id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(count > 0)
    }

    pub fn ensure_schema(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS events (
                id          TEXT PRIMARY KEY,
                timestamp   INTEGER NOT NULL,
                source      TEXT NOT NULL,
                kind        TEXT NOT NULL,
                workspace   TEXT,
                tags        TEXT NOT NULL DEFAULT '[]',
                byte_offset INTEGER NOT NULL,
                json        TEXT NOT NULL,
                content     TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_ts  ON events(timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_src ON events(source, timestamp DESC);
            CREATE INDEX IF NOT EXISTS idx_ws  ON events(workspace, timestamp DESC);
        ",
        )?;
        Ok(())
    }

    /// Returns the number of events in the index. Returns 0 if the table doesn't exist yet.
    pub fn count(&self) -> Result<usize> {
        let conn = self.connect()?;
        let n: i64 = conn
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap_or(0);
        Ok(n as usize)
    }

    pub(crate) fn insert_batch(&self, rows: &[IndexRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO events
                 (id, timestamp, source, kind, workspace, tags, byte_offset, json, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for row in rows {
                stmt.execute(params![
                    row.id,
                    row.timestamp_ms,
                    row.source,
                    row.kind,
                    row.workspace,
                    row.tags,
                    row.byte_offset as i64,
                    row.json,
                    row.content,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn cutoff_ms(hours: f64) -> i64 {
        chrono::Utc::now().timestamp_millis() - (hours * 3_600_000.0) as i64
    }

    pub fn query_recent(
        &self,
        hours: f64,
        source: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let conn = self.connect()?;
        let cutoff = Self::cutoff_ms(hours);
        let jsons: Vec<String> = if let Some(src) = source {
            let mut stmt = conn.prepare(
                "SELECT json FROM events
                 WHERE timestamp >= ?1 AND source = ?2
                 ORDER BY timestamp DESC LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![cutoff, src, limit as i64], |r| r.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        } else {
            let mut stmt = conn.prepare(
                "SELECT json FROM events
                 WHERE timestamp >= ?1
                 ORDER BY timestamp DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![cutoff, limit as i64], |r| r.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        Ok(jsons
            .into_iter()
            .filter_map(|j| serde_json::from_str(&j).ok())
            .collect())
    }

    pub fn query_search(&self, query: &str, hours: f64, limit: usize) -> Result<Vec<Event>> {
        let conn = self.connect()?;
        let cutoff = Self::cutoff_ms(hours);
        let pattern = format!("%{}%", query.to_lowercase());
        let mut stmt = conn.prepare(
            "SELECT json FROM events
             WHERE timestamp >= ?1 AND LOWER(content) LIKE ?2
             ORDER BY timestamp DESC LIMIT ?3",
        )?;
        let jsons: Vec<String> = stmt
            .query_map(params![cutoff, pattern, limit as i64], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(jsons
            .into_iter()
            .filter_map(|j| serde_json::from_str(&j).ok())
            .collect())
    }

    pub fn query_project(&self, project: &str, hours: f64, limit: usize) -> Result<Vec<Event>> {
        let conn = self.connect()?;
        let cutoff = Self::cutoff_ms(hours);
        let pattern = format!("%{}%", project.to_lowercase());
        let mut stmt = conn.prepare(
            "SELECT json FROM events
             WHERE timestamp >= ?1
               AND (LOWER(content) LIKE ?2
                    OR LOWER(tags) LIKE ?2
                    OR LOWER(COALESCE(workspace, '')) LIKE ?2)
             ORDER BY timestamp DESC LIMIT ?3",
        )?;
        let jsons: Vec<String> = stmt
            .query_map(params![cutoff, pattern, limit as i64], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(jsons
            .into_iter()
            .filter_map(|j| serde_json::from_str(&j).ok())
            .collect())
    }

    pub fn query_workspace(&self, path: &str, hours: f64, limit: usize) -> Result<Vec<Event>> {
        let conn = self.connect()?;
        let cutoff = Self::cutoff_ms(hours);
        let pattern = format!("%{}%", path.to_lowercase());
        let mut stmt = conn.prepare(
            "SELECT json FROM events
             WHERE timestamp >= ?1
               AND LOWER(COALESCE(workspace, '')) LIKE ?2
             ORDER BY timestamp DESC LIMIT ?3",
        )?;
        let jsons: Vec<String> = stmt
            .query_map(params![cutoff, pattern, limit as i64], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(jsons
            .into_iter()
            .filter_map(|j| serde_json::from_str(&j).ok())
            .collect())
    }

    /// Incrementally index any JSONL events not yet in the SQLite index.
    /// Safe to call concurrently with the writer thread; uses INSERT OR IGNORE.
    /// Returns the count of newly indexed events.
    pub fn backfill_from_journal(&self, journal_path: &Path) -> Result<usize> {
        if !journal_path.exists() {
            return Ok(0);
        }
        let file = std::fs::File::open(journal_path)?;
        let reader = BufReader::new(file);

        let mut rows: Vec<IndexRow> = Vec::new();
        let mut byte_offset: u64 = 0;

        for line_result in reader.lines() {
            let line = line_result?;
            let line_len = line.len() as u64 + 1;
            let start = byte_offset;
            byte_offset += line_len;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(row) = build_index_row(trimmed, start) {
                rows.push(row);
            }
        }

        if rows.is_empty() {
            return Ok(0);
        }

        // Batch in chunks to avoid holding a long write lock.
        let mut inserted = 0usize;
        for chunk in rows.chunks(5000) {
            let mut conn = self.connect()?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT OR IGNORE INTO events
                     (id, timestamp, source, kind, workspace, tags, byte_offset, json, content)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?;
                for row in chunk {
                    let n = stmt.execute(params![
                        row.id,
                        row.timestamp_ms,
                        row.source,
                        row.kind,
                        row.workspace,
                        row.tags,
                        row.byte_offset as i64,
                        row.json,
                        row.content,
                    ])?;
                    inserted += n;
                }
            }
            tx.commit()?;
        }
        Ok(inserted)
    }

    /// Wipe and rebuild the index from the JSONL journal in one transaction.
    pub fn rebuild_from_journal(&self, journal_path: &Path) -> Result<usize> {
        if !journal_path.exists() {
            return Ok(0);
        }
        let file = std::fs::File::open(journal_path)?;
        let reader = BufReader::new(file);

        let mut rows: Vec<IndexRow> = Vec::new();
        let mut byte_offset: u64 = 0;

        for line_result in reader.lines() {
            let line = line_result?;
            let line_len = line.len() as u64 + 1; // +1 for \n
            let start = byte_offset;
            byte_offset += line_len;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(row) = build_index_row(trimmed, start) {
                rows.push(row);
            }
        }

        let count = rows.len();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM events", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO events
                 (id, timestamp, source, kind, workspace, tags, byte_offset, json, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for row in &rows {
                stmt.execute(params![
                    row.id,
                    row.timestamp_ms,
                    row.source,
                    row.kind,
                    row.workspace,
                    row.tags,
                    row.byte_offset as i64,
                    row.json,
                    row.content,
                ])?;
            }
        }
        tx.commit()?;
        Ok(count)
    }
}

/// Parse a JSONL line into an IndexRow. Returns None on parse failure.
pub(crate) fn build_index_row(json_line: &str, byte_offset: u64) -> Option<IndexRow> {
    let v: serde_json::Value = serde_json::from_str(json_line).ok()?;
    let id = v["id"].as_str()?.to_string();
    let timestamp_ms = chrono::DateTime::parse_from_rfc3339(v["timestamp"].as_str()?)
        .ok()?
        .timestamp_millis();
    let source = v["source"].as_str()?.to_string();
    let kind = v["kind"].as_str().unwrap_or("other").to_string();
    let workspace = v["workspace"].as_str().map(|s| s.to_string());
    let tags = v["tags"]
        .as_array()
        .map(|arr| serde_json::to_string(arr).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_else(|| "[]".into());
    let content = v["content"].as_str().unwrap_or("").to_string();

    Some(IndexRow {
        id,
        timestamp_ms,
        source,
        kind,
        workspace,
        tags,
        byte_offset,
        json: json_line.to_string(),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventKind};
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let idx = Index::open(&dir.path().join("index.db")).unwrap();
        idx.ensure_schema().unwrap();
        (dir, idx)
    }

    fn make_row(source: &str, content: &str, workspace: Option<&str>) -> IndexRow {
        let mut event = Event::new(source, EventKind::Conversation, content);
        event.workspace = workspace.map(str::to_string);
        let ts_ms = event.timestamp.timestamp_millis();
        let json = serde_json::to_string(&event).unwrap();
        IndexRow {
            id: event.id.to_string(),
            timestamp_ms: ts_ms,
            source: source.to_string(),
            kind: "conversation".to_string(),
            workspace: workspace.map(str::to_string),
            tags: "[]".to_string(),
            byte_offset: 0,
            json,
            content: content.to_string(),
        }
    }

    #[test]
    fn schema_creates_table() {
        let (_dir, idx) = fixture();
        let conn = idx.connect().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='events'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn insert_and_query_recent() {
        let (_dir, idx) = fixture();
        let rows = vec![
            make_row("cli", "first event", None),
            make_row("cli", "second event", None),
        ];
        idx.insert_batch(&rows).unwrap();
        let events = idx.query_recent(1.0, None, 10).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn insert_is_idempotent() {
        let (_dir, idx) = fixture();
        let row1 = make_row("cli", "hello", None);
        let fixed_id = row1.id.clone();
        let fixed_json = row1.json.clone();
        let mut row2 = make_row("cli", "hello", None);
        row2.id = fixed_id.clone();
        row2.json = fixed_json;
        idx.insert_batch(&[row1]).unwrap();
        idx.insert_batch(&[row2]).unwrap();
        let conn = idx.connect().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE id = ?1",
                params![fixed_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn query_search_substring() {
        let (_dir, idx) = fixture();
        idx.insert_batch(&[
            make_row("cli", "the quick brown fox", None),
            make_row("cli", "something else entirely", None),
        ])
        .unwrap();
        let hits = idx.query_search("quick", 1.0, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "the quick brown fox");
    }

    #[test]
    fn query_workspace_match() {
        let (_dir, idx) = fixture();
        idx.insert_batch(&[
            make_row("cli", "in urchin", Some("/home/me/dev/urchin")),
            make_row("cli", "in other", Some("/home/me/dev/other")),
        ])
        .unwrap();
        let hits = idx.query_workspace("/home/me/dev/urchin", 1.0, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "in urchin");
    }

    #[test]
    fn rebuild_round_trips() {
        use crate::journal::Journal;

        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("events.jsonl");
        let journal = Journal::new(journal_path.clone());

        for i in 0..5 {
            journal
                .append(&Event::new(
                    "cli",
                    EventKind::Conversation,
                    format!("event {}", i),
                ))
                .unwrap();
        }
        journal.flush().unwrap();

        let idx = Index::open(&dir.path().join("index.db")).unwrap();
        idx.ensure_schema().unwrap();
        let n = idx.rebuild_from_journal(&journal_path).unwrap();
        assert_eq!(n, 5);
    }

    #[test]
    fn query_source_filter() {
        let (_dir, idx) = fixture();
        idx.insert_batch(&[
            make_row("claude", "from claude", None),
            make_row("shell", "from shell", None),
        ])
        .unwrap();
        let hits = idx.query_recent(1.0, Some("claude"), 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, "claude");
    }
}
