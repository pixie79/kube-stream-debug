//! Rendering: summary table (default) or JSONL.

use comfy_table::{Attribute, Cell, CellAlignment, Color, ContentArrangement, Table};

use crate::config::{ColorThresholds, ThresholdLevel};
use crate::drain::{format_eta, Trend};
use crate::health::{Status, TopicHealth};
use crate::state::format_duration_between;

pub fn render_table(results: &[TopicHealth], colors: &ColorThresholds, run_at: &str) -> Table {
    let show_drain = results.iter().any(|h| h.drain.is_some());
    let show_time = results.iter().any(|h| h.state_since.is_some());

    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic);

    let mut header = vec![
        Cell::new("TOPIC").add_attribute(Attribute::Bold),
        Cell::new("STATUS").add_attribute(Attribute::Bold),
    ];
    if show_time {
        header.push(Cell::new("TIME").add_attribute(Attribute::Bold));
    }
    header.extend([
        Cell::new("BACKLOG").add_attribute(Attribute::Bold),
        Cell::new("SIZE").add_attribute(Attribute::Bold),
        Cell::new("CONSUMERS").add_attribute(Attribute::Bold),
        Cell::new("UNACKED").add_attribute(Attribute::Bold),
        Cell::new("RATE OUT").add_attribute(Attribute::Bold),
        Cell::new("HEADROOM").add_attribute(Attribute::Bold),
    ]);
    if show_drain {
        header.push(Cell::new("TREND").add_attribute(Attribute::Bold));
        header.push(Cell::new("NET/s").add_attribute(Attribute::Bold));
        header.push(Cell::new("ETA").add_attribute(Attribute::Bold));
    }
    header.push(Cell::new("DETAIL").add_attribute(Attribute::Bold));
    table.set_header(header);

    for health in results {
        let mut row = vec![Cell::new(&health.topic), status_cell(health.status)];
        if show_time {
            row.push(time_in_state_cell(health, run_at));
        }
        row.extend([
            level_cell(
                health.total_backlog.to_string(),
                colors.backlog_level(health.total_backlog),
            ),
            level_cell(
                format_bytes(health.backlog_bytes),
                colors.size_level(health.backlog_bytes),
            ),
            Cell::new(health.consumers).set_alignment(CellAlignment::Right),
            level_cell(
                health.unacked_messages.to_string(),
                colors.unacked_level(health.unacked_messages),
            ),
            Cell::new(format!("{:.1}", health.msg_rate_out)).set_alignment(CellAlignment::Right),
            headroom_cell(health),
        ]);
        if show_drain {
            let (trend, net, eta) = drain_cells(health);
            row.push(trend);
            row.push(net);
            row.push(eta);
        }
        row.push(Cell::new(detail(health)));
        table.add_row(row);
    }
    table
}

/// TREND (coloured), NET/s (signed, right-aligned), and ETA cells for one row.
/// Blank when this topic has no drain sample (e.g. it errored on the second
/// read).
fn drain_cells(health: &TopicHealth) -> (Cell, Cell, Cell) {
    let Some(drain) = &health.drain else {
        let blank = || Cell::new("—").set_alignment(CellAlignment::Right);
        return (
            Cell::new("—").set_alignment(CellAlignment::Right),
            blank(),
            blank(),
        );
    };

    let trend = {
        let cell = Cell::new(drain.trend.label());
        match drain.trend {
            Trend::Draining => cell.fg(Color::Green),
            Trend::Growing => cell.fg(Color::Red).add_attribute(Attribute::Bold),
            Trend::Stable => cell.fg(Color::Yellow),
            Trend::Empty => cell.fg(Color::DarkGrey),
        }
    };

    // Signed net rate: +N growing, -N draining.
    let net = Cell::new(format!("{:+.1}", drain.net_per_sec)).set_alignment(CellAlignment::Right);

    let eta = match drain.eta_secs {
        Some(secs) => Cell::new(format_eta(secs)).set_alignment(CellAlignment::Right),
        None => Cell::new("—").set_alignment(CellAlignment::Right),
    };

    (trend, net, eta)
}

