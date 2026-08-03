//! Pod metrics: parse the Prometheus `/metrics` text a stream-sync pod exposes,
//! track a rolling per-metric history, compute trend (better/worse/flat plus
//! rate-of-change for counters), and summarise the bottleneck-relevant subset.
//!
//! Everything here is pure and testable. The actual port-forward + HTTP scrape
//! lives in `kube/client.rs` behind the `kube` feature; this module only turns
//! scraped text into structured, trended, summarised data.

use std::collections::BTreeMap;

use serde::Serialize;

/// One parsed metric sample: name, its labels, and the value. Prometheus lines
/// look like `name{label="v",...} 12.3` (or `name 12.3` with no labels).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub name: String,
    /// Sorted label key→value pairs (sorted so the series key is stable).
    pub labels: Vec<(String, String)>,
    pub value: f64,
}

impl Sample {
    /// A stable identity for this series: `name{k="v",...}`. Used to line up the
    /// same series across successive scrapes for trend computation.
    pub fn series_key(&self) -> String {
        if self.labels.is_empty() {
            return self.name.clone();
        }
        let inner: Vec<String> = self
            .labels
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect();
        format!("{}{{{}}}", self.name, inner.join(","))
    }
}

/// Parse Prometheus text-exposition format into samples. Skips `#` comment/HELP/
/// TYPE lines and blank lines; tolerates malformed lines by skipping them (a
/// pod's metrics endpoint should never take the whole scrape down).
pub fn parse_prometheus(text: &str) -> Vec<Sample> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(sample) = parse_line(line) {
            out.push(sample);
        }
    }
    out
}

/// Parse a single non-comment metric line. Returns None on anything malformed.
fn parse_line(line: &str) -> Option<Sample> {
    // Split into "name{labels}" (or "name") and the value (last whitespace field).
    // Prometheus allows an optional trailing timestamp; we take the first field
    // after the series as the value and ignore any timestamp.
    let (series, rest) = match line.find(' ') {
        Some(idx) => (&line[..idx], line[idx..].trim()),
        None => return None,
    };
    let value_str = rest.split_whitespace().next()?;
    let value = parse_value(value_str)?;

    let (name, labels) = if let Some(brace) = series.find('{') {
        let name = &series[..brace];
        let label_str = series.get(brace + 1..series.rfind('}')?)?;
        (name.to_string(), parse_labels(label_str))
    } else {
        (series.to_string(), Vec::new())
    };
    if name.is_empty() {
        return None;
    }
    Some(Sample { name, labels, value })
}

/// Prometheus values are f64; `+Inf`/`-Inf`/`NaN` are valid.
fn parse_value(s: &str) -> Option<f64> {
    match s {
        "+Inf" => Some(f64::INFINITY),
        "-Inf" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        other => other.parse::<f64>().ok(),
    }
}

/// Parse `k1="v1",k2="v2"` into sorted (key, value) pairs. Values are quoted;
/// commas inside values aren't expected for these metrics, so a simple split is
/// sufficient and robust enough.
fn parse_labels(s: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim().trim_matches('"');
            pairs.push((k.trim().to_string(), v.to_string()));
        }
    }
    pairs.sort();
    pairs
}

/// Direction of a metric between two points in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Up,
    Down,
    Flat,
}

/// Whether a direction is good, bad, or neutral depends on the metric — lag
/// going up is bad, throughput going up is good. Callers pass the polarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Polarity {
    /// Higher is better (throughput, records written).
    HigherBetter,
    /// Lower is better (lag, backlog, channel fill, memory ratio).
    LowerBetter,
    /// Neither — just report the direction.
    Neutral,
}

/// A per-series rolling history, capped to a window. Feeds trend computation.
#[derive(Debug, Clone, Default)]
pub struct Series {
    /// Most recent values, oldest first, capped to the window length.
    values: Vec<f64>,
}

impl Series {
    pub fn push(&mut self, value: f64, window: usize) {
        self.values.push(value);
        let window = window.max(1);
        if self.values.len() > window {
            let excess = self.values.len() - window;
            self.values.drain(0..excess);
        }
    }

    pub fn current(&self) -> Option<f64> {
        self.values.last().copied()
    }

    pub fn previous(&self) -> Option<f64> {
        if self.values.len() >= 2 {
            Some(self.values[self.values.len() - 2])
        } else {
            None
        }
    }

    /// Instantaneous direction: current vs the immediately preceding sample.
    /// `eps` is the fraction of the previous value under which a change reads as
    /// flat, so noise doesn't flap the trend.
    pub fn instant_direction(&self, eps: f64) -> Direction {
        match (self.current(), self.previous()) {
            (Some(cur), Some(prev)) => classify(prev, cur, eps),
            _ => Direction::Flat,
        }
    }

