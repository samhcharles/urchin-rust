use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Fixed namespace UUID for UUIDv5 deterministic event IDs.
pub const URCHIN_EVENT_NS: Uuid = Uuid::from_bytes([
    0x5a, 0x7b, 0x3c, 0x9d, 0xe1, 0x2f, 0x4b, 0x56,
    0xb8, 0x12, 0xca, 0xfe, 0xba, 0xbe, 0xde, 0xad,
]);

/// The canonical unit of memory in Urchin.
/// Every collector, intake path, and tool produces Events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub source: String,
    pub kind: EventKind,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<Actor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<EventMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    // Dev / AI activity
    Conversation,
    Agent,
    Command,
    Commit,
    File,
    Decision,
    // Personal data
    Purchase,
    Location,
    HealthMetric,
    CalendarEvent,
    SearchQuery,
    WatchHistory,
    Other(String),
}

/// Structured fields for personal data event kinds.
/// All fields are optional; populate only what the source provides.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventMeta {
    // Purchase
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merchant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    // Location
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lng: Option<f64>,
    // Health
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    // Calendar / health duration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    // Calendar
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attendees: Option<u32>,
}

/// Identity envelope — who produced this event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Hex-encoded Ed25519 public key of the originating node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

impl Event {
    pub fn new(source: impl Into<String>, kind: EventKind, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            source: source.into(),
            kind,
            content: content.into(),
            brain: None,
            workspace: None,
            session: None,
            title: None,
            tags: vec![],
            actor: None,
            meta: None,
        }
    }

    /// Produce a deterministic UUIDv5 from an event's core fields.
    ///
    /// Two events with identical source, kind, content, workspace, and
    /// minute-granularity timestamp share the same ID, making repeated
    /// ingestion of the same observation a silent no-op.
    pub fn deterministic_id(
        source: &str,
        kind: &EventKind,
        content: &str,
        workspace: Option<&str>,
        timestamp: &DateTime<Utc>,
    ) -> Uuid {
        let minute = timestamp.timestamp() / 60;
        let kind_str = serde_json::to_string(kind).unwrap_or_default();
        let key = format!(
            "{}\0{}\0{}\0{}\0{}",
            source,
            kind_str,
            content,
            workspace.unwrap_or(""),
            minute
        );
        Uuid::new_v5(&URCHIN_EVENT_NS, key.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic_event() {
        let event = Event::new("cli", EventKind::Conversation, "hello world");
        let json = serde_json::to_string(&event).unwrap();
        let decoded: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, event.id);
        assert_eq!(decoded.source, "cli");
        assert_eq!(decoded.content, "hello world");
        assert_eq!(decoded.kind, EventKind::Conversation);
    }

    #[test]
    fn no_nulls_in_output() {
        let event = Event::new("cli", EventKind::Agent, "test");
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("null"), "nulls should be omitted: {}", json);
        assert!(
            !json.contains("\"tags\":[]"),
            "empty tags should be omitted: {}",
            json
        );
    }

    #[test]
    fn personal_data_kinds_roundtrip() {
        for kind in [
            EventKind::Purchase,
            EventKind::Location,
            EventKind::HealthMetric,
            EventKind::CalendarEvent,
            EventKind::SearchQuery,
            EventKind::WatchHistory,
        ] {
            let event = Event::new("test", kind.clone(), "payload");
            let json = serde_json::to_string(&event).unwrap();
            let decoded: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.kind, kind);
            assert!(decoded.meta.is_none());
        }
    }

    #[test]
    fn event_meta_no_nulls() {
        let mut event = Event::new("bank", EventKind::Purchase, "Coffee");
        event.meta = Some(EventMeta {
            amount: Some(4.50),
            currency: Some("USD".into()),
            merchant: Some("Blue Bottle".into()),
            ..Default::default()
        });
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("null"), "nulls should be omitted: {}", json);
        assert!(json.contains("\"amount\":4.5"));
        assert!(json.contains("\"merchant\":\"Blue Bottle\""));
        assert!(
            !json.contains("lat"),
            "unset location fields should be absent"
        );
    }

    #[test]
    fn deserialize_with_unknown_fields() {
        // Node.js spike events have extra fields that should be silently dropped
        let raw = r#"{"id":"56816532-adb7-4000-8a0f-1dda8408aab5","kind":"conversation","source":"copilot","timestamp":"2026-04-22T14:03:40.032Z","summary":"ignored","content":"hello","tags":["copilot"],"metadata":{},"provenance":{},"identity":{}}"#;
        let event: Event = serde_json::from_str(raw).unwrap();
        assert_eq!(event.source, "copilot");
        assert_eq!(event.content, "hello");
        assert_eq!(event.kind, EventKind::Conversation);
        assert_eq!(event.tags, vec!["copilot"]);
        assert!(event.actor.is_none());
    }
}
