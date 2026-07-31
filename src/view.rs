//! View-state and selection logic for the interactive TUI.
//!
//! This module is deliberately pure: it decides *what* to show given the
//! current view, filter, and selection, with no terminal I/O. The ratatui event
//! loop (in `tui`, compiled only with the `tui` feature) is a thin shell that
//! mutates a `ViewState` on keypresses and asks this module for the rows to
//! draw. Keeping the decision logic here means it's fully unit-tested even
//! though the terminal loop can't be.
//!
//! The logic is consumed by the feature-gated `tui` module; without that
//! feature the binary doesn't call it, so suppress dead-code warnings there.
#![cfg_attr(not(feature = "tui"), allow(dead_code))]

use crate::health::TopicHealth;

/// Which panel the user is looking at. Cycled with a keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// One row per topic (the classic table).
    Topic,
    /// One row per partition, flattened across all topics.
    Partition,
    /// Kubernetes consumer-pod health.
    Kube,
    /// Split: topics on top, partitions (of the selected topic) below.
    Combined,
}

impl View {
    /// Cycle order for the toggle key.
    pub fn next(self) -> View {
        match self {
            View::Topic => View::Partition,
            View::Partition => View::Kube,
            View::Kube => View::Combined,
            View::Combined => View::Topic,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Topic => "topic",
            View::Partition => "partition",
            View::Kube => "kube",
            View::Combined => "combined",
        }
    }
}

/// How a filter/selection affects visible rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectMode {
    /// Show every row; the matching one is emphasised.
    Highlight,
    /// Show only matching rows; hide the rest.
    Filter,
}

/// Mutable UI state driven by keypresses.
#[derive(Debug, Clone)]
pub struct ViewState {
    pub view: View,
    /// Free-text filter/select query (matches topic or partition name).
    pub query: String,
    pub mode: SelectMode,
    /// Whether the query box is currently accepting input.
    pub editing_query: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            view: View::Topic,
            query: String::new(),
            mode: SelectMode::Highlight,
            editing_query: false,
        }
    }
}

impl ViewState {
    pub fn cycle_view(&mut self) {
        self.view = self.view.next();
    }

    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            SelectMode::Highlight => SelectMode::Filter,
            SelectMode::Filter => SelectMode::Highlight,
        };
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
    }
}

/// A flattened partition row for the partition view.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionRow {
    pub topic: String,
    pub partition: String,
    pub index: u32,
    pub backlog: i64,
    pub backlog_bytes: i64,
    pub consumers: usize,
    pub unacked_messages: i64,
    pub status: &'static str,
}

/// Flatten all topics' partitions into a single ordered row set, sorted by
/// topic then partition index. Non-partitioned topics contribute no rows here.
pub fn partition_rows(topics: &[TopicHealth]) -> Vec<PartitionRow> {
    let mut rows: Vec<PartitionRow> = Vec::new();
    for topic in topics {
        for p in &topic.partitions {
            rows.push(PartitionRow {
                topic: topic.topic.clone(),
                partition: p.partition.clone(),
                index: p.index,
                backlog: p.backlog,
                backlog_bytes: p.backlog_bytes,
                consumers: p.consumers,
                unacked_messages: p.unacked_messages,
                status: p.status,
            });
        }
    }
    rows.sort_by(|a, b| a.topic.cmp(&b.topic).then(a.index.cmp(&b.index)));
    rows
}

/// Does a name match the query? Case-insensitive substring; empty query matches
/// everything. Matches against the full partition name and its short `pN` form,
/// so `p3` matches `…-partition-3`.
pub fn matches(query: &str, name: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let n = name.to_lowercase();
    if n.contains(&q) {
        return true;
    }
    // Allow `pN` to match `-partition-N`.
    if let Some(idx) = short_partition_index(name) {
        if q == format!("p{idx}") || q == idx.to_string() {
            return true;
        }
    }
    false
}

fn short_partition_index(name: &str) -> Option<u32> {
    name.rsplit_once("-partition-")
        .and_then(|(_, n)| n.parse().ok())
}

/// Binary byte sizes (GiB/MiB/KiB), matching the table renderer. Zero → "—".
/// Delegates to the canonical implementation so both share one format.
pub fn fmt_bytes(bytes: i64) -> String {
    crate::output::format_bytes(bytes)
}

/// Apply the current mode to a set of topics: in Filter mode, keep only those
/// matching the query; in Highlight mode, keep all. Returns indices into the
/// input so the caller can mark which rows are highlighted.
pub fn visible_topic_indices(state: &ViewState, topics: &[TopicHealth]) -> Vec<usize> {
    topics
        .iter()
        .enumerate()
        .filter(|(_, t)| match state.mode {
            SelectMode::Filter => matches(&state.query, &t.topic),
            SelectMode::Highlight => true,
        })
        .map(|(i, _)| i)
        .collect()
}