    /// Rolling direction across the whole window: compares the mean of the older
    /// half to the mean of the newer half, so a single spike doesn't dominate.
    pub fn rolling_direction(&self, eps: f64) -> Direction {
        let n = self.values.len();
        if n < 2 {
            return Direction::Flat;
        }
        let mid = n / 2;
        let older = mean(&self.values[..mid.max(1)]);
        let newer = mean(&self.values[mid..]);
        classify(older, newer, eps)
    }

    /// Rate of change per sample for a counter (current minus previous). Returns
    /// None if fewer than two samples, or if the counter reset (went down).
    pub fn counter_rate(&self) -> Option<f64> {
        match (self.current(), self.previous()) {
            (Some(cur), Some(prev)) if cur >= prev => Some(cur - prev),
            _ => None,
        }
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Classify a change from `prev` to `cur` as Up/Down/Flat, treating a change
/// smaller than `eps` fraction of `prev` (or an absolute floor) as Flat.
fn classify(prev: f64, cur: f64, eps: f64) -> Direction {
    let delta = cur - prev;
    let threshold = (prev.abs() * eps).max(1e-9);
    if delta > threshold {
        Direction::Up
    } else if delta < -threshold {
        Direction::Down
    } else {
        Direction::Flat
    }
}

/// Verdict for one summarised metric: its value, both trend readings, and
/// whether the movement is good or bad given the metric's polarity.
#[derive(Debug, Clone, Serialize)]
pub struct MetricVerdict {
    pub label: String,
    pub value: f64,
    pub instant: Direction,
    pub rolling: Direction,
    /// True when the movement is in the bad direction for this metric.
    pub worsening: bool,
    /// True when clearly improving.
    pub improving: bool,
    /// True when an alert threshold was configured and the value has crossed it
    /// in the bad direction (above for lower-better, below for higher-better).
    pub breached: bool,
    /// True when `value` is a per-scrape rate (counter) rather than a gauge
    /// reading, so the display can mark it (e.g. a "/s" label suffix).
    pub is_rate: bool,
    /// The metric's functional category, for the grouped fleet summary.
    pub category: MetricCategory,
    /// True when the value actually changed since the previous scrape. Lets the
    /// display show a trend arrow only when something moved (a screen full of
    /// "flat" arrows communicates nothing).
    pub changed: bool,
    /// True when a higher-better metric is sitting at zero — i.e. the pipeline
    /// isn't moving. Notable even without a configured threshold: for a
    /// throughput metric, 0 is the alarming case, not the neutral one.
    pub stalled: bool,
    /// False when the scrape returned no sample for this configured metric — the
    /// pod isn't exposing it (or it's named differently). Rendered as "(no
    /// data)" so a configured-but-absent metric is visible, not silently
    /// dropped.
    pub present: bool,
}

/// Whether a direction is "bad" given the metric's polarity.
fn is_worsening(dir: Direction, pol: Polarity) -> bool {
    matches!(
        (dir, pol),
        (Direction::Up, Polarity::LowerBetter) | (Direction::Down, Polarity::HigherBetter)
    )
}

fn is_improving(dir: Direction, pol: Polarity) -> bool {
    matches!(
        (dir, pol),
        (Direction::Down, Polarity::LowerBetter) | (Direction::Up, Polarity::HigherBetter)
    )
}

/// Whether a value has crossed its alert threshold in the bad direction: above
/// for lower-better metrics, below for higher-better. No threshold, or neutral
/// polarity, never breaches.
fn threshold_breached(value: f64, threshold: Option<f64>, pol: Polarity) -> bool {
    let Some(t) = threshold else {
        return false;
    };
    match pol {
        Polarity::LowerBetter => value > t,
        Polarity::HigherBetter => value < t,
        Polarity::Neutral => false,
    }
}

/// Whether a metric is a cumulative counter (shown as a per-scrape rate) or an
/// instantaneous gauge (shown as its current value). Counters like the per-stage
/// `*_total` throughput are meaningless as a raw cumulative number — the rate is
/// what localises a bottleneck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricKind {
    Gauge,
    Counter,
}

impl Default for MetricKind {
    fn default() -> Self {
        MetricKind::Gauge
    }
}

/// Which functional group a metric belongs to, for the grouped fleet summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricCategory {
    Consumer,
    Throughput,
    Bottleneck,
    Health,
}

