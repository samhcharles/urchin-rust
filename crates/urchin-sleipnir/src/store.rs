//! Local SQLite store for persisted Sleipnir clusters.
//!
//! One row per cluster. Events are linked by their UUID; full event bodies
//! stay in the append-only JSONL journal. The store is a query index, not a
//! second copy of the data.

use crate::cluster::Cluster;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Deterministic UUID namespace for Sleipnir cluster IDs.
const CLUSTER_NS: Uuid = Uuid::from_bytes([
    0x73, 0x6c, 0x65, 0x69, 0x70, 0x6e, 0x69, 0x72,
    0x00, 0x63, 0x6c, 0x75, 0x73, 0x74, 0x65, 0x72,
]);

/// A cluster row as read back from the store.
#[derive(Debug, Clone)]
pub struct StoredCluster {
    pub id: String,
    pub source: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_secs: i64,
    pub event_count: usize,
    /// UUID strings of the constituent journal events, oldest-first.
    pub event_ids: Vec<String>,
    /// Content of the first event in the cluster (truncated to 200 chars).
    pub content_sample: String,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn default_path() -> PathBuf {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("urchin")
            .join("sleipnir.db")
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating store directory: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening store: {}", path.display()))?;
        let store = Self { conn };
        store.ensure_schema()?;
        Ok(store)
    }

    fn ensure_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous  = NORMAL;

            CREATE TABLE IF NOT EXISTS clusters (
                id             TEXT PRIMARY KEY,
                source         TEXT NOT NULL,
                started_at     TEXT NOT NULL,
                ended_at       TEXT NOT NULL,
                duration_secs  INTEGER NOT NULL,
                event_count    INTEGER NOT NULL,
                event_ids      TEXT NOT NULL,
                content_sample TEXT NOT NULL,
                created_at     TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS clusters_started_at
                ON clusters(started_at DESC);
            CREATE INDEX IF NOT EXISTS clusters_source
                ON clusters(source, started_at DESC);
            ",
        )
        .context("ensuring store schema")?;
        Ok(())
    }

    /// Insert a cluster. Silently skips if a cluster with the same ID already
    /// exists (idempotent — safe to call on replayed journal sections).
    pub fn insert_cluster(&self, cluster: &Cluster) -> Result<()> {
        let id = cluster_id(&cluster.source, &cluster.started_at);
        let event_ids: Vec<String> = cluster.events.iter().map(|e| e.id.to_string()).collect();
        let event_ids_json =
            serde_json::to_string(&event_ids).context("serialising event_ids")?;
        let content_sample = cluster
            .events
            .first()
            .map(|e| e.content.chars().take(200).collect::<String>())
            .unwrap_or_default();

        self.conn
            .execute(
                "INSERT OR IGNORE INTO clusters
                 (id, source, started_at, ended_at, duration_secs,
                  event_count, event_ids, content_sample, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id,
                    cluster.source,
                    cluster.started_at.to_rfc3339(),
                    cluster.ended_at.to_rfc3339(),
                    cluster.duration_secs(),
                    cluster.events.len() as i64,
                    event_ids_json,
                    content_sample,
                    Utc::now().to_rfc3339(),
                ],
            )
            .context("inserting cluster")?;
        Ok(())
    }

    /// Return clusters that started within the last `hours` hours, newest-first.
    pub fn recent_clusters(&self, hours: f64, limit: usize) -> Result<Vec<StoredCluster>> {
        let cutoff = Utc::now() - chrono::Duration::milliseconds((hours * 3_600_000.0) as i64);
        let mut stmt = self.conn.prepare(
            "SELECT id, source, started_at, ended_at, duration_secs,
                    event_count, event_ids, content_sample
             FROM   clusters
             WHERE  started_at >= ?1
             ORDER  BY started_at DESC
             LIMIT  ?2",
        )?;

        let rows = stmt
            .query_map(params![cutoff.to_rfc3339(), limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .filter_map(
                |(id, source, started_str, ended_str, dur, count, ids_json, sample)| {
                    let started_at = started_str.parse::<DateTime<Utc>>().ok()?;
                    let ended_at = ended_str.parse::<DateTime<Utc>>().ok()?;
                    let event_ids: Vec<String> =
                        serde_json::from_str(&ids_json).unwrap_or_default();
                    Some(StoredCluster {
                        id,
                        source,
                        started_at,
                        ended_at,
                        duration_secs: dur,
                        event_count: count as usize,
                        event_ids,
                        content_sample: sample,
                    })
                },
            )
            .collect();

        Ok(rows)
    }

    /// Total number of clusters in the store.
    pub fn cluster_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))?;
        Ok(n as usize)
    }
}

