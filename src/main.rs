//! pulsar-topic-health — topic-level health summary for a Pulsar subscription.
//!
//! Reads a TOML config listing the topics you care about, fetches admin stats
//! for each, and reports: per-partition backlogs over a threshold, partitions
//! or topics where the subscription is missing or consumer-less, and stats
//! fetch failures.
//!
//! Exit codes: 0 = all healthy, 1 = usage/runtime error, 2 = unhealthy topics.

mod config;
mod cursor;
mod drain;
mod health;
mod kube;
mod metrics;
mod output;
mod pulsar;
mod snapshot;
mod state;
mod timestamp;
mod tui;
mod view;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, ValueEnum};

use crate::config::Config;
use crate::drain::evaluate_drain;
use crate::health::{check_topic, sample_backlog, TopicHealth};
use crate::pulsar::{AdminClient, TopicName};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Table,
    Jsonl,
}

#[derive(Debug, Parser)]
#[command(name = "pulsar-topic-health", version, about, disable_version_flag = true)]
struct Cli {
    /// Print version.
    #[arg(short = 'v', long = "version", action = clap::ArgAction::Version)]
    version: Option<bool>,

    /// Path to the TOML config file.
    #[arg(short, long, default_value = "topics.toml")]
    config: PathBuf,

    /// Output format.
    #[arg(long, value_enum)]
    format: Option<Format>,

    /// Override the per-partition backlog threshold from the config.
    #[arg(long)]
    threshold: Option<i64>,

    /// Override the subscription name from the config.
    #[arg(short, long)]
    subscription: Option<String>,

    /// Pulsar admin service URL (overrides config).
    #[arg(long, env = "PULSAR_ADMIN_URL")]
    admin_url: Option<String>,

    /// Concurrent stats requests.
    #[arg(long)]
    concurrency: Option<usize>,

    /// Per-request timeout in seconds.
    #[arg(long)]
    timeout_secs: Option<u64>,

    /// Seconds between the two backlog samples used to compute drain trend and
    /// ETA-to-clear. Set 0 to skip the second sample and the trend columns.
    /// Ignored in --watch mode (trend is derived from consecutive cycles).
    #[arg(long)]
    drain_window_secs: Option<u64>,

    /// Run continuously, redrawing every --watch-interval-secs. Trend is derived
    /// from the previous cycle rather than a mid-cycle second sample.
    #[arg(long)]
    watch: bool,

    /// Launch the interactive TUI (view toggle, cursor navigation, drill-in).
    #[arg(long)]
    tui: bool,

    /// Seconds between watch cycles (only with --watch).
    #[arg(long)]
    watch_interval_secs: Option<u64>,

    /// Directory to write a JSON snapshot per cycle (created if absent). Works
    /// with or without --watch; in a single run it writes one snapshot.
    #[arg(long)]
    json_dir: Option<PathBuf>,

    /// Maximum snapshot files to keep in --json-dir; older ones are pruned.
    /// 0 means keep all.
    #[arg(long)]
    json_dir_max_files: Option<usize>,

    /// Only show unhealthy topics in the output.
    #[arg(long)]
    problems_only: bool,

    /// Also fetch and display Kubernetes health for the consumer deployment,
    /// correlating pod status with topic health. Requires the tool to be built
    /// with `--features kube`.
    #[arg(long)]
    kube: bool,

    /// Kubernetes namespace of the consumer pods (overrides config; default
    /// `default`).
    #[arg(long)]
    kube_namespace: Option<String>,

    /// Label selector for the consumer pods, e.g. `app=my-consumer` (overrides
    /// config).
    #[arg(long)]
    kube_selector: Option<String>,

    /// ConfigMap name to read `config.toml` from for --kube-assert checks
    /// (overrides config).
    #[arg(long)]
    kube_configmap: Option<String>,

    /// Assert a config.toml `key=value` (repeatable), merged with config
    /// asserts (CLI wins on duplicate keys).
    #[arg(long = "kube-assert", value_parser = parse_key_value)]
    kube_assert: Vec<(String, String)>,

    /// Scan the last N log lines per pod for ramp/OOM/error signals (overrides
    /// config; default 200, 0 = skip).
    #[arg(long)]
    kube_log_tail: Option<i64>,
}

/// Parse a `key=value` pair for --kube-assert.
fn parse_key_value(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() => Ok((k.trim().to_string(), v.trim().to_string())),
        _ => Err(format!("expected key=value, got '{s}'")),
    }
}

