# pulsar-topic-health

Topic-level health summary for a Pulsar subscription, driven by a TOML list of the topics you actually care about. Replaces the `pulsarctl | jq` shell pipeline with direct calls to the Pulsar admin REST API.

For each configured topic it reports:

- **Trimmed cursors** — a partition whose subscription cursor points below the topic's physical ledger floor: the next message it wants has already been GC'd by TTL/retention, so the consumer will spin forever waiting for data that no longer exists (`TRIMMED`). This is the most urgent state and outranks everything else. A `!` marker (and `waiting_with_backlog` in JSONL) means the broker *also* reports the cursor parked as if caught up while a backlog remains — independent corroboration that it's genuinely stranded.
- **Trim headroom** — for cursors not yet trimmed, how many entries of margin remain before the trimmer reaches the cursor. The `HEADROOM` column shows the *minimum* across a topic's partitions; partitions sitting in the oldest surviving ledger (one GC cycle from loss) are listed as `edge: p3(~20)`.
- **Hot partitions** — any partition whose subscription backlog exceeds the configured threshold (default 100), listed individually with its backlog.
- **Missing subscription** — the subscription is not attached to the topic at all (`MISSING_SUB`), attached but with zero consumers (`NO_CONSUMERS`), or absent/consumer-less on *some* partitions of an otherwise-attached topic (`PARTITION_GAP`).
- **Drain trend & ETA** — by default the tool takes a second backlog sample 30s later (tunable with `--drain-window-secs`, `0` to disable) and reports whether each topic is `draining`, `growing`, or `stable`, its net messages/second, and — when draining — an ETA to clear. This answers "can I catch up?" directly: a positive ETA means you'll clear at the current rate; `growing` with no ETA means producers are outpacing consumers and you need more consumer capacity.
- **Backlog size (bytes)** — alongside the message count, the tool reports the backlog *size* in bytes (`SIZE` column, per-partition in the `hot:` detail), fetched via `subscriptionBacklogSize=true`. Two partitions with identical message backlogs can have very different byte footprints; the byte figure is what matters for size-based retention limits and storage cost, and it reconciles with the "Backlog Size" column in Grafana. Reported as binary units (GiB/MiB/KiB) to match typical dashboards.
- **Time-in-state** — a `TIME` column shows how long each topic has held its current (status, trend) — e.g. `2h` in `BACKLOG/growing` vs `45s` — which separates a transient spike from a standing incident. History comes from the JSON snapshots in `--json-dir` (survives restarts, works for single runs) or, in `--watch` mode, the in-memory previous cycle. Resolution is snapshot/cycle-grained: it measures time since the last *observed* different state, so a flip-and-flip-back between observations isn't seen. The column only appears once there's prior history to compare against.
- **Stats failures** — topics whose stats fetch failed (`ERROR`), with the HTTP/transport error inline.

The trim check answers "is the descriptor we're trying to fetch already gone, and how far off that point are we?" — distinguishing a genuinely-behind consumer (large backlog, cursor well inside the ledgers, `HEADROOM` in the millions) from a stranded one (large backlog, `HEADROOM` at or below zero). It works by comparing each partition's cursor `readPosition` against the oldest `ledgerId` still present in `internalStats`. Idle, fully-caught-up partitions (zero backlog) are never flagged as at-edge, even when their cursor sits in the oldest ledger — there is nothing to lose.

## Usage

```sh
export TOKEN=...                   # or PULSAR_TOKEN — whichever is set
pulsar-topic-health --config topics.toml            # summary table (default)
pulsar-topic-health --config topics.toml --format jsonl | jq 'select(.status != "ok")'
pulsar-topic-health --threshold 500 --problems-only # override threshold, hide healthy rows
```

See `topics.example.toml` for the config format. `--subscription`, `--admin-url` (or `PULSAR_ADMIN_URL`), and `--threshold` override the config file; `--concurrency` (default 8) bounds parallel stats requests; `--timeout-secs` sets the per-request timeout; `--drain-window-secs` (default 30) sets the gap between the two backlog samples, or `0` to skip drain measurement and its columns.

### Timestamp

Every run is stamped with a single UTC time (`as of 2026-07-30T11:42:07Z` above the table; an `as_of` field on every JSONL line). The stamp is captured once at the start of the run, so both drain samples share it and successive runs sort and diff cleanly. Append JSONL runs to a file and each line carries its own timestamp for later comparison.

### Colour thresholds

An optional `[colors]` table colours the `BACKLOG`, `SIZE`, and `UNACKED` columns by magnitude: a value at or above `crit` is red, at or above `warn` is yellow, below `warn` is green. Any column with no thresholds set is left uncoloured. `size_*` values are in bytes.

