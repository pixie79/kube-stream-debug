//! Kubernetes correlation: pod health for the consumer deployment, so a
//! broker-side symptom (NO_CONSUMERS, growing backlog) can be tied to its
//! cause (an OOM-killed or mid-ramp pod).
//!
//! This module is split deliberately:
//! * The **pure** half — plain structs plus the logic that classifies pods,
//!   computes rollout skew, scans logs for signals, asserts config, and
//!   correlates pods to the subscription — has no I/O and is fully unit-tested.
//! * The **client** half (see `client` submodule, compiled only with the
//!   `kube` feature) turns live `k8s-openapi` objects into these plain structs.
//!
//! Keeping the API-object handling to a thin conversion layer means almost all
//! behaviour is testable without a cluster.
//!
//! The pure functions below are consumed by the feature-gated `client`
//! submodule and by tests; without the `kube` feature the binary doesn't call
//! them, so suppress dead-code warnings in that configuration rather than
//! littering each item.
#![cfg_attr(not(feature = "kube"), allow(dead_code))]

use serde::Serialize;

// ─────────────────────────────────────────────────────────────────────────────
// Plain data model (no kube types) — this is what the rest of the tool sees.
// ─────────────────────────────────────────────────────────────────────────────

/// A single pod's health, distilled from its container statuses.
#[derive(Debug, Clone, Serialize)]
pub struct PodSummary {
    pub name: String,
    /// containers ready / total.
    pub ready: u32,
    pub total_containers: u32,
    pub restarts: i32,
    /// Age in seconds since creation (None if unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<i64>,
    /// Container image of the first (app) container, for drift detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// High-level pod phase (Running/Pending/…); waiting/terminated reason when
    /// a container is not ready (e.g. CrashLoopBackOff, OOMKilled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// True if any container's last termination was OOMKilled.
    pub oom_killed: bool,
    /// True if this pod's logs show a transform/DLQ error — silent data loss.
    /// Set after log analysis; drives a distinct pod status in the table.
    #[serde(default)]
    pub transform_error: bool,
    /// True if this pod's logs show a pre-OOM memory-pressure warning — the
    /// chance to act before the kernel OOM-kills it. Drives a red pod status.
    #[serde(default)]
    pub memory_pressure: bool,
    /// Node this pod is scheduled on (for node-size correlation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Live CPU usage in millicores (from the metrics API); None if unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_used_milli: Option<i64>,
    /// Live memory usage in bytes (from the metrics API); None if unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_used_bytes: Option<i64>,
    /// Summed container CPU request/limit in millicores (from the pod spec).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_request_milli: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_limit_milli: Option<i64>,
    /// Summed container memory request/limit in bytes (from the pod spec).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_request_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_limit_bytes: Option<i64>,
}

impl PodSummary {
    pub fn all_ready(&self) -> bool {
        self.total_containers > 0 && self.ready == self.total_containers
    }

    /// Memory usage as a fraction of limit (0.0–1.0+), if both are known.
    pub fn mem_fraction(&self) -> Option<f64> {
        match (self.mem_used_bytes, self.mem_limit_bytes) {
            (Some(used), Some(limit)) if limit > 0 => Some(used as f64 / limit as f64),
            _ => None,
        }
    }

    /// CPU usage as a fraction of limit (0.0–1.0+), if both are known.
    pub fn cpu_fraction(&self) -> Option<f64> {
        match (self.cpu_used_milli, self.cpu_limit_milli) {
            (Some(used), Some(limit)) if limit > 0 => Some(used as f64 / limit as f64),
            _ => None,
        }
    }
}

/// A node's allocatable capacity, for showing pod resource use in context.
#[derive(Debug, Clone, Serialize)]
pub struct NodeInfo {
    pub name: String,
    /// Allocatable CPU in millicores and memory in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alloc_cpu_milli: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alloc_mem_bytes: Option<i64>,
    /// Instance type label, if present (e.g. m5.xlarge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_type: Option<String>,
}

/// A Kubernetes event we care about (OOMKilling, Evicted, etc.).
#[derive(Debug, Clone, Serialize)]
pub struct KubeEvent {
    pub reason: String,
    pub message: String,
    /// Object the event is about (e.g. pod name).
    pub involved: String,
    /// Age of the event in seconds (None if unknown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<i64>,
}

/// Result of asserting one expected config.toml key=value.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigAssertion {
    pub key: String,
    pub expected: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    pub pass: bool,
}

/// A log signal matched in a pod's recent logs.
#[derive(Debug, Clone, Serialize)]
pub struct LogSignal {
    pub pod: String,
    /// The category that matched (e.g. "OOM", "error", "ramp", "config").
    pub kind: String,
    /// The matched line (trimmed).
    pub line: String,
}

/// Aggregate statistics computed from a pod's (or the fleet's) OTEL JSON logs.
#[derive(Debug, Clone, Serialize, Default)]
pub struct LogStats {
    /// Total log lines parsed.
    pub total: usize,
    /// Count per level (ERROR/WARN/INFO/…), highest-severity first when rendered.
    pub by_level: Vec<(String, usize)>,
    /// Top normalised messages by frequency (message text → count).
    pub top_messages: Vec<(String, usize)>,
    /// Operational tallies extracted by pattern (e.g. "subscribe ok",
    /// "subscribe failed", "disconnect", "ServiceNotReady").
    pub operational: Vec<(String, usize)>,
    /// Latest RSS reading in MB seen in a pipeline health summary, if any, plus
    /// the earliest, so the caller can show a trend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_first_mb: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_last_mb: Option<i64>,
    /// Latest throughput_rps seen, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_throughput_rps: Option<i64>,
    /// Count of transform / data-quality / DLQ-diversion errors seen. These are
    /// escalated above the normal operational tallies because they usually mean
    /// silent data loss (rows diverted to a dead-letter queue while the pipeline
    /// reports healthy). Non-zero drives a prominent alert and a pod status.
    pub transform_errors: usize,
    /// Count of pre-OOM memory-pressure warnings ("RSS exceeds 70%", "OOM kill
    /// imminent"). These fire *before* the kernel OOM-kills the pod, so catching
    /// them is the chance to act before the crash.
    pub oom_warnings: usize,
    /// Count of backpressure / throttle signals (an internal channel near-full,
    /// e.g. "decoded_channel", plus a throttle/backpressure marker). Precedes a
    /// throughput stall.
    pub backpressure: usize,
    /// Count of consumer reconnect events (broker-closed, disconnected, TLS
    /// EOF). A high count over a short window is a reconnect *storm* — see
    /// `is_reconnect_storm`.
    pub reconnects: usize,
    /// True when the pod had throughput and then dropped to zero — a pod that
    /// *stopped*, distinct from one idle since start. Derived from the first and
    /// last throughput readings in the window.
    pub throughput_collapsed: bool,
}