impl Cli {
    /// Resolved values with their final defaults applied, after config merge.
    /// These are the single source of truth downstream code reads, so the
    /// clap-default / config / CLI precedence lives in exactly one place.
    fn format(&self) -> Format {
        self.format.unwrap_or(Format::Table)
    }
    fn concurrency(&self) -> usize {
        self.concurrency.unwrap_or(8)
    }
    fn timeout_secs(&self) -> u64 {
        self.timeout_secs.unwrap_or(30)
    }
    fn drain_window_secs(&self) -> u64 {
        self.drain_window_secs.unwrap_or(30)
    }
    fn watch_interval_secs(&self) -> u64 {
        self.watch_interval_secs.unwrap_or(60)
    }
    fn json_dir_max_files(&self) -> usize {
        self.json_dir_max_files.unwrap_or(100)
    }
}

/// Fold `[settings]` config defaults into the parsed CLI. For Option fields a
/// present CLI value wins; else the config value fills in; else the accessor
/// applies the final default. Bare bools are OR-ed — the flag or config can
/// enable a mode, neither can force it off (clap bools have no explicit false).
fn apply_settings(cli: &mut Cli, s: &config::Settings) {
    if cli.format.is_none() {
        cli.format = s.format.map(|f| match f {
            config::OutputFormat::Table => Format::Table,
            config::OutputFormat::Jsonl => Format::Jsonl,
        });
    }
    cli.concurrency = cli.concurrency.or(s.concurrency);
    cli.timeout_secs = cli.timeout_secs.or(s.timeout_secs);
    cli.drain_window_secs = cli.drain_window_secs.or(s.drain_window_secs);
    cli.watch_interval_secs = cli.watch_interval_secs.or(s.watch_interval_secs);
    cli.json_dir_max_files = cli.json_dir_max_files.or(s.json_dir_max_files);
    cli.json_dir = cli.json_dir.clone().or_else(|| s.json_dir.clone());
    cli.watch |= s.watch.unwrap_or(false);
    cli.tui |= s.tui.unwrap_or(false);
    cli.problems_only |= s.problems_only.unwrap_or(false);
}

fn main() -> ExitCode {
    match run() {
        Ok(exit) => exit,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let mut cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    // Merge [settings] into the CLI (CLI wins). Done here so every downstream
    // function just reads the already-resolved `cli`.
    apply_settings(&mut cli, &config.settings);

    let admin_url = cli
        .admin_url
        .clone()
        .or(config.admin_url)
        .context("admin URL not set: provide `admin_url` in config, --admin-url, or PULSAR_ADMIN_URL")?;
    let subscription = cli.subscription.clone().unwrap_or(config.subscription);
    let threshold = cli.threshold.unwrap_or(config.backlog_threshold);
    let token = resolve_token()?;

    let topics = parse_topics(&config.topics)?;
    let client = AdminClient::new(&admin_url, &token, Duration::from_secs(cli.timeout_secs()));

    let colors = config.colors;
    let kube_config = config.kube;
    let metrics_config = config.metrics;
    let admin_config = config.admin;

    if cli.tui {
        return run_tui(&cli, &kube_config, &metrics_config, &admin_config, &client, &topics, &subscription, threshold);
    }
    if cli.watch {
        watch_loop(&cli, &colors, &kube_config, &metrics_config, &client, &topics, &subscription, threshold)
    } else {
        single_run(&cli, &colors, &kube_config, &metrics_config, &client, &topics, &subscription, threshold)
    }
}

/// Launch the interactive TUI.
fn run_tui(
    cli: &Cli,
    kube_config: &config::KubeConfig,
    metrics_config: &config::MetricsConfig,
    admin_config: &config::AdminConfig,
    client: &AdminClient,
    topics: &[TopicName],
    subscription: &str,
    threshold: i64,
) -> anyhow::Result<ExitCode> {
    use std::collections::HashMap;

    // Inter-cycle drain: remember the previous cycle's backlogs, like watch mode.
    let mut prev: Option<(HashMap<String, i64>, std::time::Instant)> = None;
    let mut prev_state: HashMap<String, state::PriorState> = HashMap::new();
    let mut metrics_tracker =
        metrics::MetricsTracker::new(metrics_config.window, watch_specs(metrics_config));

    let refresh = move || {
        let now = std::time::Instant::now();
        let run_at = timestamp::now_rfc3339();
        let mut results = check_all(client, topics, subscription, threshold, cli.concurrency());
        if let Some((prev_backlogs, prev_at)) = &prev {
            let window = now.duration_since(*prev_at).as_secs_f64();
            apply_interval_drain(&mut results, prev_backlogs, window);
        }
        state::assign_state_since(&mut results, &prev_state, &run_at);
        prev_state = state::prior_from_results(&results);
        let backlogs: HashMap<String, i64> =
            results.iter().map(|h| (h.topic.clone(), h.backlog_or_zero())).collect();
        prev = Some((backlogs, now));

        let mut kube = fetch_kube_report(cli, kube_config, metrics_config);
        if let Some(report) = &mut kube {
            annotate_with_kube(&mut results, report);
            if metrics_config.enabled {
                process_metrics(
                    report,
                    &mut metrics_tracker,
                    metrics_config.capture_dir.as_deref(),
                    &run_at,
                    false, // TUI owns the screen — don't eprintln over it.
                );
            }
        }

        // Persist the snapshot, same as watch mode — otherwise --json-dir is
        // silently ignored in --tui. Written before any display filtering so the
        // on-disk history is complete. A write error is swallowed here rather
        // than eprintln'd, which would corrupt the ratatui screen.
        if let Some(dir) = &cli.json_dir {
            let _ = snapshot::write_snapshot(dir, &results, &run_at, cli.json_dir_max_files());
        }

        tui::Frame {
            run_at,
            topics: results,
            kube,
            subscription: subscription.to_string(),
        }
    };

    // Gate one: build an action executor ONLY if admin.allow_actions is true.
    // When None, the TUI's action keys are inert — the tool stays read-only.
    let executor = build_executor(admin_config);

    tui::run(
        Box::new(refresh),
        Duration::from_secs(cli.watch_interval_secs().max(1)),
        executor,
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Build the destructive-action executor for the TUI. Returns `Some` only when
/// `admin.allow_actions` is true (gate one). The closure blocks on the async
/// kube action in a short-lived runtime and returns a result message.
#[cfg(feature = "kube")]
fn build_executor(admin_config: &config::AdminConfig) -> Option<Box<tui::Executor<'static>>> {
    if !admin_config.allow_actions {
        return None;
    }
    Some(Box::new(move |action: view::PendingAction| {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => return format!("failed to start runtime: {e}"),
        };
        rt.block_on(async move {
            use view::PendingAction as A;
            match action {
                A::DeletePod { namespace, pod } => {
                    kube::actions::delete_pod(&namespace, &pod).await.message().to_string()
                }
                A::RecycleAll { namespace, selector } => {
                    let outcomes = kube::actions::recycle_all(
                        &namespace,
                        &selector,
                        std::time::Duration::from_secs(120),
                    )
                    .await;
                    summarize_outcomes(&outcomes)
                }
                A::CordonDrainNode { node } => {
                    summarize_outcomes(&kube::actions::cordon_drain_node(&node).await)
                }
            }
        })
    }))
}

/// With the kube feature off, there are no actions to execute.
#[cfg(not(feature = "kube"))]
fn build_executor(_admin_config: &config::AdminConfig) -> Option<Box<tui::Executor<'static>>> {
    None
}