```toml
[colors]
backlog_warn = 100000
backlog_crit = 1000000
size_warn    = 1073741824   # 1 GiB
size_crit    = 5368709120   # 5 GiB
unacked_warn = 50000
unacked_crit = 200000
```

Colours render in a terminal and are automatically stripped when output is piped or redirected, so captured runs stay plain-text for diffing.

## Interactive TUI

The tool includes a full-screen interactive terminal UI that refreshes live and lets you switch views and drill into a topic's partitions from the keyboard — handy when a topic-level symptom needs drilling into at the partition level, or when correlating against pods. It's built in by default:

```sh
pulsar-topic-health --tui
```

Five views, cycled with `v`:

- **topic** — the classic per-topic table, with a cursor you move with ↑/↓. TREND and ETA columns appear once drain data is available (from the second refresh onward, or with `--drain-window-secs`).
- **kube** — the Kubernetes pod-and-node health (populated when also built and run with `--kube`).
- **metrics** — a fleet-wide summary of scraped pod metrics (see [Pod metrics](#pod-metrics-scraping-optional)), grouped by category (consumer / throughput / bottleneck / health), with a per-pod pipeline flow line, per-stage rates, and trend/stall/breach colouring. Scrollable with ↑/↓ and PgUp/PgDn.
- **stability** — per-pod connection-stability detection: reconnect/cull rate, active-partition churn, and a flapping verdict, to catch consumers stuck in an idle→cull→rebalance→reconnect loop.
- **combined** — a split screen: topics on top, and the partitions of the topic you've drilled into on the bottom.

Navigating and drilling in:

- **↑/↓** (or `k`/`j`) move the cursor between topics.
- **Enter** on the selected topic drills into it — switches to the combined view with that topic's partitions in the lower pane. (This replaces a standalone partition list — drill into the topic you care about instead.)
- **Esc** backs out of the drill-in to the topic view.
- In the **kube** view, **Tab** switches the cursor between the pods and nodes sections; **Enter** opens detail for the selected pod (its resource breakdown and logs) or node (its capacity and which pods run on it). In pod-detail, the logs are a scannable one-line-per-entry list: **↑/↓** select a line, **Enter** expands it to a pretty-printed, wrapped view (timestamp, level, message, error, and the remaining fields) so you can read the whole thing — **Esc** collapses back, **w** toggles wrapping, **m** toggles the lower pane between the pod's logs and its scraped metrics. The kube panel also shows a live log-stats summary (level counts, RSS trend, throughput, operational tallies, top messages).
- **?** toggles a legend overlay explaining every status and trend (and, when admin actions are enabled, the action keys).
- **r** refreshes now, **q** (or Ctrl-C) quits.

The refresh cadence uses `--watch-interval-secs`, and drain trend is derived from consecutive refreshes just like `--watch`. Refreshing happens on a background thread, so switching views is instant and the fetch never freezes the UI — the screen always shows the most recent data and updates in place when the next refresh lands. The kube view has data when the binary is also built with `--features kube` and run with `--kube`.

## Kubernetes correlation (optional)

The subscription's consumers usually run in Kubernetes pods, and a broker-side symptom — `NO_CONSUMERS`, a growing backlog — is often caused by a pod-side fault: an OOMKill, a crash-loop, a pod stuck mid-ramp, or a bad config rollout. With the `kube` feature the tool can fetch that consumer-side health and show it alongside the topic table, so "backlog is growing" becomes "backlog is growing *because* two pods OOM-killed 4 minutes ago".

This is an **opt-in build feature** (it pulls in `kube-rs` + `tokio`), so the default binary stays small and dependency-light:

```sh
cargo build --release --features kube
```

Then enable it at runtime with `--kube` plus a namespace and label selector:

```sh
pulsar-topic-health --kube \
  --kube-namespace my-ns \
  --kube-selector app=my-consumer \
  --kube-configmap my-consumer-config \
  --kube-assert worker_count=24 --kube-assert batch_size=30
```

Authentication is whatever `kubectl` already uses — the tool calls `kube-rs`'s default config inference, which reads `~/.kube/config`, `KUBECONFIG`, or an in-cluster service-account token. No separate setup.

### Kube settings in the config file

Rather than passing the `--kube-*` flags every time, put them in a `[kube]` section of the config. Either `--kube` on the command line **or** `enabled = true` in the section activates the correlation, and any CLI flag overrides the corresponding config value:

```toml
[kube]
enabled   = true                 # activates without needing --kube
namespace = "my-ns"
selector  = "app=my-consumer"
configmap = "my-consumer-config"
log_tail  = 200
assert    = { worker_count = "24", batch_size = "30" }
```

CLI `--kube-assert` entries merge with the config `assert` table (the CLI wins on a duplicate key). A typoed key in `[kube]` is a hard error, like the rest of the config.

### Operational settings in the config file

The rest of the command-line flags can also live in the config, under a `[settings]` section, so a configuration you run often doesn't need a long command line. Every field is optional, and a CLI flag always overrides the config value:

```toml
[settings]
format              = "table"        # or "jsonl"
concurrency         = 8
timeout_secs        = 30
drain_window_secs   = 30             # 0 skips the trend/ETA second sample
watch               = false
tui                 = false
watch_interval_secs = 60
json_dir            = "./snapshots"
json_dir_max_files  = 100            # 0 keeps all
problems_only       = false
```

So a run like `pulsar-topic-health -c prod.toml --watch --tui --json-dir ./snapshots --kube` can become just `pulsar-topic-health -c prod.toml` with `watch`, `tui`, `json_dir` set in `[settings]` and `enabled = true` in `[kube]`. The mode switches (`watch`, `tui`, `problems_only`) can be turned on by the flag or the config; neither can force them off. A typoed key in `[settings]` is a hard error, like the rest of the config.

### When no pods match

If a `--kube` run matches no pods, the tool helps you find the right target rather than printing a dead "no pods matched" line. In a plain one-shot run it prints the choices and exits with code 3: if the **namespace** doesn't exist, it lists the available namespaces; if the namespace exists but the **selector** missed, it lists the label keys and their distinct values on the namespace's pods, and suggests a ready-to-paste selector when there's an obvious `app.kubernetes.io/name` (or `app`/`k8s-app`) label. In `--watch` or `--tui` the same discovery is shown in the kube panel instead, and the session keeps running (so a consumer that's briefly scaled to zero doesn't kill your watch).

It renders a pod-summary section above the topic table (pod name, ready count, restarts, age, CPU and memory usage against limits, and state — with `OOMKilled`/`CrashLoopBackOff` highlighted), a node-capacity line per node the pods run on, flags a split rollout or large rollout skew, lists recent OOM/eviction events, prints any failed `--kube-assert` config checks, and scans the last N log lines per pod (`--kube-log-tail`, default 200) for ramp/OOM/error/config signals. CPU/MEM show `used/limit` coloured by percent of limit (green <70%, yellow 70–90%, red ≥90%); live usage needs metrics-server in the cluster, otherwise the used side shows `·`. Unhealthy topics also gain a short correlation hint in their `DETAIL` (e.g. `kube: 2 pod(s) OOM-killed`). In JSONL mode the full kube report is emitted as one extra line before the topic lines.

If a pod's logs show a **transform / data-quality / DLQ error** — a DataFusion or SQL parse error, or rows being captured to a dead-letter queue — the tool treats it as a first-class problem, because it usually means *silent data loss*: rows are dropped to the DLQ while the pipeline still reports healthy and throughput looks normal. Affected pods show a red `DLQ-ERROR` state, a prominent alert banner appears under the pod table naming them, and the log summary tallies the error count. It's detected by stable substrings, so the specific failing SQL doesn't matter.

The same escalation applies to the other signals that precede an outage but are easy to miss in a wall of logs — the transitions and thresholds, not just the raw numbers. A **pre-OOM memory warning** (RSS crossing the cgroup limit, "OOM kill imminent") shows a red `MEM-CRITICAL` pod state and a banner — caught *before* the kernel kills the pod, while it's still savable (it ranks above `OOMKilled`, which has already happened). A **throughput collapse** — a pod that was processing and dropped to zero, distinct from one idle since start — raises a banner. A **reconnect storm** (a burst of broker-closed/disconnect/TLS-EOF events over one window, not incidental churn) and **backpressure** (an internal channel near-full, which precedes a stall) are flagged and tallied. All are detected from the scanned pod logs and shown in both the plain output and the TUI kube panel.

The Kubernetes side is strictly best-effort and isolated: if the cluster is unreachable or auth fails, the tool prints an `unreachable` notice and still renders the full Pulsar report. A consumer-side problem (failed pods, failed config assertion, split rollout) contributes to the non-zero exit code alongside topic health, so `--kube` works as a post-deploy gate.

## Pod metrics scraping (optional)

When the consumer pods expose Prometheus `/metrics`, the tool can port-forward to each pod, parse the metrics, track rolling trends, and surface a curated per-pod summary — turning "the pipeline is slow" into "the batch stage is backed up on these two pods". It's part of the `kube` feature and is enabled in config:

```toml
[metrics]
enabled     = true
port        = 9090       # the pod's metrics port
window      = 5          # scrapes kept per metric for the rolling trend
# capture_dir = "./metrics-capture"   # also archive every scraped metric as JSONL
```

By default a built-in curated set is summarised. To choose exactly which metrics to watch, list `[[metrics.watch]]` entries — doing so replaces the defaults:

```toml
[[metrics.watch]]
name      = "myapp_records_written_total"
label     = "written/s"
kind      = "counter"          # counters are shown as a per-scrape rate
polarity  = "higher_better"    # lower_better | higher_better | neutral
threshold = 100                # optional; alerts when crossed
category  = "throughput"       # consumer | throughput | bottleneck | health
```

Notes on matching and display:

- **Exporter suffixes are tolerated.** A configured `myapp_x` matches the pod's `myapp_x_total`, `myapp_x_bytes`, `myapp_x_total_total`, etc. — the type/unit suffixes a Prometheus/OpenTelemetry exporter appends. Histogram component series (`_bucket`/`_count`/`_sum`) are excluded.
- **Counters are shown as rates**, not their meaningless cumulative total, so the per-stage throughput reads directly.
- **A configured metric the pod doesn't expose shows as dimmed `(no data)`** rather than silently vanishing, so you can tell "healthy zero" from "not found".
- In the **metrics** TUI view, each pod leads with a pipeline flow line (`consumed → written → sink`); identical values across pods collapse to one line; pods not scraped this cycle are summarised compactly; a higher-better metric sitting at zero is flagged as stalled.

## Connection stability (optional)

Built on the same scrape, the **stability** view detects consumers stuck in an idle→cull→rebalance→reconnect loop — a pattern no single snapshot reveals. Per pod it shows the idle-cull rate, reconnect rate, active-partition count, and active-partition *churn* (total variation across the window, which catches an oscillation like 90→0→90→0 that a net-delta check misses), with a flapping verdict. When the pipeline exposes a labelled consumer-cull counter (`<prefix>_source_consumers_culled_total{reason="idle_timeout"}`) and an idle-cull threshold gauge, the view reads the idle-timeout culls specifically and shows the threshold, so a loop caused by an idle timeout set too tight is obvious.

## Admin actions (optional, off by default)

The tool is read-only unless you explicitly opt in. When enabled, the TUI can perform a few targeted remediations against the consumer pods, behind **two gates**:

1. **Config gate.** Nothing is possible unless the config enables it:

   ```toml
   [admin]
   allow_actions = true
   ```

   With this `false` (the default) or absent, the action keys are inert and the tool stays strictly read-only.

2. **Live confirmation.** Even when enabled, every action shows a confirmation prompt naming the exact target and only fires on an explicit `y`. There is no way to pre-authorise or skip the confirmation.

The actions, from the **kube** or **pod-detail** views:

- **d** — delete the selected pod (its controller recreates it — a targeted restart).
- **R** — rolling-recycle *all* consumer pods: delete one, wait for its replacement to become Ready, then the next; stops on any failure so it never takes down more than one at a time.
- **D** — cordon and drain the selected pod's node (evicts its pods, skipping DaemonSet-owned ones).

Admin actions require the `kube` feature. The keys are listed in the `?` legend when enabled.

## Watch mode

`--watch` runs continuously, clearing and redrawing the table every `--watch-interval-secs` (default 60) until interrupted. In watch mode the drain trend is derived from the **previous cycle** — consecutive cycles are the two samples, compared over the real elapsed time between them — so there's no mid-cycle sleep and `--drain-window-secs` is ignored. The first cycle shows no trend (nothing to compare yet); every cycle after shows net rate and ETA over the actual interval.

```sh
# Redraw every 30s
pulsar-topic-health --watch --watch-interval-secs 30

# Watch and archive a JSON snapshot per cycle, keeping the 200 most recent
pulsar-topic-health --watch --json-dir ./snapshots --json-dir-max-files 200
```

### JSON snapshots

`--json-dir <dir>` writes one JSON file per run/cycle into `<dir>` (created if absent), named `pth-<compact-timestamp>.json`, each a `{ "as_of": ..., "topics": [...] }` document with the full per-topic results. `--json-dir-max-files <n>` (default 100, `0` = keep all) caps the directory: once exceeded, the oldest snapshots are pruned so it never grows without bound. Only files matching the tool's own `pth-*.json` naming are ever removed — unrelated files in the directory are left alone. `--json-dir` works with or without `--watch`; a single run writes one snapshot. Snapshots always contain the full topic set, even with `--problems-only` (which only filters the on-screen view).

### Time-in-state

The `TIME` column reports how long a topic has continuously held its current state, where a *state* is the pair (status, trend) — so `BACKLOG/growing` and `BACKLOG/draining` are distinct, and moving between them restarts the clock. This is what tells a 30-second blip apart from a two-hour incident.

The history it needs comes from two places, tried in order: the most recent JSON snapshot in `--json-dir` (so it survives restarts and works for one-off runs), then the previous cycle held in memory during `--watch`. Each written snapshot carries a `state_since` timestamp per topic, so the "entered-state" time is threaded forward across runs. With no history available — a single run and no `--json-dir`, or the first watch cycle — the column shows `—` (or `0s` once a baseline exists).

The measurement is snapshot/cycle-resolution: it's "time since the last observation in a different state", not continuous. A topic that flaps between two states faster than the poll interval will look like it never left. For separating spikes from sustained incidents at a sensible polling cadence, that's exactly the intended behaviour.

When drain measurement is on, the table gains `TREND` / `NET/s` / `ETA` columns and the run takes at least `--drain-window-secs` longer (the second sample is a single cheap aggregate call per topic — no per-partition or internal-stats calls). The `NET/s` figure is second-sample-minus-first over the window; a change smaller than 1% of the starting backlog is reported as `stable` rather than a trend, so a topic drifting by a handful of messages doesn't read as growing or draining.

## Statuses

| Status | Meaning |
|---|---|
| `OK` | Subscription attached, consumers present, all partitions under threshold, no trim risk |
| `TRIMMED` | A partition's cursor is stranded past trimmed data — consumer spinning for messages that no longer exist |
| `BACKLOG` | One or more partitions over the backlog threshold (see DETAIL / `hot_partitions`) |
| `PARTITION_GAP` | Subscription missing or consumer-less on specific partitions |
| `NO_CONSUMERS` | Subscription exists but has zero consumers anywhere |
| `MISSING_SUB` | Subscription not attached to the topic |
| `ERROR` | Stats fetch failed |

Priority when several apply, worst first: `TRIMMED` → `NO_CONSUMERS` → `PARTITION_GAP` → `BACKLOG` → `OK`. The status reflects the single worst condition, but all details (trimmed, at-edge, hot, gaps) are always shown in the table and JSONL. `TRIMMED` and `NO_CONSUMERS`/`PARTITION_GAP`/`BACKLOG` all count as unhealthy for the exit code.

## Exit codes

`0` all healthy · `1` usage/config/runtime error · `2` one or more unhealthy topics. Suitable for cron/CI alerting: pipe JSONL to your alerter, or just check the exit code.

## Design notes

- Base topics are checked with `GET …/partitioned-stats?perPartition=true&subscriptionBacklogSize=true` (one request per topic, per-partition breakdown and backlog byte sizes included). A 404 means the topic is non-partitioned, and it falls back to plain `…/stats`. Explicit `-partition-N` config entries go straight to plain stats.
- The trim check additionally fetches `GET …/internalStats` **per partition** (the broker doesn't aggregate cursor/ledger data across partitions), so a 6-partition topic costs 1 partitioned-stats call plus 6 internal-stats calls. These run through the same bounded worker pool. If an internal-stats call fails, that partition's trim verdict degrades to "unknown" (blank `HEADROOM`, no `TRIMMED`/`edge` entry) rather than failing the whole topic.
- Position comparison is lexicographic on `(ledgerId, entryId)`, matching Pulsar's message ordering; `entryId` is signed because `-1` ("before entry 0") is a valid cursor position. Headroom sums whole ledgers below the read ledger plus the entry offset within it.
- Output row order always matches the config file, regardless of request completion order.
- Blocking HTTP (`ureq`) with a bounded scoped-thread worker pool — no async runtime needed for an ops CLI doing ~100 GETs.
- JSONL omits empty `hot_partitions` / `partition_gaps` arrays and absent `error` fields, keeping `jq` filters clean.

## Toolchain

Pinned to Rust 1.94.0 via `rust-toolchain.toml` (edition 2024, `rust-version = "1.94"`). 

## Agent skill

`skills/pulsar-topic-health/` bundles an agent skill (`SKILL.md` plus a generic `config-example.toml`) that teaches an AI coding/ops agent how to configure, run, and — crucially — *interpret* this tool: the status priority, what `HEADROOM` blank-vs-zero means, and the two distinctions that matter most (behind vs. lost, catching-up vs. falling-behind). Point your agent tooling at that folder, or copy it into your agent's skills directory.