impl MetricCategory {
    pub fn label(self) -> &'static str {
        match self {
            MetricCategory::Consumer => "consumer",
            MetricCategory::Throughput => "throughput",
            MetricCategory::Bottleneck => "bottleneck",
            MetricCategory::Health => "health",
        }
    }
}

/// A metric to monitor: name, a human label, its polarity, and an optional
/// alert threshold. Produced either from the built-in curated defaults or from
/// the operator's `[[metrics.watch]]` config.
#[derive(Debug, Clone)]
pub struct MetricSpec {
    pub name: String,
    pub label: String,
    pub polarity: Polarity,
    pub threshold: Option<f64>,
    pub kind: MetricKind,
    pub category: MetricCategory,
}

/// The built-in curated specs, used when the operator hasn't configured their
/// own watch list.
pub fn default_specs() -> Vec<MetricSpec> {
    curated_series()
        .iter()
        .map(|(name, label, pol, kind, cat)| MetricSpec {
            name: (*name).to_string(),
            label: (*label).to_string(),
            polarity: *pol,
            threshold: None,
            kind: *kind,
            category: *cat,
        })
        .collect()
}

/// The curated set of series, each with a human label, polarity, kind, and
/// category. Names are matched so a labelled series (e.g. per-topic lag)
/// aggregates under one entry. `*_total` counters are Counter (shown as a rate).
pub fn curated_series() -> &'static [(&'static str, &'static str, Polarity, MetricKind, MetricCategory)]
{
    use MetricCategory::{Bottleneck, Consumer, Health, Throughput};
    use MetricKind::{Counter, Gauge};
    &[
        // Consumer health.
        ("ssync_pulsar_consumer_lag", "consumer lag", Polarity::LowerBetter, Gauge, Consumer),
        ("ssync_pulsar_backlog_bytes", "backlog bytes", Polarity::LowerBetter, Gauge, Consumer),
        ("ssync_pulsar_unacked_messages", "unacked", Polarity::LowerBetter, Gauge, Consumer),
        ("ssync_pulsar_reconnections", "reconnections", Polarity::LowerBetter, Gauge, Consumer),
        ("ssync_source_partitions_idle", "idle partitions", Polarity::LowerBetter, Gauge, Consumer),
        // Throughput (per stage) — counters shown as a rate.
        ("ssync_throughput_rate", "throughput rps", Polarity::HigherBetter, Gauge, Throughput),
        ("ssync_source_records_consumed_total", "consumed/s", Polarity::HigherBetter, Counter, Throughput),
        ("ssync_records_written_total", "written/s", Polarity::HigherBetter, Counter, Throughput),
        ("ssync_sink_records_sent_total", "sink sent/s", Polarity::HigherBetter, Counter, Throughput),
        // Bottleneck detection — channel depths/fill.
        ("ssync_source_channel_fill_ratio", "source chan fill", Polarity::LowerBetter, Gauge, Bottleneck),
        ("ssync_decoded_channel_depth", "decoded chan depth", Polarity::LowerBetter, Gauge, Bottleneck),
        ("ssync_batcher_channel_depth", "batcher chan depth", Polarity::LowerBetter, Gauge, Bottleneck),
        ("ssync_writer_channel_fill_ratio", "writer chan fill", Polarity::LowerBetter, Gauge, Bottleneck),
        // Health / pressure.
        ("ssync_backpressure_state", "backpressure", Polarity::LowerBetter, Gauge, Health),
        ("ssync_memory_rss_ratio", "mem rss ratio", Polarity::LowerBetter, Gauge, Health),
        ("ssync_pipeline_unhealthy", "unhealthy", Polarity::LowerBetter, Gauge, Health),
        ("ssync_destination_dlq_rows_total", "dlq rows/s", Polarity::LowerBetter, Counter, Health),
    ]
}

/// Whether an actual scraped metric name matches a configured name, tolerating
/// the type/unit suffixes a Prometheus/OpenTelemetry exporter appends. A counter
/// `ssync_x` may be exported as `ssync_x_total`; a byte gauge `ssync_x` as
/// `ssync_x_bytes`; and combinations like `ssync_x_bytes_total` occur. We accept
/// the exact name, or the name followed by any run of known suffixes.
fn name_matches(actual: &str, configured: &str) -> bool {
    if actual == configured {
        return true;
    }
    let Some(rest) = actual.strip_prefix(configured) else {
        return false;
    };
    // `rest` must be a sequence of `_<suffix>` segments and nothing else, so we
    // don't match a different metric that merely shares a prefix.
    // Type/unit suffixes an exporter appends to scalar metrics. Deliberately
    // excludes histogram components (_bucket/_count/_sum), which are separate
    // series and must not be folded into a scalar metric's value.
    const SUFFIXES: &[&str] = &[
        "_total",
        "_bytes",
        "_milliseconds",
        "_seconds",
        "_ratio",
    ];
    let mut rem = rest;
    if rem.is_empty() {
        return false;
    }
    while !rem.is_empty() {
        match SUFFIXES.iter().find(|suf| rem.starts_with(**suf)) {
            Some(suf) => rem = &rem[suf.len()..],
            None => return false,
        }
    }
    true
}