/// Fold a multi-step action's outcomes into one status-line message: the first
/// error if any step failed, else the last success.
#[cfg(feature = "kube")]
fn summarize_outcomes(outcomes: &[kube::actions::ActionOutcome]) -> String {
    if let Some(err) = outcomes.iter().find(|o| !o.is_ok()) {
        return err.message().to_string();
    }
    outcomes
        .last()
        .map(|o| o.message().to_string())
        .unwrap_or_else(|| "no-op".to_string())
}

/// One-shot run: check, optionally take a mid-cycle drain sample, render, and
/// optionally write a JSON snapshot. Exit code reflects health.
fn single_run(
    cli: &Cli,
    colors: &config::ColorThresholds,
    kube_config: &config::KubeConfig,
    metrics_config: &config::MetricsConfig,
    client: &AdminClient,
    topics: &[TopicName],
    subscription: &str,
    threshold: i64,
) -> anyhow::Result<ExitCode> {
    let run_at = timestamp::now_rfc3339();
    let mut results = check_all(client, topics, subscription, threshold, cli.concurrency());

    // Kubernetes correlation (optional). Fetch this *before* the drain sample:
    // if --kube matched no pods, we're going to exit with discovery help
    // regardless, so there's no point waiting out the drain window first.
    let kube_report = fetch_kube_report(cli, kube_config, metrics_config);
    if let Some(report) = &kube_report
        && let Some(discovery) = &report.discovery {
            eprintln!("kubernetes: namespace {}", report.namespace);
            eprint!("{}", crate::kube::format_discovery(discovery));
            return Ok(ExitCode::from(3));
        }

    if cli.drain_window_secs() > 0 {
        measure_drain(
            client,
            topics,
            subscription,
            &mut results,
            cli.drain_window_secs(),
            cli.concurrency(),
        );
    }

    // Time-in-state: read prior state from the latest snapshot (if a json-dir is
    // in use), then stamp each topic's state_since. A single run with no
    // snapshot history has no prior, so every topic starts "now".
    let prior = cli
        .json_dir
        .as_ref()
        .and_then(|dir| snapshot::read_latest_snapshot(dir))
        .map(|json| state::prior_from_snapshot_json(&json))
        .unwrap_or_default();
    state::assign_state_since(&mut results, &prior, &run_at);

    if let Some(report) = &kube_report {
        annotate_with_kube(&mut results, report);
    }

    if let Some(dir) = &cli.json_dir
        && let Err(err) = snapshot::write_snapshot(dir, &results, &run_at, cli.json_dir_max_files()) {
            eprintln!("Warning: {err}");
        }

    let display: Vec<TopicHealth> = if cli.problems_only {
        results.into_iter().filter(|h| !h.status.is_healthy()).collect()
    } else {
        results
    };

    match cli.format() {
        Format::Table => {
            println!("as of {run_at}");
            if let Some(report) = &kube_report {
                print!("{}", output::render_kube_section(report));
            }
            println!("{}", output::render_table(&display, colors, &run_at));
        }
        Format::Jsonl => {
            if let Some(report) = &kube_report
                && let Ok(line) = serde_json::to_string(report) {
                    println!("{line}");
                }
            print!("{}", output::render_jsonl(&display, &run_at)?);
        }
    }

    let unhealthy = display.iter().filter(|h| !h.status.is_healthy()).count();
    let kube_unhealthy = kube_report.as_ref().map(|r| !r.is_healthy()).unwrap_or(false);
    if unhealthy > 0 {
        eprintln!(
            "{unhealthy} of {} topic(s) unhealthy (subscription: {subscription}, threshold: {threshold})",
            topics.len()
        );
    }
    if let Some(report) = &kube_report {
        eprintln!("{}", report.summary_line());
    }
    if unhealthy > 0 || kube_unhealthy {
        return Ok(ExitCode::from(2));
    }
    Ok(ExitCode::SUCCESS)
}