/// Same for flattened partition rows: filter matches against either the parent
/// topic name or the partition name.
pub fn visible_partition_indices(state: &ViewState, rows: &[PartitionRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| match state.mode {
            SelectMode::Filter => matches(&state.query, &r.partition) || matches(&state.query, &r.topic),
            SelectMode::Highlight => true,
        })
        .map(|(i, _)| i)
        .collect()
}

/// Whether a given name should be emphasised (Highlight mode with a non-empty
/// query that matches).
pub fn is_highlighted(state: &ViewState, name: &str) -> bool {
    state.mode == SelectMode::Highlight && !state.query.is_empty() && matches(&state.query, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{PartitionDetail, Status, TopicHealth};

    fn topic_with_partitions(name: &str, parts: &[(u32, i64, &'static str)]) -> TopicHealth {
        let partitions = parts
            .iter()
            .map(|(idx, backlog, status)| PartitionDetail {
                partition: format!("{name}-partition-{idx}"),
                index: *idx,
                backlog: *backlog,
                backlog_bytes: backlog * 1000,
                consumers: if *status == "no_consumers" { 0 } else { 3 },
                unacked_messages: 0,
                status,
            })
            .collect();
        TopicHealth {
            topic: name.to_string(),
            status: Status::Backlog,
            total_backlog: parts.iter().map(|(_, b, _)| b).sum(),
            backlog_bytes: 0,
            consumers: 3,
            unacked_messages: 0,
            msg_rate_out: 0.0,
            min_headroom: None,
            trimmed_partitions: Vec::new(),
            at_edge_partitions: Vec::new(),
            hot_partitions: Vec::new(),
            partition_gaps: Vec::new(),
            partitions,
            drain: None,
            state_since: None,
            kube_hint: None,
            error: None,
        }
    }

    #[test]
    fn view_cycles_through_four() {
        let mut v = View::Topic;
        v = v.next();
        assert_eq!(v, View::Partition);
        v = v.next();
        assert_eq!(v, View::Kube);
        v = v.next();
        assert_eq!(v, View::Combined);
        v = v.next();
        assert_eq!(v, View::Topic);
    }

    #[test]
    fn partition_rows_flattened_and_sorted() {
        let topics = vec![
            topic_with_partitions("b-topic", &[(1, 10, "ok"), (0, 20, "hot")]),
            topic_with_partitions("a-topic", &[(0, 5, "ok")]),
        ];
        let rows = partition_rows(&topics);
        // Sorted by topic then index: a-topic/0, b-topic/0, b-topic/1.
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].topic, "a-topic");
        assert_eq!(rows[1].partition, "b-topic-partition-0");
        assert_eq!(rows[2].partition, "b-topic-partition-1");
    }

    #[test]
    fn matches_handles_short_partition_form() {
        assert!(matches("p3", "foo-partition-3"));
        assert!(matches("3", "foo-partition-3"));
        assert!(matches("foo", "foo-partition-3"));
        assert!(!matches("p4", "foo-partition-3"));
        assert!(matches("", "anything"), "empty query matches all");
    }

    #[test]
    fn filter_mode_hides_nonmatching() {
        let topics = vec![
            topic_with_partitions("soccer", &[(0, 5, "ok")]),
            topic_with_partitions("tennis", &[(0, 5, "ok")]),
        ];
        let mut state = ViewState {
            mode: SelectMode::Filter,
            query: "soccer".to_string(),
            ..Default::default()
        };
        let visible = visible_topic_indices(&state, &topics);
        assert_eq!(visible, vec![0]);

        // Highlight mode keeps all.
        state.mode = SelectMode::Highlight;
        assert_eq!(visible_topic_indices(&state, &topics).len(), 2);
    }

    #[test]
    fn highlight_flags_only_in_highlight_mode() {
        let mut state = ViewState {
            mode: SelectMode::Highlight,
            query: "tennis".to_string(),
            ..Default::default()
        };
        assert!(is_highlighted(&state, "tennis"));
        assert!(!is_highlighted(&state, "soccer"));
        // In filter mode nothing is "highlighted" (rows are hidden instead).
        state.mode = SelectMode::Filter;
        assert!(!is_highlighted(&state, "tennis"));
    }

    #[test]
    fn partition_filter_matches_topic_or_partition() {
        let topics = vec![topic_with_partitions("soccer", &[(0, 5, "ok"), (3, 99, "hot")])];
        let rows = partition_rows(&topics);
        let state = ViewState {
            mode: SelectMode::Filter,
            query: "p3".to_string(),
            ..Default::default()
        };
        let visible = visible_partition_indices(&state, &rows);
        assert_eq!(visible.len(), 1);
        assert_eq!(rows[visible[0]].index, 3);
    }

    #[test]
    fn toggle_mode_flips() {
        let mut state = ViewState::default();
        assert_eq!(state.mode, SelectMode::Highlight);
        state.toggle_mode();
        assert_eq!(state.mode, SelectMode::Filter);
        state.toggle_mode();
        assert_eq!(state.mode, SelectMode::Highlight);
    }
}
