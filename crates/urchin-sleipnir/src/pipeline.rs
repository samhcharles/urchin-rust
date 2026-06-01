//! Main processing pipeline: journal -> cursor -> filter -> cluster -> store -> vault inbox.
//!
//! One `run_pass` call consumes all journal bytes since the last cursor
//! position, filters noise, clusters activity windows, persists clusters to
//! SQLite, writes markdown stubs to the vault inbox, and advances the cursor.

use crate::cluster::{cluster_by_source_and_time, Cluster};
use crate::cursor::Cursor;
use crate::filter::{dedup_consecutive, is_signal};
use crate::store::Store;
use anyhow::{Context, Result};
use chrono::Timelike;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use urchin_core::event::Event;

pub struct PassConfig {
    /// Path to the Urchin JSONL journal.
    pub journal_path: PathBuf,
    /// Path to the Sleipnir cursor file.
    pub cursor_path: PathBuf,
    /// Path to the Sleipnir SQLite store.
    pub store_path: PathBuf,
    /// Maximum idle gap in seconds before a new cluster begins.
    pub cluster_gap_secs: i64,
    /// Vault inbox directory for cluster stubs.
    /// `None` disables inbox writing (tests, dry-run mode).
    pub inbox_path: Option<PathBuf>,
}

impl PassConfig {
    pub fn with_defaults(journal_path: PathBuf) -> Self {
        let inbox_path = dirs::home_dir()
            .map(|h| h.join("vault").join("inbox").join("sleipnir"));
        Self {
            journal_path,
            cursor_path: Cursor::default_path(),
            store_path: Store::default_path(),
            cluster_gap_secs: 300,
            inbox_path,
        }
    }
}

#[derive(Debug)]
pub struct PassResult {
    pub events_read: usize,
    pub events_kept: usize,
    pub clusters_produced: usize,
    pub stubs_written: usize,
    pub new_offset: u64,
}

/// Run one distillation pass over new journal bytes.
///
/// Returns immediately with zero counts if the journal has not grown since
/// the last cursor checkpoint. Safe to call in a tight poll loop.
pub fn run_pass(config: &PassConfig) -> Result<PassResult> {
    let mut cursor =
        Cursor::load_or_create_at(&config.journal_path, &config.cursor_path)?;

    let (mut events, file_size) = read_since(&config.journal_path, cursor.offset)?;
    let events_read = events.len();

    if events_read == 0 {
        return Ok(PassResult {
            events_read: 0,
            events_kept: 0,
            clusters_produced: 0,
            stubs_written: 0,
            new_offset: cursor.offset,
        });
    }

    events.retain(|e| is_signal(e));
    events.sort_by_key(|e| e.timestamp);
    dedup_consecutive(&mut events);

    let events_kept = events.len();
    let clusters = cluster_by_source_and_time(events, config.cluster_gap_secs);
    let clusters_produced = clusters.len();

    if clusters_produced > 0 {
        let store = Store::open(&config.store_path)?;
        for cluster in &clusters {
            store.insert_cluster(cluster)?;
        }
    }

    let stubs_written = if let Some(ref inbox) = config.inbox_path {
        write_inbox_stubs(&clusters, inbox)?
    } else {
        0
    };

    cursor.advance_to(file_size, file_size, &config.cursor_path)?;

    tracing::debug!(
        events_read,
        events_kept,
        clusters_produced,
        stubs_written,
        new_offset = file_size,
        "pipeline pass complete",
    );

    Ok(PassResult {
        events_read,
        events_kept,
        clusters_produced,
        stubs_written,
        new_offset: file_size,
    })
}

/// Write one markdown stub per cluster into the vault inbox directory.
///
/// Filenames are deterministic from (source, started_at) so replaying the
/// same clusters is a no-op — existing stubs are skipped, not overwritten.
/// Returns the number of new stubs written.
fn write_inbox_stubs(clusters: &[Cluster], inbox_path: &Path) -> Result<usize> {
    std::fs::create_dir_all(inbox_path)
        .with_context(|| format!("creating inbox dir: {}", inbox_path.display()))?;

    let mut written = 0;
    for cluster in clusters {
        let name = stub_filename(cluster);
        let dest = inbox_path.join(&name);
        if dest.exists() {
            continue;
        }
        let content = render_stub(cluster);
        let tmp = dest.with_extension("tmp");
        std::fs::write(&tmp, content.as_bytes())
            .with_context(|| format!("writing stub tmp: {}", tmp.display()))?;
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("renaming stub into place: {}", dest.display()))?;
        written += 1;
    }
    Ok(written)
}