/// Attach a Kubernetes correlation hint to the DETAIL of each unhealthy topic.
fn annotate_with_kube(results: &mut [TopicHealth], report: &kube::KubeReport) {
    for health in results.iter_mut() {
        let topic_unhealthy = !health.status.is_healthy();
        if let Some(hint) = kube::correlation_hint(report, topic_unhealthy) {
            health.kube_hint = Some(hint);
        }
    }
}

/// Continuous watch: redraw every `--watch-interval-secs`, deriving drain trend
/// from the previous cycle (no mid-cycle sleep). Runs until interrupted.
fn watch_loop(
    cli: &Cli,
    colors: &config::ColorThresholds,
    kube_config: &config::KubeConfig,
    metrics_config: &config::MetricsConfig,
    client: &AdminClient,
    topics: &[TopicName],
    subscription: &str,
    threshold: i64,
) -> anyhow::Result<ExitCode> {
    let interval = Duration::from_secs(cli.watch_interval_secs().max(1));

    // Rolling metrics tracker, persisted across cycles so trends accumulate.
    let mut metrics_tracker =
        metrics::MetricsTracker::new(metrics_config.window, watch_specs(metrics_config));

    // Previous cycle's per-topic backlog (keyed by topic name) and the instant
    // it was captured, so the next cycle can compute net drain over real
    // elapsed time.
    let mut prev: Option<(std::collections::HashMap<String, i64>, std::time::Instant)> = None;

    // Prior state for time-in-state. Seeded once from the latest snapshot (so a
    // restarted watch inherits history), then carried forward in memory from
    // each cycle's results.
    let mut prior_state: std::collections::HashMap<String, state::PriorState> = cli
        .json_dir
        .as_ref()
        .and_then(|dir| snapshot::read_latest_snapshot(dir))
        .map(|json| state::prior_from_snapshot_json(&json))
        .unwrap_or_default();

    loop {
        let cycle_start = std::time::Instant::now();
        let run_at = timestamp::now_rfc3339();
        let mut results = check_all(client, topics, subscription, threshold, cli.concurrency());

        // Derive drain from the previous cycle rather than a fresh second sample.
        if let Some((prev_backlogs, prev_at)) = &prev {
            let window = cycle_start.duration_since(*prev_at).as_secs_f64();
            apply_interval_drain(&mut results, prev_backlogs, window);
        }

        // Stamp time-in-state against the prior cycle/snapshot, then carry this
        // cycle's states forward for the next iteration.
        state::assign_state_since(&mut results, &prior_state, &run_at);
        prior_state = state::prior_from_results(&results);

        // Kubernetes correlation each cycle (optional).
        let mut kube_report = fetch_kube_report(cli, kube_config, metrics_config);
        if let Some(report) = &mut kube_report {
            annotate_with_kube(&mut results, report);
            if metrics_config.enabled {
                process_metrics(
                    report,
                    &mut metrics_tracker,
                    metrics_config.capture_dir.as_deref(),
                    &run_at,
                    true, // plain watch mode — stderr summary is fine.
                );
            }
        }

        // Snapshot the full (unfiltered) results before any --problems-only trim.
        if let Some(dir) = &cli.json_dir
            && let Err(err) =
                snapshot::write_snapshot(dir, &results, &run_at, cli.json_dir_max_files())
            {
                eprintln!("Warning: {err}");
            }

        // Remember this cycle's backlogs for the next iteration's drain.
        let backlogs: std::collections::HashMap<String, i64> = results
            .iter()
            .map(|h| (h.topic.clone(), h.backlog_or_zero()))
            .collect();
        prev = Some((backlogs, cycle_start));

        let display: Vec<TopicHealth> = if cli.problems_only {
            results
                .iter()
                .filter(|h| !h.status.is_healthy())
                .cloned()
                .collect()
        } else {
            results.clone()
        };

        clear_screen();
        let unhealthy = display.iter().filter(|h| !h.status.is_healthy()).count();
        match cli.format() {
            Format::Table => {
                println!(
                    "as of {run_at}   (watch: every {}s, ctrl-c to stop)",
                    cli.watch_interval_secs()
                );
                if let Some(report) = &kube_report {
                    print!("{}", output::render_kube_section(report));
                }
                println!("{}", output::render_table(&display, colors, &run_at));
                println!(
                    "{unhealthy} of {} topic(s) unhealthy (subscription: {subscription}, threshold: {threshold})",
                    topics.len()
                );
                if let Some(report) = &kube_report {
                    println!("{}", report.summary_line());
                }
            }
            Format::Jsonl => print!("{}", output::render_jsonl(&display, &run_at)?),
        }

        std::thread::sleep(interval);
    }
}

