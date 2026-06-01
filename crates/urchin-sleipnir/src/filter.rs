//! Noise and typo filtering for raw journal events.
//! Drops events that carry no signal: blank content, internal heartbeats,
//! duplicate bursts, and content below a configurable length floor.

use urchin_core::event::Event;

/// Returns true if `event` should be kept for clustering.
pub fn is_signal(event: &Event) -> bool {
    let content = event.content.trim();
    if content.is_empty() {
        return false;
    }
    // Sub-4-char content is almost always a typo or a stub flush.
    if content.len() < 4 {
        return false;
    }
    // Urchin internal heartbeat events are bookkeeping, not activity.
    if event.source == "sleipnir" || event.source == "urchin-intake-health" {
        return false;
    }
    true
}

/// Deduplicate a pre-sorted (by timestamp) slice in-place, removing events
/// whose (source, kind, content) triple is identical to the immediately
/// preceding event. This collapses burst retries without touching real runs.
pub fn dedup_consecutive(events: &mut Vec<Event>) {
    events.dedup_by(|b, a| {
        // dedup_by compares consecutive pairs; `a` is the element already kept.
        a.source == b.source
            && serde_json::to_string(&a.kind).ok() == serde_json::to_string(&b.kind).ok()
            && a.content == b.content
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use urchin_core::event::EventKind;

    fn make(source: &str, content: &str) -> Event {
        Event::new(source, EventKind::Command, content)
    }

    #[test]
    fn blank_content_dropped() {
        assert!(!is_signal(&make("shell", "")));
        assert!(!is_signal(&make("shell", "   ")));
    }

    #[test]
    fn short_content_dropped() {
        assert!(!is_signal(&make("shell", "ls")));
        assert!(is_signal(&make("shell", "ls -l")));
    }

    #[test]
    fn heartbeat_source_dropped() {
        assert!(!is_signal(&make("sleipnir", "heartbeat")));
        assert!(!is_signal(&make("urchin-intake-health", "ok")));
    }

    #[test]
    fn normal_events_kept() {
        assert!(is_signal(&make("git", "cargo check passed")));
        assert!(is_signal(&make("shell", "cargo build --release")));
    }

    #[test]
    fn dedup_removes_consecutive_identical() {
        let mut events = vec![
            make("shell", "cargo build"),
            make("shell", "cargo build"),
            make("shell", "cargo test"),
        ];
        dedup_consecutive(&mut events);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content, "cargo build");
        assert_eq!(events[1].content, "cargo test");
    }

    #[test]
    fn dedup_keeps_non_consecutive_identical() {
        let mut events = vec![
            make("shell", "cargo build"),
            make("shell", "cargo test"),
            make("shell", "cargo build"),
        ];
        dedup_consecutive(&mut events);
        assert_eq!(events.len(), 3);
    }
}
