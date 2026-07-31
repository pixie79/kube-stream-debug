//! Health evaluation: turn raw admin stats into a per-topic verdict.

use serde::Serialize;

use crate::cursor::{evaluate_cursor, CursorTrimStatus, TrimVerdict};
use crate::drain::DrainStats;
use crate::pulsar::{AdminClient, InternalStats, PulsarError, TopicName, TopicStats};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Subscription present, consumers attached, all partitions under threshold.
    Ok,
    /// One or more partitions over the backlog threshold.
    Backlog,
    /// Subscription present overall, but absent (or consumer-less) on some partitions.
    PartitionGap,
    /// Subscription exists but has zero consumers attached anywhere.
    NoConsumers,
    /// Subscription is not attached to this topic at all.
    MissingSub,
    /// A cursor is stranded past trimmed data: the next entry it wants has been
    /// GC'd, so the consumer will spin forever. The most urgent state.
    Trimmed,
    /// Stats could not be fetched.
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Backlog => "BACKLOG",
            Status::PartitionGap => "PARTITION_GAP",
            Status::NoConsumers => "NO_CONSUMERS",
            Status::MissingSub => "MISSING_SUB",
            Status::Trimmed => "TRIMMED",
            Status::Error => "ERROR",
        }
    }

    pub fn is_healthy(self) -> bool {
        self == Status::Ok
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HotPartition {
    pub partition: String,
    pub backlog: i64,
    /// Backlog size in bytes for this partition (0 if the broker didn't report it).
    pub backlog_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PartitionGap {
    pub partition: String,
    /// "subscription_missing" | "no_consumers"
    pub reason: &'static str,
}

/// A partition whose cursor is stranded past trimmed data.
#[derive(Debug, Clone, Serialize)]
pub struct TrimmedPartition {
    pub partition: String,
    /// True when the broker also reports the cursor parked-and-waiting despite
    /// a backlog — independent corroboration that it's genuinely stuck.
    pub waiting_with_backlog: bool,
}

/// A partition whose cursor sits in the oldest surviving ledger — one GC cycle
/// from data loss. `headroom` is the entries of margin remaining.
#[derive(Debug, Clone, Serialize)]
pub struct AtEdgePartition {
    pub partition: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headroom: Option<i64>,
}

/// Per-partition status for the partition-level view. Distinct from the
/// summary detail structs (hot/gap/edge) — this is the *complete* row set,
/// one entry per partition, whatever its state.
#[derive(Debug, Clone, Serialize)]
pub struct PartitionDetail {
    /// Full partition topic name (…-partition-N).
    pub partition: String,
    /// Numeric partition index for stable sorting/selection.
    pub index: u32,
    pub backlog: i64,
    pub backlog_bytes: i64,
    pub consumers: usize,
    pub unacked_messages: i64,
    /// Per-partition status label: "ok" | "hot" | "no_consumers" |
    /// "subscription_missing" | "trimmed" | "at_edge".
    pub status: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct TopicHealth {
    pub topic: String,
    pub status: Status,
    /// Backlog in messages. `None` when unmeasurable — the subscription is
    /// absent (MISSING_SUB) or the stats fetch failed (ERROR) — as distinct
    /// from a real `Some(0)`.
    pub total_backlog: Option<i64>,
    /// Aggregate backlog size in bytes. `None` when unmeasurable (see above).
    pub backlog_bytes: Option<i64>,
    /// Consumer count. `None` when unmeasurable (subscription absent / error).
    pub consumers: Option<i64>,
    /// Unacked messages. `None` when unmeasurable (see above).
    pub unacked_messages: Option<i64>,
    pub msg_rate_out: f64,
    /// Smallest per-partition headroom (entries before the trimmer reaches the
    /// cursor) across all partitions of this topic. `None` when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_headroom: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trimmed_partitions: Vec<TrimmedPartition>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub at_edge_partitions: Vec<AtEdgePartition>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hot_partitions: Vec<HotPartition>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub partition_gaps: Vec<PartitionGap>,
    /// Complete per-partition rows for the partition-level view (empty for
    /// non-partitioned topics or when partition stats were unavailable).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub partitions: Vec<PartitionDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drain: Option<DrainStats>,
    /// UTC RFC3339 timestamp at which this topic's current (status, trend) was
    /// first observed — for the time-in-state column. Populated from snapshot
    /// history or the in-memory watch session; `None` when unknown (no prior
    /// observation available, e.g. a single run with no snapshots).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_since: Option<String>,
    /// Correlation hint from the Kubernetes side (e.g. "kube: 1 pod OOM-killed")
    /// attached to unhealthy topics when --kube is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kube_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TopicHealth {
    fn error(topic: &TopicName, error: &PulsarError) -> Self {
        TopicHealth {
            topic: topic.to_string(),
            status: Status::Error,
            // Unmeasurable: the fetch failed, so we know nothing about backlog.
            total_backlog: None,
            backlog_bytes: None,
            consumers: None,
            unacked_messages: None,
            msg_rate_out: 0.0,
            min_headroom: None,
            trimmed_partitions: Vec::new(),
            at_edge_partitions: Vec::new(),
            hot_partitions: Vec::new(),
            partition_gaps: Vec::new(),
            partitions: Vec::new(),
            drain: None,
            state_since: None,
            kube_hint: None,
            error: Some(error.to_string()),
        }
    }

    /// Backlog for internal math (comparisons, drain): unmeasured → 0. Use the
    /// raw `total_backlog` Option for display/serialization.
    pub fn backlog_or_zero(&self) -> i64 {
        self.total_backlog.unwrap_or(0)
    }

    /// Backlog bytes for internal math; unmeasured → 0.
    pub fn backlog_bytes_or_zero(&self) -> i64 {
        self.backlog_bytes.unwrap_or(0)
    }

    /// Consumer count for internal math; unmeasured → 0.
    pub fn consumers_or_zero(&self) -> i64 {
        self.consumers.unwrap_or(0)
    }

    /// Unacked for internal math; unmeasured → 0. Part of the `_or_zero`
    /// accessor set for symmetry; not all callers use it yet.
    #[allow(dead_code)]
    pub fn unacked_or_zero(&self) -> i64 {
        self.unacked_messages.unwrap_or(0)
    }

    /// Whether the subscription's figures were actually measured (false for
    /// MISSING_SUB and ERROR, where the numbers are unknown, not zero).
    #[allow(dead_code)]
    pub fn is_measured(&self) -> bool {
        self.total_backlog.is_some()
    }
}

/// Fetch stats for one configured topic and classify its health.
///
/// * Explicit `-partition-N` entries are checked with plain stats only.
/// * Base topics try partitioned-stats first (per-partition breakdown) and
///   fall back to plain stats on 404 (non-partitioned topic).
pub fn check_topic(client: &AdminClient, topic: &TopicName, subscription: &str, threshold: i64) -> TopicHealth {
    if topic.is_partition() {
        return match client.stats(topic) {
            Ok(stats) => {
                let internal = fetch_internal(client, std::slice::from_ref(topic));
                evaluate(topic, &stats, subscription, threshold, false, &internal)
            }
            Err(err) => TopicHealth::error(topic, &err),
        };
    }

    match client.partitioned_stats(topic) {
        Ok(stats) => {
            let partitions = partition_topics(&stats);
            let internal = fetch_internal(client, &partitions);
            evaluate(topic, &stats, subscription, threshold, true, &internal)
        }
        Err(PulsarError::NotFound) => match client.stats(topic) {
            Ok(stats) => {
                let internal = fetch_internal(client, std::slice::from_ref(topic));
                evaluate(topic, &stats, subscription, threshold, false, &internal)
            }
            Err(err) => TopicHealth::error(topic, &err),
        },
        Err(err) => TopicHealth::error(topic, &err),
    }
}

/// Parse the partition topic names from a partitioned-stats response.
/// Cheap aggregate-backlog reading for one topic, for the second drain sample.
/// Mirrors `check_topic`'s partition/base/fallback dispatch but fetches only the
/// aggregate subscription backlog — no per-partition or internal-stats calls.
/// Returns `None` if the topic can't be read or the subscription is absent.
pub fn sample_backlog(client: &AdminClient, topic: &TopicName, subscription: &str) -> Option<i64> {
    let stats = if topic.is_partition() {
        client.stats(topic).ok()?
    } else {
        match client.aggregate_stats(topic) {
            Ok(stats) => stats,
            Err(PulsarError::NotFound) => client.stats(topic).ok()?,
            Err(_) => return None,
        }
    };
    stats.subscriptions.get(subscription).map(|s| s.msg_backlog)
}

fn partition_topics(stats: &TopicStats) -> Vec<TopicName> {
    stats
        .partitions
        .keys()
        .filter_map(|name| TopicName::parse(name).ok())
        .collect()
}

/// Fetch `internalStats` for each given topic/partition, keyed by full topic
/// name. Failures are simply omitted — the cursor check degrades to Unknown
/// for that partition rather than failing the whole topic.
fn fetch_internal(
    client: &AdminClient,
    topics: &[TopicName],
) -> std::collections::HashMap<String, InternalStats> {
    let mut out = std::collections::HashMap::new();
    for topic in topics {
        if let Ok(internal) = client.internal_stats(topic) {
            out.insert(topic.to_string(), internal);
        }
    }
    out
}

fn evaluate(
    topic: &TopicName,
    stats: &TopicStats,
    subscription: &str,
    threshold: i64,
    partitioned: bool,
    internal: &std::collections::HashMap<String, InternalStats>,
) -> TopicHealth {
    let mut health = TopicHealth {
        topic: topic.to_string(),
        status: Status::Ok,
        // Start unmeasured; the Some(sub) branch below fills these in. If the
        // subscription is absent we return early with these as None (correct:
        // a missing subscription has no measurable backlog).
        total_backlog: None,
        backlog_bytes: None,
        consumers: None,
        unacked_messages: None,
        msg_rate_out: stats.msg_rate_out,
        min_headroom: None,
        trimmed_partitions: Vec::new(),
        at_edge_partitions: Vec::new(),
        hot_partitions: Vec::new(),
        partition_gaps: Vec::new(),
        partitions: Vec::new(),
        drain: None,
        state_since: None,
        kube_hint: None,
        error: None,
    };

    // Aggregate view of the subscription (partitioned-stats sums these for us;
    // for plain topics it is simply the topic's own subscription entry).
    match stats.subscriptions.get(subscription) {
        None => {
            health.status = Status::MissingSub;
            return health;
        }
        Some(sub) => {
            health.total_backlog = Some(sub.msg_backlog);
            health.backlog_bytes = Some(sub.backlog_size);
            health.consumers = Some(sub.consumers.len() as i64);
            health.unacked_messages = Some(sub.unacked_total());
        }
    }

    if partitioned {
        collect_partition_detail(&mut health, stats, subscription, threshold);
        // Aggregated consumer count on partitioned-stats can be sparse on some
        // broker versions; take the per-partition sum when it is larger.
        let partition_consumer_sum: usize = stats
            .partitions
            .values()
            .filter_map(|p| p.subscriptions.get(subscription))
            .map(|s| s.consumers.len())
            .sum();
        // Aggregated consumer count on partitioned-stats can be sparse on some
        // broker versions; take the per-partition sum when it is larger.
        health.consumers = Some(health.consumers_or_zero().max(partition_consumer_sum as i64));

        // Per-partition detail + cursor trim check: read each partition's
        // backlog from the stats we already have, judge it against its ledger
        // floor, and record a complete row for the partition-level view.
        let mut names: Vec<&String> = stats.partitions.keys().collect();
        names.sort_by_key(|name| partition_index(name));
        for name in names {
            let sub = stats
                .partitions
                .get(name)
                .and_then(|p| p.subscriptions.get(subscription));
            let backlog = sub.map(|s| s.msg_backlog).unwrap_or(0);
            let backlog_bytes = sub.map(|s| s.backlog_size).unwrap_or(0);
            let consumers = sub.map(|s| s.consumers.len()).unwrap_or(0);
            let unacked = sub.map(|s| s.unacked_total()).unwrap_or(0);

            let mut cursor_status = None;
            if let Some(is) = internal.get(name) {
                let status = evaluate_cursor(is, subscription, backlog);
                cursor_status = Some(status.verdict);
                record_cursor(&mut health, name, &status);
            }

            let status = partition_status(sub.is_some(), consumers, backlog, threshold, cursor_status);
            health.partitions.push(PartitionDetail {
                partition: name.clone(),
                index: partition_index(name),
                backlog,
                backlog_bytes,
                consumers,
                unacked_messages: unacked,
                status,
            });
        }
    } else {
        let backlog = health.backlog_or_zero();
        if backlog > threshold {
            // Non-partitioned topic: the topic itself is the "partition".
            health.hot_partitions.push(HotPartition {
                partition: topic.to_string(),
                backlog,
                backlog_bytes: health.backlog_bytes_or_zero(),
            });
        }
        let key = topic.to_string();
        if let Some(is) = internal.get(&key) {
            let status = evaluate_cursor(is, subscription, backlog);
            record_cursor(&mut health, &key, &status);
        }
    }

    health.status = classify(&health);
    health
}

/// Fold one partition's cursor verdict into the topic health, tracking the
/// minimum headroom seen so far.
fn record_cursor(health: &mut TopicHealth, partition: &str, status: &CursorTrimStatus) {
    if let Some(headroom) = status.headroom_entries {
        health.min_headroom = Some(match health.min_headroom {
            Some(current) => current.min(headroom),
            None => headroom,
        });
    }
    match status.verdict {
        TrimVerdict::Stuck => health.trimmed_partitions.push(TrimmedPartition {
            partition: partition.to_string(),
            waiting_with_backlog: status.waiting_with_backlog,
        }),
        TrimVerdict::AtEdge => health.at_edge_partitions.push(AtEdgePartition {
            partition: partition.to_string(),
            headroom: status.headroom_entries,
        }),
        TrimVerdict::Safe | TrimVerdict::Unknown => {}
    }
}

fn collect_partition_detail(
    health: &mut TopicHealth,
    stats: &TopicStats,
    subscription: &str,
    threshold: i64,
) {
    let mut names: Vec<&String> = stats.partitions.keys().collect();
    names.sort_by_key(|name| partition_index(name));

    for name in names {
        let partition_stats = &stats.partitions[name];
        match partition_stats.subscriptions.get(subscription) {
            None => health.partition_gaps.push(PartitionGap {
                partition: name.clone(),
                reason: "subscription_missing",
            }),
            Some(sub) if sub.consumers.is_empty() => health.partition_gaps.push(PartitionGap {
                partition: name.clone(),
                reason: "no_consumers",
            }),
            Some(sub) => {
                if sub.msg_backlog > threshold {
                    health.hot_partitions.push(HotPartition {
                        partition: name.clone(),
                        backlog: sub.msg_backlog,
                        backlog_bytes: sub.backlog_size,
                    });
                }
            }
        }
    }
}

fn classify(health: &TopicHealth) -> Status {
    // Trimmed cursor is the most urgent: the consumer is spinning for data that
    // no longer exists. It outranks everything else.
    if !health.trimmed_partitions.is_empty() {
        return Status::Trimmed;
    }
    if health.consumers_or_zero() == 0 {
        return Status::NoConsumers;
    }
    if !health.partition_gaps.is_empty() {
        return Status::PartitionGap;
    }
    if !health.hot_partitions.is_empty() {
        return Status::Backlog;
    }
    Status::Ok
}

/// Extract the numeric suffix of `…-partition-N` for stable sort order.
/// Classify a single partition's status, worst-first, mirroring the topic-level
/// priority: trimmed > missing subscription > no consumers > hot > ok.
fn partition_status(
    sub_present: bool,
    consumers: usize,
    backlog: i64,
    threshold: i64,
    cursor: Option<TrimVerdict>,
) -> &'static str {
    if matches!(cursor, Some(TrimVerdict::Stuck)) {
        return "trimmed";
    }
    if !sub_present {
        return "subscription_missing";
    }
    if consumers == 0 {
        return "no_consumers";
    }
    if matches!(cursor, Some(TrimVerdict::AtEdge)) {
        return "at_edge";
    }
    if backlog > threshold {
        return "hot";
    }
    "ok"
}

fn partition_index(name: &str) -> u32 {
    name.rsplit("-partition-")
        .next()
        .and_then(|suffix| suffix.parse().ok())
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic(name: &str) -> TopicName {
        TopicName::parse(name).unwrap()
    }

    fn parse_stats(json: &str) -> TopicStats {
        serde_json::from_str(json).unwrap()
    }

    fn empty_internal() -> std::collections::HashMap<String, InternalStats> {
        std::collections::HashMap::new()
    }

    fn internal_map(
        entries: &[(&str, &str)],
    ) -> std::collections::HashMap<String, InternalStats> {
        entries
            .iter()
            .map(|(name, json)| ((*name).to_string(), serde_json::from_str(json).unwrap()))
            .collect()
    }

    #[test]
    fn trimmed_partition_sets_top_priority_status() {
        // Partition-0 backlog is huge but its cursor points below the ledger
        // floor: the consumer is stuck on trimmed data.
        let stats = parse_stats(
            r#"{
            "subscriptions": {"my-sub": {"msgBacklog": 500, "consumers": [{}]}},
            "partitions": {
                "persistent://a/b/c-partition-0": {
                    "subscriptions": {"my-sub": {"msgBacklog": 500, "consumers": [{}]}}
                }
            }
        }"#,
        );
        let internal = internal_map(&[(
            "persistent://a/b/c-partition-0",
            r#"{"ledgers": [{"ledgerId": 20, "entries": 100}],
                "cursors": {"my-sub": {"readPosition": "10:5", "waitingReadOp": true}}}"#,
        )]);
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, true, &internal);
        assert_eq!(health.status, Status::Trimmed);
        assert_eq!(health.trimmed_partitions.len(), 1);
        assert!(health.trimmed_partitions[0].waiting_with_backlog);
        // Trimmed outranks the backlog that is also present.
        assert!(!health.hot_partitions.is_empty());
    }

    #[test]
    fn at_edge_partition_recorded_with_min_headroom() {
        let stats = parse_stats(
            r#"{
            "subscriptions": {"my-sub": {"msgBacklog": 10, "consumers": [{}]}},
            "partitions": {
                "persistent://a/b/c-partition-0": {
                    "subscriptions": {"my-sub": {"msgBacklog": 10, "consumers": [{}]}}
                }
            }
        }"#,
        );
        let internal = internal_map(&[(
            "persistent://a/b/c-partition-0",
            r#"{"ledgers": [{"ledgerId": 20, "entries": 100}, {"ledgerId": 30, "entries": 50}],
                "cursors": {"my-sub": {"readPosition": "20:7", "waitingReadOp": false}}}"#,
        )]);
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, true, &internal);
        // Cursor in oldest ledger, not stuck: healthy status but edge recorded.
        assert_eq!(health.status, Status::Ok);
        assert_eq!(health.at_edge_partitions.len(), 1);
        assert_eq!(health.min_headroom, Some(7));
    }

    #[test]
    fn safe_cursor_leaves_status_clean() {
        let stats = parse_stats(
            r#"{"subscriptions": {"my-sub": {"msgBacklog": 2, "consumers": [{}]}}}"#,
        );
        let internal = internal_map(&[(
            "persistent://a/b/c",
            r#"{"ledgers": [{"ledgerId": 10, "entries": 100}, {"ledgerId": 20, "entries": 5}],
                "cursors": {"my-sub": {"readPosition": "20:3", "waitingReadOp": false}}}"#,
        )]);
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, false, &internal);
        assert_eq!(health.status, Status::Ok);
        assert!(health.trimmed_partitions.is_empty());
        assert!(health.at_edge_partitions.is_empty());
        assert_eq!(health.min_headroom, Some(103));
    }

    #[test]
    fn missing_subscription_is_flagged() {
        let stats = parse_stats(r#"{"subscriptions": {"other-sub": {"msgBacklog": 1}}}"#);
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, false, &empty_internal());
        assert_eq!(health.status, Status::MissingSub);
    }

    #[test]
    fn zero_consumers_is_flagged() {
        let stats =
            parse_stats(r#"{"subscriptions": {"my-sub": {"msgBacklog": 5, "consumers": []}}}"#);
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, false, &empty_internal());
        assert_eq!(health.status, Status::NoConsumers);
    }

    #[test]
    fn hot_partitions_listed_and_sorted() {
        let stats = parse_stats(
            r#"{
            "subscriptions": {"my-sub": {"msgBacklog": 700, "consumers": [{}]}},
            "partitions": {
                "persistent://a/b/c-partition-10": {
                    "subscriptions": {"my-sub": {"msgBacklog": 400, "consumers": [{}]}}
                },
                "persistent://a/b/c-partition-2": {
                    "subscriptions": {"my-sub": {"msgBacklog": 250, "consumers": [{}]}}
                },
                "persistent://a/b/c-partition-5": {
                    "subscriptions": {"my-sub": {"msgBacklog": 50, "consumers": [{}]}}
                }
            }
        }"#,
        );
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, true, &empty_internal());
        assert_eq!(health.status, Status::Backlog);
        let partitions: Vec<&str> = health
            .hot_partitions
            .iter()
            .map(|p| p.partition.as_str())
            .collect();
        assert_eq!(
            partitions,
            vec![
                "persistent://a/b/c-partition-2",
                "persistent://a/b/c-partition-10"
            ]
        );
    }

    #[test]
    fn partition_gap_outranks_backlog() {
        let stats = parse_stats(
            r#"{
            "subscriptions": {"my-sub": {"msgBacklog": 900, "consumers": [{}]}},
            "partitions": {
                "persistent://a/b/c-partition-0": {
                    "subscriptions": {"my-sub": {"msgBacklog": 900, "consumers": [{}]}}
                },
                "persistent://a/b/c-partition-1": {
                    "subscriptions": {}
                }
            }
        }"#,
        );
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, true, &empty_internal());
        assert_eq!(health.status, Status::PartitionGap);
        assert_eq!(health.partition_gaps.len(), 1);
        assert_eq!(health.partition_gaps[0].reason, "subscription_missing");
        assert_eq!(health.hot_partitions.len(), 1);
    }

    #[test]
    fn non_partitioned_topic_over_threshold_reports_itself() {
        let stats =
            parse_stats(r#"{"subscriptions": {"my-sub": {"msgBacklog": 250, "consumers": [{}]}}}"#);
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, false, &empty_internal());
        assert_eq!(health.status, Status::Backlog);
        assert_eq!(health.hot_partitions[0].backlog, 250);
    }

    #[test]
    fn healthy_topic_is_ok() {
        let stats = parse_stats(
            r#"{"subscriptions": {"my-sub": {"msgBacklog": 3, "consumers": [{"unackedMessages": 2}]}}}"#,
        );
        let health = evaluate(&topic("a/b/c"), &stats, "my-sub", 100, false, &empty_internal());
        assert_eq!(health.status, Status::Ok);
        assert_eq!(health.unacked_messages, Some(2));
        assert!(health.status.is_healthy());
    }
}