/// Attach drain stats to `results` by comparing each topic's current backlog to
/// its value in `prev_backlogs` over `window` seconds. Topics absent from the
/// previous cycle (first appearance) get no trend.
fn apply_interval_drain(
    results: &mut [TopicHealth],
    prev_backlogs: &std::collections::HashMap<String, i64>,
    window: f64,
) {
    const STABLE_FRAC: f64 = 0.01;
    if window <= 0.0 {
        return;
    }
    for health in results.iter_mut() {
        if health.error.is_some() {
            continue;
        }
        if let Some(&previous) = prev_backlogs.get(&health.topic) {
            health.drain = Some(evaluate_drain(
                previous,
                health.backlog_or_zero(),
                window,
                STABLE_FRAC,
            ));
        }
    }
}

/// Clear the terminal and move the cursor home (ANSI). Cheap and portable
/// enough for the redraw; harmless if output isn't a terminal.
fn clear_screen() {
    print!("\x1b[2J\x1b[H");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

/// Fetch the Kubernetes report if `--kube` is set. Feature-gated: without the
/// The merged Kubernetes settings after combining config file and CLI flags.
/// CLI values win over config; activation is `--kube` OR `kube.enabled`. Fields
/// are consumed by the feature-gated fetch, so allow them to be unread without
/// the `kube` feature.
#[cfg_attr(not(feature = "kube"), allow(dead_code))]
struct ResolvedKube {
    namespace: String,
    selector: Option<String>,
    configmap: Option<String>,
    log_tail: i64,
    assert: Vec<(String, String)>,
}

/// Resolve kube settings from config + CLI, or `None` if kube isn't active.
/// Pure and testable — no async, no feature gate.
fn resolve_kube(cli: &Cli, kc: &config::KubeConfig) -> Option<ResolvedKube> {
    merge_kube_settings(
        cli.kube,
        kc,
        cli.kube_namespace.as_deref(),
        cli.kube_selector.as_deref(),
        cli.kube_configmap.as_deref(),
        cli.kube_log_tail,
        &cli.kube_assert,
    )
}

/// The pure core of `resolve_kube`, taking plain values so it can be unit-tested
/// without constructing a clap `Cli`. CLI arguments override config; activation
/// is `flag_kube` OR `kc.enabled`.
fn merge_kube_settings(
    flag_kube: bool,
    kc: &config::KubeConfig,
    cli_namespace: Option<&str>,
    cli_selector: Option<&str>,
    cli_configmap: Option<&str>,
    cli_log_tail: Option<i64>,
    cli_assert: &[(String, String)],
) -> Option<ResolvedKube> {
    if !(flag_kube || kc.enabled) {
        return None;
    }
    let namespace = cli_namespace
        .map(str::to_string)
        .or_else(|| kc.namespace.clone())
        .unwrap_or_else(|| "default".to_string());
    let selector = cli_selector
        .map(str::to_string)
        .or_else(|| kc.selector.clone());
    let configmap = cli_configmap
        .map(str::to_string)
        .or_else(|| kc.configmap.clone());
    let log_tail = cli_log_tail.or(kc.log_tail).unwrap_or(200);
    // Merge asserts: config first, CLI appended (CLI wins on duplicate keys).
    let mut assert: Vec<(String, String)> =
        kc.assert.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    for (k, v) in cli_assert {
        assert.retain(|(ek, _)| ek != k);
        assert.push((k.clone(), v.clone()));
    }
    Some(ResolvedKube {
        namespace,
        selector,
        configmap,
        log_tail,
        assert,
    })
}

/// Fetch the Kubernetes report if kube is active (via `--kube` or config).
/// Feature-gated: without the `kube` feature this returns a helpful message so
/// the flag isn't silently ignored.
/// Convert the operator's `[[metrics.watch]]` config into the metrics module's
/// specs. An empty list yields an empty vec; `MetricsTracker::new` then falls
/// back to the built-in curated defaults.
fn watch_specs(metrics_config: &config::MetricsConfig) -> Vec<metrics::MetricSpec> {
    metrics_config
        .watch
        .iter()
        .map(|w| metrics::MetricSpec {
            name: w.name.clone(),
            label: w.label.clone().unwrap_or_else(|| w.name.clone()),
            polarity: match w.polarity {
                config::MetricPolarity::LowerBetter => metrics::Polarity::LowerBetter,
                config::MetricPolarity::HigherBetter => metrics::Polarity::HigherBetter,
                config::MetricPolarity::Neutral => metrics::Polarity::Neutral,
            },
            threshold: w.threshold,
            kind: match w.kind {
                config::MetricKind::Gauge => metrics::MetricKind::Gauge,
                config::MetricKind::Counter => metrics::MetricKind::Counter,
            },
            category: match w.category {
                config::MetricCategory::Consumer => metrics::MetricCategory::Consumer,
                config::MetricCategory::Throughput => metrics::MetricCategory::Throughput,
                config::MetricCategory::Bottleneck => metrics::MetricCategory::Bottleneck,
                config::MetricCategory::Health => metrics::MetricCategory::Health,
            },
        })
        .collect()
}

/// Fold a fetched report's scraped pod metrics into the rolling tracker, log a
/// curated per-pod summary line, and (if a capture dir is set) append every raw
/// sample as a JSONL record for later tuning. Called once per refresh cycle;
/// the tracker persists across cycles so trends accumulate.
fn process_metrics(
    report: &mut kube::KubeReport,
    tracker: &mut metrics::MetricsTracker,
    capture_dir: Option<&std::path::Path>,
    run_at: &str,
    log: bool,
) {
    const EPS: f64 = 0.02; // 2% change threshold for flat.
    // Collect (pod, text, health) first so we can then borrow report mutably to
    // write the summaries without overlapping borrows.
    let scraped: Vec<(String, String, Option<String>)> = report
        .pod_metrics
        .iter()
        .map(|pm| (pm.pod.clone(), pm.metrics_text.clone(), pm.health.clone()))
        .collect();

    let mut summaries = Vec::with_capacity(scraped.len());
    for (pod, text, health) in &scraped {
        // Fold into the rolling tracker and (outside the TUI) log the summary.
        let summary = tracker.observe(pod, text, EPS);
        if log {
            eprintln!("{summary}");
        }

        // Build the structured per-pod summary for the TUI from the verdicts.
        let verdicts = tracker.verdicts_for(pod, EPS);
        let lines = verdicts
            .iter()
            .map(|v| kube::MetricLine {
                label: v.label.clone(),
                value: v.value,
                arrow: match v.rolling {
                    metrics::Direction::Up => "↑".to_string(),
                    metrics::Direction::Down => "↓".to_string(),
                    metrics::Direction::Flat => "".to_string(),
                },
                breached: v.breached,
                worsening: v.worsening,
                improving: v.improving,
                category: v.category.label().to_string(),
                changed: v.changed,
                stalled: v.stalled,
                is_rate: v.is_rate,
                present: v.present,
            })
            .collect();
        summaries.push(kube::PodMetricSummary {
            pod: pod.clone(),
            lines,
            health: health.clone(),
        });

        // Raw capture for offline tuning.
        if let Some(dir) = capture_dir {
            let samples = metrics::parse_prometheus(text);
            let record = metrics::capture_record(run_at, pod, &samples);
            if let Err(e) = append_capture(dir, pod, &record) {
                if log {
                    eprintln!("Warning: metrics capture failed: {e}");
                }
            }
        }
    }
    report.pod_metric_summaries = summaries;

    // Per-pod connection-stability verdicts (flapping detection).
    report.pod_stability = scraped
        .iter()
        .map(|(pod, _, _)| {
            let s = tracker.stability_for(pod);
            kube::PodStabilityLine {
                pod: pod.clone(),
                reconnect_rate: s.reconnect_rate,
                throttle_transition_rate: s.throttle_transition_rate,
                active_parts_churn: s.active_parts_churn,
                active_parts: s.active_parts,
                idle_cull_rate: s.idle_cull_rate,
                idle_cull_threshold_secs: s.idle_cull_threshold_secs,
                flapping_rate: s.flapping_rate,
                flapping_rebalance: s.flapping_rebalance,
                idle_cull_loop: s.idle_cull_loop,
            }
        })
        .collect();
}

/// Append one JSONL record to `<dir>/metrics-<pod>.jsonl`, creating the dir.
fn append_capture(dir: &std::path::Path, pod: &str, record: &str) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let safe: String = pod
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = dir.join(format!("metrics-{safe}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{record}")
}

#[cfg(feature = "kube")]
fn fetch_kube_report(
    cli: &Cli,
    kube_config: &config::KubeConfig,
    metrics_config: &config::MetricsConfig,
) -> Option<kube::KubeReport> {
    let resolved = resolve_kube(cli, kube_config)?;
    let Some(selector) = resolved.selector.clone() else {
        return Some(kube::KubeReport::unreachable(
            &resolved.namespace,
            "kube selector is required (set --kube-selector or kube.selector in config)"
                .to_string(),
        ));
    };
    let query = kube::client::KubeQuery {
        namespace: resolved.namespace.clone(),
        selector,
        configmap: resolved.configmap.clone(),
        expected_config: resolved.assert.clone(),
        log_tail: (resolved.log_tail > 0).then_some(resolved.log_tail),
        event_window_secs: 30 * 60,
        metrics_port: metrics_config.enabled.then_some(metrics_config.port),
    };
    // Small dedicated runtime: the rest of the tool is sync, so we don't want a
    // top-level async main. Block on the gather.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            return Some(kube::KubeReport::unreachable(
                &resolved.namespace,
                format!("failed to start async runtime: {e}"),
            ))
        }
    };
    Some(runtime.block_on(kube::client::gather(&query)))
}