fn cluster_id(source: &str, started_at: &DateTime<Utc>) -> String {
    let key = format!("{}\0{}", source, started_at.timestamp_micros());
    Uuid::new_v5(&CLUSTER_NS, key.as_bytes()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::Cluster;
    use tempfile::tempdir;
    use urchin_core::event::{Event, EventKind};

    fn make_cluster(source: &str, n_events: usize) -> Cluster {
        let base = Utc::now();
        let events: Vec<Event> = (0..n_events)
            .map(|i| {
                let mut e = Event::new(source, EventKind::Command, format!("cmd {}", i));
                e.timestamp = base + chrono::Duration::seconds(i as i64 * 30);
                e
            })
            .collect();
        let ended_at = events.last().map(|e| e.timestamp).unwrap_or(base);
        Cluster {
            source: source.to_owned(),
            started_at: base,
            ended_at,
            events,
        }
    }

    #[test]
    fn open_creates_schema() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.cluster_count().unwrap(), 0);
    }

    #[test]
    fn insert_and_count() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();

        store.insert_cluster(&make_cluster("shell", 3)).unwrap();
        store.insert_cluster(&make_cluster("git", 2)).unwrap();
        assert_eq!(store.cluster_count().unwrap(), 2);
    }

    #[test]
    fn insert_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let cluster = make_cluster("shell", 2);

        store.insert_cluster(&cluster).unwrap();
        store.insert_cluster(&cluster).unwrap();
        assert_eq!(store.cluster_count().unwrap(), 1);
    }

    #[test]
    fn recent_clusters_returns_within_window() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();

        store.insert_cluster(&make_cluster("shell", 2)).unwrap();
        store.insert_cluster(&make_cluster("git", 1)).unwrap();

        let recent = store.recent_clusters(1.0, 10).unwrap();
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn recent_clusters_ordered_newest_first() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();

        let mut c1 = make_cluster("shell", 1);
        let c2 = make_cluster("git", 1);
        c1.started_at = Utc::now() - chrono::Duration::seconds(120);
        c1.ended_at = c1.started_at;
        c1.events[0].timestamp = c1.started_at;
        // c2 started_at is ~now (more recent)

        store.insert_cluster(&c1).unwrap();
        store.insert_cluster(&c2).unwrap();

        let recent = store.recent_clusters(1.0, 10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].source, "git");
        assert_eq!(recent[1].source, "shell");
    }

    #[test]
    fn event_ids_roundtrip() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let cluster = make_cluster("shell", 3);
        let expected_ids: Vec<String> =
            cluster.events.iter().map(|e| e.id.to_string()).collect();

        store.insert_cluster(&cluster).unwrap();
        let stored = store.recent_clusters(1.0, 10).unwrap();
        assert_eq!(stored[0].event_ids, expected_ids);
    }

    #[test]
    fn content_sample_truncated_at_200_chars() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("test.db")).unwrap();
        let long_content = "x".repeat(500);
        let base = Utc::now();
        let mut e = Event::new("shell", EventKind::Command, long_content);
        e.timestamp = base;
        let cluster = Cluster {
            source: "shell".into(),
            started_at: base,
            ended_at: base,
            events: vec![e],
        };
        store.insert_cluster(&cluster).unwrap();
        let stored = store.recent_clusters(1.0, 10).unwrap();
        assert_eq!(stored[0].content_sample.len(), 200);
    }
}
