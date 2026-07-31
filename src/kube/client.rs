//! Live Kubernetes client layer. Compiled only with the `kube` feature.
//!
//! This is the one part of the tool that touches `kube-rs`/`k8s-openapi`. It
//! turns live API objects into the plain structs in the parent module, so all
//! downstream logic stays cluster-free and unit-tested. Auth is whatever
//! `kube::Client::try_default()` infers — the same `~/.kube/config`,
//! `KUBECONFIG`, or in-cluster service-account token that `kubectl` uses.
//!
//! Everything here is best-effort: any failure degrades to an `unreachable`
//! report (or empty sub-sections) rather than propagating, so a Kubernetes
//! problem never breaks the Pulsar side of the tool.
//!
//! ## Compilation note
//!
//! This module was written against kube 0.99 / k8s-openapi 0.24 API shapes but
//! could not be compiled in the environment where it was authored. If the first
//! build with `--features kube` fails, the most likely culprits, in order:
//!   * `Event::event_time` type — expected `Option<MicroTime>`; if the k8s-openapi
//!     version differs it may be `Option<Time>`. Adjust `age_secs_from_micro`.
//!   * `LogParams` / `ListParams` builder shape (field names, `.labels()`).
//!   * `ContainerStatus.state` / `.last_state` → `ContainerState` →
//!     `.waiting` / `.terminated` reason access.
//! All of these are thin conversions; the pure logic they feed (in the parent
//! module) is fully tested and shape-independent.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ConfigMap, Event, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Api, ListParams, LogParams};
use kube::Client;

use super::{ConfigAssertion, KubeEvent, KubeReport, LogSignal, PodSummary};

/// Options controlling what the client gathers.
pub struct KubeQuery {
    pub namespace: String,
    /// Label selector for the deployment's pods, e.g. `app=my-consumer`.
    pub selector: String,
    /// ConfigMap name to read `config.toml` from (None = skip config checks).
    pub configmap: Option<String>,
    /// Expected `key=value` config assertions.
    pub expected_config: Vec<(String, String)>,
    /// How many log lines per pod to scan (None = skip log scan).
    pub log_tail: Option<i64>,
    /// Only consider events newer than this many seconds.
    pub event_window_secs: i64,
}

/// Gather the full Kubernetes report. Never returns Err — on a connection or
/// auth failure it yields `KubeReport::unreachable` so the caller can still
/// render the Pulsar side.
pub async fn gather(query: &KubeQuery) -> KubeReport {
    let client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => return KubeReport::unreachable(&query.namespace, e.to_string()),
    };

    let mut report = KubeReport {
        namespace: query.namespace.clone(),
        ..Default::default()
    };

    // Pods (the core signal).
    let pods: Api<Pod> = Api::namespaced(client.clone(), &query.namespace);
    match pods
        .list(&ListParams::default().labels(&query.selector))
        .await
    {
        Ok(list) => {
            report.pods = list.items.iter().map(pod_summary).collect();
        }
        Err(e) => {
            return KubeReport::unreachable(&query.namespace, e.to_string());
        }
    }

    report.images = super::distinct_images(&report.pods);
    report.rollout_skew_secs = super::rollout_skew_secs(&report.pods);

    // Events (OOMKilling / Evicted), best-effort.
    let events: Api<Event> = Api::namespaced(client.clone(), &query.namespace);
    if let Ok(list) = events.list(&ListParams::default()).await {
        report.events = list
            .items
            .iter()
            .filter_map(kube_event)
            .filter(|e| interesting_event(&e.reason))
            .filter(|e| e.age_secs.map(|a| a <= query.event_window_secs).unwrap_or(true))
            .collect();
    }

    // ConfigMap assertions, best-effort.
    if let (Some(cm_name), false) = (&query.configmap, query.expected_config.is_empty()) {
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), &query.namespace);
        if let Ok(cm) = cms.get(cm_name).await {
            if let Some(body) = config_toml_body(&cm) {
                report.config_assertions = super::assert_config(&body, &query.expected_config);
            } else {
                // ConfigMap present but no config.toml key: mark all as missing.
                report.config_assertions = query
                    .expected_config
                    .iter()
                    .map(|(k, v)| ConfigAssertion {
                        key: k.clone(),
                        expected: v.clone(),
                        actual: None,
                        pass: false,
                    })
                    .collect();
            }
        }
    }

    // Per-pod log scan, best-effort.
    if let Some(tail) = query.log_tail {
        report.log_signals = gather_log_signals(&pods, &report.pods, tail).await;
    }

    report
}