/// One rendered metric line for a pod: label, current value, a trend arrow
/// (↑/↓/→), and flags for breach/worsening/improving. A flat, serializable view
/// of a `metrics::MetricVerdict`, kept here so the report has no dependency on
/// the metrics module's internal types.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MetricLine {
    pub label: String,
    pub value: f64,
    pub arrow: String,
    pub breached: bool,
    pub worsening: bool,
    pub improving: bool,
    /// Functional category ("consumer" / "throughput" / "bottleneck" /
    /// "health"), for the grouped fleet summary.
    #[serde(default)]
    pub category: String,
    /// Whether the value moved since the previous scrape (arrow shown only if so).
    #[serde(default)]
    pub changed: bool,
    /// Whether a higher-better metric is sitting at zero (pipeline not moving).
    #[serde(default)]
    pub stalled: bool,
    /// Whether the value is a per-scrape rate (counter), for a "/s" suffix.
    #[serde(default)]
    pub is_rate: bool,
    /// False when the scrape returned no sample — rendered as "(no data)".
    #[serde(default)]
    pub present: bool,
}

/// The stability view's per-pod row: churn rates and the flapping verdicts. A
/// flat, serializable mirror of `metrics::PodStability`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PodStabilityLine {
    pub pod: String,
    pub reconnect_rate: f64,
    pub throttle_transition_rate: f64,
    pub active_parts_churn: f64,
    pub active_parts: f64,
    pub idle_cull_rate: f64,
    pub idle_cull_threshold_secs: f64,
    pub flapping_rate: bool,
    pub flapping_rebalance: bool,
    pub idle_cull_loop: bool,
}

/// The curated metric summary for one pod.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PodMetricSummary {
    pub pod: String,
    pub lines: Vec<MetricLine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
}

/// Raw scraped metrics for one pod: the Prometheus `/metrics` text and the
/// `/health` body. Parsed and trended by the `metrics` module.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PodMetrics {
    pub pod: String,
    pub metrics_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
}

/// Raw recent log lines for one pod, retained for the node-detail view.
#[derive(Debug, Clone, Serialize, Default)]
pub struct PodLogs {
    pub pod: String,
    pub lines: Vec<String>,
}

/// The full Kubernetes-side picture, assembled for rendering.
#[derive(Debug, Clone, Serialize, Default)]
pub struct KubeReport {
    pub namespace: String,
    pub pods: Vec<PodSummary>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<KubeEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub config_assertions: Vec<ConfigAssertion>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub log_signals: Vec<LogSignal>,
    /// Aggregate log statistics across all pods (summary view).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_stats: Option<LogStats>,
    /// Raw recent logs per pod, for the node-detail drill-down.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pod_logs: Vec<PodLogs>,
    /// Scraped per-pod metrics (Prometheus text + health), when metrics scraping
    /// is enabled. Empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pod_metrics: Vec<PodMetrics>,
    /// Per-pod curated metric summaries (label, value, trend, breach), computed
    /// from the scrape and carried here so the TUI can render them. Empty when
    /// metrics scraping is off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pod_metric_summaries: Vec<PodMetricSummary>,
    /// Per-pod connection-stability verdicts (flapping detection). Empty when
    /// metrics scraping is off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pod_stability: Vec<PodStabilityLine>,
    /// Distinct images across pods; more than one means a split rollout.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Node capacity for nodes the pods run on (keyed by name via the vec).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeInfo>,
    /// Seconds between the oldest and newest pod creation — rollout skew.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollout_skew_secs: Option<i64>,
    /// Populated if the client couldn't reach the cluster; the Pulsar side of
    /// the report is still valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Populated when the namespace or selector matched nothing: what the user
    /// could have selected instead. Drives the "here's what exists" help.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<Discovery>,
}

/// Help emitted when a `--kube` run matches no pods: either the namespace
/// doesn't exist (list what does) or it exists but the selector missed (list
/// the labels present on its pods).
#[derive(Debug, Clone, Serialize)]
pub enum Discovery {
    /// The namespace wasn't found; these are the available namespaces.
    Namespaces(Vec<String>),
    /// The namespace exists but the selector matched nothing; these are the
    /// label keys and their distinct values across pods in the namespace.
    Labels {
        namespace: String,
        /// (key, sorted distinct values) pairs, sorted by key.
        keys: Vec<(String, Vec<String>)>,
        /// A ready-to-paste selector suggestion if an obvious app label exists.
        #[serde(skip_serializing_if = "Option::is_none")]
        suggestion: Option<String>,
    },
}

impl KubeReport {
    pub fn unreachable(namespace: &str, error: String) -> Self {
        KubeReport {
            namespace: namespace.to_string(),
            error: Some(error),
            ..Default::default()
        }
    }

    /// Overall consumer-side health verdict for the summary line and exit code.
    pub fn is_healthy(&self) -> bool {
        self.error.is_none()
            && !self.pods.is_empty()
            && self.pods.iter().all(|p| p.all_ready() && !p.oom_killed)
            && self.config_assertions.iter().all(|a| a.pass)
            && self.images.len() <= 1
    }