/// Time-in-state cell: how long the topic has held its current (status, trend),
/// as a human duration. Right-aligned. Blank (`—`) when no prior observation is
/// available (e.g. a single run with no snapshot history, or the first watch
/// cycle).
fn time_in_state_cell(health: &TopicHealth, run_at: &str) -> Cell {
    let text = health
        .state_since
        .as_deref()
        .and_then(|since| format_duration_between(since, run_at))
        .unwrap_or_else(|| "—".to_string());
    Cell::new(text).set_alignment(CellAlignment::Right)
}

/// Render the Kubernetes pod-summary section that prints above the topic table
/// when --kube is used. Returns a string (may be multi-line) ending in a
/// newline, or an unreachable notice.
pub fn render_kube_section(report: &crate::kube::KubeReport) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(out, "kubernetes: namespace {}", report.namespace);

    if let Some(err) = &report.error {
        let _ = writeln!(out, "  unreachable: {err}");
        return out;
    }

    if report.pods.is_empty() {
        let _ = writeln!(out, "  no pods matched the selector");
        return out;
    }

    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_FULL_CONDENSED)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            Cell::new("POD").add_attribute(Attribute::Bold),
            Cell::new("READY").add_attribute(Attribute::Bold),
            Cell::new("RESTARTS").add_attribute(Attribute::Bold),
            Cell::new("AGE").add_attribute(Attribute::Bold),
            Cell::new("CPU").add_attribute(Attribute::Bold),
            Cell::new("MEM").add_attribute(Attribute::Bold),
            Cell::new("STATE").add_attribute(Attribute::Bold),
        ]);

    for pod in &report.pods {
        let ready = format!("{}/{}", pod.ready, pod.total_containers);
        let ready_cell = if pod.all_ready() {
            Cell::new(ready).fg(Color::Green)
        } else {
            Cell::new(ready).fg(Color::Red).add_attribute(Attribute::Bold)
        };
        let age = pod
            .age_secs
            .map(|s| format_short_duration(s))
            .unwrap_or_else(|| "—".to_string());
        let state = if pod.oom_killed {
            Cell::new("OOMKilled")
                .fg(Color::White)
                .bg(Color::Red)
                .add_attribute(Attribute::Bold)
        } else {
            let text = pod.reason.clone().unwrap_or_else(|| "Running".to_string());
            if text == "Running" {
                Cell::new(text).fg(Color::Green)
            } else {
                Cell::new(text).fg(Color::Yellow)
            }
        };
        table.add_row(vec![
            Cell::new(&pod.name),
            ready_cell,
            Cell::new(pod.restarts).set_alignment(CellAlignment::Right),
            Cell::new(age).set_alignment(CellAlignment::Right),
            resource_cell(cpu_usage_text(pod), pod.cpu_fraction()),
            resource_cell(mem_usage_text(pod), pod.mem_fraction()),
            state,
        ]);
    }
    let _ = writeln!(out, "{table}");

    // Node capacity context.
    for node in &report.nodes {
        let cpu = node
            .alloc_cpu_milli
            .map(crate::kube::format_cpu)
            .unwrap_or_else(|| "?".to_string());
        let mem = node
            .alloc_mem_bytes
            .map(format_bytes)
            .unwrap_or_else(|| "?".to_string());
        let inst = node
            .instance_type
            .as_deref()
            .map(|i| format!(" [{i}]"))
            .unwrap_or_default();
        let _ = writeln!(out, "  node {}{}: {} CPU / {} allocatable", node.name, inst, cpu, mem);
    }

    // Notable lines below the pod table.
    if report.images.len() > 1 {
        let _ = writeln!(
            out,
            "  ⚠ split rollout: {} distinct images ({})",
            report.images.len(),
            report.images.join(", ")
        );
    }
    if let Some(skew) = report.rollout_skew_secs {
        if skew > 300 {
            let _ = writeln!(
                out,
                "  ⚠ rollout skew {} — pods not restarted together",
                format_short_duration(skew)
            );
        }
    }
    for assertion in report.config_assertions.iter().filter(|a| !a.pass) {
        let _ = writeln!(
            out,
            "  ✗ config {}: expected {}, got {}",
            assertion.key,
            assertion.expected,
            assertion.actual.as_deref().unwrap_or("(absent)")
        );
    }
    if !report.events.is_empty() {
        let _ = writeln!(out, "  events:");
        for ev in report.events.iter().take(10) {
            let age = ev
                .age_secs
                .map(|s| format!(" ({} ago)", format_short_duration(s)))
                .unwrap_or_default();
            let _ = writeln!(out, "    {} {}{}", ev.reason, ev.involved, age);
        }
    }
    if !report.log_signals.is_empty() {
        let _ = writeln!(out, "  log signals:");
        for sig in report.log_signals.iter().take(15) {
            let _ = writeln!(out, "    [{}] {}: {}", sig.kind, sig.pod, sig.line);
        }
    }

    out
}

