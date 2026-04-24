use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use uuid::Uuid;

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
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<Actor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Conversation,
    Agent,
    Command,
    Commit,
    File,
    Other(String),
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
}

impl Event {
    pub fn new(source: impl Into<String>, kind: EventKind, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            timestamp: Utc::now(),
            source: source.into(),
            kind,
            content: content.into(),
            workspace: None,
            session: None,
            title: None,
            tags: vec![],
            actor: None,
        }
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
        assert!(!json.contains("\"tags\":[]"), "empty tags should be omitted: {}", json);
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
