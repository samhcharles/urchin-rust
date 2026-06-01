//! Apple Health export connector.
//!
//! Drop the export.xml from the Health app into:
//!   ~/.local/share/urchin/imports/apple-health/export.xml
//!
//! Parses <Record> and <Workout> elements using quick-xml event-based streaming
//! so large exports (100MB+) don't load into memory all at once.
//!
//! Checkpoint: ISO 8601 timestamp of the last endDate processed.

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::{fs, str};

use anyhow::Result;
use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader;

use urchin_core::{
    event::{Actor, Event, EventKind, EventMeta},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

pub struct AppleHealthOpts {
    pub export_path: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl AppleHealthOpts {
    pub fn defaults() -> Self {
        let base = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".local/share/urchin/imports/apple-health");
        Self {
            export_path: base.join("export.xml"),
            checkpoint_path: state_dir().join("apple-health.txt"),
        }
    }
}

pub fn collect(journal: &Journal, identity: &Identity, opts: &AppleHealthOpts) -> Result<usize> {
    if !opts.export_path.exists() {
        return Ok(0);
    }

    let last_ts = fs::read_to_string(&opts.checkpoint_path)
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let file = File::open(&opts.export_path)?;
    let reader = BufReader::new(file);
    let mut xml = Reader::from_reader(reader);
    xml.config_mut().trim_text(true);

    let mut count = 0;
    let mut newest_ts = last_ts.clone();
    let mut buf = Vec::new();

    loop {
        match xml.read_event_into(&mut buf)? {
            XmlEvent::Start(ref e) | XmlEvent::Empty(ref e) => {
                let tag = e.name();
                let tag_bytes = tag.as_ref();

                if tag_bytes == b"Record" {
                    if let Some((kind, content, ts, meta)) = parse_record(e) {
                        if ts > last_ts {
                            let mut event = Event::new("apple-health", kind, content);
                            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&ts) {
                                event.timestamp = parsed.with_timezone(&chrono::Utc);
                            }
                            event.meta = Some(meta);
                            event.actor = Some(Actor {
                                account: Some(identity.account.clone()),
                                device: Some(identity.device.clone()),
                                workspace: None,
                                node_id: Some(identity.node_id.clone()),
                            });
                            journal.append(&event)?;
                            count += 1;
                            if ts > newest_ts {
                                newest_ts = ts;
                            }
                        }
                    }
                } else if tag_bytes == b"Workout" {
                    if let Some((content, ts, meta)) = parse_workout(e) {
                        if ts > last_ts {
                            let mut event =
                                Event::new("apple-health", EventKind::HealthMetric, content);
                            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&ts) {
                                event.timestamp = parsed.with_timezone(&chrono::Utc);
                            }
                            event.meta = Some(meta);
                            event.actor = Some(Actor {
                                account: Some(identity.account.clone()),
                                device: Some(identity.device.clone()),
                                workspace: None,
                                node_id: Some(identity.node_id.clone()),
                            });
                            journal.append(&event)?;
                            count += 1;
                            if ts > newest_ts {
                                newest_ts = ts;
                            }
                        }
                    }
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    if count > 0 {
        if let Some(parent) = opts.checkpoint_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&opts.checkpoint_path, &newest_ts)?;
        journal.flush()?;
    }
    Ok(count)
}

fn attr(e: &quick_xml::events::BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| str::from_utf8(&a.value).ok().map(|s| s.to_string()))
}

fn normalize_ts(s: &str) -> String {
    // Apple exports dates as "2024-01-15 10:00:00 -0800". Convert to RFC3339.
    let parts: Vec<&str> = s.splitn(3, ' ').collect();
    if parts.len() == 3 {
        let offset = parts[2];
        let sign = if offset.starts_with('-') { '-' } else { '+' };
        let digits: String = offset.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() >= 4 {
            let hh = &digits[..2];
            let mm = &digits[2..4];
            return format!("{}T{}{}{}:{}", parts[0], parts[1], sign, hh, mm);
        }
    }
    s.to_string()
}

fn parse_record(
    e: &quick_xml::events::BytesStart<'_>,
) -> Option<(EventKind, String, String, EventMeta)> {
    let rec_type = attr(e, b"type")?;
    let end_date = attr(e, b"endDate").map(|s| normalize_ts(&s))?;
    let value_str = attr(e, b"value").unwrap_or_default();
    let unit_str = attr(e, b"unit");

    let value: Option<f64> = value_str.parse().ok();

    let (kind, content) = match rec_type.as_str() {
        "HKQuantityTypeIdentifierStepCount" => {
            (EventKind::HealthMetric, format!("steps: {}", value_str))
        }
        "HKQuantityTypeIdentifierHeartRate" => (
            EventKind::HealthMetric,
            format!("heart rate: {} bpm", value_str),
        ),
        "HKCategoryTypeIdentifierSleepAnalysis" => {
            (EventKind::HealthMetric, format!("sleep: {}", value_str))
        }
        "HKQuantityTypeIdentifierActiveEnergyBurned" => (
            EventKind::HealthMetric,
            format!("active calories: {} kcal", value_str),
        ),
        "HKQuantityTypeIdentifierDistanceWalkingRunning" => (
            EventKind::HealthMetric,
            format!(
                "distance: {} {}",
                value_str,
                unit_str.as_deref().unwrap_or("")
            ),
        ),
        _ => return None,
    };

    let meta = EventMeta {
        value,
        unit: unit_str,
        category: Some(rec_type),
        ..Default::default()
    };

    Some((kind, content, end_date, meta))
}

fn parse_workout(e: &quick_xml::events::BytesStart<'_>) -> Option<(String, String, EventMeta)> {
    let workout_type = attr(e, b"workoutActivityType")?;
    let end_date = attr(e, b"endDate").map(|s| normalize_ts(&s))?;
    let duration_str = attr(e, b"duration").unwrap_or_default();
    let duration_unit = attr(e, b"durationUnit").unwrap_or_else(|| "min".to_string());

    let label = workout_type
        .strip_prefix("HKWorkoutActivityType")
        .unwrap_or(&workout_type)
        .to_lowercase();

    let duration_secs: Option<u64> = duration_str.parse::<f64>().ok().map(|d| {
        if duration_unit.to_lowercase().starts_with("min") {
            (d * 60.0) as u64
        } else {
            d as u64
        }
    });

    let content = format!("workout: {} ({} {})", label, duration_str, duration_unit);
    let meta = EventMeta {
        category: Some(label),
        duration_secs,
        ..Default::default()
    };

    Some((content, end_date, meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use urchin_core::identity::Identity;
    use urchin_core::journal::Journal;

    fn setup(tmp: &TempDir) -> (Journal, Identity, AppleHealthOpts) {
        let journal = Journal::new(tmp.path().join("journal.jsonl"));
        let identity = Identity::for_test("test", "test");
        let opts = AppleHealthOpts {
            export_path: tmp.path().join("export.xml"),
            checkpoint_path: tmp.path().join("ckpt.txt"),
        };
        (journal, identity, opts)
    }

    const SAMPLE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<HealthData locale="en_US">
  <Record type="HKQuantityTypeIdentifierStepCount"
          sourceName="iPhone"
          value="8432"
          unit="count"
          startDate="2024-01-15 09:00:00 -0800"
          endDate="2024-01-15 10:00:00 -0800"/>
  <Record type="HKQuantityTypeIdentifierHeartRate"
          sourceName="Apple Watch"
          value="72"
          unit="count/min"
          startDate="2024-01-15 10:05:00 -0800"
          endDate="2024-01-15 10:05:10 -0800"/>
  <Workout workoutActivityType="HKWorkoutActivityTypeRunning"
           duration="30" durationUnit="min"
           startDate="2024-01-15 07:00:00 -0800"
           endDate="2024-01-15 07:30:00 -0800"/>
</HealthData>"#;

    #[test]
    fn no_export_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn parses_steps_hr_workout() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::write(&opts.export_path, SAMPLE_XML).unwrap();

        let n = collect(&j, &id, &opts).unwrap();
        assert_eq!(n, 3);

        let events = j.read_all().unwrap();
        assert!(events.iter().any(|e| e.content.contains("steps")));
        assert!(events.iter().any(|e| e.content.contains("heart rate")));
        assert!(events.iter().any(|e| e.content.contains("workout")));
    }

    #[test]
    fn checkpoint_prevents_duplicates() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::write(&opts.export_path, SAMPLE_XML).unwrap();

        assert_eq!(collect(&j, &id, &opts).unwrap(), 3);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn meta_fields_populated() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::write(&opts.export_path, SAMPLE_XML).unwrap();
        collect(&j, &id, &opts).unwrap();

        let events = j.read_all().unwrap();
        let step_event = events.iter().find(|e| e.content.contains("steps")).unwrap();
        let meta = step_event.meta.as_ref().unwrap();
        assert_eq!(meta.value, Some(8432.0));
    }
}
