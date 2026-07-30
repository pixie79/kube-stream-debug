//! Time-in-state tracking.
//!
//! A topic's "state" for this purpose is the pair (status, trend) — e.g.
//! `BACKLOG/growing` is a different state from `BACKLOG/draining`. Each run
//! records, per topic, the timestamp at which its current state was first
//! observed (`state_since`); the table shows `now - state_since` as a human
//! duration, letting a 2-hour standing incident be told apart from a 30-second
//! spike.
//!
//! History comes from two places, preferred in this order:
//!   1. The most recent JSON snapshot in `--json-dir` (survives restarts, works
//!      for single runs).
//!   2. The previous cycle held in memory during a `--watch` session.
//!
//! Resolution is snapshot/cycle-grained: this measures "time since we last
//! observed a different state", not continuous time. A state that flips and
//! flips back between observations is not seen.

use std::collections::HashMap;

use serde::Deserialize;

use crate::health::TopicHealth;

/// A compact, comparable description of a topic's current state.
/// Two topics with the same status and trend share a state key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateKey(String);

impl StateKey {
    /// Derive the state key from a health record: status plus the drain trend
    /// when present (e.g. `backlog|growing`, `ok|empty`, `ok`).
    ///
    /// The status is taken via its serde representation (lowercase snake_case)
    /// so it matches exactly what a snapshot stores — the two must agree for
    /// state-change detection to work across runs.
    pub fn of(health: &TopicHealth) -> Self {
        let status = serialized_status(health.status);
        match &health.drain {
            Some(d) => StateKey(format!("{status}|{}", d.trend.label())),
            None => StateKey(status),
        }
    }
}

/// The serde/JSON representation of a status (matches what snapshots store).
fn serialized_status(status: crate::health::Status) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| status.label().to_string())
}

/// Per-topic prior observation: the state it was in and when that state began.
#[derive(Debug, Clone)]
pub struct PriorState {
    pub key: StateKey,
    pub since: String,
}

/// The subset of a snapshot we need to reconstruct prior state. Mirrors the
/// snapshot document shape (`{ "as_of", "topics": [...] }`) but only the fields
/// that matter here; everything else is ignored on deserialize.
#[derive(Debug, Deserialize)]
struct SnapshotDoc {
    #[serde(default)]
    topics: Vec<SnapshotTopic>,
}

#[derive(Debug, Deserialize)]
struct SnapshotTopic {
    topic: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    drain: Option<SnapshotDrain>,
    #[serde(default)]
    state_since: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SnapshotDrain {
    #[serde(default)]
    trend: Option<String>,
}

impl SnapshotTopic {
    /// Rebuild the state key exactly as `StateKey::of` would have, from the
    /// serialized status + trend, so comparison is apples-to-apples.
    fn key(&self) -> Option<StateKey> {
        let status = self.status.as_ref()?;
        let key = match self.drain.as_ref().and_then(|d| d.trend.as_ref()) {
            Some(trend) => format!("{status}|{trend}"),
            None => status.clone(),
        };
        Some(StateKey(key))
    }
}

/// Build a prior-state map from a snapshot document's raw JSON.
pub fn prior_from_snapshot_json(json: &str) -> HashMap<String, PriorState> {
    let doc: SnapshotDoc = match serde_json::from_str(json) {
        Ok(d) => d,
        Err(_) => return HashMap::new(),
    };
    let mut out = HashMap::new();
    for topic in doc.topics {
        // Only useful if we can both identify the state and know when it began.
        if let (Some(key), Some(since)) = (topic.key(), topic.state_since.clone()) {
            out.insert(topic.topic.clone(), PriorState { key, since });
        } else if let Some(key) = topic.key() {
            // Snapshot predates state tracking (no state_since): treat the
            // snapshot's own time as the state start if the caller supplies it
            // later; for now record the key with an empty since so a state
            // *change* is still detected.
            out.insert(
                topic.topic,
                PriorState {
                    key,
                    since: String::new(),
                },
            );
        }
    }
    out
}

/// In-memory prior state built from the previous cycle's results (watch mode).
pub fn prior_from_results(results: &[TopicHealth]) -> HashMap<String, PriorState> {
    results
        .iter()
        .filter_map(|h| {
            h.state_since.clone().map(|since| {
                (
                    h.topic.clone(),
                    PriorState {
                        key: StateKey::of(h),
                        since,
                    },
                )
            })
        })
        .collect()
}

/// Assign `state_since` to each topic in `results`, given the prior state map
/// and the current run timestamp.
///
/// - Unchanged state → inherit the prior `since` (state has persisted).
/// - Changed state, or no usable prior → `since = now` (state just began, as
///   far as we can observe).
pub fn assign_state_since(
    results: &mut [TopicHealth],
    prior: &HashMap<String, PriorState>,
    now: &str,
) {
    for health in results.iter_mut() {
        let current = StateKey::of(health);
        let since = match prior.get(&health.topic) {
            Some(p) if p.key == current && !p.since.is_empty() => p.since.clone(),
            _ => now.to_string(),
        };
        health.state_since = Some(since);
    }
}

/// Format the duration between an RFC3339 `since` and `now` as a compact human
/// string: `45s`, `15m`, `2h`, `3d`. Returns `None` if either timestamp can't
/// be parsed or `since` is after `now`.
pub fn format_duration_between(since: &str, now: &str) -> Option<String> {
    let a = crate::timestamp::parse_rfc3339(since)?;
    let b = crate::timestamp::parse_rfc3339(now)?;
    if b < a {
        return None;
    }
    Some(format_secs((b - a) as u64))
}

fn format_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drain::{DrainStats, Trend};
    use crate::health::Status;