/// "used/limit" CPU text for a pod, using whatever's known. Examples:
/// `120m/2` (used and limit), `120m/·` (no limit), `·/2` (no usage).
fn cpu_usage_text(pod: &crate::kube::PodSummary) -> String {
    let used = pod
        .cpu_used_milli
        .map(crate::kube::format_cpu)
        .unwrap_or_else(|| "·".to_string());
    let limit = pod
        .cpu_limit_milli
        .map(crate::kube::format_cpu)
        .unwrap_or_else(|| "·".to_string());
    format!("{used}/{limit}")
}

/// "used/limit" memory text for a pod.
fn mem_usage_text(pod: &crate::kube::PodSummary) -> String {
    let used = pod
        .mem_used_bytes
        .map(format_bytes)
        .unwrap_or_else(|| "·".to_string());
    let limit = pod
        .mem_limit_bytes
        .map(format_bytes)
        .unwrap_or_else(|| "·".to_string());
    format!("{used}/{limit}")
}

/// A resource cell coloured by usage fraction: green <70%, yellow 70–90%,
/// red ≥90%. Uncoloured when the fraction is unknown.
fn resource_cell(text: String, fraction: Option<f64>) -> Cell {
    let cell = Cell::new(text).set_alignment(CellAlignment::Right);
    match fraction {
        Some(f) if f >= 0.90 => cell.fg(Color::Red).add_attribute(Attribute::Bold),
        Some(f) if f >= 0.70 => cell.fg(Color::Yellow),
        Some(_) => cell.fg(Color::Green),
        None => cell,
    }
}

/// Compact duration for pod ages / event ages: `45s`, `12m`, `3h`, `2d`.
fn format_short_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// A right-aligned cell coloured by a configured threshold level. `None` leaves
/// it uncoloured (no thresholds set for that column).
fn level_cell(text: String, level: ThresholdLevel) -> Cell {
    let cell = Cell::new(text).set_alignment(CellAlignment::Right);
    match level {
        ThresholdLevel::None => cell,
        ThresholdLevel::Ok => cell.fg(Color::Green),
        ThresholdLevel::Warn => cell.fg(Color::Yellow),
        ThresholdLevel::Crit => cell.fg(Color::Red).add_attribute(Attribute::Bold),
    }
}

/// Minimum per-partition trim headroom, right-aligned. Blank (`—`) when there's
/// no backlog at risk or internal-stats were unavailable. Coloured by risk:
/// red when a partition is already trimmed, yellow when at the edge, green when
/// there's a real, comfortable margin.
fn headroom_cell(health: &TopicHealth) -> Cell {
    match health.min_headroom {
        None => Cell::new("—").set_alignment(CellAlignment::Right),
        Some(h) => {
            let cell = Cell::new(h).set_alignment(CellAlignment::Right);
            if !health.trimmed_partitions.is_empty() || h <= 0 {
                cell.fg(Color::Red).add_attribute(Attribute::Bold)
            } else if !health.at_edge_partitions.is_empty() {
                cell.fg(Color::Yellow)
            } else {
                cell.fg(Color::Green)
            }
        }
    }
}

