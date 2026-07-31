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
//!   * The metrics fetch (`gather_pod_metrics`) uses the dynamic-object API
//!     (`DynamicObject`, `GroupVersionKind::gvk`, `ApiResource::from_gvk`,
//!     `Api::namespaced_with`) to reach metrics.k8s.io, which isn't in
//!     k8s-openapi. The `.data` JSON shape (`containers[].usage.{cpu,memory}`)
//!     is the standard PodMetrics response, but the kube-rs entry points are
//!     the thing most likely to have moved between versions.
//!   * `Container.resources.{requests,limits}` are `BTreeMap<String, Quantity>`;
//!     `Quantity` is a newtype whose inner string is `.0`.
//!   * `Node.status.allocatable` is `Option<BTreeMap<String, Quantity>>`.
//!   * `Event::event_time` type — expected `Option<MicroTime>`.
//!   * `LogParams` / `ListParams` builder shape.
//!   * `ContainerStatus.state` / `.last_state` reason access.
//! All of these are thin conversions; the pure logic they feed (quantity
//! parsing, resource fractions, formatting, in the parent module) is fully
//! tested and shape-independent. The metrics/node fetches are best-effort:
//! failures leave usage as None and the rest of the report is unaffected.

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

    // Live CPU/memory from the metrics API (metrics.k8s.io), best-effort — it's
    // only present if metrics-server is installed. Fills used_* on each pod.
    gather_pod_metrics(&client, &query.namespace, &mut report.pods).await;

    // Node capacity for the nodes these pods run on, best-effort.
    report.nodes = gather_nodes(&client, &report.pods).await;

    report
}

/// Fetch pod metrics from metrics.k8s.io/v1beta1 and merge CPU/mem usage into
/// the matching PodSummary. Best-effort: any failure (no metrics-server, RBAC)
/// leaves usage as None. The API isn't in k8s-openapi, so we query it as a raw
/// dynamic object and sum container usage.
async fn gather_pod_metrics(client: &Client, namespace: &str, pods: &mut [PodSummary]) {
    use kube::core::{DynamicObject, GroupVersionKind};
    use kube::discovery::ApiResource;

    let gvk = GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "PodMetrics");
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &ar);

    let list = match api.list(&ListParams::default()).await {
        Ok(l) => l,
        Err(_) => return, // metrics-server absent or not permitted; skip quietly.
    };

    for item in list.items {
        let Some(name) = item.metadata.name.clone() else { continue };
        // The object's `containers` field is [{name, usage:{cpu, memory}}, …].
        let containers = item
            .data
            .get("containers")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        let mut cpu_milli = 0i64;
        let mut mem_bytes = 0i64;
        let mut any = false;
        for c in containers {
            if let Some(usage) = c.get("usage") {
                if let Some(cpu) = usage.get("cpu").and_then(|v| v.as_str()) {
                    if let Some(m) = super::parse_cpu_milli(cpu) {
                        cpu_milli += m;
                        any = true;
                    }
                }
                if let Some(mem) = usage.get("memory").and_then(|v| v.as_str()) {
                    if let Some(b) = super::parse_mem_bytes(mem) {
                        mem_bytes += b;
                        any = true;
                    }
                }
            }
        }
        if !any {
            continue;
        }
        if let Some(pod) = pods.iter_mut().find(|p| p.name == name) {
            pod.cpu_used_milli = Some(cpu_milli);
            pod.mem_used_bytes = Some(mem_bytes);
        }
    }
}

/// Fetch capacity for the distinct nodes the pods run on. Best-effort.
async fn gather_nodes(client: &Client, pods: &[PodSummary]) -> Vec<super::NodeInfo> {
    use k8s_openapi::api::core::v1::Node;

    let mut names: Vec<String> = pods.iter().filter_map(|p| p.node.clone()).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Vec::new();
    }

    let api: Api<Node> = Api::all(client.clone());
    let mut out = Vec::new();
    for name in names {
        if let Ok(node) = api.get(&name).await {
            let alloc = node.status.as_ref().and_then(|s| s.allocatable.as_ref());
            let alloc_cpu_milli = alloc
                .and_then(|a| a.get("cpu"))
                .and_then(|q| super::parse_cpu_milli(&q.0));
            let alloc_mem_bytes = alloc
                .and_then(|a| a.get("memory"))
                .and_then(|q| super::parse_mem_bytes(&q.0));
            let instance_type = node
                .metadata
                .labels
                .as_ref()
                .and_then(|l| {
                    l.get("node.kubernetes.io/instance-type")
                        .or_else(|| l.get("beta.kubernetes.io/instance-type"))
                })
                .cloned();
            out.push(super::NodeInfo {
                name,
                alloc_cpu_milli,
                alloc_mem_bytes,
                instance_type,
            });
        }
    }
    out
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

    // Resource requests/limits summed across containers, and the node.
    let node = pod.spec.as_ref().and_then(|s| s.node_name.clone());
    let (cpu_request_milli, cpu_limit_milli, mem_request_bytes, mem_limit_bytes) = pod
        .spec
        .as_ref()
        .map(|s| {
            let req_cpu = super::sum_quantity(
                s.containers.iter().map(|c| container_res(c, "requests", "cpu")),
                super::parse_cpu_milli,
            );
            let lim_cpu = super::sum_quantity(
                s.containers.iter().map(|c| container_res(c, "limits", "cpu")),
                super::parse_cpu_milli,
            );
            let req_mem = super::sum_quantity(
                s.containers.iter().map(|c| container_res(c, "requests", "memory")),
                super::parse_mem_bytes,
            );
            let lim_mem = super::sum_quantity(
                s.containers.iter().map(|c| container_res(c, "limits", "memory")),
                super::parse_mem_bytes,
            );
            (req_cpu, lim_cpu, req_mem, lim_mem)
        })
        .unwrap_or((None, None, None, None));

    PodSummary {
        name,
        ready,
        total_containers: total,
        restarts,
        age_secs,
        image,
        reason,
        oom_killed,
        node,
        // Live usage is filled in later from the metrics API, if available.
        cpu_used_milli: None,
        mem_used_bytes: None,
        cpu_request_milli,
        cpu_limit_milli,
        mem_request_bytes,
        mem_limit_bytes,
    }
}

/// Pull a container's `resources.{requests|limits}.{cpu|memory}` as a raw
/// quantity string, if declared.
fn container_res<'a>(
    c: &'a k8s_openapi::api::core::v1::Container,
    kind: &str,
    resource: &str,
) -> Option<&'a str> {
    let resources = c.resources.as_ref()?;
    let map = match kind {
        "requests" => resources.requests.as_ref()?,
        _ => resources.limits.as_ref()?,
    };
    map.get(resource).map(|q| q.0.as_str())
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