    /// A one-line human summary of the consumer side.
    pub fn summary_line(&self) -> String {
        if let Some(err) = &self.error {
            return format!("kube: unreachable ({err})");
        }
        let ready = self.pods.iter().filter(|p| p.all_ready()).count();
        let ooms = self.pods.iter().filter(|p| p.oom_killed).count();
        let mut parts = vec![format!("{}/{} pods ready", ready, self.pods.len())];
        if ooms > 0 {
            parts.push(format!("{ooms} OOM-killed"));
        }
        if self.images.len() > 1 {
            parts.push(format!("{} image versions (split rollout)", self.images.len()));
        }
        let failed_cfg = self.config_assertions.iter().filter(|a| !a.pass).count();
        if failed_cfg > 0 {
            parts.push(format!("{failed_cfg} config assertion(s) failed"));
        }
        parts.join(", ")
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure logic (unit-tested)
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a Kubernetes CPU quantity into millicores. Accepts `"250m"` (milli),
/// `"1"` / `"2"` (whole cores), `"1500m"`, and fractional `"0.5"`. Returns None
/// on anything unparseable.
pub fn parse_cpu_milli(q: &str) -> Option<i64> {
    let q = q.trim();
    if let Some(m) = q.strip_suffix('m') {
        return m.trim().parse::<i64>().ok();
    }
    // Whole or fractional cores → millicores.
    q.parse::<f64>().ok().map(|cores| (cores * 1000.0).round() as i64)
}

/// Parse a Kubernetes memory quantity into bytes. Accepts binary suffixes
/// (Ki, Mi, Gi, Ti, Pi), decimal suffixes (k, M, G, T, P), and a bare number.
pub fn parse_mem_bytes(q: &str) -> Option<i64> {
    let q = q.trim();
    let binary = [("Ki", 1i64 << 10), ("Mi", 1 << 20), ("Gi", 1 << 30), ("Ti", 1i64 << 40), ("Pi", 1i64 << 50)];
    for (suffix, mult) in binary {
        if let Some(n) = q.strip_suffix(suffix) {
            return n.trim().parse::<f64>().ok().map(|v| (v * mult as f64) as i64);
        }
    }
    let decimal = [("k", 1_000i64), ("M", 1_000_000), ("G", 1_000_000_000), ("T", 1_000_000_000_000), ("P", 1_000_000_000_000_000)];
    for (suffix, mult) in decimal {
        if let Some(n) = q.strip_suffix(suffix) {
            return n.trim().parse::<f64>().ok().map(|v| (v * mult as f64) as i64);
        }
    }
    q.parse::<i64>().ok()
}

/// Format millicores compactly: `250m`, `1.5` (cores when ≥1000m).
pub fn format_cpu(milli: i64) -> String {
    if milli >= 1000 && milli % 1000 == 0 {
        format!("{}", milli / 1000)
    } else if milli >= 1000 {
        format!("{:.1}", milli as f64 / 1000.0)
    } else {
        format!("{milli}m")
    }
}

/// Sum a per-container resource across a pod, via a picker returning the raw
/// quantity string. None if no container declares it.
pub fn sum_quantity<'a>(
    containers: impl Iterator<Item = Option<&'a str>>,
    parse: fn(&str) -> Option<i64>,
) -> Option<i64> {
    let mut total = 0i64;
    let mut any = false;
    for q in containers.flatten() {
        if let Some(v) = parse(q) {
            total += v;
            any = true;
        }
    }
    any.then_some(total)
}

/// Build the label-discovery from the label maps of pods in a namespace.
/// Collects each key's distinct values (sorted), sorts keys, and suggests a
/// ready-to-paste selector when a conventional app label is present.
pub fn labels_discovery(
    namespace: &str,
    pod_label_maps: &[std::collections::BTreeMap<String, String>],
) -> Discovery {
    use std::collections::BTreeMap;
    let mut by_key: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for map in pod_label_maps {
        for (k, v) in map {
            by_key.entry(k.clone()).or_default().insert(v.clone());
        }
    }
    let keys: Vec<(String, Vec<String>)> = by_key
        .into_iter()
        .map(|(k, vs)| (k, vs.into_iter().collect()))
        .collect();
    let suggestion = suggest_selector(&keys);
    Discovery::Labels {
        namespace: namespace.to_string(),
        keys,
        suggestion,
    }
}

/// Suggest a selector from harvested labels, preferring the conventional
/// `app.kubernetes.io/name`, then `app`, then `k8s-app`. Only suggests when the
/// chosen key has exactly one value (an unambiguous target).
fn suggest_selector(keys: &[(String, Vec<String>)]) -> Option<String> {
    for preferred in ["app.kubernetes.io/name", "app", "k8s-app"] {
        if let Some((k, vs)) = keys.iter().find(|(k, _)| k == preferred)
            && vs.len() == 1 {
                return Some(format!("{k}={}", vs[0]));
            }
    }
    None
}

/// Render a `Discovery` as plain help text for a one-shot run.
pub fn format_discovery(d: &Discovery) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    match d {
        Discovery::Namespaces(names) => {
            if names.is_empty() {
                out.push_str("  no namespaces found (or insufficient permissions)\n");
            } else {
                let _ = writeln!(out, "  namespace not found. Available namespaces:");
                for n in names {
                    let _ = writeln!(out, "    {n}");
                }
            }
        }
        Discovery::Labels {
            namespace,
            keys,
            suggestion,
        } => {
            if keys.is_empty() {
                let _ = writeln!(
                    out,
                    "  no pods matched, and no labelled pods found in {namespace}."
                );
            } else {
                let _ = writeln!(
                    out,
                    "  no pods matched the selector. Labels present on pods in {namespace}:"
                );
                for (k, vs) in keys {
                    // Cap the values shown so a high-cardinality label (e.g.
                    // pod-template-hash) doesn't flood the output.
                    let shown: Vec<String> = vs.iter().take(6).cloned().collect();
                    let more = vs.len().saturating_sub(shown.len());
                    let suffix = if more > 0 { format!(" (+{more} more)") } else { String::new() };
                    let _ = writeln!(out, "    {k} = {}{}", shown.join(", "), suffix);
                }
                if let Some(s) = suggestion {
                    let _ = writeln!(out, "\n  try: --kube-selector {s}");
                }
            }
        }
    }
    out
}

/// Distinct images across pods, sorted, deduplicated. >1 ⇒ split rollout.
pub fn distinct_images(pods: &[PodSummary]) -> Vec<String> {
    let mut images: Vec<String> = pods.iter().filter_map(|p| p.image.clone()).collect();
    images.sort();
    images.dedup();
    images
}

/// Rollout skew: seconds between the youngest and oldest pod. A large skew means
/// pods didn't restart together — a rollout may be incomplete or a pod is stale.
pub fn rollout_skew_secs(pods: &[PodSummary]) -> Option<i64> {
    let ages: Vec<i64> = pods.iter().filter_map(|p| p.age_secs).collect();
    let (min, max) = (ages.iter().min()?, ages.iter().max()?);
    Some(max - min)
}

/// Assert expected `key=value` pairs against a parsed config.toml body. Uses a
/// lenient scan: for each key, find `key = value` (TOML-ish) and compare the
/// value with surrounding quotes/whitespace stripped. This avoids a full TOML
/// parse so it works even if the config has sections/types we don't model.
pub fn assert_config(config_toml: &str, expected: &[(String, String)]) -> Vec<ConfigAssertion> {
    expected
        .iter()
        .map(|(key, want)| {
            let actual = find_toml_value(config_toml, key);
            let pass = actual.as_deref() == Some(want.as_str());
            ConfigAssertion {
                key: key.clone(),
                expected: want.clone(),
                actual,
                pass,
            }
        })
        .collect()
}

/// Find a scalar value for `key` in a TOML body: the first line matching
/// `key = <value>` (ignoring leading whitespace), with quotes and trailing
/// comments stripped. Returns None if absent.
fn find_toml_value(body: &str, key: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                return Some(clean_toml_value(value));
            }
        }
    }
    None
}

fn clean_toml_value(raw: &str) -> String {
    let mut v = raw.trim();
    // Strip an inline comment (naive: a ` #` not inside quotes). Good enough for
    // scalar config values.
    if !v.starts_with('"')
        && let Some(idx) = v.find(" #") {
            v = v[..idx].trim();
        }
    v.trim().trim_matches('"').trim().to_string()
}