    fn health(topic: &str, status: Status, trend: Option<Trend>) -> TopicHealth {
        TopicHealth {
            topic: topic.to_string(),
            status,
            total_backlog: 0,
            backlog_bytes: 0,
            consumers: 1,
            unacked_messages: 0,
            msg_rate_out: 0.0,
            min_headroom: None,
            trimmed_partitions: Vec::new(),
            at_edge_partitions: Vec::new(),
            hot_partitions: Vec::new(),
            partition_gaps: Vec::new(),
            drain: trend.map(|t| DrainStats {
                trend: t,
                delta: 0,
                net_per_sec: 0.0,
                eta_secs: None,
            }),
            state_since: None,
            error: None,
        }
    }

    #[test]
    fn state_key_includes_trend() {
        let a = StateKey::of(&health("t", Status::Backlog, Some(Trend::Growing)));
        let b = StateKey::of(&health("t", Status::Backlog, Some(Trend::Draining)));
        let c = StateKey::of(&health("t", Status::Backlog, Some(Trend::Growing)));
        assert_ne!(a, b, "growing vs draining are different states");
        assert_eq!(a, c);
    }

    #[test]
    fn unchanged_state_inherits_since() {
        let mut results = vec![health("t", Status::Backlog, Some(Trend::Growing))];
        let mut prior = HashMap::new();
        prior.insert(
            "t".to_string(),
            PriorState {
                key: StateKey::of(&results[0]),
                since: "2026-07-30T10:00:00Z".to_string(),
            },
        );
        assign_state_since(&mut results, &prior, "2026-07-30T12:00:00Z");
        assert_eq!(results[0].state_since.as_deref(), Some("2026-07-30T10:00:00Z"));
    }

    #[test]
    fn changed_state_resets_since_to_now() {
        let mut results = vec![health("t", Status::Backlog, Some(Trend::Draining))];
        let mut prior = HashMap::new();
        prior.insert(
            "t".to_string(),
            PriorState {
                // Was growing, now draining — a state change.
                key: StateKey::of(&health("t", Status::Backlog, Some(Trend::Growing))),
                since: "2026-07-30T10:00:00Z".to_string(),
            },
        );
        assign_state_since(&mut results, &prior, "2026-07-30T12:00:00Z");
        assert_eq!(results[0].state_since.as_deref(), Some("2026-07-30T12:00:00Z"));
    }

    #[test]
    fn no_prior_sets_since_to_now() {
        let mut results = vec![health("t", Status::Ok, None)];
        assign_state_since(&mut results, &HashMap::new(), "2026-07-30T12:00:00Z");
        assert_eq!(results[0].state_since.as_deref(), Some("2026-07-30T12:00:00Z"));
    }

    #[test]
    fn reads_prior_from_snapshot_json() {
        let json = r#"{
            "as_of": "2026-07-30T10:00:00Z",
            "topics": [
                {"topic": "a", "status": "backlog",
                 "drain": {"trend": "growing"}, "state_since": "2026-07-30T09:00:00Z"},
                {"topic": "b", "status": "ok", "state_since": "2026-07-30T08:00:00Z"}
            ]
        }"#;
        let prior = prior_from_snapshot_json(json);
        assert_eq!(prior["a"].key, StateKey("backlog|growing".to_string()));
        assert_eq!(prior["a"].since, "2026-07-30T09:00:00Z");
        assert_eq!(prior["b"].key, StateKey("ok".to_string()));
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(
            format_duration_between("2026-07-30T12:00:00Z", "2026-07-30T12:00:45Z").as_deref(),
            Some("45s")
        );
        assert_eq!(
            format_duration_between("2026-07-30T12:00:00Z", "2026-07-30T12:15:00Z").as_deref(),
            Some("15m")
        );
        assert_eq!(
            format_duration_between("2026-07-30T10:00:00Z", "2026-07-30T12:00:00Z").as_deref(),
            Some("2h")
        );
        assert_eq!(
            format_duration_between("2026-07-28T12:00:00Z", "2026-07-30T12:00:00Z").as_deref(),
            Some("2d")
        );
        // since after now → None
        assert!(format_duration_between("2026-07-30T13:00:00Z", "2026-07-30T12:00:00Z").is_none());
    }
}

