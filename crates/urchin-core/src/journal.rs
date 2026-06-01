//! Append-only journal. Events are written once, never mutated.
//! The journal file at ~/.local/share/urchin/journal/events.jsonl is the source of truth.

use crate::event::Event;
use crate::index::{build_index_row, Index, IndexRow};
use anyhow::Result;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

enum JournalOp {
    Append { line: String, id: uuid::Uuid },
    Flush(std::sync::mpsc::SyncSender<()>),
}

pub struct Journal {
    path: PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<JournalOp>,
    index: Option<Arc<Index>>,
}

pub struct JournalStats {
    pub event_count: usize,
    pub file_size_bytes: u64,
    pub last_event: Option<Event>,
}

fn open_writer(path: &PathBuf) -> BufWriter<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("journal: failed to open file for writing");
    BufWriter::new(f)
}

fn spawn_writer(
    path: PathBuf,
    index_opt: Option<Arc<Index>>,
) -> tokio::sync::mpsc::UnboundedSender<JournalOp> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<JournalOp>();
    let writer_path = path;
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("journal: failed to build writer runtime")
            .block_on(async move {
                let mut file: Option<BufWriter<std::fs::File>> = None;
                // Track write position for the index byte_offset column.
                // Initialise from the current file size so we pick up where
                // a previous run left off; 0 if the file does not exist yet.
                let mut byte_offset: u64 = std::fs::metadata(&writer_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                let mut pending_rows: Vec<IndexRow> = Vec::new();

                while let Some(op) = rx.recv().await {
                    match op {
                        JournalOp::Append { line, id } => {
                            // Idempotency: skip JSONL write if the event ID is already indexed.
                            if let Some(ref idx) = index_opt {
                                if idx.exists_by_id(&id.to_string()).unwrap_or(false) {
                                    continue;
                                }
                            }
                            let f = file.get_or_insert_with(|| open_writer(&writer_path));
                            let start = byte_offset;
                            let _ = writeln!(f, "{}", line);
                            byte_offset += line.len() as u64 + 1; // +1 for '\n'

                            if index_opt.is_some() {
                                if let Some(row) = build_index_row(&line, start) {
                                    pending_rows.push(row);
                                }
                            }
                        }
                        JournalOp::Flush(ack) => {
                            // 1. Flush JSONL first — always, unconditionally.
                            if let Some(f) = &mut file {
                                let _ = f.flush();
                            }
                            // 2. Batch-insert pending rows into SQLite (best-effort).
                            //    An index error must never block the ack or crash the daemon.
                            if let Some(ref idx) = index_opt {
                                if !pending_rows.is_empty() {
                                    if let Err(e) = idx.insert_batch(&pending_rows) {
                                        tracing::warn!("index insert failed (JSONL intact): {}", e);
                                    }
                                }
                            }
                            pending_rows.clear();
                            // 3. Always ack — callers must not hang on SQLite errors.
                            let _ = ack.send(());
                        }
                    }
                }
            });
    });
    tx
}

impl Journal {
    /// Create a journal with no SQLite index. All writes go to JSONL only.
    /// Query methods fall back to a full JSONL scan.
    /// Use this in tests and lightweight one-shot CLI commands.
    pub fn new(path: PathBuf) -> Self {
        let tx = spawn_writer(path.clone(), None);
        Self {
            path,
            tx,
            index: None,
        }
    }