/// Log-signal categories and the substrings that match them. Case-insensitive.
/// Kept as (kind, needles) so the set is easy to extend.
pub fn signal_patterns() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        ("OOM", &["oomkill", "exit 137", "killed"]),
        ("error", &["error", "panic", "fatal"]),
        ("ramp", &["subscrib", "partition batch", "dropping partition batch", "ramp"]),
        ("config", &["batcher_worker", "auto-tune", "auto.tun"]),
        ("no_consumers", &["no_consumers"]),
    ]
}

/// Scan a pod's recent log text for known signals. Returns one `LogSignal` per
/// matching line (a line may match multiple categories → multiple entries).
pub fn scan_log_signals(pod: &str, log_text: &str) -> Vec<LogSignal> {
    let mut out = Vec::new();
    for line in log_text.lines() {
        let lower = line.to_lowercase();
        for (kind, needles) in signal_patterns() {
            if needles.iter().any(|n| lower.contains(n)) {
                out.push(LogSignal {
                    pod: pod.to_string(),
                    kind: (*kind).to_string(),
                    line: line.trim().to_string(),
                });
            }
        }
    }
    out
}

/// Aggregate OTEL JSON log lines (across pods) into summary statistics: level
/// counts, top messages, operational tallies, and RSS/throughput trend. Lines
/// that aren't JSON are still counted in `total` but contribute no level.
///
/// Message normalisation strips the volatile bits (UUIDs, consumer ids, topic
/// partitions, numbers) so "closed consumer 27 …partition-1" and "closed
/// consumer 36 …partition-3" collapse into one ranked entry.
/// A compact one-line summary of an OTEL JSON log line for the scannable list:
/// `LEVEL  message` (message truncated). Falls back to the raw line when it
/// isn't JSON. Pure and testable.
pub fn log_line_summary(line: &str, max_len: usize) -> String {
    let (level, message) = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) => {
            let level = v.get("level").and_then(|l| l.as_str()).unwrap_or("").to_string();
            let msg = v
                .get("fields")
                .and_then(|f| f.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            (level, msg)
        }
        Err(_) => return truncate_str(line, max_len),
    };
    let mut s = if level.is_empty() {
        message
    } else {
        format!("{level:<5} {message}")
    };
    s = truncate_str(&s, max_len);
    s
}

/// Pretty-print an OTEL JSON log line into indented key/value lines for the
/// expanded detail view: timestamp, level, message, error (if any), then the
/// remaining fields. Non-JSON lines are returned as-is. Pure and testable.
pub fn pretty_log_line(line: &str) -> Vec<String> {
    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![line.to_string()],
    };
    let mut out = Vec::new();
    if let Some(ts) = value.get("timestamp").and_then(|v| v.as_str()) {
        out.push(format!("timestamp: {ts}"));
    }
    if let Some(level) = value.get("level").and_then(|v| v.as_str()) {
        out.push(format!("level:     {level}"));
    }
    if let Some(target) = value.get("target").and_then(|v| v.as_str()) {
        out.push(format!("target:    {target}"));
    }
    if let Some(fields) = value.get("fields").and_then(|f| f.as_object()) {
        // message and error first (the things you actually want to read), then
        // the rest of the fields.
        if let Some(msg) = fields.get("message").and_then(|m| m.as_str()) {
            out.push(format!("message:   {msg}"));
        }
        if let Some(err) = fields.get("error").and_then(|e| e.as_str()) {
            out.push(format!("error:     {err}"));
        }
        for (k, v) in fields {
            if k == "message" || k == "error" {
                continue;
            }
            out.push(format!("  {k}: {}", json_scalar(v)));
        }
    }
    out
}

/// Render a JSON scalar/compact value for the detail view.
fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn truncate_str(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_len.saturating_sub(1)).collect();
    out.push('…');
    out
}

