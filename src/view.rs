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
    /// One row per topic (the classic table). Enter drills into a topic's
    /// partitions via the combined view.
    Topic,
    /// Kubernetes consumer-pod health.
    Kube,
    /// Split: topics on top, partitions (of the selected topic) below.
    Combined,
    /// Detail for a single pod (its resource breakdown, log stats, and raw
    /// logs). Reached by pressing Enter on a pod in the kube view.
    PodDetail,
    /// Detail for a single node (its capacity and which pods run on it).
    /// Reached by pressing Enter on a node in the kube view. Neither detail
    /// view is part of the `v` cycle.
    NodeDetail,
}

impl View {
    /// Cycle order for the toggle key. The detail views are excluded — they're
    /// reached by drilling in, and cycling from them returns to the topic view.
    pub fn next(self) -> View {
        match self {
            View::Topic => View::Kube,
            View::Kube => View::Combined,
            View::Combined => View::Topic,
            View::PodDetail | View::NodeDetail => View::Topic,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Topic => "topic",
            View::Kube => "kube",
            View::Combined => "combined",
            View::PodDetail => "pod",
            View::NodeDetail => "node",
        }
    }
}


/// Within the kube view, which of the two sections (pods / nodes) the cursor is
/// currently in. Tab switches between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KubeFocus {
    Pods,
    Nodes,
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
    /// In the kube view, which section (pods/nodes) the cursor is in.
    pub kube_focus: KubeFocus,
    /// When set, the pod-detail view is showing this pod.
    pub selected_pod: Option<String>,
    /// Cursor within the pod-detail log list (which line is highlighted).
    pub log_cursor: usize,
    /// Whether the selected log line is expanded to pretty-printed detail.
    pub log_expanded: bool,
    /// Whether long lines wrap (true) or are truncated (false) in the detail.
    pub log_wrap: bool,
    /// When set, the node-detail view is showing this node.
    pub selected_node: Option<String>,
    /// Whether the status-legend help overlay is showing.
    pub show_help: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            view: View::Topic,
            cursor: 0,
            drilled_topic: None,
            kube_focus: KubeFocus::Pods,
            selected_pod: None,
            log_cursor: 0,
            log_expanded: false,
            log_wrap: true,
            selected_node: None,
            show_help: false,
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

    /// Back out of a drill-in. Topic/combined → topic view; the kube detail
    /// views (pod/node) → back to the kube view with focus preserved.
    pub fn drill_out(&mut self) {
        match self.view {
            View::PodDetail => {
                self.selected_pod = None;
                self.view = View::Kube;
            }
            View::NodeDetail => {
                self.selected_node = None;
                self.view = View::Kube;
            }
            _ => {
                self.drilled_topic = None;
                self.view = View::Topic;
            }
        }
    }

    /// Toggle the kube-view cursor between the pods and nodes sections, resetting
    /// the cursor to the top of the newly-focused section.
    pub fn toggle_kube_focus(&mut self) {
        self.kube_focus = match self.kube_focus {
            KubeFocus::Pods => KubeFocus::Nodes,
            KubeFocus::Nodes => KubeFocus::Pods,
        };
        self.cursor = 0;
    }

    /// Drill into whatever the kube cursor is on: a pod (→ PodDetail) when the
    /// pods section is focused, or a node (→ NodeDetail) when nodes is focused.
    /// `pods` and `nodes` are the names in the same order they're rendered.
    pub fn kube_drill_in(&mut self, pods: &[String], nodes: &[String]) {
        match self.kube_focus {
            KubeFocus::Pods => {
                if let Some(name) = pods.get(self.cursor) {
                    self.selected_pod = Some(name.clone());
                    self.view = View::PodDetail;
                    self.cursor = 0;
                    self.log_cursor = 0;
                    self.log_expanded = false;
                }
            }
            KubeFocus::Nodes => {
                if let Some(name) = nodes.get(self.cursor) {
                    self.selected_node = Some(name.clone());
                    self.view = View::NodeDetail;
                    self.cursor = 0;
                }
            }
        }
    }

    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
    }

    /// Move the log cursor down within `len` lines (pod-detail list). No-op when
    /// a line is expanded (Esc collapses first).
    pub fn log_cursor_down(&mut self, len: usize) {
        if !self.log_expanded && len > 0 && self.log_cursor + 1 < len {
            self.log_cursor += 1;
        }
    }

    pub fn log_cursor_up(&mut self) {
        if !self.log_expanded {
            self.log_cursor = self.log_cursor.saturating_sub(1);
        }
    }

    /// Enter on a log line: expand it to pretty-printed detail. Esc (handled via
    /// drill_out) collapses. Returns true if it changed state (so the caller
    /// knows Enter was consumed here rather than drilling elsewhere).
    pub fn log_expand(&mut self) {
        self.log_expanded = true;
    }

    /// Collapse an expanded log line back to the list. Returns true if it was
    /// expanded (so Esc collapses first, then a second Esc leaves pod-detail).
    pub fn log_collapse(&mut self) -> bool {
        if self.log_expanded {
            self.log_expanded = false;
            true
        } else {
            false
        }
    }

    pub fn toggle_log_wrap(&mut self) {
        self.log_wrap = !self.log_wrap;
    }
}