/// Aggregate the current value of a metric across all its label series (e.g.
/// per-topic lag summed). Matching tolerates exporter type/unit suffixes (see
/// `name_matches`). Histogram component series (`_bucket`) are excluded so a
/// histogram doesn't sum into a meaningless total.
fn aggregate_value(samples: &[Sample], name_prefix: &str) -> Option<f64> {
    let matching: Vec<f64> = samples
        .iter()
        .filter(|s| {
            // Exclude histogram component series entirely.
            !s.name.ends_with("_bucket")
                && !s.name.ends_with("_count")
                && !s.name.ends_with("_sum")
                && name_matches(&s.name, name_prefix)
        })
        .map(|s| s.value)
        .filter(|v| v.is_finite())
        .collect();
    if matching.is_empty() {
        None
    } else {
        Some(matching.iter().sum())
    }
}

/// Rolling per-pod metric history, keyed by watched metric name. Updated each
/// scrape against a set of specs (operator-configured or the built-in default);
/// produces the verdict list for the summary.
#[derive(Debug, Default)]
pub struct MetricHistory {
    series: BTreeMap<String, Series>,
}

impl MetricHistory {
    /// Fold a fresh scrape in, updating each watched metric's rolling history.
    pub fn update(&mut self, samples: &[Sample], specs: &[MetricSpec], window: usize) {
        for spec in specs {
            if let Some(v) = aggregate_value(samples, &spec.name) {
                self.series.entry(spec.name.clone()).or_default().push(v, window);
            }
        }
    }

    /// Produce a verdict per watched metric, ordered worst-first (threshold
    /// breaches, then worsening/stalled, then flat, then improving). A configured
    /// metric the scrape never returned gets a `present = false` verdict so it's
    /// shown as "(no data)" rather than silently dropped.
    pub fn verdicts(&self, specs: &[MetricSpec], eps: f64) -> Vec<MetricVerdict> {
        let mut out = Vec::new();
        for spec in specs {
            // No series (or no sample yet) → emit a "no data" verdict.
            let series = self.series.get(&spec.name);
            let current = series.and_then(|s| s.current());
            let Some(value) = current else {
                out.push(MetricVerdict {
                    label: spec.label.clone(),
                    value: 0.0,
                    instant: Direction::Flat,
                    rolling: Direction::Flat,
                    worsening: false,
                    improving: false,
                    breached: false,
                    is_rate: spec.kind == MetricKind::Counter,
                    category: spec.category,
                    changed: false,
                    stalled: false,
                    present: false,
                });
                continue;
            };
            let series = series.expect("series present when current() is Some");
            let instant = series.instant_direction(eps);
            let rolling = series.rolling_direction(eps);
            // Counters are shown as a per-scrape rate (the cumulative total is
            // meaningless to watch); gauges as their current value. A counter
            // with only one sample (no rate yet) falls back to 0 rate.
            let is_rate = spec.kind == MetricKind::Counter;
            let display_value = if is_rate {
                series.counter_rate().unwrap_or(0.0)
            } else {
                value
            };
            // Threshold is checked against the displayed value (the rate for a
            // counter, so a "writes/s below N" alert works as expected).
            let breached = threshold_breached(display_value, spec.threshold, spec.polarity);
            // Did the displayed value move since the last scrape?
            let changed = match (series.current(), series.previous()) {
                (Some(c), Some(p)) => (c - p).abs() > (p.abs() * eps).max(1e-9),
                _ => false,
            };
            // A higher-better metric at zero means the pipeline isn't moving —
            // notable even with no threshold configured.
            let stalled = spec.polarity == Polarity::HigherBetter && display_value == 0.0;
            out.push(MetricVerdict {
                label: spec.label.clone(),
                value: display_value,
                instant,
                rolling,
                worsening: is_worsening(rolling, spec.polarity),
                improving: is_improving(rolling, spec.polarity),
                breached,
                is_rate,
                category: spec.category,
                changed,
                stalled,
                present: true,
            });
        }
        // Worst-first: breach, then worsening/stalled, then flat, then improving,
        // then absent (no data) last.
        out.sort_by_key(|v| {
            if !v.present {
                4
            } else if v.breached {
                0
            } else if v.worsening || v.stalled {
                1
            } else if v.improving {
                3
            } else {
                2
            }
        });
        out
    }
}