pub fn aggregate_log_stats(lines: &[String], top_n: usize) -> LogStats {
    use std::collections::HashMap;

    let mut stats = LogStats {
        total: lines.len(),
        ..Default::default()
    };
    let mut levels: HashMap<String, usize> = HashMap::new();
    let mut messages: HashMap<String, usize> = HashMap::new();
    let mut ops: HashMap<&'static str, usize> = HashMap::new();
    // First throughput reading in the window, to detect a collapse to zero.
    let mut first_throughput: Option<i64> = None;

    for line in lines {
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(level) = value.get("level").and_then(|v| v.as_str()) {
            *levels.entry(level.to_string()).or_default() += 1;
        }
        let message = value
            .get("fields")
            .and_then(|f| f.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        if !message.is_empty() {
            *messages.entry(normalise_message(message)).or_default() += 1;
            tally_operational(message, &mut ops);
            if is_transform_error(message) {
                stats.transform_errors += 1;
            }
            if is_oom_warning(message) {
                stats.oom_warnings += 1;
            }
            if is_backpressure(message) {
                stats.backpressure += 1;
            }
            if is_reconnect(message) {
                stats.reconnects += 1;
            }
        }
        // RSS + throughput from "Pipeline health summary" lines.
        if let Some(fields) = value.get("fields") {
            if let Some(rss) = fields.get("rss_mb").and_then(|v| v.as_i64()) {
                stats.rss_first_mb.get_or_insert(rss);
                stats.rss_last_mb = Some(rss);
            }
            if let Some(rps) = fields.get("throughput_rps").and_then(json_as_i64) {
                first_throughput.get_or_insert(rps);
                stats.last_throughput_rps = Some(rps);
            }
        }
    }

    // Throughput collapse: the pod was doing work (first reading > 0) and has
    // since dropped to zero. Distinct from a pod idle since start (first == 0),
    // which isn't an incident.
    if let (Some(first), Some(last)) = (first_throughput, stats.last_throughput_rps) {
        stats.throughput_collapsed = first > 0 && last == 0;
    }

    stats.by_level = sort_desc(levels);
    stats.top_messages = truncate(sort_desc(messages), top_n);
    stats.operational = sort_desc(ops.into_iter().map(|(k, v)| (k.to_string(), v)).collect());
    stats
}

/// throughput_rps is emitted as a quoted string ("0") in these logs; accept
/// both string and number.
fn json_as_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn normalise_token(token: &str) -> String {
    // partition suffix anywhere in the token.
    if let Some(idx) = token.find("-partition-") {
        return format!("{}-partition-<n>", &token[..idx]);
    }
    let hexish = token.len() >= 6
        && token.chars().all(|c| c.is_ascii_hexdigit() || c == '-' || c == '_')
        && token.chars().any(|c| c.is_ascii_digit());
    if hexish {
        return "<id>".to_string();
    }
    if !token.is_empty() && token.chars().all(|c| c.is_ascii_digit()) {
        return "<n>".to_string();
    }
    token.to_string()
}

/// Replace embedded volatile substrings (consumer ids, topic paths) that don't
/// sit on whitespace boundaries, before per-token normalisation.
fn prenormalise(msg: &str) -> String {
    let mut s = String::with_capacity(msg.len());
    let bytes = msg.as_bytes();
    let mut i = 0;
    while i < msg.len() {
        // consumer_<alnum>
        if msg[i..].starts_with("consumer_") {
            s.push_str("consumer_<id>");
            i += "consumer_".len();
            while i < msg.len() && (bytes[i] as char).is_ascii_alphanumeric() {
                i += 1;
            }
            continue;
        }
        // persistent:// or non-persistent:// topic path (up to a delimiter)
        let topic_prefix = if msg[i..].starts_with("persistent://") {
            Some("persistent://")
        } else if msg[i..].starts_with("non-persistent://") {
            Some("non-persistent://")
        } else {
            None
        };
        if let Some(prefix) = topic_prefix {
            // Consume the path, but keep a trailing -partition-<n> marker if present.
            let start = i;
            i += prefix.len();
            while i < msg.len() && !matches!(bytes[i] as char, ' ' | ']' | ')' | ',') {
                i += 1;
            }
            let path = &msg[start..i];
            if path.contains("-partition-") {
                s.push_str("<topic>-partition-<n>");
            } else {
                s.push_str("<topic>");
            }
            continue;
        }
        let ch = msg[i..].chars().next().unwrap();
        s.push(ch);
        i += ch.len_utf8();
    }
    s
}

/// Collapse a message to a stable shape for frequency counting: replace UUIDs,
/// hex ids, consumer names, partition suffixes, and bare numbers with markers.
fn normalise_message(msg: &str) -> String {
    let pre = prenormalise(msg);
    let mut out = String::with_capacity(pre.len());
    for token in pre.split_whitespace() {
        let (lead, core, trail) = split_affixes(token);
        let norm = normalise_token(core);
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(lead);
        out.push_str(&norm);
        out.push_str(trail);
    }
    if out.len() > 120 {
        out.truncate(117);
        out.push_str("...");
    }
    out
}

/// Split leading/trailing non-alphanumeric punctuation off a token, returning
/// (leading, core, trailing). Angle-bracket markers like `<id>` are preserved.
fn split_affixes(token: &str) -> (&str, &str, &str) {
    if token.starts_with('<') && token.ends_with('>') {
        return ("", token, "");
    }
    let is_edge = |c: char| !c.is_ascii_alphanumeric() && c != '<' && c != '>';
    let start = token.find(|c: char| !is_edge(c)).unwrap_or(token.len());
    let end = token
        .rfind(|c: char| !is_edge(c))
        .map(|i| i + token[i..].chars().next().map(char::len_utf8).unwrap_or(1))
        .unwrap_or(start);
    (&token[..start], &token[start..end], &token[end..])
}

/// Operational patterns worth tallying, matched case-insensitively on message.
/// Names of pods whose recent logs contain a transform/DLQ error, so the pod
/// table can flag them distinctly. Pure over the raw per-pod logs.
pub fn pods_with_transform_errors(pod_logs: &[PodLogs]) -> Vec<String> {
    pods_matching(pod_logs, is_transform_error)
}

/// Extract the human message from a log line (JSON `fields.message`, else the
/// raw line), so signal predicates can run against it.
fn line_message(line: &str) -> String {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("fields")
                .and_then(|f| f.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| line.to_string())
}

/// Names of pods any of whose recent log lines match `pred`. Shared by every
/// per-pod signal (transform errors, OOM warnings, backpressure, …).
pub fn pods_matching(pod_logs: &[PodLogs], pred: fn(&str) -> bool) -> Vec<String> {
    pod_logs
        .iter()
        .filter(|pl| pl.lines.iter().any(|line| pred(&line_message(line))))
        .map(|pl| pl.pod.clone())
        .collect()
}

/// Whether a log message indicates a transform / data-quality / DLQ-diversion
/// error — the class that usually means silent data loss (rows sent to a
/// dead-letter queue, or a transform failing outright) while the pipeline still
/// reports healthy. Matched on stable substrings so the specific SQL/error text
/// doesn't matter.
pub fn is_transform_error(message: &str) -> bool {
    let m = message.to_lowercase();
    const NEEDLES: &[&str] = &[
        "capturing dropped rows to dlq",
        "transform/dq error",
        "datafusion error",
        "parsererror",
        "sql error",
        "dropped rows to dlq",
    ];
    NEEDLES.iter().any(|n| m.contains(n))
}

/// Pre-OOM memory-pressure warning: the pipeline warns that RSS has crossed a
/// fraction of the cgroup limit and an OOM kill is imminent. Catching this is
/// the chance to act *before* the kernel kills the pod (after which k8s reports
/// OOMKilled, but the data's already lost).
pub fn is_oom_warning(message: &str) -> bool {
    let m = message.to_lowercase();
    const NEEDLES: &[&str] = &[
        "oom kill imminent",
        "rss exceeds",
        "high memory warning",
        "memory pressure",
    ];
    NEEDLES.iter().any(|n| m.contains(n))
}

/// Backpressure / throttle: an internal channel is near-full (rows backing up
/// faster than the sink drains), which precedes a throughput stall.
pub fn is_backpressure(message: &str) -> bool {
    let m = message.to_lowercase();
    // A channel-fullness marker, or an explicit throttle/backpressure log.
    (m.contains("channel") && (m.contains("full") || m.contains("% full")))
        || m.contains("backpressure")
        || m.contains("throttle")
}

/// A single reconnect event (broker-closed consumer, disconnect, TLS EOF). A
/// high count of these over one log window is a reconnect *storm* — see
/// `is_reconnect_storm`.
pub fn is_reconnect(message: &str) -> bool {
    let m = message.to_lowercase();
    const NEEDLES: &[&str] = &[
        "broker notification of closed consumer",
        "is not valid: disconnected",
        "unexpectedeof",
        "reconnecting",
        "reconnect",
    ];
    NEEDLES.iter().any(|n| m.contains(n))
}

/// Whether a reconnect count constitutes a storm rather than incidental churn.
/// Threshold chosen so a couple of reconnects don't alarm, but a burst does.
pub fn is_reconnect_storm(reconnects: usize) -> bool {
    reconnects >= 10
}

fn tally_operational(message: &str, ops: &mut std::collections::HashMap<&'static str, usize>) {
    let m = message.to_lowercase();
    let checks: &[(&str, &'static str)] = &[
        ("success after", "subscribe ok"),
        ("servicenotready", "subscribe failed (ServiceNotReady)"),
        ("broker notification of closed consumer", "consumer closed by broker"),
        ("could not close consumer", "consumer close failed"),
        ("is not valid: disconnected", "disconnected"),
        ("unexpectedeof", "TLS EOF"),
        ("oomkill", "OOMKill"),
        ("hash_salt is absent", "HASH_SALT absent"),
    ];
    for (needle, label) in checks {
        if m.contains(needle) {
            *ops.entry(label).or_default() += 1;
        }
    }
}

fn sort_desc(map: std::collections::HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = map.into_iter().collect();
    // Highest count first; ties broken alphabetically for stable output.
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

fn truncate(mut v: Vec<(String, usize)>, n: usize) -> Vec<(String, usize)> {
    v.truncate(n);
    v
}

/// Correlate the consumer deployment's health to a topic, producing a short hint
/// for the topic's DETAIL column. The reasoning: if the subscription shows
/// NO_CONSUMERS or a growing/standing backlog, a pod-side fault is the likely
/// cause, so surface the most relevant pod signal.
///
/// `topic_unhealthy` is whether the topic itself is in a non-OK state, so we
/// only annotate rows where the correlation is actionable.
pub fn correlation_hint(report: &KubeReport, topic_unhealthy: bool) -> Option<String> {
    if report.error.is_some() || !topic_unhealthy {
        return None;
    }
    let oom = report.pods.iter().filter(|p| p.oom_killed).count();
    let not_ready = report.pods.iter().filter(|p| !p.all_ready()).count();

    if oom > 0 {
        Some(format!("kube: {oom} pod(s) OOM-killed"))
    } else if not_ready > 0 {
        Some(format!("kube: {not_ready} pod(s) not ready"))
    } else if report.images.len() > 1 {
        Some("kube: split rollout".to_string())
    } else {
        None
    }
}

#[cfg(feature = "kube")]
pub mod client;
#[cfg(feature = "kube")]
pub mod actions;

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(name: &str, ready: u32, total: u32, restarts: i32, age: i64, image: &str, oom: bool) -> PodSummary {
        PodSummary {
            name: name.to_string(),
            ready,
            total_containers: total,
            restarts,
            age_secs: Some(age),
            image: Some(image.to_string()),
            reason: None,
            oom_killed: oom, transform_error: false, memory_pressure: false,
            node: None,
            cpu_used_milli: None,
            mem_used_bytes: None,
            cpu_request_milli: None,
            cpu_limit_milli: None,
            mem_request_bytes: None,
            mem_limit_bytes: None,
        }
    }

    #[test]
    fn all_ready_reflects_container_counts() {
        assert!(pod("a", 1, 1, 0, 100, "img:1", false).all_ready());
        assert!(!pod("b", 0, 1, 3, 100, "img:1", false).all_ready());
    }

    #[test]
    fn parses_cpu_quantities() {
        assert_eq!(parse_cpu_milli("250m"), Some(250));
        assert_eq!(parse_cpu_milli("1"), Some(1000));
        assert_eq!(parse_cpu_milli("2"), Some(2000));
        assert_eq!(parse_cpu_milli("1500m"), Some(1500));
        assert_eq!(parse_cpu_milli("0.5"), Some(500));
        assert_eq!(parse_cpu_milli("garbage"), None);
    }

    #[test]
    fn parses_mem_quantities() {
        assert_eq!(parse_mem_bytes("1Gi"), Some(1 << 30));
        assert_eq!(parse_mem_bytes("512Mi"), Some(512 * (1 << 20)));
        assert_eq!(parse_mem_bytes("1000000"), Some(1_000_000));
        assert_eq!(parse_mem_bytes("1M"), Some(1_000_000));
        assert_eq!(parse_mem_bytes("28672Mi"), Some(28672 * (1 << 20))); // ~28GiB, matches the sample
        assert_eq!(parse_mem_bytes("nope"), None);
    }

    #[test]
    fn formats_cpu_millicores() {
        assert_eq!(format_cpu(250), "250m");
        assert_eq!(format_cpu(1000), "1");
        assert_eq!(format_cpu(1500), "1.5");
        assert_eq!(format_cpu(2000), "2");
    }

    #[test]
    fn sum_quantity_sums_present_values() {
        let containers = vec![Some("250m"), None, Some("500m")];
        assert_eq!(sum_quantity(containers.into_iter(), parse_cpu_milli), Some(750));
        // All-None → None.
        let empty: Vec<Option<&str>> = vec![None, None];
        assert_eq!(sum_quantity(empty.into_iter(), parse_cpu_milli), None);
    }

    #[test]
    fn resource_fractions() {
        let mut p = pod("a", 1, 1, 0, 100, "i", false);
        p.mem_used_bytes = Some(1500);
        p.mem_limit_bytes = Some(2000);
        assert_eq!(p.mem_fraction(), Some(0.75));
        p.cpu_used_milli = Some(900);
        p.cpu_limit_milli = Some(1000);
        assert_eq!(p.cpu_fraction(), Some(0.9));
        // Missing limit → None.
        p.mem_limit_bytes = None;
        assert_eq!(p.mem_fraction(), None);
    }

    #[test]
    fn detects_oom_warning() {
        assert!(is_oom_warning("HIGH MEMORY WARNING — RSS exceeds 70% of cgroup limit, OOM kill imminent"));
        assert!(is_oom_warning("memory pressure detected"));
        assert!(!is_oom_warning("Pipeline health summary"));
    }

    #[test]
    fn detects_backpressure() {
        assert!(is_backpressure("decoded_channel is 85% full"));
        assert!(is_backpressure("applying backpressure to source"));
        assert!(!is_backpressure("channel opened"));
    }

    #[test]
    fn detects_reconnect_and_storm() {
        assert!(is_reconnect("Broker notification of closed consumer"));
        assert!(is_reconnect("connection is not valid: Disconnected"));
        assert!(!is_reconnect("Pipeline health summary"));
        assert!(!is_reconnect_storm(9));
        assert!(is_reconnect_storm(10));
    }

    #[test]
    fn throughput_collapse_vs_idle() {
        // Was doing 230 rps, dropped to 0 → collapse.
        let collapsed = vec![
            r#"{"level":"INFO","fields":{"message":"Pipeline health summary","throughput_rps":230}}"#.to_string(),
            r#"{"level":"INFO","fields":{"message":"Pipeline health summary","throughput_rps":0}}"#.to_string(),
        ];
        assert!(aggregate_log_stats(&collapsed, 5).throughput_collapsed);

        // Idle since start (0 → 0) → NOT a collapse.
        let idle = vec![
            r#"{"level":"INFO","fields":{"message":"Pipeline health summary","throughput_rps":0}}"#.to_string(),
            r#"{"level":"INFO","fields":{"message":"Pipeline health summary","throughput_rps":0}}"#.to_string(),
        ];
        assert!(!aggregate_log_stats(&idle, 5).throughput_collapsed);
    }

    #[test]
    fn aggregator_counts_new_signals() {
        let lines = vec![
            r#"{"level":"WARN","fields":{"message":"HIGH MEMORY WARNING — RSS exceeds 70%, OOM kill imminent"}}"#.to_string(),
            r#"{"level":"WARN","fields":{"message":"decoded_channel is 85% full"}}"#.to_string(),
            r#"{"level":"WARN","fields":{"message":"Broker notification of closed consumer"}}"#.to_string(),
        ];
        let s = aggregate_log_stats(&lines, 5);
        assert_eq!(s.oom_warnings, 1);
        assert_eq!(s.backpressure, 1);
        assert_eq!(s.reconnects, 1);
    }

    #[test]
    fn detects_transform_error_class() {
        assert!(is_transform_error("Primary transform/DQ error — capturing dropped rows to DLQ"));
        assert!(is_transform_error("DataFusion error: SQL error: ParserError(...)"));
        assert!(is_transform_error("capturing dropped rows to DLQ"));
        // Unrelated errors don't trigger it.
        assert!(!is_transform_error("Consumer: connection is not valid: Disconnected"));
        assert!(!is_transform_error("Pipeline health summary"));
    }

    #[test]
    fn aggregator_counts_transform_errors() {
        let lines = vec![
            r#"{"level":"WARN","fields":{"message":"Primary transform/DQ error — capturing dropped rows to DLQ","error":"DataFusion error: SQL error: ParserError"}}"#.to_string(),
            r#"{"level":"WARN","fields":{"message":"Primary transform/DQ error — capturing dropped rows to DLQ"}}"#.to_string(),
            r#"{"level":"INFO","fields":{"message":"Pipeline health summary","rss_mb":100}}"#.to_string(),
        ];
        let stats = aggregate_log_stats(&lines, 5);
        assert_eq!(stats.transform_errors, 2);
    }

    #[test]
    fn per_pod_transform_error_attribution() {
        let logs = vec![
            PodLogs {
                pod: "pod-good".to_string(),
                lines: vec![r#"{"level":"INFO","fields":{"message":"all fine"}}"#.to_string()],
            },
            PodLogs {
                pod: "pod-bad".to_string(),
                lines: vec![r#"{"level":"WARN","fields":{"message":"capturing dropped rows to DLQ"}}"#.to_string()],
            },
        ];
        let flagged = pods_with_transform_errors(&logs);
        assert_eq!(flagged, vec!["pod-bad"]);
    }

    #[test]
    fn log_line_summary_extracts_level_and_message() {
        let line = r#"{"timestamp":"2026-07-31T08:12:32Z","level":"WARN","fields":{"message":"transform lane Zerobus write error","error":"schema validation failed"},"target":"ssync::sink"}"#;
        let s = log_line_summary(line, 60);
        assert!(s.starts_with("WARN"));
        assert!(s.contains("transform lane Zerobus write error"));
        // Non-JSON falls back to the raw (truncated) line.
        assert_eq!(log_line_summary("plain text here", 60), "plain text here");
    }

    #[test]
    fn log_line_summary_truncates() {
        let long = format!(r#"{{"level":"WARN","fields":{{"message":"{}"}}}}"#, "x".repeat(200));
        let s = log_line_summary(&long, 40);
        assert!(s.chars().count() <= 40);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn pretty_log_line_expands_fields() {
        let line = r#"{"timestamp":"2026-07-31T08:12:32Z","level":"ERROR","fields":{"message":"Stream setup failed","error":"Client field '_ssync_record_id' does not exist in Delta schema"},"target":"ssync::sink::zerobus"}"#;
        let out = pretty_log_line(line);
        let joined = out.join("\n");
        assert!(joined.contains("timestamp: 2026-07-31T08:12:32Z"));
        assert!(joined.contains("level:     ERROR"));
        assert!(joined.contains("message:   Stream setup failed"));
        // The full error — the whole point — is present, untruncated.
        assert!(joined.contains("Client field '_ssync_record_id' does not exist in Delta schema"));
        // Non-JSON returns as-is.
        assert_eq!(pretty_log_line("not json"), vec!["not json"]);
    }

    #[test]
    fn labels_discovery_harvests_and_suggests() {
        use std::collections::BTreeMap;
        let pods = vec![
            BTreeMap::from([
                ("app.kubernetes.io/name".to_string(), "consumer".to_string()),
                ("pod-template-hash".to_string(), "abc".to_string()),
            ]),
            BTreeMap::from([
                ("app.kubernetes.io/name".to_string(), "consumer".to_string()),
                ("pod-template-hash".to_string(), "def".to_string()),
            ]),
        ];
        let d = labels_discovery("ns", &pods);
        match d {
            Discovery::Labels { namespace, keys, suggestion } => {
                assert_eq!(namespace, "ns");
                // app.kubernetes.io/name has one distinct value; hash has two.
                let name_vals = keys.iter().find(|(k, _)| k == "app.kubernetes.io/name").unwrap();
                assert_eq!(name_vals.1, vec!["consumer"]);
                let hash_vals = keys.iter().find(|(k, _)| k == "pod-template-hash").unwrap();
                assert_eq!(hash_vals.1.len(), 2);
                // Suggestion picks the single-valued app label.
                assert_eq!(suggestion.as_deref(), Some("app.kubernetes.io/name=consumer"));
            }
            _ => panic!("expected Labels"),
        }
    }

    #[test]
    fn no_suggestion_when_app_label_ambiguous() {
        use std::collections::BTreeMap;
        // app label has two distinct values → ambiguous → no suggestion.
        let pods = vec![
            BTreeMap::from([("app".to_string(), "a".to_string())]),
            BTreeMap::from([("app".to_string(), "b".to_string())]),
        ];
        match labels_discovery("ns", &pods) {
            Discovery::Labels { suggestion, .. } => assert!(suggestion.is_none()),
            _ => panic!("expected Labels"),
        }
    }

    #[test]
    fn format_discovery_caps_values() {
        use std::collections::BTreeMap;
        // 10 distinct values for one key → capped at 6 with "+4 more".
        let pods: Vec<BTreeMap<String, String>> = (0..10)
            .map(|i| BTreeMap::from([("k".to_string(), format!("v{i}"))]))
            .collect();
        let text = format_discovery(&labels_discovery("ns", &pods));
        assert!(text.contains("+4 more"), "high-cardinality values are capped");
    }

    #[test]
    fn format_discovery_lists_namespaces() {
        let d = Discovery::Namespaces(vec!["a".to_string(), "b".to_string()]);
        let text = format_discovery(&d);
        assert!(text.contains("Available namespaces"));
        assert!(text.contains("  a") && text.contains("  b"));
    }

    #[test]
    fn distinct_images_detects_split() {
        let pods = vec![
            pod("a", 1, 1, 0, 100, "img:1", false),
            pod("b", 1, 1, 0, 100, "img:2", false),
            pod("c", 1, 1, 0, 100, "img:1", false),
        ];
        assert_eq!(distinct_images(&pods), vec!["img:1", "img:2"]);
    }

    #[test]
    fn rollout_skew_spans_ages() {
        let pods = vec![
            pod("a", 1, 1, 0, 100, "i", false),
            pod("b", 1, 1, 0, 340, "i", false),
        ];
        assert_eq!(rollout_skew_secs(&pods), Some(240));
    }

    #[test]
    fn config_assertions_pass_and_fail() {
        let cfg = r#"
            batcher_worker_count = 24
            partition_batch_size = 30
            durable = true
            name = "streamer"
        "#;
        let expected = vec![
            ("batcher_worker_count".to_string(), "24".to_string()),
            ("partition_batch_size".to_string(), "99".to_string()), // wrong
            ("name".to_string(), "streamer".to_string()),           // quoted
            ("missing_key".to_string(), "x".to_string()),           // absent
        ];
        let results = assert_config(cfg, &expected);
        assert!(results[0].pass, "24 == 24");
        assert!(!results[1].pass, "30 != 99");
        assert_eq!(results[1].actual.as_deref(), Some("30"));
        assert!(results[2].pass, "quoted value stripped");
        assert!(!results[3].pass && results[3].actual.is_none(), "absent key fails");
    }

    #[test]
    fn config_value_strips_inline_comment() {
        let cfg = "batcher_worker_count = 24  # tuned for prod\n";
        assert_eq!(find_toml_value(cfg, "batcher_worker_count").as_deref(), Some("24"));
    }

    #[test]
    fn log_scan_categorises_signals() {
        let logs = "\
            2026-07-30 subscribing to 90 partitions\n\
            2026-07-30 ERROR failed to decode\n\
            2026-07-30 batcher_worker_count=24\n\
            2026-07-30 java.lang.OutOfMemory OOMKilled exit 137\n\
            2026-07-30 all good\n";
        let signals = scan_log_signals("pod-1", logs);
        let kinds: Vec<&str> = signals.iter().map(|s| s.kind.as_str()).collect();
        assert!(kinds.contains(&"ramp"));
        assert!(kinds.contains(&"error"));
        assert!(kinds.contains(&"config"));
        assert!(kinds.contains(&"OOM"));
        assert!(signals.iter().all(|s| s.pod == "pod-1"));
    }

    #[test]
    fn aggregates_otel_log_stats() {
        // Shaped like the real OTEL JSON: level, fields.message, rss_mb, rps.
        let lines: Vec<String> = vec![
            r#"{"level":"ERROR","fields":{"message":"Broker notification of closed consumer 27: [27 - sub(consumer_JlObSTqT): persistent://a/b/c-partition-2]"}}"#.to_string(),
            r#"{"level":"ERROR","fields":{"message":"Broker notification of closed consumer 36: [36 - sub(consumer_j7ka0hCw): persistent://a/b/c-partition-3]"}}"#.to_string(),
            r#"{"level":"ERROR","fields":{"message":"TopicConsumer::subscribe(x) answered ServiceNotReady"}}"#.to_string(),
            r#"{"level":"INFO","fields":{"message":"subscribe success after 2 retries"}}"#.to_string(),
            r#"{"level":"INFO","fields":{"message":"Pipeline health summary","rss_mb":232,"throughput_rps":"0"}}"#.to_string(),
            r#"{"level":"INFO","fields":{"message":"Pipeline health summary","rss_mb":1716,"throughput_rps":"0"}}"#.to_string(),
            "not json at all".to_string(),
        ];
        let stats = aggregate_log_stats(&lines, 5);
        assert_eq!(stats.total, 7);

        // Level counts: 3 ERROR, 3 INFO.
        let err = stats.by_level.iter().find(|(l, _)| l == "ERROR").map(|(_, c)| *c);
        assert_eq!(err, Some(3));

        // The two "closed consumer" messages normalise to one entry, count 2.
        let closed = stats
            .top_messages
            .iter()
            .find(|(m, _)| m.contains("closed consumer"))
            .map(|(_, c)| *c);
        assert_eq!(closed, Some(2), "volatile ids collapse into one message");

        // Operational tallies.
        let ops: std::collections::HashMap<_, _> = stats.operational.iter().cloned().collect();
        assert_eq!(ops.get("consumer closed by broker"), Some(&2));
        assert_eq!(ops.get("subscribe failed (ServiceNotReady)"), Some(&1));
        assert_eq!(ops.get("subscribe ok"), Some(&1));

        // RSS trend captured first→last.
        assert_eq!(stats.rss_first_mb, Some(232));
        assert_eq!(stats.rss_last_mb, Some(1716));
        assert_eq!(stats.last_throughput_rps, Some(0));
    }

    #[test]
    fn normalise_collapses_volatile_tokens() {
        let a = normalise_message("closed consumer 27 topic persistent://a/b/c-partition-2");
        let b = normalise_message("closed consumer 36 topic persistent://a/b/c-partition-9");
        assert_eq!(a, b, "ids and partition numbers normalised away");
    }

    #[test]
    fn correlation_hint_prioritises_oom() {
        let mut report = KubeReport {
            namespace: "ns".to_string(),
            pods: vec![
                pod("a", 0, 1, 5, 100, "i", true),
                pod("b", 1, 1, 0, 100, "i", false),
            ],
            ..Default::default()
        };
        report.images = distinct_images(&report.pods);
        assert_eq!(
            correlation_hint(&report, true).as_deref(),
            Some("kube: 1 pod(s) OOM-killed")
        );
        // A healthy topic gets no hint even if pods are unhealthy.
        assert!(correlation_hint(&report, false).is_none());
    }

    #[test]
    fn correlation_hint_none_when_unreachable() {
        let report = KubeReport::unreachable("ns", "connection refused".to_string());
        assert!(correlation_hint(&report, true).is_none());
    }

    #[test]
    fn health_verdict_and_summary() {
        let mut report = KubeReport {
            namespace: "ns".to_string(),
            pods: vec![pod("a", 1, 1, 0, 100, "i:1", false)],
            ..Default::default()
        };
        report.images = distinct_images(&report.pods);
        assert!(report.is_healthy());
        assert!(report.summary_line().contains("1/1 pods ready"));

        report.pods.push(pod("b", 0, 1, 9, 100, "i:1", true));
        report.images = distinct_images(&report.pods);
        assert!(!report.is_healthy());
        assert!(report.summary_line().contains("OOM-killed"));
    }
}