    /// Create a journal backed by a SQLite projection index.
    /// The index is created and schema-initialised at `index_path` if it does
    /// not exist. Query methods hit SQLite instead of scanning JSONL.
    /// Falls through to `Journal::new` style if the index cannot be opened.
    pub fn new_with_index(path: PathBuf, index_path: PathBuf) -> Result<Self> {
        let index = Arc::new(Index::open(&index_path)?);
        index.ensure_schema()?;

        // Backfill any events that collectors wrote directly to the JSONL
        // (bypassing the intake HTTP endpoint and therefore the writer channel).
        // Runs in a background thread so startup is not blocked.
        let backfill_index = Arc::clone(&index);
        let backfill_path = path.clone();
        std::thread::spawn(move || {
            match backfill_index.backfill_from_journal(&backfill_path) {
                Ok(0) => {}
                Ok(n) => tracing::info!("index backfill: {} events indexed from JSONL", n),
                Err(e) => tracing::warn!("index backfill failed (JSONL intact): {}", e),
            }
        });

        let tx = spawn_writer(path.clone(), Some(Arc::clone(&index)));
        Ok(Self {
            path,
            tx,
            index: Some(index),
        })
    }

    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("urchin")
            .join("journal")
            .join("events.jsonl")
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Queue an event for writing. Returns immediately; the writer task flushes asynchronously.
    /// Call flush() to guarantee the event is on disk before reading back.
    /// If a SQLite index is present and the event ID already exists, the write is a silent no-op.
    pub fn append(&self, event: &Event) -> Result<()> {
        let line = serde_json::to_string(event)?;
        self.tx
            .send(JournalOp::Append { line, id: event.id })
            .map_err(|_| anyhow::anyhow!("journal writer has stopped"))
    }

