//! Time/source clustering of filtered events into activity windows.
//!
//! A cluster is a contiguous run of events from the same source within a
//! configurable idle gap. Any break longer than `gap_secs` starts a new cluster.

use chrono::{DateTime, Utc};
use urchin_core::event::Event;

/// A group of temporally adjacent events from the same source.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub source: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub events: Vec<Event>,
}

impl Cluster {
    pub fn duration_secs(&self) -> i64 {
        (self.ended_at - self.started_at).num_seconds()
    }
}

/// Partition `events` (must be sorted oldest-first by timestamp) into clusters.
///
/// A new cluster begins when either:
/// - the source changes, or
/// - the gap between consecutive events exceeds `gap_secs`.
pub fn cluster_by_source_and_time(events: Vec<Event>, gap_secs: i64) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();

    for event in events {
        let append = clusters.last_mut().and_then(|c| {
            let gap = (event.timestamp - c.ended_at).num_seconds();
            if c.source == event.source && gap <= gap_secs {
                Some(c)
            } else {
                None
            }
        });

        match append {
            Some(c) => {
                c.ended_at = event.timestamp;
                c.events.push(event);
            }
            None => clusters.push(Cluster {
                source: event.source.clone(),
                started_at: event.timestamp,
                ended_at: event.timestamp,
                events: vec![event],
            }),
        }
    }

    clusters
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use urchin_core::event::EventKind;

    fn make_at(source: &str, offset_secs: i64) -> Event {
        let mut e = Event::new(source, EventKind::Command, format!("cmd at +{}s", offset_secs));
        e.timestamp = Utc::now() + Duration::seconds(offset_secs);
        e
    }

    #[test]
    fn single_event_is_one_cluster() {
        let events = vec![make_at("shell", 0)];
        let clusters = cluster_by_source_and_time(events, 300);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].events.len(), 1);
    }

    #[test]
    fn same_source_within_gap_merges() {
        let events = vec![make_at("shell", 0), make_at("shell", 60), make_at("shell", 120)];
        let clusters = cluster_by_source_and_time(events, 300);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].events.len(), 3);
    }

    #[test]
    fn gap_exceeds_threshold_splits() {
        let events = vec![make_at("shell", 0), make_at("shell", 600)];
        let clusters = cluster_by_source_and_time(events, 300);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn source_change_splits() {
        let events = vec![make_at("shell", 0), make_at("git", 1), make_at("shell", 2)];
        let clusters = cluster_by_source_and_time(events, 300);
        assert_eq!(clusters.len(), 3);
    }

    #[test]
    fn cluster_duration_is_correct() {
        let events = vec![make_at("shell", 0), make_at("shell", 90)];
        let clusters = cluster_by_source_and_time(events, 300);
        assert_eq!(clusters[0].duration_secs(), 90);
    }
}