#[cfg(not(feature = "kube"))]
fn fetch_kube_report(
    cli: &Cli,
    kube_config: &config::KubeConfig,
    _metrics_config: &config::MetricsConfig,
) -> Option<kube::KubeReport> {
    if resolve_kube(cli, kube_config).is_some() {
        eprintln!(
            "Warning: Kubernetes correlation requires the tool to be built with `--features kube`; ignoring."
        );
    }
    None
}

fn resolve_token() -> anyhow::Result<String> {
    for var in ["TOKEN", "PULSAR_TOKEN"] {
        if let Ok(value) = std::env::var(var)
            && !value.trim().is_empty() {
                return Ok(value);
            }
    }
    bail!("no auth token: set TOKEN or PULSAR_TOKEN");
}

fn parse_topics(raw: &[String]) -> anyhow::Result<Vec<TopicName>> {
    raw.iter()
        .map(|name| TopicName::parse(name).map_err(anyhow::Error::from))
        .collect()
}

/// Bounded worker pool over scoped threads; results are collected by input
/// index so output order always matches the config file.
fn check_all(
    client: &AdminClient,
    topics: &[TopicName],
    subscription: &str,
    threshold: i64,
    concurrency: usize,
) -> Vec<TopicHealth> {
    parallel_map(topics, concurrency, |topic| {
        check_topic(client, topic, subscription, threshold)
    })
}