/// Severity of a status/partition-status, for colour mapping. Pure so both the
/// comfy-table and ratatui render paths can share one classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Bad,
}

/// Map a status label (topic or partition) to a severity.
pub fn status_severity(label: &str) -> Severity {
    match label {
        "OK" | "ok" | "empty" => Severity::Ok,
        "BACKLOG" | "backlog" | "PARTITION_GAP" | "hot" | "at_edge" | "draining" | "stable" => {
            Severity::Warn
        }
        "NO_CONSUMERS" | "no_consumers" | "MISSING_SUB" | "subscription_missing" | "TRIMMED"
        | "trimmed" | "ERROR" | "error" | "growing" | "DLQ-ERROR" | "OOMKilled" => Severity::Bad,
        _ => Severity::Warn,
    }
}

/// The status-legend text shown in the help overlay: each topic status and what
/// it means. Kept here (pure) so it's testable and the TUI just renders it.
pub fn status_legend() -> &'static [(&'static str, &'static str)] {
    &[
        ("OK", "consumers attached, backlog under threshold — healthy"),
        ("BACKLOG", "consumers attached but backlog over threshold"),
        ("NO_CONSUMERS", "subscription EXISTS but no consumers attached (backlog is real)"),
        ("MISSING_SUB", "subscription does NOT exist on the topic — 0 backlog means unmeasurable, not empty"),
        ("PARTITION_GAP", "some partitions have consumers, others are missing/unconsumed"),
        ("TRIMMED", "cursor fell behind the oldest ledger — data was lost"),
        ("ERROR", "the stats fetch for this topic failed"),
    ]
}

/// Legend for the per-partition statuses shown in the partition view.
pub fn partition_status_legend() -> &'static [(&'static str, &'static str)] {
    &[
        ("ok", "consumers attached, backlog under threshold"),
        ("hot", "consumers attached but this partition's backlog is over threshold"),
        ("no_consumers", "no consumer attached to this partition"),
        ("subscription_missing", "the subscription is absent on this partition"),
        ("at_edge", "cursor near the oldest ledger — trim risk"),
        ("trimmed", "cursor fell behind the oldest ledger — data lost"),
    ]
}

