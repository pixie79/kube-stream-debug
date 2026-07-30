//! Minimal Pulsar admin REST client — just what the health check needs.
//!
//! Endpoints used (admin/v2):
//! * `GET {domain}/{tenant}/{ns}/{topic}/partitioned-stats?perPartitionStats=true`
//! * `GET {domain}/{tenant}/{ns}/{topic}/stats`
//! * `GET {domain}/{tenant}/{ns}/{topic}/internalStats` (per partition — cursor
//!   position vs. physical ledger floor, for trim-risk detection)

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;

// ─────────────────────────────────────────────────────────────────────────────
// Positions (ledgerId:entryId)
// ─────────────────────────────────────────────────────────────────────────────

/// A `ledgerId:entryId` position, as used by cursors and `lastConfirmedEntry`.
///
/// Ordering is lexicographic on (ledger, entry), matching Pulsar's own message
/// ordering. `entryId` may be `-1` (meaning "before entry 0 of this ledger"),
/// so it is stored signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    pub ledger_id: i64,
    pub entry_id: i64,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid position '{0}': expected ledgerId:entryId")]
pub struct PositionError(String);

impl FromStr for Position {
    type Err = PositionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (ledger, entry) = s
            .split_once(':')
            .ok_or_else(|| PositionError(s.to_string()))?;
        Ok(Position {
            ledger_id: ledger
                .trim()
                .parse()
                .map_err(|_| PositionError(s.to_string()))?,
            entry_id: entry
                .trim()
                .parse()
                .map_err(|_| PositionError(s.to_string()))?,
        })
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.ledger_id, self.entry_id)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Topic names
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicName {
    pub domain: String, // "persistent" | "non-persistent"
    pub tenant: String,
    pub namespace: String,
    pub local: String,
}

#[derive(Debug, thiserror::Error)]
#[error("invalid topic name '{0}': expected [persistent://]tenant/namespace/topic")]
pub struct TopicNameError(pub String);

impl TopicName {
    /// Parse `persistent://tenant/ns/topic`, `non-persistent://…`, or the
    /// schemeless `tenant/ns/topic` (assumed persistent).
    pub fn parse(raw: &str) -> Result<Self, TopicNameError> {
        let (domain, rest) = match raw.split_once("://") {
            Some(("persistent", rest)) => ("persistent", rest),
            Some(("non-persistent", rest)) => ("non-persistent", rest),
            Some(_) => return Err(TopicNameError(raw.to_string())),
            None => ("persistent", raw),
        };
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        match parts.as_slice() {
            [tenant, namespace, local]
                if !tenant.is_empty() && !namespace.is_empty() && !local.is_empty() =>
            {
                Ok(TopicName {
                    domain: domain.to_string(),
                    tenant: tenant.to_string(),
                    namespace: namespace.to_string(),
                    local: local.to_string(),
                })
            }
            _ => Err(TopicNameError(raw.to_string())),
        }
    }

    pub fn is_partition(&self) -> bool {
        self.local.contains("-partition-")
    }

    fn rest_path(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.domain, self.tenant, self.namespace, self.local
        )
    }
}

