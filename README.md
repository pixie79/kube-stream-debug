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

## Interactive TUI (optional)

With the `tui` feature the tool offers a full-screen interactive terminal UI that refreshes live and lets you switch views and select partitions from the keyboard — handy when a topic-level symptom needs drilling into at the partition level, or when correlating against pods.

```sh
cargo build --release --features tui
pulsar-topic-health --tui
```

Four views, cycled with `v`:

- **topic** — the classic per-topic table.
- **partition** — one row per partition, flattened across all topics, so you can scan every partition's backlog/consumers/status at once.
- **kube** — the Kubernetes pod-summary section (populated when also built and run with `--kube`).
- **combined** — a split screen: topics on top, partitions below.

Selecting and filtering:

- `/` edits a query that matches a topic or partition name (`p3` matches `…-partition-3`); Enter applies, Esc cancels.
- `f` toggles between **Highlight** mode (keep all rows, emphasise matches) and **Filter** mode (hide non-matching rows).
- `c` clears the query, `r` refreshes now, `q` (or Ctrl-C) quits.

The refresh cadence uses `--watch-interval-secs`, and drain trend is derived from consecutive refreshes just like `--watch`. The `tui` and `kube` features are independent — combine them (`--features tui,kube`) for the kube view to have data.

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

It renders a pod-summary section above the topic table (pod name, ready count, restarts, age, state — with `OOMKilled`/`CrashLoopBackOff` highlighted), flags a split rollout (more than one image) or large rollout skew (pods not restarted together), lists recent OOM/eviction events, prints any failed `--kube-assert` config checks, and scans the last N log lines per pod (`--kube-log-tail`, default 200) for ramp/OOM/error/config signals. Unhealthy topics also gain a short correlation hint in their `DETAIL` (e.g. `kube: 2 pod(s) OOM-killed`). In JSONL mode the full kube report is emitted as one extra line before the topic lines.

The Kubernetes side is strictly best-effort and isolated: if the cluster is unreachable or auth fails, the tool prints an `unreachable` notice and still renders the full Pulsar report. A consumer-side problem (failed pods, failed config assertion, split rollout) contributes to the non-zero exit code alongside topic health, so `--kube` works as a post-deploy gate.

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

Pinned to Rust 1.94.0 via `rust-toolchain.toml` (edition 2024, `rust-version = "1.94"`). `Cargo.lock` is committed with current crate versions resolved fresh — run `cargo update` whenever you want to roll deps forward.

## Agent skill

`skills/pulsar-topic-health/` bundles an agent skill (`SKILL.md` plus a generic `config-example.toml`) that teaches an AI coding/ops agent how to configure, run, and — crucially — *interpret* this tool: the status priority, what `HEADROOM` blank-vs-zero means, and the two distinctions that matter most (behind vs. lost, catching-up vs. falling-behind). Point your agent tooling at that folder, or copy it into your agent's skills directory.