/// Read `config.toml` from a ConfigMap's `data` map.
fn config_toml_body(cm: &ConfigMap) -> Option<String> {
    cm.data
        .as_ref()
        .and_then(|d: &BTreeMap<String, String>| d.get("config.toml"))
        .cloned()
}

async fn gather_log_signals(pods: &Api<Pod>, summaries: &[PodSummary], tail: i64) -> Vec<LogSignal> {
    let mut signals = Vec::new();
    for summary in summaries {
        let params = LogParams {
            tail_lines: Some(tail),
            ..Default::default()
        };
        if let Ok(text) = pods.logs(&summary.name, &params).await {
            signals.extend(super::scan_log_signals(&summary.name, &text));
        }
    }
    signals
}

/// Convert a live `Pod` into the plain summary.
fn pod_summary(pod: &Pod) -> PodSummary {
    let name = pod.metadata.name.clone().unwrap_or_default();
    let age_secs = pod
        .metadata
        .creation_timestamp
        .as_ref()
        .map(|t| age_secs_from(t));

    let (image, total) = pod
        .spec
        .as_ref()
        .map(|s| {
            let img = s.containers.first().and_then(|c| c.image.clone());
            (img, s.containers.len() as u32)
        })
        .unwrap_or((None, 0));

    let mut ready = 0u32;
    let mut restarts = 0i32;
    let mut oom_killed = false;
    let mut reason: Option<String> = None;

    if let Some(status) = &pod.status {
        if let Some(container_statuses) = &status.container_statuses {
            for cs in container_statuses {
                if cs.ready {
                    ready += 1;
                }
                restarts += cs.restart_count;

                // Waiting reason (e.g. CrashLoopBackOff) or last-terminated
                // reason (e.g. OOMKilled).
                if let Some(state) = &cs.state {
                    if let Some(waiting) = &state.waiting {
                        if let Some(r) = &waiting.reason {
                            reason.get_or_insert_with(|| r.clone());
                        }
                    }
                }
                if let Some(last) = &cs.last_state {
                    if let Some(term) = &last.terminated {
                        if let Some(r) = &term.reason {
                            if r == "OOMKilled" {
                                oom_killed = true;
                            }
                            reason.get_or_insert_with(|| r.clone());
                        }
                    }
                }
            }
        }
        // Fall back to the pod phase if nothing more specific was found.
        if reason.is_none() {
            reason = status.phase.clone();
        }
    }

    PodSummary {
        name,
        ready,
        total_containers: total,
        restarts,
        age_secs,
        image,
        reason,
        oom_killed,
    }
}

fn kube_event(ev: &Event) -> Option<KubeEvent> {
    let reason = ev.reason.clone()?;
    let message = ev.message.clone().unwrap_or_default();
    let involved = ev.involved_object.name.clone().unwrap_or_default();
    // Prefer last_timestamp, fall back to event_time.
    let age_secs = ev
        .last_timestamp
        .as_ref()
        .map(age_secs_from)
        .or_else(|| ev.event_time.as_ref().map(|mt| age_secs_from_micro(mt)));
    Some(KubeEvent {
        reason,
        message,
        involved,
        age_secs,
    })
}

fn interesting_event(reason: &str) -> bool {
    matches!(
        reason,
        "OOMKilling" | "Evicted" | "BackOff" | "Failed" | "FailedScheduling" | "Unhealthy"
    )
}

/// Seconds since a `Time`, computed against the system clock. Negative clamped
/// to 0 (clock skew).
fn age_secs_from(t: &Time) -> i64 {
    let then = t.0.timestamp();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(then);
    (now - then).max(0)
}

/// Same for a `MicroTime` (event_time uses microsecond precision).
fn age_secs_from_micro(t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime) -> i64 {
    let then = t.0.timestamp();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(then);
    (now - then).max(0)
}