/// Deterministic filename: `YYYY-MM-DD-HHmm-source.md`
fn stub_filename(cluster: &Cluster) -> String {
    let source_slug = cluster
        .source
        .to_lowercase()
        .replace(['/', ' ', ':', '.'], "-");
    format!(
        "{}-{:02}{:02}-{}.md",
        cluster.started_at.format("%Y-%m-%d"),
        cluster.started_at.hour(),
        cluster.started_at.minute(),
        source_slug,
    )
}

/// Render a vault-schema-compliant markdown stub for a cluster.
fn render_stub(cluster: &Cluster) -> String {
    let started = cluster.started_at;
    let ended = cluster.ended_at;
    let date = started.format("%Y-%m-%d").to_string();

    let dur_secs = cluster.duration_secs();
    let duration_str = if dur_secs < 60 {
        format!("{}s", dur_secs)
    } else {
        format!("{}m", dur_secs / 60)
    };

    let title = format!(
        "{} · {:02}:{:02}–{:02}:{:02}",
        cluster.source,
        started.hour(),
        started.minute(),
        ended.hour(),
        ended.minute(),
    );

    let event_ids = cluster
        .events
        .iter()
        .map(|e| format!("  - {}", e.id))
        .collect::<Vec<_>>()
        .join("\n");

    let sample = cluster
        .events
        .first()
        .map(|e| e.content.chars().take(200).collect::<String>())
        .unwrap_or_default();

    format!(
        "---\ntitle: \"{title}\"\npartition: sessions\ntype: activity-cluster\nsource: {source}\nstarted_at: {started}\nended_at: {ended}\nduration_secs: {dur_s}\nduration: {duration_str}\nevent_count: {count}\nevent_ids:\n{event_ids}\ntags: [sleipnir, activity, {source}]\ncreated: {date}\n---\n\n# {title}\n\n**Duration:** {duration_str} · **Events:** {count}\n\n## Sample\n\n{sample}\n",
        title = title,
        source = cluster.source,
        started = started.to_rfc3339(),
        ended = ended.to_rfc3339(),
        dur_s = dur_secs,
        duration_str = duration_str,
        count = cluster.events.len(),
        event_ids = event_ids,
        date = date,
        sample = sample,
    )
}