impl fmt::Display for TopicName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}://{}/{}/{}",
            self.domain, self.tenant, self.namespace, self.local
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Stats model (subset of the admin API response)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopicStats {
    #[serde(default)]
    pub msg_rate_out: f64,
    #[serde(default)]
    pub subscriptions: HashMap<String, SubscriptionStats>,
    /// Present only in partitioned-stats responses with perPartitionStats=true;
    /// keyed by full partition topic name.
    #[serde(default)]
    pub partitions: HashMap<String, TopicStats>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionStats {
    #[serde(default)]
    pub msg_backlog: i64,
    /// Backlog size in bytes. Only populated when the stats request sets
    /// `subscriptionBacklogSize=true`; defaults to 0 otherwise.
    #[serde(default)]
    pub backlog_size: i64,
    #[serde(default)]
    pub consumers: Vec<ConsumerStats>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsumerStats {
    #[serde(default)]
    pub unacked_messages: i64,
}

impl SubscriptionStats {
    pub fn unacked_total(&self) -> i64 {
        self.consumers.iter().map(|c| c.unacked_messages).sum()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal stats (cursor positions + physical ledger list)
// ─────────────────────────────────────────────────────────────────────────────

/// Subset of `…/internalStats` for one (non-partitioned) topic or partition.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InternalStats {
    /// Ordered list of ledgers physically present. The first still-present
    /// ledger is the trim floor: anything before it has been GC'd.
    #[serde(default)]
    pub ledgers: Vec<LedgerInfo>,
    /// Cursor state per subscription name.
    #[serde(default)]
    pub cursors: HashMap<String, CursorInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerInfo {
    #[serde(default)]
    pub ledger_id: i64,
    #[serde(default)]
    pub entries: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorInfo {
    /// Next position the subscriber will read from.
    #[serde(default)]
    pub read_position: String,
    /// True when the cursor has caught up to the last published message and is
    /// parked waiting for new ones. Combined with a non-zero backlog this is a
    /// strong "stranded past trimmed data" signal.
    #[serde(default)]
    pub waiting_read_op: bool,
}

impl InternalStats {
    /// The trim floor: the oldest ledger still physically present. `None` when
    /// the ledger list is empty (freshly created / offloaded-only topic).
    pub fn floor_ledger_id(&self) -> Option<i64> {
        self.ledgers.iter().map(|l| l.ledger_id).min()
    }
}

impl CursorInfo {
    pub fn read(&self) -> Option<Position> {
        self.read_position.parse().ok()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Client
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum PulsarError {
    #[error("topic not found (HTTP 404)")]
    NotFound,
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("failed to decode stats JSON: {0}")]
    Decode(#[from] std::io::Error),
}

pub struct AdminClient {
    agent: ureq::Agent,
    base_url: String,
    token: String,
}

impl AdminClient {
    pub fn new(base_url: &str, token: &str, timeout: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(timeout)
            .build();
        AdminClient {
            agent,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// Partitioned-topic stats with per-partition breakdown.
    /// Returns `PulsarError::NotFound` for non-partitioned topics, which
    /// callers should treat as "fall back to plain stats".
    pub fn partitioned_stats(&self, topic: &TopicName) -> Result<TopicStats, PulsarError> {
        let url = format!(
            "{}/admin/v2/{}/partitioned-stats?perPartition=true&subscriptionBacklogSize=true",
            self.base_url,
            topic.rest_path()
        );
        self.get_json(&url)
    }

    /// Plain topic stats (also works for explicit `-partition-N` topics).
    pub fn stats(&self, topic: &TopicName) -> Result<TopicStats, PulsarError> {
        let url = format!(
            "{}/admin/v2/{}/stats?subscriptionBacklogSize=true",
            self.base_url,
            topic.rest_path()
        );
        self.get_json(&url)
    }

    /// Internal stats for a single topic or partition — cursor positions plus
    /// the physical ledger list. Not aggregated across partitions by the
    /// broker, so callers fetch it per partition.
    pub fn internal_stats(&self, topic: &TopicName) -> Result<InternalStats, PulsarError> {
        let url = format!(
            "{}/admin/v2/{}/internalStats",
            self.base_url,
            topic.rest_path()
        );
        self.get_json(&url)
    }

    /// Aggregate stats for a topic (partitioned-stats *without* the per-
    /// partition breakdown, or plain stats for a single partition). Cheap
    /// second-sample fetch: we only need the subscription's `msgBacklog`.
    /// Returns `NotFound` for non-partitioned base topics so callers can fall
    /// back to plain `stats`.
    pub fn aggregate_stats(&self, topic: &TopicName) -> Result<TopicStats, PulsarError> {
        let url = format!(
            "{}/admin/v2/{}/partitioned-stats",
            self.base_url,
            topic.rest_path()
        );
        self.get_json(&url)
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, PulsarError> {
        let response = self
            .agent
            .get(url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .call();

        match response {
            Ok(resp) => Ok(resp.into_json::<T>()?),
            Err(ureq::Error::Status(404, _)) => Err(PulsarError::NotFound),
            Err(ureq::Error::Status(status, resp)) => {
                let body = resp
                    .into_string()
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                Err(PulsarError::Status { status, body })
            }
            Err(ureq::Error::Transport(t)) => Err(PulsarError::Transport(t.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_topic_name() {
        let t = TopicName::parse("persistent://widgetco/toybox/marbles").unwrap();
        assert_eq!(t.tenant, "widgetco");
        assert_eq!(t.local, "marbles");
        assert!(!t.is_partition());
    }

    #[test]
    fn assumes_persistent_when_schemeless() {
        let t = TopicName::parse("widgetco/toybox/robots-partition-3").unwrap();
        assert_eq!(t.domain, "persistent");
        assert!(t.is_partition());
    }

    #[test]
    fn rejects_malformed_names() {
        assert!(TopicName::parse("http://a/b/c").is_err());
        assert!(TopicName::parse("just-a-topic").is_err());
        assert!(TopicName::parse("tenant//topic").is_err());
    }

    #[test]
    fn parses_and_orders_positions() {
        let a: Position = "49:-1".parse().unwrap();
        let b: Position = "49:0".parse().unwrap();
        let c: Position = "65:0".parse().unwrap();
        assert!(a < b, "-1 entry precedes entry 0 in same ledger");
        assert!(b < c, "lower ledger precedes higher ledger");
        assert_eq!(a.to_string(), "49:-1");
    }

    #[test]
    fn rejects_malformed_positions() {
        assert!("nope".parse::<Position>().is_err());
        assert!("1:".parse::<Position>().is_err());
    }

    #[test]
    fn deserialises_internal_stats() {
        let json = r#"{
            "lastConfirmedEntry": "65:4211",
            "ledgers": [
                {"ledgerId": 49, "entries": 1},
                {"ledgerId": 65, "entries": 0}
            ],
            "cursors": {
                "my-sub": {
                    "markDeletePosition": "49:-1",
                    "readPosition": "49:0",
                    "waitingReadOp": false
                }
            }
        }"#;
        // Unmodelled keys (lastConfirmedEntry, markDeletePosition) are ignored.
        let stats: InternalStats = serde_json::from_str(json).unwrap();
        assert_eq!(stats.floor_ledger_id(), Some(49));
        let cursor = &stats.cursors["my-sub"];
        assert_eq!(cursor.read(), Some(Position { ledger_id: 49, entry_id: 0 }));
    }

    #[test]
    fn deserialises_map_style_subscriptions() {
        let json = r#"{
            "msgRateOut": 12.5,
            "subscriptions": {
                "my-sub": {
                    "msgBacklog": 42,
                    "consumers": [{"unackedMessages": 7}, {"unackedMessages": 3}]
                }
            }
        }"#;
        let stats: TopicStats = serde_json::from_str(json).unwrap();
        let sub = &stats.subscriptions["my-sub"];
        assert_eq!(sub.msg_backlog, 42);
        assert_eq!(sub.unacked_total(), 10);
    }
}