/// Take a second cheap backlog sample `window_secs` after the first, then
/// attach drain trend + ETA to each topic in `results`. The first sample's
/// backlog is the `total_backlog` already on each `TopicHealth`.
fn measure_drain(
    client: &AdminClient,
    topics: &[TopicName],
    subscription: &str,
    results: &mut [TopicHealth],
    window_secs: u64,
    concurrency: usize,
) {
    eprintln!("Sampling backlog again in {window_secs}s to measure drain…");
    std::thread::sleep(Duration::from_secs(window_secs));

    let second: Vec<Option<i64>> = parallel_map(topics, concurrency, |topic| {
        sample_backlog(client, topic, subscription)
    });

    // ~1% of backlog is treated as noise rather than a real trend.
    const STABLE_FRAC: f64 = 0.01;
    for (health, second_backlog) in results.iter_mut().zip(second) {
        // Only meaningful where the first sample succeeded (not ERROR/MISSING).
        if health.error.is_some() {
            continue;
        }
        if let Some(second_backlog) = second_backlog {
            health.drain = Some(evaluate_drain(
                health.backlog_or_zero(),
                second_backlog,
                window_secs as f64,
                STABLE_FRAC,
            ));
        }
    }
}

/// Run `f` over `items` on a bounded scoped-thread pool, returning results in
/// input order.
fn parallel_map<T, R, F>(items: &[T], concurrency: usize, f: F) -> Vec<R>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> R + Sync,
{
    let workers = concurrency.clamp(1, 32).min(items.len().max(1));
    let (task_tx, task_rx) = mpsc::channel::<(usize, &T)>();
    let task_rx = Arc::new(Mutex::new(task_rx));
    let results: Arc<Mutex<Vec<Option<R>>>> =
        Arc::new(Mutex::new((0..items.len()).map(|_| None).collect()));

    for pair in items.iter().enumerate() {
        // Send cannot fail: receiver outlives the loop.
        let _ = task_tx.send(pair);
    }
    drop(task_tx);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let task_rx = Arc::clone(&task_rx);
            let results = Arc::clone(&results);
            let f = &f;
            scope.spawn(move || loop {
                let task = {
                    let guard = task_rx.lock().expect("task queue lock poisoned");
                    guard.recv()
                };
                let Ok((index, item)) = task else { break };
                let out = f(item);
                results.lock().expect("results lock poisoned")[index] = Some(out);
            });
        }
    });

    let collected = match Arc::try_unwrap(results) {
        Ok(mutex) => mutex,
        Err(_) => unreachable!("worker threads have exited, sole Arc owner remains"),
    };
    collected
        .into_inner()
        .expect("results lock poisoned")
        .into_iter()
        .map(|slot| match slot {
            Some(value) => value,
            None => unreachable!("every item produces a result"),
        })
        .collect()
}

