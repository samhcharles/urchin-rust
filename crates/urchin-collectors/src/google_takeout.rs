//! Google Takeout import connector.
//!
//! Drop Takeout export contents into ~/.local/share/urchin/imports/google-takeout/:
//!   Location History/Records.json       -> EventKind::Location
//!   My Activity/Search/MyActivity.json  -> EventKind::SearchQuery
//!   My Activity/YouTube/MyActivity.json -> EventKind::WatchHistory
//!
//! Checkpoint: JSON { "location_last_ts": "<ISO>", "search_seen": [...], "youtube_seen": [...] }

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use urchin_core::{
    event::{Actor, Event, EventKind, EventMeta},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

pub struct GoogleTakeoutOpts {
    pub import_dir: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl GoogleTakeoutOpts {
    pub fn defaults() -> Self {
        let import_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".local/share/urchin/imports/google-takeout");
        Self {
            import_dir,
            checkpoint_path: state_dir().join("google-takeout.json"),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Checkpoint {
    #[serde(default)]
    location_last_ts: Option<String>,
    #[serde(default)]
    search_seen: HashSet<String>,
    #[serde(default)]
    youtube_seen: HashSet<String>,
}

// ── Location History ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct LocationRecord {
    timestamp: String,
    #[serde(rename = "latitudeE7")]
    latitude_e7: Option<i64>,
    #[serde(rename = "longitudeE7")]
    longitude_e7: Option<i64>,
}

#[derive(Deserialize)]
struct LocationHistory {
    locations: Vec<LocationRecord>,
}

// ── Activity (Search / YouTube) ───────────────────────────────────────────────

#[derive(Deserialize)]
struct ActivityEntry {
    time: Option<String>,
    title: Option<String>,
}

pub fn collect(journal: &Journal, identity: &Identity, opts: &GoogleTakeoutOpts) -> Result<usize> {
    if !opts.import_dir.exists() {
        return Ok(0);
    }

    let mut ckpt = load_checkpoint(&opts.checkpoint_path);
    let mut count = 0;

    count += ingest_locations(journal, identity, opts, &mut ckpt)?;
    count += ingest_activity(
        journal,
        identity,
        opts,
        "My Activity/Search/MyActivity.json",
        EventKind::SearchQuery,
        &mut ckpt.search_seen,
    )?;
    count += ingest_activity(
        journal,
        identity,
        opts,
        "My Activity/YouTube/MyActivity.json",
        EventKind::WatchHistory,
        &mut ckpt.youtube_seen,
    )?;

    if count > 0 {
        save_checkpoint(&opts.checkpoint_path, &ckpt)?;
        journal.flush()?;
    }
    Ok(count)
}

fn ingest_locations(
    journal: &Journal,
    identity: &Identity,
    opts: &GoogleTakeoutOpts,
    ckpt: &mut Checkpoint,
) -> Result<usize> {
    let path = opts.import_dir.join("Location History/Records.json");
    if !path.exists() {
        return Ok(0);
    }

    let raw = fs::read_to_string(&path)?;
    let history: LocationHistory = serde_json::from_str(&raw)?;

    let last_ts = ckpt.location_last_ts.clone().unwrap_or_default();
    let mut count = 0;
    let mut newest_ts = last_ts.clone();

    for rec in &history.locations {
        if rec.timestamp <= last_ts {
            continue;
        }
        let lat = rec.latitude_e7.map(|v| v as f64 / 1e7);
        let lng = rec.longitude_e7.map(|v| v as f64 / 1e7);

        let content = match (lat, lng) {
            (Some(lat), Some(lng)) => format!("{:.6},{:.6}", lat, lng),
            _ => rec.timestamp.clone(),
        };

        let ts = parse_ts(&rec.timestamp);
        let mut event = Event::new("google-takeout", EventKind::Location, content);
        if let Some(t) = ts {
            event.timestamp = t;
        }
        event.meta = Some(EventMeta {
            lat,
            lng,
            ..Default::default()
        });
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: None,
            node_id: Some(identity.node_id.clone()),
        });
        journal.append(&event)?;
        count += 1;

        if rec.timestamp > newest_ts {
            newest_ts = rec.timestamp.clone();
        }
    }

    if !newest_ts.is_empty() {
        ckpt.location_last_ts = Some(newest_ts);
    }
    Ok(count)
}

fn ingest_activity(
    journal: &Journal,
    identity: &Identity,
    opts: &GoogleTakeoutOpts,
    rel_path: &str,
    kind: EventKind,
    seen: &mut HashSet<String>,
) -> Result<usize> {
    let path = opts.import_dir.join(rel_path);
    if !path.exists() {
        return Ok(0);
    }

    let raw = fs::read_to_string(&path)?;
    let entries: Vec<ActivityEntry> = serde_json::from_str(&raw)?;

    let mut count = 0;
    for entry in &entries {
        let title = entry.title.clone().unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let time_str = entry.time.clone().unwrap_or_default();
        let key = format!("{}|{}", time_str, title);
        if seen.contains(&key) {
            continue;
        }

        let ts = parse_ts(&time_str);
        let mut event = Event::new("google-takeout", kind.clone(), title);
        if let Some(t) = ts {
            event.timestamp = t;
        }
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: None,
            node_id: Some(identity.node_id.clone()),
        });
        journal.append(&event)?;
        seen.insert(key);
        count += 1;
    }
    Ok(count)
}

fn parse_ts(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
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
    fs::write(path, serde_json::to_string(ckpt)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use urchin_core::journal::Journal;

    fn setup(tmp: &TempDir) -> (Journal, Identity, GoogleTakeoutOpts) {
        let journal = Journal::new(tmp.path().join("journal.jsonl"));
        let identity = Identity::for_test("test", "test");
        let opts = GoogleTakeoutOpts {
            import_dir: tmp.path().join("google-takeout"),
            checkpoint_path: tmp.path().join("ckpt.json"),
        };
        (journal, identity, opts)
    }

    fn write(tmp: &TempDir, rel: &str, content: &str) {
        let path = tmp.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn no_import_dir_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn location_records_ingested() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);

        write(
            &tmp,
            "google-takeout/Location History/Records.json",
            r#"{
            "locations": [
                {"timestamp": "2024-01-15T10:00:00Z", "latitudeE7": 477654321, "longitudeE7": -1224567890},
                {"timestamp": "2024-01-16T10:00:00Z", "latitudeE7": 477654322, "longitudeE7": -1224567891}
            ]
        }"#,
        );

        assert_eq!(collect(&j, &id, &opts).unwrap(), 2);
        let events = j.read_all().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, EventKind::Location);
        let meta = events[0].meta.as_ref().unwrap();
        assert!(meta.lat.is_some());
        assert!(meta.lng.is_some());
    }

    #[test]
    fn location_checkpoint_skips_seen() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);

        write(
            &tmp,
            "google-takeout/Location History/Records.json",
            r#"{
            "locations": [
                {"timestamp": "2024-01-15T10:00:00Z", "latitudeE7": 100000000, "longitudeE7": 200000000}
            ]
        }"#,
        );

        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn search_activity_ingested() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);

        write(
            &tmp,
            "google-takeout/My Activity/Search/MyActivity.json",
            r#"[
            {"time": "2024-01-15T10:00:00Z", "title": "Searched for rust"},
            {"time": "2024-01-15T11:00:00Z", "title": "Searched for urchin"}
        ]"#,
        );

        assert_eq!(collect(&j, &id, &opts).unwrap(), 2);
        let events = j.read_all().unwrap();
        assert!(events.iter().any(|e| e.kind == EventKind::SearchQuery));
    }
}
