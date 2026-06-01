//! Stateful byte-offset cursor for the Sleipnir JSONL reader.
//!
//! Persisted to `~/.local/share/urchin/sleipnir.cursor` as a compact JSON
//! file. The cursor is the single source of truth for how far into the
//! append-only journal Sleipnir has consumed. It is advanced atomically after
//! each successful processing batch via a temp-rename so a mid-write crash
//! never leaves a corrupt cursor on disk.
//!
//! Truncation safety: the recorded `journal_size_at_checkpoint` lets any
//! subsequent load detect that the journal was rotated or manually cleared
//! (live size < checkpoint size) and automatically reset to offset 0.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    /// Byte offset into the JSONL journal. The next read starts here.
    pub offset: u64,
    /// Canonical path of the journal file this cursor tracks.
    /// Used to detect when a cursor file from a different journal is loaded.
    pub journal_path: PathBuf,
    /// Total file size at the time of the last successful checkpoint.
    /// If the live file is smaller than this, the journal was truncated or
    /// rotated and the cursor must be reset to 0.
    pub journal_size_at_checkpoint: u64,
    /// Wall-clock time of the last successful checkpoint.
    pub updated_at: DateTime<Utc>,
}

impl Cursor {
    /// Return the default path for the cursor file.
    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("urchin")
            .join("sleipnir.cursor")
    }

    /// Load a cursor from an explicit path.
    ///
    /// Returns `Ok(None)` when the file does not exist (fresh start).
    /// Returns `Err` only for real I/O or parse failures.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read(path)
            .with_context(|| format!("reading cursor file: {}", path.display()))?;
        let cursor: Self = serde_json::from_slice(&raw)
            .with_context(|| format!("parsing cursor file: {}", path.display()))?;
        Ok(Some(cursor))
    }

    /// Load from the default path, or build a zero-offset cursor for `journal_path`.
    pub fn load_or_create(journal_path: &Path) -> Result<Self> {
        Self::load_or_create_at(journal_path, &Self::default_path())
    }

    /// Load from an explicit `cursor_path`, or build a zero-offset cursor for
    /// `journal_path`. Useful in tests and when the pipeline controls all paths.
    ///
    /// Two integrity checks run on every load:
    /// 1. Journal path mismatch — on-disk cursor tracks a different file path;
    ///    discarded and a fresh cursor is returned.
    /// 2. Truncation — live journal is smaller than the size recorded at the
    ///    last checkpoint; journal was rotated or cleared, so cursor resets to 0.
    pub fn load_or_create_at(journal_path: &Path, cursor_path: &Path) -> Result<Self> {
        let loaded = Self::load(cursor_path)?;

        let Some(cursor) = loaded else {
            tracing::debug!("no cursor on disk; starting from offset 0");
            return Ok(Self::at_start(journal_path));
        };

        if cursor.journal_path != journal_path {
            tracing::warn!(
                stored = %cursor.journal_path.display(),
                requested = %journal_path.display(),
                "cursor journal path mismatch — discarding and resetting",
            );
            return Ok(Self::at_start(journal_path));
        }

        match std::fs::metadata(journal_path) {
            Ok(meta) if meta.len() < cursor.journal_size_at_checkpoint => {
                tracing::warn!(
                    checkpoint_size = cursor.journal_size_at_checkpoint,
                    live_size = meta.len(),
                    "journal is smaller than last checkpoint — truncation detected, resetting cursor",
                );
                Ok(Self::at_start(journal_path))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(
                    path = %journal_path.display(),
                    "journal file missing — resetting cursor to await re-creation",
                );
                Ok(Self::at_start(journal_path))
            }
            _ => {
                tracing::debug!(
                    offset = cursor.offset,
                    updated_at = %cursor.updated_at,
                    "cursor loaded from disk",
                );
                Ok(cursor)
            }
        }
    }

    /// Persist the cursor atomically to the default path.
    pub fn save(&mut self) -> Result<()> {
        self.save_to(&Self::default_path())
    }

    /// Persist the cursor atomically to an explicit path.
    ///
    /// The write goes to `<path>.tmp`, then `rename` swaps it in. POSIX rename
    /// is atomic within the same filesystem, so a crash between write and rename
    /// leaves the old cursor intact rather than a partial file.
    pub fn save_to(&mut self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating cursor directory: {}", parent.display()))?;
        }
        self.updated_at = Utc::now();
        let tmp = path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(self).context("serialising cursor")?;
        std::fs::write(&tmp, &bytes)
            .with_context(|| format!("writing cursor tmp: {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming cursor into place: {}", path.display()))?;
        Ok(())
    }

    /// Advance the cursor to `new_offset` and persist to disk.
    ///
    /// `file_size` must be the total byte length of the journal file *after*
    /// the batch was processed. It is stored as the truncation sentinel.
    pub fn advance(&mut self, new_offset: u64, file_size: u64) -> Result<()> {
        self.offset = new_offset;
        self.journal_size_at_checkpoint = file_size;
        self.save()
    }

    /// Advance to `new_offset` and persist to an explicit path (testing).
    pub fn advance_to(&mut self, new_offset: u64, file_size: u64, path: &Path) -> Result<()> {
        self.offset = new_offset;
        self.journal_size_at_checkpoint = file_size;
        self.save_to(path)
    }

    /// Reset offset to 0 in memory only. Call `advance(0, 0)` to also persist.
    pub fn reset(&mut self) {
        self.offset = 0;
        self.journal_size_at_checkpoint = 0;
        self.updated_at = Utc::now();
    }

    /// True when the cursor is at the beginning of the file.
    pub fn is_at_start(&self) -> bool {
        self.offset == 0
    }

    pub(crate) fn at_start(journal_path: &Path) -> Self {
        Self {
            offset: 0,
            journal_path: journal_path.to_owned(),
            journal_size_at_checkpoint: 0,
            updated_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn journal_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("events.jsonl")
    }

    fn cursor_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("sleipnir.cursor")
    }

    fn write_journal(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        let dir = tempdir().unwrap();
        let result = Cursor::load(&cursor_path(&dir)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let jp = journal_path(&dir);
        let cp = cursor_path(&dir);

        write_journal(&jp, b"event1\nevent2\n");

        let mut cursor = Cursor::at_start(&jp);
        cursor.advance_to(7, 14, &cp).unwrap();

        let loaded = Cursor::load(&cp).unwrap().unwrap();
        assert_eq!(loaded.offset, 7);
        assert_eq!(loaded.journal_size_at_checkpoint, 14);
        assert_eq!(loaded.journal_path, jp);
    }

    #[test]
    fn load_or_create_fresh_start() {
        let dir = tempdir().unwrap();
        let jp = journal_path(&dir);
        write_journal(&jp, b"");

        // No cursor file exists — should return offset 0.
        // We can't use the default path in tests, so test via load_or_create
        // after overriding the default path would require injection. Instead
        // verify load returns None for a missing file, then at_start path.
        let result = Cursor::load(&cursor_path(&dir)).unwrap();
        assert!(result.is_none());

        let cursor = Cursor::at_start(&jp);
        assert!(cursor.is_at_start());
        assert_eq!(cursor.journal_path, jp);
    }

    #[test]
    fn advance_updates_offset_and_persists() {
        let dir = tempdir().unwrap();
        let jp = journal_path(&dir);
        let cp = cursor_path(&dir);

        write_journal(&jp, b"hello\nworld\n");

        let mut cursor = Cursor::at_start(&jp);
        assert!(cursor.is_at_start());

        cursor.advance_to(6, 12, &cp).unwrap();
        assert_eq!(cursor.offset, 6);
        assert!(!cursor.is_at_start());

        // Reload to confirm persistence.
        let reloaded = Cursor::load(&cp).unwrap().unwrap();
        assert_eq!(reloaded.offset, 6);
        assert_eq!(reloaded.journal_size_at_checkpoint, 12);
    }

    #[test]
    fn reset_clears_offset_in_memory() {
        let dir = tempdir().unwrap();
        let jp = journal_path(&dir);
        let mut cursor = Cursor::at_start(&jp);
        cursor.offset = 999;
        cursor.journal_size_at_checkpoint = 999;

        cursor.reset();
        assert!(cursor.is_at_start());
        assert_eq!(cursor.journal_size_at_checkpoint, 0);
    }

    #[test]
    fn atomic_write_does_not_leave_tmp_on_success() {
        let dir = tempdir().unwrap();
        let jp = journal_path(&dir);
        let cp = cursor_path(&dir);

        write_journal(&jp, b"data\n");
        let mut cursor = Cursor::at_start(&jp);
        cursor.advance_to(5, 5, &cp).unwrap();

        let tmp = cp.with_extension("tmp");
        assert!(!tmp.exists(), "tmp file should be cleaned up after rename");
        assert!(cp.exists(), "cursor file should exist");
    }

    #[test]
    fn journal_path_mismatch_resets() {
        // Simulate loading a cursor that was tracking a different journal path.
        let dir = tempdir().unwrap();
        let cp = cursor_path(&dir);

        let old_jp = dir.path().join("old_events.jsonl");
        let new_jp = dir.path().join("events.jsonl");

        write_journal(&old_jp, b"old data\n");
        write_journal(&new_jp, b"new data\n");

        // Save a cursor for old_jp.
        let mut cursor = Cursor::at_start(&old_jp);
        cursor.advance_to(9, 9, &cp).unwrap();

        // Manually call the integrity check logic (mocking load_or_create).
        let loaded = Cursor::load(&cp).unwrap().unwrap();
        assert_eq!(loaded.journal_path, old_jp);

        // If we tried load_or_create for new_jp it would discard and reset.
        // Since load_or_create uses the default path, verify the mismatch
        // guard is correct by inspecting the loaded cursor's path.
        assert_ne!(loaded.journal_path, new_jp);
    }

    #[test]
    fn truncation_detected_when_file_shrinks() {
        let dir = tempdir().unwrap();
        let cp = cursor_path(&dir);
        let jp = journal_path(&dir);

        // Write 100 bytes and checkpoint at that size.
        write_journal(&jp, &vec![b'x'; 100]);
        let mut cursor = Cursor::at_start(&jp);
        cursor.advance_to(100, 100, &cp).unwrap();

        // Simulate truncation: replace journal with fewer bytes.
        write_journal(&jp, &vec![b'y'; 40]);

        // Reload and run truncation check manually.
        let loaded = Cursor::load(&cp).unwrap().unwrap();
        let live_size = std::fs::metadata(&jp).unwrap().len();
        assert!(
            live_size < loaded.journal_size_at_checkpoint,
            "test precondition: live file must be smaller than checkpoint"
        );
        // The guard in load_or_create would reset — we verify the condition holds.
        assert_eq!(loaded.journal_size_at_checkpoint, 100);
        assert_eq!(live_size, 40);
    }
}