fn status_cell(status: Status) -> Cell {
    let cell = Cell::new(status.label());
    match status {
        Status::Ok => cell.fg(Color::Green),
        Status::Backlog => cell.fg(Color::Yellow),
        Status::PartitionGap | Status::NoConsumers | Status::MissingSub => cell.fg(Color::Red),
        Status::Trimmed => cell
            .fg(Color::White)
            .bg(Color::Red)
            .add_attribute(Attribute::Bold),
        Status::Error => cell.fg(Color::Red).add_attribute(Attribute::Bold),
    }
}

/// Compact, human-scannable detail column: hot partitions with their backlog
/// and any subscription gaps, using short partition suffixes (p3=245).
fn detail(health: &TopicHealth) -> String {
    if let Some(error) = &health.error {
        return error.clone();
    }

    let mut parts: Vec<String> = Vec::new();

    // Most urgent first: cursors stranded past trimmed data.
    if !health.trimmed_partitions.is_empty() {
        let trimmed: Vec<String> = health
            .trimmed_partitions
            .iter()
            .map(|t| {
                let mark = if t.waiting_with_backlog { "!" } else { "" };
                format!("{}{}", short_partition(&t.partition), mark)
            })
            .collect();
        parts.push(format!("TRIMMED: {}", trimmed.join(", ")));
    }

    if !health.at_edge_partitions.is_empty() {
        let edge: Vec<String> = health
            .at_edge_partitions
            .iter()
            .map(|e| match e.headroom {
                Some(h) => format!("{}(~{})", short_partition(&e.partition), h),
                None => short_partition(&e.partition),
            })
            .collect();
        parts.push(format!("edge: {}", edge.join(", ")));
    }

    if !health.hot_partitions.is_empty() {
        let hot: Vec<String> = health
            .hot_partitions
            .iter()
            .map(|p| {
                if p.backlog_bytes > 0 {
                    format!(
                        "{}={} ({})",
                        short_partition(&p.partition),
                        p.backlog,
                        format_bytes(p.backlog_bytes)
                    )
                } else {
                    format!("{}={}", short_partition(&p.partition), p.backlog)
                }
            })
            .collect();
        parts.push(format!("hot: {}", hot.join(", ")));
    }

    if !health.partition_gaps.is_empty() {
        let gaps: Vec<String> = health
            .partition_gaps
            .iter()
            .map(|g| {
                let reason = match g.reason {
                    "subscription_missing" => "no-sub",
                    "no_consumers" => "no-consumers",
                    other => other,
                };
                format!("{} ({})", short_partition(&g.partition), reason)
            })
            .collect();
        parts.push(format!("gaps: {}", gaps.join(", ")));
    }

    if let Some(hint) = &health.kube_hint {
        parts.push(hint.clone());
    }

    parts.join("; ")
}

/// `persistent://t/ns/topic-partition-7` → `p7`; non-partition names collapse
/// to their local topic name.
fn short_partition(full: &str) -> String {
    if let Some((_, index)) = full.rsplit_once("-partition-") {
        return format!("p{index}");
    }
    full.rsplit('/').next().unwrap_or(full).to_string()
}

pub fn render_jsonl(results: &[TopicHealth], run_at: &str) -> serde_json::Result<String> {
    let mut out = String::new();
    for health in results {
        // Serialize to a JSON object, then inject `as_of` so every line is
        // self-contained and successive runs can be diffed/ordered by time.
        let mut value = serde_json::to_value(health)?;
        if let serde_json::Value::Object(map) = &mut value {
            map.insert(
                "as_of".to_string(),
                serde_json::Value::String(run_at.to_string()),
            );
        }
        out.push_str(&serde_json::to_string(&value)?);
        out.push('\n');
    }
    Ok(out)
}