/// Read all JSONL lines from `offset` to EOF.
/// Returns (events, total_file_size).
fn read_since(journal_path: &Path, offset: u64) -> Result<(Vec<Event>, u64)> {
    if !journal_path.exists() {
        return Ok((vec![], 0));
    }
    let mut file = std::fs::File::open(journal_path)?;
    let file_size = file.seek(SeekFrom::End(0))?;
    if offset >= file_size {
        return Ok((vec![], file_size));
    }
    file.seek(SeekFrom::Start(offset))?;
    let reader = BufReader::new(file);
    let events = reader
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    Ok((events, file_size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use urchin_core::event::{Event, EventKind};

    fn write_events(path: &Path, events: &[Event]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for e in events {
            writeln!(f, "{}", serde_json::to_string(e).unwrap()).unwrap();
        }
    }

    fn make(source: &str, content: &str) -> Event {
        Event::new(source, EventKind::Command, content)
    }

    fn config_for(dir: &tempfile::TempDir) -> PassConfig {
        PassConfig {
            journal_path: dir.path().join("events.jsonl"),
            cursor_path: dir.path().join("sleipnir.cursor"),
            store_path: dir.path().join("sleipnir.db"),
            cluster_gap_secs: 300,
            inbox_path: Some(dir.path().join("inbox")),
        }
    }

    #[test]
    fn empty_journal_returns_zero_counts() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        std::fs::write(&config.journal_path, "").unwrap();
        let result = run_pass(&config).unwrap();
        assert_eq!(result.events_read, 0);
        assert_eq!(result.stubs_written, 0);
    }

    #[test]
    fn missing_journal_does_not_error() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        let result = run_pass(&config).unwrap();
        assert_eq!(result.events_read, 0);
    }

    #[test]
    fn processes_events_and_produces_clusters() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        let events = vec![
            make("shell", "cargo build --release"),
            make("shell", "cargo test"),
            make("git", "git commit -m fix"),
        ];
        write_events(&config.journal_path, &events);
        let result = run_pass(&config).unwrap();
        assert_eq!(result.events_read, 3);
        assert!(result.clusters_produced >= 1);
        assert!(result.new_offset > 0);
    }

    #[test]
    fn noise_events_are_filtered() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        let events = vec![
            make("sleipnir", "heartbeat"),
            make("shell", "ls"),
            make("shell", "cargo build"),
        ];
        write_events(&config.journal_path, &events);
        let result = run_pass(&config).unwrap();
        assert_eq!(result.events_read, 3);
        assert_eq!(result.events_kept, 1);
    }

    #[test]
    fn second_pass_reads_only_new_bytes() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        write_events(&config.journal_path, &[make("shell", "cargo build --release")]);
        let r1 = run_pass(&config).unwrap();
        assert_eq!(r1.events_read, 1);

        let r2 = run_pass(&config).unwrap();
        assert_eq!(r2.events_read, 0);

        write_events(&config.journal_path, &[make("git", "git push origin main")]);
        let r3 = run_pass(&config).unwrap();
        assert_eq!(r3.events_read, 1);
        assert_eq!(r3.clusters_produced, 1);
    }

    #[test]
    fn inbox_stubs_written_per_cluster() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        let events = vec![
            make("shell", "cargo build --release"),
            make("git", "git push origin main"),
        ];
        write_events(&config.journal_path, &events);
        let result = run_pass(&config).unwrap();
        assert!(result.stubs_written >= 1);
        let inbox = config.inbox_path.unwrap();
        let files: Vec<_> = std::fs::read_dir(&inbox)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(files.len(), result.stubs_written);
    }

    #[test]
    fn stub_is_valid_frontmatter() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        write_events(
            &config.journal_path,
            &[make("shell", "cargo build --release")],
        );
        run_pass(&config).unwrap();
        let inbox = config.inbox_path.as_ref().unwrap();
        let stub_path = std::fs::read_dir(inbox)
            .unwrap()
            .filter_map(|e| e.ok())
            .next()
            .unwrap()
            .path();
        let content = std::fs::read_to_string(&stub_path).unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains("partition: sessions"));
        assert!(content.contains("type: activity-cluster"));
        assert!(content.contains("source: shell"));
        assert!(content.contains("tags: [sleipnir, activity, shell]"));
    }

    #[test]
    fn stub_write_is_idempotent() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        write_events(
            &config.journal_path,
            &[make("shell", "cargo build --release")],
        );
        let r1 = run_pass(&config).unwrap();
        assert_eq!(r1.stubs_written, 1);

        // Reset cursor to replay the same bytes.
        let mut cursor = Cursor::at_start(&config.journal_path);
        cursor.advance_to(0, 0, &config.cursor_path).unwrap();

        let r2 = run_pass(&config).unwrap();
        // Cluster exists in store (idempotent insert), stub already on disk — 0 new writes.
        assert_eq!(r2.stubs_written, 0);
    }

    #[test]
    fn no_inbox_path_skips_stub_writing() {
        let dir = tempdir().unwrap();
        let mut config = config_for(&dir);
        config.inbox_path = None;
        write_events(
            &config.journal_path,
            &[make("shell", "cargo build --release")],
        );
        let result = run_pass(&config).unwrap();
        assert!(result.clusters_produced >= 1);
        assert_eq!(result.stubs_written, 0);
    }

    #[test]
    fn clusters_persisted_to_store() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        let events = vec![
            make("shell", "cargo build --release"),
            make("shell", "cargo test -- --nocapture"),
        ];
        write_events(&config.journal_path, &events);
        run_pass(&config).unwrap();
        let store = Store::open(&config.store_path).unwrap();
        assert!(store.cluster_count().unwrap() >= 1);
    }

    #[test]
    fn idempotent_on_replay() {
        let dir = tempdir().unwrap();
        let config = config_for(&dir);
        write_events(
            &config.journal_path,
            &[make("shell", "cargo check --all-targets")],
        );
        run_pass(&config).unwrap();
        let mut cursor = Cursor::at_start(&config.journal_path);
        cursor.advance_to(0, 0, &config.cursor_path).unwrap();
        run_pass(&config).unwrap();
        let store = Store::open(&config.store_path).unwrap();
        assert_eq!(store.cluster_count().unwrap(), 1);
    }

    #[test]
    fn read_since_returns_correct_file_size() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, b"hello\nworld\n").unwrap();
        let (_, size) = read_since(&path, 0).unwrap();
        assert_eq!(size, 12);
        let (_, size2) = read_since(&path, 12).unwrap();
        assert_eq!(size2, 12);
    }
}
