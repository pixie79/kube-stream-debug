//! Destructive Kubernetes actions: delete a pod, rolling-recycle all consumer
//! pods, and cordon+drain a node. Isolated from the read-only client on purpose
//! — everything that can mutate the cluster lives here, behind the two gates
//! (config `admin.allow_actions` + an interactive confirmation the caller must
//! obtain before calling any of these).
//!
//! None of these functions check the gates themselves; gating is the caller's
//! responsibility (main/TUI). They assume authorisation has already been
//! granted, and simply perform the operation and report the outcome.

use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, DeleteParams, EvictParams, ListParams, Patch, PatchParams};
use kube::Client;

/// Outcome of an action, for surfacing back to the TUI.
#[derive(Debug, Clone)]
pub enum ActionOutcome {
    Ok(String),
    Err(String),
}

impl ActionOutcome {
    pub fn message(&self) -> &str {
        match self {
            ActionOutcome::Ok(m) | ActionOutcome::Err(m) => m,
        }
    }
    pub fn is_ok(&self) -> bool {
        matches!(self, ActionOutcome::Ok(_))
    }
}

/// Delete a single pod (the deployment/statefulset controller recreates it).
/// This is the mildest action — a targeted restart of one consumer.
pub async fn delete_pod(namespace: &str, pod: &str) -> ActionOutcome {
    let client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => return ActionOutcome::Err(format!("kube client: {e}")),
    };
    let pods: Api<Pod> = Api::namespaced(client, namespace);
    match pods.delete(pod, &DeleteParams::default()).await {
        Ok(_) => ActionOutcome::Ok(format!("deleted pod {pod}")),
        Err(e) => ActionOutcome::Err(format!("delete {pod}: {e}")),
    }
}

/// Rolling-recycle every pod matching the selector: delete one, wait for its
/// replacement to become Ready, then move to the next. Never takes down more
/// than one at a time, so the consumer group keeps making progress. Returns a
/// per-pod outcome list.
///
/// `ready_timeout` bounds the wait for each replacement; if it elapses the
/// recycle stops (rather than pressing on and taking down a second pod while the
/// first is still unhealthy).
pub async fn recycle_all(
    namespace: &str,
    selector: &str,
    ready_timeout: Duration,
) -> Vec<ActionOutcome> {
    let client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => return vec![ActionOutcome::Err(format!("kube client: {e}"))],
    };
    let pods: Api<Pod> = Api::namespaced(client, namespace);

    // Snapshot the current pod names up front. We delete these specific pods one
    // at a time; replacements have new names and are not re-recycled.
    let names: Vec<String> = match pods.list(&ListParams::default().labels(selector)).await {
        Ok(list) => list.items.iter().filter_map(|p| p.metadata.name.clone()).collect(),
        Err(e) => return vec![ActionOutcome::Err(format!("list pods: {e}"))],
    };
    if names.is_empty() {
        return vec![ActionOutcome::Err(format!("no pods match selector {selector}"))];
    }

    let mut outcomes = Vec::with_capacity(names.len());
    let want = names.len();
    for (i, name) in names.iter().enumerate() {
        if let Err(e) = pods.delete(name, &DeleteParams::default()).await {
            outcomes.push(ActionOutcome::Err(format!("delete {name}: {e}")));
            // Stop the roll on a delete failure — don't cascade.
            break;
        }
        // Wait for the group to return to full Ready count before the next one.
        match wait_ready(&pods, selector, want, ready_timeout).await {
            Ok(()) => outcomes.push(ActionOutcome::Ok(format!(
                "recycled {name} ({}/{want})",
                i + 1
            ))),
            Err(e) => {
                outcomes.push(ActionOutcome::Err(format!(
                    "after {name}: {e} — stopping roll"
                )));
                break;
            }
        }
    }
    outcomes
}

/// Wait until at least `want` pods matching the selector are Ready, or the
/// timeout elapses.
async fn wait_ready(
    pods: &Api<Pod>,
    selector: &str,
    want: usize,
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        let ready = match pods.list(&ListParams::default().labels(selector)).await {
            Ok(list) => list.items.iter().filter(|p| pod_ready(p)).count(),
            Err(e) => return Err(format!("list while waiting: {e}")),
        };
        if ready >= want {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(format!("timed out waiting for {want} ready (saw {ready})"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Whether a pod reports Ready via its status conditions.
fn pod_ready(p: &Pod) -> bool {
    p.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conds| {
            conds
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
        .unwrap_or(false)
}

/// Cordon a node (mark unschedulable) then evict its pods (drain). This is the
/// heaviest action — it moves every workload off the node, not just our
/// consumers. Cordon first so evicted pods don't reschedule back onto it.
pub async fn cordon_drain_node(node: &str) -> Vec<ActionOutcome> {
    let client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => return vec![ActionOutcome::Err(format!("kube client: {e}"))],
    };
    let nodes: Api<Node> = Api::all(client.clone());
    let mut outcomes = Vec::new();

    // 1. Cordon: patch spec.unschedulable = true.
    let patch = serde_json::json!({ "spec": { "unschedulable": true } });
    match nodes
        .patch(node, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => outcomes.push(ActionOutcome::Ok(format!("cordoned {node}"))),
        Err(e) => {
            outcomes.push(ActionOutcome::Err(format!("cordon {node}: {e}")));
            return outcomes; // no point draining if cordon failed
        }
    }

    // 2. Drain: evict pods scheduled on this node (all namespaces).
    let all_pods: Api<Pod> = Api::all(client.clone());
    let field = format!("spec.nodeName={node}");
    let on_node = match all_pods
        .list(&ListParams::default().fields(&field))
        .await
    {
        Ok(list) => list.items,
        Err(e) => {
            outcomes.push(ActionOutcome::Err(format!("list pods on {node}: {e}")));
            return outcomes;
        }
    };

    let mut evicted = 0usize;
    for p in &on_node {
        let (Some(name), Some(ns)) = (p.metadata.name.clone(), p.metadata.namespace.clone())
        else {
            continue;
        };
        // Skip DaemonSet-owned pods — they're expected to stay and can't be
        // meaningfully evicted (they'd just reschedule to the same node).
        if is_daemonset_pod(p) {
            continue;
        }
        let ns_pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
        match ns_pods.evict(&name, &EvictParams::default()).await {
            Ok(_) => evicted += 1,
            Err(e) => outcomes.push(ActionOutcome::Err(format!("evict {ns}/{name}: {e}"))),
        }
    }
    outcomes.push(ActionOutcome::Ok(format!("drained {node}: evicted {evicted} pods")));
    outcomes
}

/// Whether a pod is owned by a DaemonSet (should not be evicted during drain).
fn is_daemonset_pod(p: &Pod) -> bool {
    p.metadata
        .owner_references
        .as_ref()
        .map(|refs| refs.iter().any(|r| r.kind == "DaemonSet"))
        .unwrap_or(false)
}