/// A one-line-per-metric summary string for logging (per pod, per minute).
pub fn format_summary(pod: &str, verdicts: &[MetricVerdict]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = write!(out, "metrics[{pod}]:");
    for v in verdicts {
        if !v.present {
            let _ = write!(out, " {}=(no data)", v.label);
            continue;
        }
        // Arrow only when the value moved.
        let arrow = if v.changed {
            match v.rolling {
                Direction::Up => "↑",
                Direction::Down => "↓",
                Direction::Flat => "",
            }
        } else {
            ""
        };
        let mark = if v.breached {
            " ‼"
        } else if v.stalled {
            " ⊘" // stalled: higher-better sitting at zero
        } else if v.worsening {
            " ⚠"
        } else if v.improving {
            " ✓"
        } else {
            ""
        };
        let suffix = if v.is_rate { "/s" } else { "" };
        let _ = write!(out, " {}={:.0}{suffix}{arrow}{mark}", v.label, v.value);
    }
    out
}

/// Tracks rolling metric history across scrapes for every pod, produces the
/// per-pod summary lines, and (optionally) captures raw scraped metrics to disk
/// as JSONL for later tuning. Lives across refresh cycles so trends accumulate.
#[derive(Debug, Default)]
pub struct MetricsTracker {
    /// Per-pod rolling histories.
    per_pod: BTreeMap<String, MetricHistory>,
    window: usize,
    /// The metrics to watch — operator-configured or the built-in default.
    specs: Vec<MetricSpec>,
}

impl MetricsTracker {
    /// Build a tracker with an explicit set of specs. Pass `default_specs()` for
    /// the built-in curated set, or specs derived from `[[metrics.watch]]`.
    pub fn new(window: usize, specs: Vec<MetricSpec>) -> Self {
        let specs = if specs.is_empty() {
            default_specs()
        } else {
            specs
        };
        MetricsTracker {
            per_pod: BTreeMap::new(),
            window: window.max(1),
            specs,
        }
    }

    /// Fold one pod's freshly-scraped Prometheus text into its history and
    /// return the summary line for logging. `eps` is the flat-threshold.
    pub fn observe(&mut self, pod: &str, metrics_text: &str, eps: f64) -> String {
        let samples = parse_prometheus(metrics_text);
        let hist = self.per_pod.entry(pod.to_string()).or_default();
        hist.update(&samples, &self.specs, self.window);
        let verdicts = hist.verdicts(&self.specs, eps);
        format_summary(pod, &verdicts)
    }

    /// Verdicts for a pod, for a caller that wants to render them (TUI) rather
    /// than the formatted string.
    pub fn verdicts_for(&self, pod: &str, eps: f64) -> Vec<MetricVerdict> {
        self.per_pod
            .get(pod)
            .map(|h| h.verdicts(&self.specs, eps))
            .unwrap_or_default()
    }
}

/// Build one JSONL record capturing every scraped sample for a pod at a moment,
/// for offline tuning. Shape: `{"as_of":..,"pod":..,"metrics":{series_key:val}}`.
/// Pure (returns the string); the caller appends it to the capture file.
pub fn capture_record(as_of: &str, pod: &str, samples: &[Sample]) -> String {
    let map: BTreeMap<String, f64> = samples
        .iter()
        .filter(|s| s.value.is_finite())
        .map(|s| (s.series_key(), s.value))
        .collect();
    // Hand-build to avoid pulling serde_json structures for one line; values are
    // plain floats and keys are already-escaped-enough metric identifiers.
    let mut parts: Vec<String> = Vec::with_capacity(map.len());
    for (k, v) in &map {
        parts.push(format!("{}:{}", json_string(k), v));
    }
    format!(
        "{{\"as_of\":{},\"pod\":{},\"metrics\":{{{}}}}}",
        json_string(as_of),
        json_string(pod),
        parts.join(",")
    )
}