#[cfg(test)]
mod kube_resolve_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn kc(enabled: bool) -> config::KubeConfig {
        config::KubeConfig {
            enabled,
            namespace: Some("cfg-ns".to_string()),
            selector: Some("app=cfg".to_string()),
            configmap: Some("cfg-cm".to_string()),
            log_tail: Some(100),
            assert: BTreeMap::from([
                ("worker_count".to_string(), "24".to_string()),
                ("batch_size".to_string(), "30".to_string()),
            ]),
        }
    }

    #[test]
    fn inactive_when_neither_flag_nor_enabled() {
        let disabled = config::KubeConfig::default();
        assert!(merge_kube_settings(false, &disabled, None, None, None, None, &[]).is_none());
    }

    #[test]
    fn flag_activates_with_defaults() {
        let disabled = config::KubeConfig::default();
        let r = merge_kube_settings(true, &disabled, None, Some("app=x"), None, None, &[])
            .expect("active via flag");
        assert_eq!(r.namespace, "default");
        assert_eq!(r.selector.as_deref(), Some("app=x"));
        assert_eq!(r.log_tail, 200);
    }

    #[test]
    fn config_enabled_activates_without_flag() {
        let r = merge_kube_settings(false, &kc(true), None, None, None, None, &[])
            .expect("active via config.enabled");
        assert_eq!(r.namespace, "cfg-ns");
        assert_eq!(r.selector.as_deref(), Some("app=cfg"));
        assert_eq!(r.log_tail, 100);
        assert_eq!(r.assert.len(), 2);
    }

    #[test]
    fn cli_overrides_config() {
        let r = merge_kube_settings(
            true,
            &kc(true),
            Some("cli-ns"),
            Some("app=cli"),
            Some("cli-cm"),
            Some(500),
            &[],
        )
        .unwrap();
        assert_eq!(r.namespace, "cli-ns");
        assert_eq!(r.selector.as_deref(), Some("app=cli"));
        assert_eq!(r.configmap.as_deref(), Some("cli-cm"));
        assert_eq!(r.log_tail, 500);
    }

    #[test]
    fn asserts_merge_cli_wins_on_duplicate() {
        let cli_assert = vec![
            ("worker_count".to_string(), "48".to_string()), // overrides config's 24
            ("new_key".to_string(), "1".to_string()),       // additional
        ];
        let r = merge_kube_settings(true, &kc(true), None, None, None, None, &cli_assert).unwrap();
        let map: std::collections::HashMap<_, _> = r.assert.into_iter().collect();
        assert_eq!(map.get("worker_count").map(String::as_str), Some("48"));
        assert_eq!(map.get("batch_size").map(String::as_str), Some("30"));
        assert_eq!(map.get("new_key").map(String::as_str), Some("1"));
    }
}