/// Binary byte sizes matching the Pulsar dashboard's units (GiB/MiB/KiB).
/// Zero renders as "—" so unpopulated sizes don't read as a real 0 B.
pub(crate) fn format_bytes(bytes: i64) -> String {
    if bytes <= 0 {
        return "—".to_string();
    }
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_partition_names() {
        assert_eq!(short_partition("persistent://a/b/c-partition-12"), "p12");
        assert_eq!(short_partition("persistent://a/b/plain-topic"), "plain-topic");
    }

    #[test]
    fn threshold_colours_render_when_styling_forced() {
        use crate::health::Status;
        let colors = ColorThresholds {
            backlog_warn: Some(100),
            backlog_crit: Some(1000),
            ..Default::default()
        };
        let health = TopicHealth {
            topic: "persistent://a/b/c".to_string(),
            status: Status::Backlog,
            total_backlog: 5000, // over crit → should be styled
            backlog_bytes: 0,
            consumers: 1,
            unacked_messages: 0,
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
            error: None,
        };
        let mut table = render_table(std::slice::from_ref(&health), &colors, "2026-07-30T12:00:00Z");
        table.enforce_styling();
        let rendered = table.to_string();
        assert!(
            rendered.contains('\u{1b}'),
            "forced styling should emit ANSI colour codes"
        );
    }

    #[test]
    fn jsonl_includes_timestamp() {
        use crate::health::Status;
        let health = TopicHealth {
            topic: "persistent://a/b/c".to_string(),
            status: Status::Ok,
            total_backlog: 0,
            backlog_bytes: 0,
            consumers: 1,
            unacked_messages: 0,
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
            error: None,
        };
        let out = render_jsonl(std::slice::from_ref(&health), "2026-07-30T10:00:00Z").unwrap();
        assert!(out.contains(r#""as_of":"2026-07-30T10:00:00Z""#));
    }
}
#[cfg(test)]
mod kube_render_tests {
    use super::render_kube_section;
    use crate::kube::{ConfigAssertion, KubeReport, PodSummary};

    fn pod(name: &str, ready: u32, total: u32, restarts: i32, age: i64, img: &str, oom: bool, reason: Option<&str>) -> PodSummary {
        PodSummary { name: name.into(), ready, total_containers: total, restarts,
            age_secs: Some(age), image: Some(img.into()),
            reason: reason.map(|s| s.into()), oom_killed: oom,
            node: None, cpu_used_milli: None, mem_used_bytes: None,
            cpu_request_milli: None, cpu_limit_milli: None,
            mem_request_bytes: None, mem_limit_bytes: None }
    }

    #[test]
    fn renders_healthy_pod_section() {
        let mut report = KubeReport { namespace: "my-ns".into(), ..Default::default() };
        report.pods = vec![
            pod("app-1", 1, 1, 0, 3600, "img:v2", false, None),
            pod("app-2", 1, 1, 0, 3600, "img:v2", false, None),
        ];
        report.images = crate::kube::distinct_images(&report.pods);
        let out = render_kube_section(&report);
        assert!(out.contains("namespace my-ns"));
        assert!(out.contains("app-1"));
        assert!(!out.contains("split rollout"));
    }

    #[test]
    fn renders_problems() {
        let mut report = KubeReport { namespace: "ns".into(), ..Default::default() };
        report.pods = vec![
            pod("app-1", 0, 1, 7, 120, "img:v2", true, Some("OOMKilled")),
            pod("app-2", 1, 1, 0, 6000, "img:v1", false, None),
        ];
        report.images = crate::kube::distinct_images(&report.pods);
        report.rollout_skew_secs = crate::kube::rollout_skew_secs(&report.pods);
        report.config_assertions = vec![ConfigAssertion {
            key: "worker_count".into(), expected: "24".into(),
            actual: Some("16".into()), pass: false }];
        let out = render_kube_section(&report);
        assert!(out.contains("OOMKilled"), "oom state shown");
        assert!(out.contains("split rollout"), "two images flagged");
        assert!(out.contains("config worker_count"), "failed assertion shown");
    }

    #[test]
    fn renders_unreachable() {
        let report = KubeReport::unreachable("ns", "connection refused".into());
        let out = render_kube_section(&report);
        assert!(out.contains("unreachable"));
    }
}