    /// Block until all queued events are flushed to the OS buffer.
    pub fn flush(&self) -> Result<()> {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(0);
        self.tx
            .send(JournalOp::Flush(ack_tx))
            .map_err(|_| anyhow::anyhow!("journal writer has stopped"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("journal writer exited before flush ack"))
    }

    pub fn read_all(&self) -> Result<Vec<Event>> {
        if !self.path.exists() {
            return Ok(vec![]);
        }
        let file = std::fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        let events = reader
            .lines()
            .map_while(Result::ok)
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect();
        Ok(events)
    }

    /// Read the last `n` events without loading the whole file.
    pub fn read_tail(&self, n: usize) -> Result<Vec<Event>> {
        if n == 0 || !self.path.exists() {
            return Ok(vec![]);
        }
        let mut file = std::fs::File::open(&self.path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        if file_len == 0 {
            return Ok(vec![]);
        }
        const CHUNK: u64 = 65_536;
        let mut newlines: usize = 0;
        let mut start: u64 = 0;
        let mut pos: u64 = file_len;
        'scan: loop {
            let read_from = pos.saturating_sub(CHUNK);
            let read_len = (pos - read_from) as usize;
            file.seek(SeekFrom::Start(read_from))?;
            let mut buf = vec![0u8; read_len];
            file.read_exact(&mut buf)?;
            for i in (0..read_len).rev() {
                if buf[i] == b'\n' {
                    newlines += 1;
                    if newlines > n {
                        start = read_from + i as u64 + 1;
                        break 'scan;
                    }
                }
            }
            if read_from == 0 {
                break;
            }
            pos = read_from;
        }
        let (events, _) = self.read_from_byte_offset(start)?;
        let skip = events.len().saturating_sub(n);
        Ok(events.into_iter().skip(skip).collect())
    }

    /// Read a window of events at newest-first positions [offset, offset+limit).
    /// Bounded read: only the last `offset + limit` events are scanned from EOF.
    pub fn read_window(&self, offset: usize, limit: usize) -> Result<Vec<Event>> {
        if limit == 0 {
            return Ok(vec![]);
        }
        let mut events = self.read_tail(offset + limit)?;
        events.reverse();
        Ok(events.into_iter().skip(offset).take(limit).collect())
    }

    /// Read events starting from a byte offset in the file.
    /// Returns (events, new_offset) where new_offset is the file position after reading.
    pub fn read_from_byte_offset(&self, offset: u64) -> Result<(Vec<Event>, u64)> {
        if !self.path.exists() {
            return Ok((vec![], 0));
        }
        let mut file = std::fs::File::open(&self.path)?;
        let file_len = file.seek(SeekFrom::End(0))?;
        if offset >= file_len {
            return Ok((vec![], file_len));
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;
        let events = buf
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        Ok((events, file_len))
    }

    /// Recent events, newest-first. Uses the SQLite index when available,
    /// otherwise falls back to a full JSONL scan.
    pub fn query_recent(
        &self,
        hours: f64,
        source: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Event>> {
        if let Some(ref idx) = self.index {
            idx.query_recent(hours, source, limit)
        } else {
            let events = self.read_all()?;
            Ok(crate::query::recent(&events, hours, source, limit)
                .into_iter()
                .cloned()
                .collect())
        }
    }

    /// Content substring search. Uses the SQLite index when available.
    pub fn query_search(&self, q: &str, hours: f64, limit: usize) -> Result<Vec<Event>> {
        if let Some(ref idx) = self.index {
            idx.query_search(q, hours, limit)
        } else {
            let events = self.read_all()?;
            Ok(crate::query::search_content(&events, q, hours, limit)
                .into_iter()
                .cloned()
                .collect())
        }
    }

    /// Project-scoped events. Uses the SQLite index when available.
    pub fn query_project(&self, project: &str, hours: f64, limit: usize) -> Result<Vec<Event>> {
        if let Some(ref idx) = self.index {
            idx.query_project(project, hours, limit)
        } else {
            let events = self.read_all()?;
            Ok(
                crate::query::project_context(&events, project, hours, limit)
                    .into_iter()
                    .cloned()
                    .collect(),
            )
        }
    }

    /// Workspace-scoped events. Uses the SQLite index when available.
    pub fn query_workspace(&self, path: &str, hours: f64, limit: usize) -> Result<Vec<Event>> {
        if let Some(ref idx) = self.index {
            idx.query_workspace(path, hours, limit)
        } else {
            let events = self.read_all()?;
            Ok(crate::query::workspace_context(&events, path, hours, limit)
                .into_iter()
                .cloned()
                .collect())
        }
    }

    /// Fast stats: raw byte scan for line count, tail seek for last event.
    pub fn stats(&self) -> Result<JournalStats> {
        if !self.path.exists() {
            return Ok(JournalStats {
                event_count: 0,
                file_size_bytes: 0,
                last_event: None,
            });
        }
        let file_size_bytes = std::fs::metadata(&self.path)?.len();
        if file_size_bytes == 0 {
            return Ok(JournalStats {
                event_count: 0,
                file_size_bytes: 0,
                last_event: None,
            });
        }
        let mut file = std::fs::File::open(&self.path)?;
        let mut event_count: usize = 0;
        let mut chunk = vec![0u8; 65_536];
        loop {
            let n = file.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            for &b in &chunk[..n] {
                if b == b'\n' {
                    event_count += 1;
                }
            }
        }
        let tail_start = file_size_bytes.saturating_sub(4096);
        file.seek(SeekFrom::Start(tail_start))?;
        let mut tail = String::new();
        file.read_to_string(&mut tail)?;
        let last_event = tail
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| serde_json::from_str(l).ok());
        Ok(JournalStats {
            event_count,
            file_size_bytes,
            last_event,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;
    use tempfile::NamedTempFile;

    #[test]
    fn append_and_read_roundtrip() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());

        let e1 = Event::new("cli", EventKind::Conversation, "first");
        let e2 = Event::new("cli", EventKind::Agent, "second");

        journal.append(&e1).unwrap();
        journal.append(&e2).unwrap();
        journal.flush().unwrap();

        let events = journal.read_all().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content, "first");
        assert_eq!(events[1].content, "second");
    }

    #[test]
    fn stats_on_empty_journal() {
        let tmp = NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "").unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());
        let stats = journal.stats().unwrap();
        assert_eq!(stats.event_count, 0);
        assert!(stats.last_event.is_none());
    }

