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
}

impl PodSummary {
    pub fn all_ready(&self) -> bool {
        self.total_containers > 0 && self.ready == self.total_containers
    }
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
    /// Distinct images across pods; more than one means a split rollout.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Seconds between the oldest and newest pod creation — rollout skew.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollout_skew_secs: Option<i64>,
    /// Populated if the client couldn't reach the cluster; the Pulsar side of
    /// the report is still valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
    if !v.starts_with('"') {
        if let Some(idx) = v.find(" #") {
            v = v[..idx].trim();
        }
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
            oom_killed: oom,
        }
    }

    #[test]
    fn all_ready_reflects_container_counts() {
        assert!(pod("a", 1, 1, 0, 100, "img:1", false).all_ready());
        assert!(!pod("b", 0, 1, 3, 100, "img:1", false).all_ready());
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