/// Minimal JSON string escaping for the capture record.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_accumulates_and_summarises() {
        let mut t = MetricsTracker::new(5, default_specs());
        let scrape1 = "ssync_pulsar_consumer_lag 100\nssync_throughput_rate 50";
        let scrape2 = "ssync_pulsar_consumer_lag 200\nssync_throughput_rate 40";
        t.observe("pod-a", scrape1, 0.01);
        let line = t.observe("pod-a", scrape2, 0.01);
        // Lag rose (bad), throughput fell (bad) → both flagged.
        assert!(line.contains("metrics[pod-a]"));
        assert!(line.contains("consumer lag=200"));
        let verdicts = t.verdicts_for("pod-a", 0.01);
        let lag = verdicts.iter().find(|v| v.label == "consumer lag").unwrap();
        assert!(lag.worsening);
    }

    #[test]
    fn capture_record_is_valid_jsonish() {
        let samples = vec![
            Sample { name: "m".into(), labels: vec![("t".into(), "x".into())], value: 1.5 },
            Sample { name: "n".into(), labels: vec![], value: 2.0 },
        ];
        let rec = capture_record("2026-07-31T00:00:00Z", "pod-a", &samples);
        assert!(rec.starts_with("{\"as_of\":\"2026-07-31T00:00:00Z\",\"pod\":\"pod-a\""));
        assert!(rec.contains("\"m{t=\\\"x\\\"}\":1.5"));
        assert!(rec.contains("\"n\":2"));
    }

    #[test]
    fn counter_metrics_shown_as_rate() {
        // A counter spec: verdict value should be the per-scrape delta, not the
        // cumulative total.
        let specs = vec![MetricSpec {
            name: "ssync_records_written_total".into(),
            label: "written/s".into(),
            polarity: Polarity::HigherBetter,
            threshold: Some(100.0),
            kind: MetricKind::Counter,
            category: MetricCategory::Throughput,
        }];
        let mut h = MetricHistory::default();
        // Cumulative totals 1000 → 1150 → 1300: rate is 150/scrape.
        for total in [1000.0, 1150.0, 1300.0] {
            let samples = vec![Sample {
                name: "ssync_records_written_total".into(),
                labels: vec![],
                value: total,
            }];
            h.update(&samples, &specs, 5);
        }
        let v = h.verdicts(&specs, 0.01);
        let written = v.iter().find(|x| x.label == "written/s").unwrap();
        assert!(written.is_rate, "counter should be flagged as a rate");
        assert_eq!(written.value, 150.0, "value should be the per-scrape rate");
        assert!(!written.breached, "150/s is above the 100 floor");
    }

    #[test]
    fn parses_labeled_and_bare_lines() {
        let text = "\
# HELP ssync_throughput_rate rps
# TYPE ssync_throughput_rate gauge
ssync_throughput_rate 230.5
ssync_pulsar_consumer_lag{topic=\"a\",partition=\"0\"} 1200
ssync_pulsar_consumer_lag{topic=\"a\",partition=\"1\"} 800

malformed line with no value structure that still has spaces
ssync_pipeline_unhealthy 0";
        let samples = parse_prometheus(text);
        // 4 valid samples (the "malformed" line actually parses name=malformed,
        // value=line? no — "malformed" then "line" isn't a float, so skipped).
        let rate = samples.iter().find(|s| s.name == "ssync_throughput_rate").unwrap();
        assert_eq!(rate.value, 230.5);
        assert!(rate.labels.is_empty());
        let lag: Vec<&Sample> = samples.iter().filter(|s| s.name == "ssync_pulsar_consumer_lag").collect();
        assert_eq!(lag.len(), 2);
        // Labels are sorted.
        assert_eq!(lag[0].labels[0].0, "partition");
        assert_eq!(lag[0].labels[1].0, "topic");
    }

    #[test]
    fn series_key_is_stable() {
        let s = Sample {
            name: "m".into(),
            labels: vec![("b".into(), "2".into()), ("a".into(), "1".into())],
            value: 1.0,
        };
        // labels already sorted by parse, but construct here unsorted to confirm
        // the key format; sort manually as parse would.
        let mut s2 = s.clone();
        s2.labels.sort();
        assert_eq!(s2.series_key(), "m{a=\"1\",b=\"2\"}");
    }

    #[test]
    fn instant_and_rolling_direction() {
        let mut s = Series::default();
        for v in [100.0, 110.0, 120.0, 130.0] {
            s.push(v, 10);
        }
        assert_eq!(s.instant_direction(0.01), Direction::Up);
        assert_eq!(s.rolling_direction(0.01), Direction::Up);
        assert_eq!(s.current(), Some(130.0));

        // A tiny wobble reads as flat.
        let mut f = Series::default();
        f.push(1000.0, 10);
        f.push(1002.0, 10); // 0.2% < 1% eps
        assert_eq!(f.instant_direction(0.01), Direction::Flat);
    }

    #[test]
    fn window_caps_history() {
        let mut s = Series::default();
        for v in 0..10 {
            s.push(v as f64, 3);
        }
        // Only the last 3 kept.
        assert_eq!(s.current(), Some(9.0));
        assert_eq!(s.previous(), Some(8.0));
    }

    #[test]
    fn counter_rate_and_reset() {
        let mut s = Series::default();
        s.push(100.0, 5);
        s.push(150.0, 5);
        assert_eq!(s.counter_rate(), Some(50.0));
        // Reset (counter went down) → None.
        s.push(10.0, 5);
        assert_eq!(s.counter_rate(), None);
    }

    #[test]
    fn verdicts_flag_worsening_by_polarity() {
        let mut h = MetricHistory::default();
        let specs = default_specs();
        // Lag climbing (LowerBetter) → worsening. Throughput climbing → improving.
        for (lag, tput) in [(100.0, 50.0), (200.0, 60.0), (300.0, 70.0)] {
            let samples = vec![
                Sample { name: "ssync_pulsar_consumer_lag".into(), labels: vec![], value: lag },
                Sample { name: "ssync_throughput_rate".into(), labels: vec![], value: tput },
            ];
            h.update(&samples, &specs, 5);
        }
        let v = h.verdicts(&specs, 0.01);
        let lag = v.iter().find(|x| x.label == "consumer lag").unwrap();
        assert!(lag.worsening, "climbing lag should be worsening");
        let tput = v.iter().find(|x| x.label == "throughput rps").unwrap();
        assert!(tput.improving, "climbing throughput should be improving");
        // Worst-first ordering: the worsening metric sorts before the improving one.
        assert_eq!(v[0].label, "consumer lag");
    }

    #[test]
    fn custom_specs_and_thresholds() {
        // Operator watches one metric with a threshold; breach outranks trend.
        let specs = vec![
            MetricSpec {
                name: "ssync_pulsar_consumer_lag".into(),
                label: "lag".into(),
                polarity: Polarity::LowerBetter,
                threshold: Some(1000.0),
                kind: MetricKind::Gauge,
                category: MetricCategory::Consumer,
            },
            MetricSpec {
                name: "ssync_throughput_rate".into(),
                label: "tput".into(),
                polarity: Polarity::HigherBetter,
                threshold: Some(100.0),
                kind: MetricKind::Gauge,
                category: MetricCategory::Throughput,
            },
        ];
        let mut h = MetricHistory::default();
        // Lag under threshold, then over; throughput below its floor.
        for (lag, tput) in [(500.0, 50.0), (1500.0, 40.0)] {
            let samples = vec![
                Sample { name: "ssync_pulsar_consumer_lag".into(), labels: vec![], value: lag },
                Sample { name: "ssync_throughput_rate".into(), labels: vec![], value: tput },
            ];
            h.update(&samples, &specs, 5);
        }
        let v = h.verdicts(&specs, 0.01);
        // Only the two configured metrics appear (not the whole curated set).
        assert_eq!(v.len(), 2);
        let lag = v.iter().find(|x| x.label == "lag").unwrap();
        assert!(lag.breached, "lag 1500 > threshold 1000 should breach");
        let tput = v.iter().find(|x| x.label == "tput").unwrap();
        assert!(tput.breached, "throughput 40 < floor 100 should breach");
    }

    #[test]
    fn threshold_breach_direction() {
        // lower_better: breach when above.
        assert!(threshold_breached(150.0, Some(100.0), Polarity::LowerBetter));
        assert!(!threshold_breached(50.0, Some(100.0), Polarity::LowerBetter));
        // higher_better: breach when below.
        assert!(threshold_breached(40.0, Some(100.0), Polarity::HigherBetter));
        assert!(!threshold_breached(150.0, Some(100.0), Polarity::HigherBetter));
        // no threshold never breaches.
        assert!(!threshold_breached(999.0, None, Polarity::LowerBetter));
    }

    #[test]
    fn aggregates_labeled_series() {
        // Per-partition lag sums into one headline value.
        let samples = vec![
            Sample { name: "ssync_pulsar_consumer_lag".into(), labels: vec![("partition".into(), "0".into())], value: 1200.0 },
            Sample { name: "ssync_pulsar_consumer_lag".into(), labels: vec![("partition".into(), "1".into())], value: 800.0 },
        ];
        assert_eq!(aggregate_value(&samples, "ssync_pulsar_consumer_lag"), Some(2000.0));
    }

    #[test]
    fn name_matches_tolerates_exporter_suffixes() {
        // Exact.
        assert!(name_matches("ssync_decoded_channel_depth", "ssync_decoded_channel_depth"));
        // Single _total (counter convention).
        assert!(name_matches("ssync_batches_flushed_total", "ssync_batches_flushed"));
        // Double _total (config already ended in _total, exporter added another).
        assert!(name_matches("ssync_sink_records_sent_total_total", "ssync_sink_records_sent_total"));
        // Unit + total combo.
        assert!(name_matches("ssync_bytes_received_total_bytes_total", "ssync_bytes_received_total"));
        // Unit only.
        assert!(name_matches("ssync_memory_rss_bytes_bytes", "ssync_memory_rss_bytes"));
        // A different metric sharing a prefix must NOT match.
        assert!(!name_matches("ssync_batch_size_count", "ssync_batch"));
        assert!(!name_matches("ssync_batcher_channel_depth", "ssync_batch"));
        // Non-suffix trailing text must not match.
        assert!(!name_matches("ssync_batch_extra", "ssync_batch"));
    }

    #[test]
    fn aggregate_finds_suffixed_counter() {
        let samples = vec![
            Sample { name: "ssync_sink_records_sent_total_total".into(), labels: vec![], value: 4200.0 },
        ];
        // Configured without the exporter's extra _total.
        assert_eq!(aggregate_value(&samples, "ssync_sink_records_sent_total"), Some(4200.0));
    }

    #[test]
    fn aggregate_excludes_histogram_components() {
        let samples = vec![
            Sample { name: "ssync_batch_size_bucket".into(), labels: vec![("le".into(), "10".into())], value: 99.0 },
            Sample { name: "ssync_batch_size_count".into(), labels: vec![], value: 5.0 },
            Sample { name: "ssync_batch_size_sum".into(), labels: vec![], value: 50.0 },
        ];
        // None of the histogram components should match a scalar "ssync_batch_size".
        assert_eq!(aggregate_value(&samples, "ssync_batch_size"), None);
    }

    #[test]
    fn absent_metric_emits_no_data_verdict() {
        // Configure two metrics; scrape returns only one. The absent one still
        // gets a verdict, marked not-present.
        let specs = vec![
            MetricSpec {
                name: "ssync_present_one".into(),
                label: "present".into(),
                polarity: Polarity::LowerBetter,
                threshold: None,
                kind: MetricKind::Gauge,
                category: MetricCategory::Health,
            },
            MetricSpec {
                name: "ssync_missing_one".into(),
                label: "missing".into(),
                polarity: Polarity::LowerBetter,
                threshold: None,
                kind: MetricKind::Gauge,
                category: MetricCategory::Health,
            },
        ];
        let mut h = MetricHistory::default();
        let samples = vec![Sample { name: "ssync_present_one".into(), labels: vec![], value: 5.0 }];
        h.update(&samples, &specs, 5);
        let v = h.verdicts(&specs, 0.01);
        assert_eq!(v.len(), 2, "both configured metrics get a verdict");
        let present = v.iter().find(|x| x.label == "present").unwrap();
        assert!(present.present);
        let missing = v.iter().find(|x| x.label == "missing").unwrap();
        assert!(!missing.present, "absent metric marked not present");
        // Absent sorts after present.
        assert_eq!(v.last().unwrap().label, "missing");
    }

    #[test]
    fn higher_better_at_zero_is_stalled() {
        let specs = vec![MetricSpec {
            name: "ssync_throughput_rate".into(),
            label: "tput".into(),
            polarity: Polarity::HigherBetter,
            threshold: None,
            kind: MetricKind::Gauge,
            category: MetricCategory::Throughput,
        }];
        let mut h = MetricHistory::default();
        for v in [0.0, 0.0] {
            let samples = vec![Sample { name: "ssync_throughput_rate".into(), labels: vec![], value: v }];
            h.update(&samples, &specs, 5);
        }
        let verdicts = h.verdicts(&specs, 0.01);
        let t = verdicts.iter().find(|x| x.label == "tput").unwrap();
        assert!(t.stalled, "higher-better metric at 0 should be flagged stalled");
        assert!(!t.changed, "0 → 0 didn't change");
    }

    #[test]
    fn summary_formats_with_arrows() {
        let verdicts = vec![MetricVerdict {
            label: "consumer lag".into(),
            value: 2000.0,
            instant: Direction::Up,
            rolling: Direction::Up,
            worsening: true,
            improving: false,
            breached: false,
            is_rate: false,
            category: MetricCategory::Consumer,
            changed: true,
            stalled: false,
            present: true,
        }];
        let s = format_summary("pod-a", &verdicts);
        assert!(s.contains("metrics[pod-a]"));
        assert!(s.contains("consumer lag=2000↑ ⚠"));
    }
}