    #[test]
    fn stats_counts_correctly() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());

        for i in 0..5 {
            let e = Event::new("cli", EventKind::Conversation, format!("event {}", i));
            journal.append(&e).unwrap();
        }
        journal.flush().unwrap();

        let stats = journal.stats().unwrap();
        assert_eq!(stats.event_count, 5);
        assert!(stats.last_event.is_some());
        assert_eq!(stats.last_event.unwrap().content, "event 4");
    }

    #[test]
    fn read_all_on_missing_file_returns_empty() {
        let journal = Journal::new(PathBuf::from("/nonexistent/path/events.jsonl"));
        let events = journal.read_all().unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn read_tail_returns_last_n() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());
        for i in 0..10 {
            journal
                .append(&Event::new(
                    "cli",
                    EventKind::Conversation,
                    format!("event {}", i),
                ))
                .unwrap();
        }
        journal.flush().unwrap();
        let tail = journal.read_tail(3).unwrap();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].content, "event 7");
        assert_eq!(tail[1].content, "event 8");
        assert_eq!(tail[2].content, "event 9");
    }

    #[test]
    fn read_tail_fewer_events_than_n() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());
        for i in 0..3 {
            journal
                .append(&Event::new(
                    "cli",
                    EventKind::Conversation,
                    format!("e{}", i),
                ))
                .unwrap();
        }
        journal.flush().unwrap();
        let tail = journal.read_tail(10).unwrap();
        assert_eq!(tail.len(), 3);
    }

    #[test]
    fn read_window_first_page_is_newest_first() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());
        for i in 0..10 {
            journal
                .append(&Event::new(
                    "cli",
                    EventKind::Conversation,
                    format!("e{}", i),
                ))
                .unwrap();
        }
        journal.flush().unwrap();
        let window = journal.read_window(0, 3).unwrap();
        assert_eq!(window.len(), 3);
        assert_eq!(window[0].content, "e9");
        assert_eq!(window[1].content, "e8");
        assert_eq!(window[2].content, "e7");
    }

    #[test]
    fn read_window_offset_skips_newest() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());
        for i in 0..10 {
            journal
                .append(&Event::new(
                    "cli",
                    EventKind::Conversation,
                    format!("e{}", i),
                ))
                .unwrap();
        }
        journal.flush().unwrap();
        let window = journal.read_window(3, 3).unwrap();
        assert_eq!(window.len(), 3);
        assert_eq!(window[0].content, "e6");
        assert_eq!(window[1].content, "e5");
        assert_eq!(window[2].content, "e4");
    }

    #[test]
    fn read_window_past_end_returns_empty() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());
        for i in 0..3 {
            journal
                .append(&Event::new(
                    "cli",
                    EventKind::Conversation,
                    format!("e{}", i),
                ))
                .unwrap();
        }
        journal.flush().unwrap();
        let window = journal.read_window(10, 5).unwrap();
        assert!(window.is_empty());
    }

    #[test]
    fn journal_with_index_query_recent() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("events.jsonl");
        let index_path = dir.path().join("index.db");
        let journal = Journal::new_with_index(journal_path, index_path).unwrap();

        journal
            .append(&Event::new("cli", EventKind::Conversation, "hello index"))
            .unwrap();
        journal.flush().unwrap();

        let events = journal.query_recent(1.0, None, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "hello index");
    }

    #[test]
    fn journal_with_index_query_search() {
        let dir = tempfile::tempdir().unwrap();
        let journal_path = dir.path().join("events.jsonl");
        let index_path = dir.path().join("index.db");
        let journal = Journal::new_with_index(journal_path, index_path).unwrap();

        journal
            .append(&Event::new(
                "cli",
                EventKind::Conversation,
                "needle in a haystack",
            ))
            .unwrap();
        journal
            .append(&Event::new(
                "cli",
                EventKind::Conversation,
                "something unrelated",
            ))
            .unwrap();
        journal.flush().unwrap();

        let hits = journal.query_search("needle", 1.0, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].content, "needle in a haystack");
    }

    #[test]
    fn journal_without_index_falls_back() {
        let tmp = NamedTempFile::new().unwrap();
        let journal = Journal::new(tmp.path().to_path_buf());
        journal
            .append(&Event::new("cli", EventKind::Conversation, "fallback test"))
            .unwrap();
        journal.flush().unwrap();

        // query_recent should fall back to JSONL scan when no index is present.
        let events = journal.query_recent(1.0, None, 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "fallback test");
    }

    #[test]
    fn concurrent_writers_all_land() {
        use std::sync::Arc;
        let tmp = NamedTempFile::new().unwrap();
        let journal = Arc::new(Journal::new(tmp.path().to_path_buf()));
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let j = journal.clone();
                std::thread::spawn(move || {
                    for k in 0..1_000 {
                        j.append(&Event::new(
                            "test",
                            EventKind::Command,
                            format!("thread {} event {}", i, k),
                        ))
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        journal.flush().unwrap();
        let events = journal.read_all().unwrap();
        assert_eq!(events.len(), 10_000);
    }
}