/// Legend for the drain trend column.
pub fn trend_legend() -> &'static [(&'static str, &'static str)] {
    &[
        ("draining", "backlog shrinking; ETA shows time-to-clear"),
        ("growing", "backlog rising; producers outpace consumers"),
        ("stable", "backlog roughly flat (within ~1%)"),
        ("empty", "no backlog"),
    ]
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
            total_backlog: Some(parts.iter().map(|(_, b, _)| b).sum()),
            backlog_bytes: Some(0),
            consumers: Some(3),
            unacked_messages: Some(0),
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
    fn help_toggles() {
        let mut state = ViewState::default();
        assert!(!state.show_help);
        state.toggle_help();
        assert!(state.show_help);
        state.toggle_help();
        assert!(!state.show_help);
    }

    #[test]
    fn severity_classification() {
        assert_eq!(status_severity("OK"), Severity::Ok);
        assert_eq!(status_severity("BACKLOG"), Severity::Warn);
        assert_eq!(status_severity("NO_CONSUMERS"), Severity::Bad);
        assert_eq!(status_severity("TRIMMED"), Severity::Bad);
        assert_eq!(status_severity("growing"), Severity::Bad);
        assert_eq!(status_severity("DLQ-ERROR"), Severity::Bad);
        assert_eq!(status_severity("draining"), Severity::Warn);
        // partition statuses
        assert_eq!(status_severity("no_consumers"), Severity::Bad);
        assert_eq!(status_severity("hot"), Severity::Warn);
        assert_eq!(status_severity("ok"), Severity::Ok);
    }

    #[test]
    fn legends_are_populated() {
        assert!(status_legend().iter().any(|(k, _)| *k == "NO_CONSUMERS"));
        assert!(trend_legend().iter().any(|(k, _)| *k == "growing"));
    }

    #[test]
    fn view_cycles_through_three() {
        let mut v = View::Topic;
        v = v.next();
        assert_eq!(v, View::Kube);
        v = v.next();
        assert_eq!(v, View::Combined);
        v = v.next();
        assert_eq!(v, View::Topic);
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
            ..Default::default()
        };
        state.drill_out();
        assert_eq!(state.view, View::Topic);
        assert!(state.drilled_topic.is_none());
        assert_eq!(state.cursor, 1, "cursor preserved on the way out");
    }

    #[test]
    fn log_cursor_and_expand() {
        let mut state = ViewState { view: View::PodDetail, ..Default::default() };
        state.log_cursor_down(3);
        state.log_cursor_down(3);
        assert_eq!(state.log_cursor, 2);
        state.log_cursor_down(3); // clamp
        assert_eq!(state.log_cursor, 2);
        // Expand freezes the cursor.
        state.log_expand();
        assert!(state.log_expanded);
        state.log_cursor_up(); // no-op while expanded
        assert_eq!(state.log_cursor, 2);
        // Collapse returns true, then cursor moves again.
        assert!(state.log_collapse());
        assert!(!state.log_expanded);
        state.log_cursor_up();
        assert_eq!(state.log_cursor, 1);
        // Collapse when not expanded returns false (so Esc can leave the view).
        assert!(!state.log_collapse());
    }

    #[test]
    fn log_wrap_defaults_on_and_toggles() {
        let mut state = ViewState::default();
        assert!(state.log_wrap);
        state.toggle_log_wrap();
        assert!(!state.log_wrap);
    }

    #[test]
    fn kube_focus_toggles() {
        let mut state = ViewState { view: View::Kube, cursor: 3, ..Default::default() };
        assert_eq!(state.kube_focus, KubeFocus::Pods);
        state.toggle_kube_focus();
        assert_eq!(state.kube_focus, KubeFocus::Nodes);
        assert_eq!(state.cursor, 0, "cursor resets when switching section");
        state.toggle_kube_focus();
        assert_eq!(state.kube_focus, KubeFocus::Pods);
    }

    #[test]
    fn kube_drill_opens_pod_or_node_by_focus() {
        let pods = vec!["pod-a".to_string(), "pod-b".to_string()];
        let nodes = vec!["node-x".to_string(), "node-y".to_string()];

        // Pods focused → Enter opens pod-detail for the cursored pod.
        let mut state = ViewState { view: View::Kube, cursor: 1, ..Default::default() };
        state.kube_drill_in(&pods, &nodes);
        assert_eq!(state.view, View::PodDetail);
        assert_eq!(state.selected_pod.as_deref(), Some("pod-b"));
        state.drill_out();
        assert_eq!(state.view, View::Kube, "pod-detail backs out to kube");
        assert!(state.selected_pod.is_none());

        // Nodes focused → Enter opens node-detail.
        let mut state = ViewState { view: View::Kube, kube_focus: KubeFocus::Nodes, cursor: 0, ..Default::default() };
        state.kube_drill_in(&pods, &nodes);
        assert_eq!(state.view, View::NodeDetail);
        assert_eq!(state.selected_node.as_deref(), Some("node-x"));
        state.drill_out();
        assert_eq!(state.view, View::Kube, "node-detail backs out to kube");
        assert!(state.selected_node.is_none());
    }

    #[test]
    fn partition_status_legend_covers_hot() {
        assert!(partition_status_legend().iter().any(|(k, _)| *k == "hot"));
        assert!(partition_status_legend().iter().any(|(k, _)| *k == "no_consumers"));
    }

    #[test]
    fn cycle_view_resets_cursor() {
        let mut state = ViewState { cursor: 4, ..Default::default() };
        state.cycle_view();
        assert_eq!(state.view, View::Kube);
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
