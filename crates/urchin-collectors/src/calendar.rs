//! iCal connector.
//!
//! Drop any .ics files into ~/.local/share/urchin/imports/calendar/.
//! Parses VEVENT blocks: start time, end time, summary, and attendee count.
//!
//! Checkpoint: JSON set of seen UIDs (content-hash as fallback if UID absent).

use std::collections::HashSet;
use std::path::PathBuf;
use std::{collections::HashMap, fs};

use anyhow::Result;

use urchin_core::{
    event::{Actor, Event, EventKind, EventMeta},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

pub struct CalendarOpts {
    pub import_dir: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl CalendarOpts {
    pub fn defaults() -> Self {
        let import_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".local/share/urchin/imports/calendar");
        Self {
            import_dir,
            checkpoint_path: state_dir().join("calendar.json"),
        }
    }
}

pub fn collect(journal: &Journal, identity: &Identity, opts: &CalendarOpts) -> Result<usize> {
    if !opts.import_dir.exists() {
        return Ok(0);
    }

    let mut seen: HashSet<String> = fs::read_to_string(&opts.checkpoint_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut total = 0;

    let mut ics_files: Vec<PathBuf> = fs::read_dir(&opts.import_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ics"))
        .collect();
    ics_files.sort();

    for path in &ics_files {
        let raw = fs::read_to_string(path)?;
        let events = parse_ical(&raw);
        for vevent in events {
            let uid = vevent.get("UID").cloned().unwrap_or_else(|| {
                format!("{:x}", hash_str(vevent.get("SUMMARY").map_or("", |s| s)))
            });
            if seen.contains(&uid) {
                continue;
            }

            let summary = vevent.get("SUMMARY").cloned().unwrap_or_default();
            if summary.is_empty() {
                continue;
            }

            let dt_start = vevent.get("DTSTART").cloned().unwrap_or_default();
            let dt_end = vevent.get("DTEND").cloned().unwrap_or_default();
            let attendees: u32 = vevent
                .get("ATTENDEE_COUNT")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let duration_secs = duration_between(&dt_start, &dt_end);
            let ts = parse_ical_dt(&dt_start);

            let mut event = Event::new("calendar", EventKind::CalendarEvent, summary.clone());
            if let Some(t) = ts {
                event.timestamp = t;
            }
            event.title = Some(summary);
            event.meta = Some(EventMeta {
                duration_secs,
                attendees: if attendees > 0 { Some(attendees) } else { None },
                ..Default::default()
            });
            event.actor = Some(Actor {
                account: Some(identity.account.clone()),
                device: Some(identity.device.clone()),
                workspace: None,
                node_id: Some(identity.node_id.clone()),
            });
            journal.append(&event)?;
            seen.insert(uid);
            total += 1;
        }
    }

    if total > 0 {
        if let Some(parent) = opts.checkpoint_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&opts.checkpoint_path, serde_json::to_string(&seen)?)?;
        journal.flush()?;
    }
    Ok(total)
}

fn parse_ical(content: &str) -> Vec<HashMap<String, String>> {
    let mut events = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;
    let mut attendee_count = 0u32;

    let unfolded = unfold_ical(content);

    for line in unfolded.lines() {
        if line == "BEGIN:VEVENT" {
            current = Some(HashMap::new());
            attendee_count = 0;
        } else if line == "END:VEVENT" {
            if let Some(mut map) = current.take() {
                if attendee_count > 0 {
                    map.insert("ATTENDEE_COUNT".to_string(), attendee_count.to_string());
                }
                events.push(map);
            }
        } else if let Some(ref mut map) = current {
            if let Some((key, val)) = split_ical_line(line) {
                if key.starts_with("ATTENDEE") {
                    attendee_count += 1;
                } else {
                    map.insert(key, val);
                }
            }
        }
    }

    events
}

// iCal lines can be folded (continuation lines start with a space/tab).
fn unfold_ical(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    for line in content.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            out.push_str(line.trim_start());
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    out
}

fn split_ical_line(line: &str) -> Option<(String, String)> {
    // KEY;param=val:VALUE or KEY:VALUE
    let colon = line.find(':')?;
    let key_part = &line[..colon];
    let value = line[colon + 1..].to_string();
    // Strip parameters (e.g. DTSTART;TZID=America/New_York -> DTSTART)
    let key = key_part.split(';').next()?.trim().to_uppercase();
    Some((key, value))
}

fn parse_ical_dt(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};

    let s = s.trim();
    // UTC: 20240115T100000Z
    if s.ends_with('Z') && s.len() == 16 {
        let naive = NaiveDateTime::parse_from_str(&s[..15], "%Y%m%dT%H%M%S").ok()?;
        return Some(Utc.from_utc_datetime(&naive));
    }
    // Local: 20240115T100000 (treat as UTC for simplicity)
    if s.len() == 15 && s.contains('T') {
        let naive = NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%S").ok()?;
        return Some(Utc.from_utc_datetime(&naive));
    }
    // Date only: 20240115
    if s.len() == 8 {
        let d = NaiveDate::parse_from_str(s, "%Y%m%d").ok()?;
        return Some(d.and_hms_opt(0, 0, 0)?.and_utc());
    }
    None
}

fn duration_between(start: &str, end: &str) -> Option<u64> {
    let s = parse_ical_dt(start)?;
    let e = parse_ical_dt(end)?;
    let diff = (e - s).num_seconds();
    if diff > 0 {
        Some(diff as u64)
    } else {
        None
    }
}

fn hash_str(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use urchin_core::identity::Identity;
    use urchin_core::journal::Journal;

    fn setup(tmp: &TempDir) -> (Journal, Identity, CalendarOpts) {
        let journal = Journal::new(tmp.path().join("journal.jsonl"));
        let identity = Identity::for_test("test", "test");
        let opts = CalendarOpts {
            import_dir: tmp.path().join("calendar"),
            checkpoint_path: tmp.path().join("ckpt.json"),
        };
        (journal, identity, opts)
    }

    const SAMPLE_ICS: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:abc123@example.com\r\n\
SUMMARY:Team standup\r\n\
DTSTART:20240115T100000Z\r\n\
DTEND:20240115T103000Z\r\n\
ATTENDEE:mailto:alice@example.com\r\n\
ATTENDEE:mailto:bob@example.com\r\n\
END:VEVENT\r\n\
BEGIN:VEVENT\r\n\
UID:def456@example.com\r\n\
SUMMARY:Lunch\r\n\
DTSTART:20240115T120000Z\r\n\
DTEND:20240115T130000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn no_import_dir_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn parses_two_events() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::create_dir_all(&opts.import_dir).unwrap();
        fs::write(opts.import_dir.join("cal.ics"), SAMPLE_ICS).unwrap();

        assert_eq!(collect(&j, &id, &opts).unwrap(), 2);
        let events = j.read_all().unwrap();
        assert_eq!(events[0].kind, EventKind::CalendarEvent);
        assert_eq!(events[0].title.as_deref(), Some("Team standup"));
    }

    #[test]
    fn attendees_in_meta() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::create_dir_all(&opts.import_dir).unwrap();
        fs::write(opts.import_dir.join("cal.ics"), SAMPLE_ICS).unwrap();
        collect(&j, &id, &opts).unwrap();

        let events = j.read_all().unwrap();
        let standup = events
            .iter()
            .find(|e| e.content.contains("standup"))
            .unwrap();
        assert_eq!(standup.meta.as_ref().unwrap().attendees, Some(2));
    }

    #[test]
    fn duration_computed() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::create_dir_all(&opts.import_dir).unwrap();
        fs::write(opts.import_dir.join("cal.ics"), SAMPLE_ICS).unwrap();
        collect(&j, &id, &opts).unwrap();

        let events = j.read_all().unwrap();
        let standup = events
            .iter()
            .find(|e| e.content.contains("standup"))
            .unwrap();
        assert_eq!(standup.meta.as_ref().unwrap().duration_secs, Some(1800));
    }

    #[test]
    fn checkpoint_prevents_duplicates() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::create_dir_all(&opts.import_dir).unwrap();
        fs::write(opts.import_dir.join("cal.ics"), SAMPLE_ICS).unwrap();

        assert_eq!(collect(&j, &id, &opts).unwrap(), 2);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }
}
