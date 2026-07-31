//! View-state and selection logic for the interactive TUI.
//!
//! This module is deliberately pure: it decides *what* to show given the
//! current view, filter, and selection, with no terminal I/O. The ratatui event
//! loop (in `tui`, compiled only with the `tui` feature) is a thin shell that
//! mutates a `ViewState` on keypresses and asks this module for the rows to
//! draw. Keeping the decision logic here means it's fully unit-tested even
//! though the terminal loop can't be.

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


/// Mutable UI state driven by keypresses. Navigation is a cursor (↑/↓) plus a
/// drill-in focus (Enter on a topic scopes the combined view to it; Esc backs
/// out). There is no text filter.
#[derive(Debug, Clone)]
pub struct ViewState {
    pub view: View,
    /// Index of the highlighted row in the current view's row set.
    pub cursor: usize,
    /// When set, the user has drilled into this topic: the combined view shows
    /// its partitions in the lower pane. Cleared by Esc.
    pub drilled_topic: Option<String>,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            view: View::Topic,
            cursor: 0,
            drilled_topic: None,
        }
    }
}

impl ViewState {
    pub fn cycle_view(&mut self) {
        self.view = self.view.next();
        self.cursor = 0;
    }

    /// Move the cursor down, clamped to `len-1`. No-op on an empty set.
    pub fn cursor_down(&mut self, len: usize) {
        if len == 0 {
            return;
        }
        if self.cursor + 1 < len {
            self.cursor += 1;
        }
    }

    /// Move the cursor up, clamped to 0.
    pub fn cursor_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Clamp the cursor into range after the row set changes (e.g. a refresh
    /// removed rows). Keeps the cursor on the last row rather than out of bounds.
    pub fn clamp_cursor(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// Drill into the topic at the cursor: switch to the combined view scoped to
    /// that topic. `topics` is the currently-visible topic ordering so the
    /// cursor index resolves to the right name.
    pub fn drill_in(&mut self, topics: &[TopicHealth]) {
        if let Some(t) = topics.get(self.cursor) {
            self.drilled_topic = Some(t.topic.clone());
            self.view = View::Combined;
        }
    }

    /// Back out of a drill-in: clear the focus and return to the topic view.
    pub fn drill_out(&mut self) {
        self.drilled_topic = None;
        self.view = View::Topic;
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

/// Binary byte sizes (GiB/MiB/KiB), matching the table renderer. Zero → "—".
/// Delegates to the canonical implementation so both share one format.
pub fn fmt_bytes(bytes: i64) -> String {
    crate::output::format_bytes(bytes)
}

/// Partition rows for a single named topic (the drilled-into topic), sorted by
/// partition index. Empty if the topic isn't found or has no partitions.
pub fn partition_rows_for_topic(topics: &[TopicHealth], topic: &str) -> Vec<PartitionRow> {
    let mut rows: Vec<PartitionRow> = topics
        .iter()
        .filter(|t| t.topic == topic)
        .flat_map(|t| {
            t.partitions.iter().map(move |p| PartitionRow {
                topic: t.topic.clone(),
                partition: p.partition.clone(),
                index: p.index,
                backlog: p.backlog,
                backlog_bytes: p.backlog_bytes,
                consumers: p.consumers,
                unacked_messages: p.unacked_messages,
                status: p.status,
            })
        })
        .collect();
    rows.sort_by_key(|r| r.index);
    rows
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
    fn cursor_moves_and_clamps() {
        let mut state = ViewState::default();
        assert_eq!(state.cursor, 0);
        state.cursor_up(); // already at top, stays
        assert_eq!(state.cursor, 0);
        state.cursor_down(3);
        state.cursor_down(3);
        assert_eq!(state.cursor, 2);
        state.cursor_down(3); // at last row, clamps
        assert_eq!(state.cursor, 2);
        state.cursor_up();
        assert_eq!(state.cursor, 1);
    }

    #[test]
    fn cursor_down_noop_on_empty() {
        let mut state = ViewState::default();
        state.cursor_down(0);
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn clamp_after_rowset_shrinks() {
        let mut state = ViewState { cursor: 5, ..Default::default() };
        state.clamp_cursor(3);
        assert_eq!(state.cursor, 2, "clamped to last row");
        state.clamp_cursor(0);
        assert_eq!(state.cursor, 0, "empty set resets to 0");
    }

    #[test]
    fn drill_in_scopes_to_selected_topic_and_switches_view() {
        let topics = vec![
            topic_with_partitions("soccer", &[(0, 5, "ok")]),
            topic_with_partitions("tennis", &[(0, 5, "ok"), (1, 9, "hot")]),
        ];
        let mut state = ViewState::default();
        state.cursor_down(topics.len()); // cursor now on "tennis"
        state.drill_in(&topics);
        assert_eq!(state.drilled_topic.as_deref(), Some("tennis"));
        assert_eq!(state.view, View::Combined);

        // The lower pane shows only tennis's partitions.
        let rows = partition_rows_for_topic(&topics, state.drilled_topic.as_deref().unwrap());
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.topic == "tennis"));
    }

    #[test]
    fn drill_out_returns_to_topic_view() {
        let mut state = ViewState {
            view: View::Combined,
            drilled_topic: Some("tennis".to_string()),
            cursor: 1,
        };
        state.drill_out();
        assert_eq!(state.view, View::Topic);
        assert!(state.drilled_topic.is_none());
        assert_eq!(state.cursor, 1, "cursor preserved on the way out");
    }

    #[test]
    fn cycle_view_resets_cursor() {
        let mut state = ViewState { cursor: 4, ..Default::default() };
        state.cycle_view();
        assert_eq!(state.view, View::Partition);
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn partition_rows_for_topic_only_that_topic() {
        let topics = vec![
            topic_with_partitions("soccer", &[(0, 5, "ok"), (1, 7, "ok")]),
            topic_with_partitions("tennis", &[(0, 5, "ok")]),
        ];
        let rows = partition_rows_for_topic(&topics, "soccer");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.topic == "soccer"));
        // Unknown topic → empty.
        assert!(partition_rows_for_topic(&topics, "nope").is_empty());
    }
}
