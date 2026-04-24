use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use uuid::Uuid;

/// A single captured activity event. This is the canonical unit of memory in Urchin.
/// Every collector, every tool, every intake path produces Events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub source: String,      // "claude" | "copilot" | "gemini" | "shell" | "git" | ...
    pub kind: EventKind,
    pub content: String,
    pub workspace: Option<String>,
    pub session: Option<String>,
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub actor: Option<Actor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    Conversation,
    Agent,
    Command,
    Commit,
    File,
    Other(String),
}

/// Who produced this event — the identity envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub account: Option<String>,
    pub device: Option<String>,
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
