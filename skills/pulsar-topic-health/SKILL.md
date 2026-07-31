---
name: pulsar-topic-health
description: Run and interpret the pulsar-topic-health CLI — a read-only tool that reports subscription health for a configured set of Apache Pulsar topics. Use whenever the user wants to check Pulsar backlogs, find hot partitions, detect a subscription that is missing/consumer-less/stuck on trimmed data, measure whether a backlog is draining or growing (with an ETA to clear), or reconcile the tool's numbers against a Grafana/broker dashboard. Trigger on mentions of "topic health", "Pulsar backlog", "is my consumer keeping up", "are we draining or falling behind", "did the cursor fall off retention", "TRIMMED", "HEADROOM", "backlog size in bytes", or on requests to build, configure (the TOML file), or run the tool. Also use when a run's output needs explaining — status meanings, exit codes, why a topic is red — or when deciding whether a backlog is safe (catchable) versus lost (trimmed).
---

# pulsar-topic-health — operating and interpreting the CLI

`pulsar-topic-health` is a **read-only** Rust CLI. It reads a TOML file listing the topics you care about, queries the Pulsar admin REST API for each, and prints a one-row-per-topic health summary (table by default, JSONL on request). It never mutates topics or subscriptions — no cursor resets, no acks, no deletes.

This skill covers how to configure it, run it, and — most importantly — read its output correctly, including the two distinctions people most often get wrong: *behind vs. lost* (backlog vs. trimmed) and *catching up vs. falling behind* (drain trend).

## When to use this skill

- The user wants to run the tool or set up its config.
- A run's output needs interpreting ("why is this topic red?", "what does TRIMMED mean?", "can I catch up?").
- The user is comparing the tool against a dashboard and the numbers seem to disagree.
- The user asks whether a backlog is recoverable or already lost.

If the user wants to *build* the binary, see "Building" below; the code targets a recent stable Rust toolchain (edition 2024).

## Mental model — what the tool actually measures

Three independent questions, per topic:

1. **How far behind is the subscription?** — message backlog and its byte size, plus which partitions are over the "hot" threshold.
2. **Is the data still there, or has it aged out?** — the trim check. A subscription cursor pointing below the oldest surviving ledger is stuck on data that retention/TTL already deleted; it will spin forever. This is the difference between "behind but catchable" and "permanently lost".
3. **Are we winning or losing right now?** — the drain trend. A second backlog sample taken a configurable interval later shows whether the pile is shrinking (with an ETA to clear), growing, or holding steady.

A topic can be badly behind yet perfectly healthy in the sense that matters: if the data is present and the trend is draining, waiting fixes it. The dangerous states are *trimmed* (data gone) and *growing* (consumers can't keep up, so waiting never fixes it).

## Configuration

The tool reads a TOML file (default `topics.toml`, override with `-c/--config`). See `config-example.toml` in this skill folder for a fully generic starting point.

```toml
admin_url    = "https://pulsar-admin.example.internal"   # optional here; see precedence below
subscription = "my-consumer-subscription"
backlog_threshold = 100                                   # optional; per-partition "hot" cutoff, default 100

topics = [
  "persistent://widgetco/toybox/marbles",                 # base topic → per-partition breakdown
  "persistent://widgetco/toybox/robots",
  "widgetco/toybox/dominoes-partition-3",                 # explicit partition; "persistent://" assumed if omitted
]

# Optional — colour thresholds for the BACKLOG / SIZE / UNACKED columns.
[colors]
backlog_warn = 100000
backlog_crit = 1000000
size_warn    = 1073741824      # 1 GiB (bytes)
size_crit    = 5368709120      # 5 GiB (bytes)
unacked_warn = 50000
unacked_crit = 200000
```

Rules the agent should hold to when editing config:

- `topics` must be non-empty. Entries are either base topics (the tool fetches a per-partition breakdown) or explicit `-partition-N` names (checked individually). The `persistent://` scheme is assumed when omitted; `non-persistent://` is supported explicitly.
- Unknown TOML keys are rejected — a typo like `backlog_treshold` is a hard error, not a silent default. Spell fields exactly. This also applies inside `[colors]`.
- Keep example configs generic. Never commit real tenant/namespace/subscription names into the repo or into this skill. Use `tenant/namespace/...` and `my-consumer-subscription` placeholders.

### Optional colour thresholds

An optional `[colors]` table colours the `BACKLOG`, `SIZE`, and `UNACKED` columns in the table output by magnitude — red at/above `crit`, yellow at/above `warn`, green below `warn`; a column with no thresholds is left uncoloured. `size_*` are in bytes.

```toml
[colors]
backlog_warn = 100000
backlog_crit = 1000000
size_warn    = 1073741824   # 1 GiB
size_crit    = 5368709120   # 5 GiB
unacked_warn = 50000
unacked_crit = 200000
```

Colours render only to a terminal; they're stripped automatically when output is piped or redirected, so captured runs stay diff-friendly. These are display cues, not health states — the `STATUS` column and exit code are unaffected by colour thresholds.

### Timestamp

Every run prints a single UTC timestamp: `as of <RFC3339>` above the table, and an `as_of` field on every JSONL line. It's captured once per run (shared by both drain samples). Appending JSONL runs to a file gives a self-timestamped log that can be diffed or ordered — the intended way to track a backlog's trajectory across runs.

### Credentials and URL precedence

- **Admin URL**, highest priority first: `--admin-url` flag → `PULSAR_ADMIN_URL` env → `admin_url` in the config file. If none is set, the tool errors.
- **Auth token**, env-only: `TOKEN` first, then `PULSAR_TOKEN`. Never put the token in the config file or on the command line where it lands in shell history. Export it:

```bash
export TOKEN="…"        # or PULSAR_TOKEN
```

The agent must never echo, log, or paste a token value into files or output.

## Running the tool

```bash
# Default: table, 30s drain sample, all topics from ./topics.toml
pulsar-topic-health

# Point at a config, show only unhealthy topics
pulsar-topic-health -c prod.toml --problems-only

# Machine-readable, no drain sampling (fast single pass), pipe to jq
pulsar-topic-health --drain-window-secs 0 --format jsonl | jq 'select(.status != "ok")'

# Override the hot threshold and the subscription for a one-off
pulsar-topic-health --threshold 1000 --subscription other-sub
```

Flags worth knowing:

| Flag | Effect |
|---|---|
| `-c, --config <path>` | Config file (default `topics.toml`). |
| `--format table\|jsonl` | Output format (default `table`). |
| `--threshold <n>` | Override per-partition hot cutoff. |
| `-s, --subscription <name>` | Override the subscription. |
| `--admin-url <url>` | Override admin URL (also `PULSAR_ADMIN_URL`). |
| `--drain-window-secs <n>` | Gap between the two backlog samples (default 30; `0` disables drain measurement and its columns). Ignored under `--watch`. |
| `--watch` | Run continuously, clearing/redrawing every interval; trend derived from consecutive cycles. |
| `--watch-interval-secs <n>` | Seconds between watch cycles (default 60). |
| `--json-dir <dir>` | Write a JSON snapshot per run/cycle into `<dir>` (created if absent). |
| `--json-dir-max-files <n>` | Cap snapshot files in `--json-dir` (default 100; `0` = keep all; oldest pruned). |
| `--kube` | Fetch + show Kubernetes consumer-pod health (needs a binary built with `--features kube`). |
| `--kube-namespace <ns>` | Namespace of the consumer pods (default `default`). |
| `--kube-selector <sel>` | Label selector for the pods, e.g. `app=my-consumer` (required with `--kube`). |
| `--kube-configmap <name>` | ConfigMap holding `config.toml` for `--kube-assert` checks. |
| `--kube-assert key=value` | Assert a `config.toml` value (repeatable). |
| `--kube-log-tail <n>` | Log lines scanned per pod for signals (default 200; 0 = skip). |
| `--tui` | Launch the interactive TUI (needs a binary built with `--features tui`). |
| `--concurrency <n>` | Parallel admin requests (default 8). |
| `--timeout-secs <n>` | Per-request timeout (default 30). |
| `--problems-only` | Hide healthy (`OK`) rows. |

When drain measurement is on, the command takes at least `--drain-window-secs` longer to return — it deliberately sleeps between samples. If the user needs an instant read, tell them to pass `--drain-window-secs 0`.

## Watch mode and JSON snapshots

`--watch` runs continuously, clearing and redrawing the table every `--watch-interval-secs` (default 60) until the user interrupts it (ctrl-c). Two things differ from a single run:

- **Drain is derived from the previous cycle.** Consecutive cycles are the two samples, compared over the real elapsed time between them — so there's no mid-cycle sleep, and `--drain-window-secs` is ignored under `--watch`. The first cycle shows no trend (nothing to compare); every cycle after shows net rate and ETA over the actual interval. This makes watch mode the natural way to answer "is this draining or growing?" — leave it running and watch the `TREND`/`NET/s` columns settle.
- **The screen is cleared each cycle**, so you always see the current state, not a scrollback.

`--json-dir <dir>` writes one JSON snapshot per run/cycle into `<dir>` (created if absent), named `pth-<compact-timestamp>.json` — each a `{ "as_of": ..., "topics": [...] }` document with the full per-topic results. It works with or without `--watch`; a single run writes one snapshot, a watch session writes one per cycle. `--json-dir-max-files <n>` (default 100, `0` = keep all) caps the directory, pruning the oldest snapshots once exceeded so it never fills the disk. Only the tool's own `pth-*.json` files are ever pruned; other files in the directory are left untouched. Snapshots always carry the full topic set even under `--problems-only` (which only filters the on-screen view), so an archived series is complete for later analysis.

Typical uses to suggest:

- Live monitoring during an incident: `--watch --watch-interval-secs 30`.
- Building a diffable history: `--watch --json-dir ./snapshots` then compare successive files, or process them with `jq`.

## Interactive TUI (optional, `--tui`)

With a binary built `--features tui`, `--tui` launches a full-screen terminal UI that refreshes live. Four views cycle with `v`: **topic** (the classic table), **partition** (one row per partition flattened across all topics), **kube** (pod summary, populated when also run with `--kube`), and **combined** (topics on top, the drilled-into topic's partitions below). Navigate with **↑/↓** (or `k`/`j`) to move the cursor between topics; **Enter** drills into the selected topic (combined view scoped to its partitions); **Esc** backs out; **r** refreshes, **q** quits. Refresh cadence is `--watch-interval-secs`; drain trend comes from consecutive refreshes. Like `--kube`, it's a build feature — without it, `--tui` prints guidance and exits. The features are independent; combine `--features tui,kube` for the kube view to have data.

## Kubernetes correlation (optional, `--kube`)

The subscription's consumers usually run in Kubernetes pods, and a broker-side symptom is often caused by a pod-side fault: an OOMKill, a crash-loop, a pod stuck mid-ramp, a bad config rollout. The `--kube` flag fetches consumer-side health and shows it alongside the topic table, turning "backlog is growing" into "backlog is growing *because* two pods OOM-killed 4 minutes ago".

**This is an opt-in build feature.** It pulls in `kube-rs` + `tokio`, so it's compiled only when the binary is built with `cargo build --features kube`. Without that, `--kube` prints a warning and is ignored. If a user wants Kubernetes correlation, first confirm the binary was built with the feature.

At runtime it needs a namespace and label selector:

```sh
pulsar-topic-health --kube \
  --kube-namespace my-ns \
  --kube-selector app=my-consumer \
  --kube-configmap my-consumer-config \
  --kube-assert worker_count=24 --kube-assert batch_size=30
```

Flags: `--kube` (enable), `--kube-namespace` (default `default`), `--kube-selector` (label selector, **required** with `--kube`), `--kube-configmap` (ConfigMap holding `config.toml`), `--kube-assert key=value` (repeatable config assertions), `--kube-log-tail N` (log lines scanned per pod, default 200, 0 to skip).

Authentication is inherited from the environment exactly as `kubectl` gets it — `kube-rs`'s default config inference reads `~/.kube/config`, `KUBECONFIG`, or an in-cluster service-account token. No token flag, and the agent must never put kube credentials in files or output.

**Settings can live in the config file** instead of on the command line, via a `[kube]` section. Either `--kube` or `enabled = true` in the section activates correlation; CLI flags override config values, and `--kube-assert` entries merge with the config `assert` table (CLI wins on duplicate keys):

```toml
[kube]
enabled   = true
namespace = "my-ns"
selector  = "app=my-consumer"
configmap = "my-consumer-config"
log_tail  = 200
assert    = { worker_count = "24", batch_size = "30" }
```

What it shows:
- A **pod-summary section** above the topic table: pod name, ready count, restarts, age, and state (with `OOMKilled` / `CrashLoopBackOff` highlighted).
- **Rollout flags**: a split rollout (more than one image across pods) or large rollout skew (pods not restarted together — a rollout may be incomplete or a pod is stale).
- **Events**: recent OOMKilling / Evicted / Failed events in the namespace.
- **Config assertions**: any failed `--kube-assert` printed as `✗ config <key>: expected X, got Y`.
- **Log signals**: matches for ramp / OOM / error / config patterns in each pod's recent logs.
- **Correlation hint in DETAIL**: unhealthy topics gain a short pod-side cause, e.g. `kube: 2 pod(s) OOM-killed`.

In JSONL mode the full kube report is emitted as one extra line before the topic lines.

How to reason about it:
- The kube side is **best-effort and isolated** — if the cluster is unreachable or auth fails, the tool prints an `unreachable` notice and still renders the complete Pulsar report. Never treat a kube failure as a reason to withhold the topic health.
- A consumer-side problem (failed pods, failed config assertion, split rollout) contributes to the non-zero exit code alongside topic health, so `--kube` doubles as a post-deploy gate.
- The most useful correlation: a `NO_CONSUMERS` or `growing` topic *with* an OOMKilled pod is a memory-pressure story — the consumer died, so nobody's draining. A `growing` topic with all pods healthy is a capacity story — consumers are fine but outnumbered by producers. The pod section is what tells these apart.

## Reading the output

### Columns

A single `as of <UTC RFC3339>` line prints above the table (and an `as_of` field on every JSONL line). Columns: `TOPIC`, `STATUS`, `TIME` (how long in the current state — only shown when prior history exists), `BACKLOG` (messages), `SIZE` (backlog bytes, binary units), `CONSUMERS`, `UNACKED`, `RATE OUT` (msg/s delivered), `HEADROOM` (trim-safety margin), then — only when drain is on — `TREND` / `NET/s` / `ETA`, then `DETAIL`. `BACKLOG`, `SIZE`, and `UNACKED` can be colour-coded via the optional `[colors]` config (see above).

**`TIME` (time-in-state)** shows how long a topic has continuously held its current (status, trend) pair — `2h` in `BACKLOG/growing` vs `45s` — so a sustained incident is distinguishable from a transient spike. It needs history: the latest snapshot in `--json-dir`, or the in-memory previous cycle under `--watch`. It's blank (`—`) when there's nothing to compare against (a single run with no `--json-dir`, or the first watch cycle). Resolution is snapshot/cycle-grained — it's "time since we last observed a *different* state", so state flips faster than the poll interval aren't seen. To get useful `TIME` values from repeated single runs, point them all at the same `--json-dir`.

### Status, worst-first

| Status | Meaning | Catchable? |
|---|---|---|
| `TRIMMED` | A partition's cursor is below the oldest surviving ledger — the next message it wants is **deleted**. Consumer spins forever. | **No** — needs a cursor reset (a human decision; the tool won't do it). |
| `NO_CONSUMERS` | Subscription exists but has zero consumers anywhere. | After attaching a consumer. |
| `PARTITION_GAP` | Subscription missing or consumer-less on *some* partitions of an otherwise-attached topic. | After fixing the affected partitions. |
| `BACKLOG` | One or more partitions over the hot threshold; data is all present. | **Yes** — if the trend is draining. |
| `MISSING_SUB` | Subscription not attached to the topic at all. | After creating/attaching. |
| `OK` | Attached, has consumers, under threshold, no trim risk. | — |
| `ERROR` | Stats fetch failed (HTTP/transport error shown inline). | Fix connectivity/auth. |

The status is the single worst condition, but the `DETAIL` column and JSONL always list *all* findings (trimmed, at-edge, hot partitions, gaps). Exit code: `0` all healthy, `1` usage/runtime error, `2` one or more unhealthy topics — suitable for cron/CI.

### HEADROOM — the catch that trips people up

`HEADROOM` is the entries of margin before retention would reach the cursor:

- A **large** number (green) = comfortable; the trimmer is nowhere near. A badly-behind topic with big headroom is still safe.
- **At-edge** (yellow) = cursor sits in the oldest surviving ledger; one GC cycle from loss. Shown as `edge: pN(~k)` in DETAIL.
- **Zero or negative** on a topic *with* a backlog (red) = at or past the trim floor — this is the `TRIMMED` case.
- **Blank (`—`)** = there is no backlog to lose (idle, caught-up topic) or internal-stats were unavailable. A blank headroom on an `OK` topic is the healthiest possible state, not a missing reading. Do **not** read a blank as "0" or as danger.

### TREND / NET/s / ETA — can I catch up?

- `draining` (green), negative `NET/s`, and an `ETA` = clearing; the ETA is a naive `backlog ÷ net-rate` projection — treat it as order-of-magnitude (minutes/hours/days), not a promise.
- `growing` (red), positive `NET/s`, no ETA = producers are outpacing consumers. Waiting will not clear it; the fix is more consumer capacity. This is the signal that matters most for a standing backlog.
- `stable` (yellow) = net change is under ~1% of the backlog; treading water. A high, flat backlog that stays `stable` for hours is a *standing* backlog — arguably worse than a spike, because it's the steady state.
- `empty` = zero backlog at both samples.

**Why `ETA` is usually blank.** ETA is only shown for `draining` topics — it's time-to-clear, which only exists when the backlog is shrinking. A `stable` or `growing` topic has no finite ETA by definition, so the column shows `—`. A run where every ETA is blank means nothing is currently draining; it is not a bug. ETA populates the moment a topic flips to `draining` with a negative `NET/s`.

### DETAIL

Compact, worst-first: `TRIMMED: pN` (a `!` marks broker-corroborated stuck), then `edge: pN(~k)`, then `hot: pN=<msgs> (<bytes>)`, then `gaps: pN (reason)`. Partition names are shortened to `pN`.

## Interpreting common situations

- **Everything `BACKLOG` but `stable` with small positive `NET/s`.** Data is present (check HEADROOM is not red), so nothing is lost, but consumers aren't keeping pace. Not an emergency; it's a capacity signal. The fastest-growing topic (largest positive `NET/s`) is where to look first.
- **A topic shows huge `BACKLOG` but green `HEADROOM` in the millions.** Behind, not in danger. Catchable if the trend cooperates. Contrast with a huge backlog and red/zero headroom, which is genuine data loss.
- **`TRIMMED` appears.** Stop and flag it clearly. The consumer is reading deleted ledgers; no amount of waiting helps. Remediation is a `pulsar-admin topics reset-cursor` (to earliest or a timestamp) — a deliberate, irreversible human action that this read-only tool intentionally does not perform. Do not run it on the user's behalf without explicit confirmation.
- **The tool disagrees with a Grafana panel.** Usually a window mismatch: the CLI is an instantaneous snapshot, the dashboard is a time range. A flat, high line on the dashboard is a `stable` standing backlog, which is consistent with the CLI screaming `BACKLOG`. If the dashboard's per-partition *byte* sizes differ from the CLI, confirm the CLI is on a version that fetches `SIZE` (backlog bytes); older builds only had message counts.

## Cost and load notes

- Base topics cost one partitioned-stats call plus one internal-stats call **per partition** (the broker doesn't aggregate cursor/ledger data across partitions). A 6-partition topic is ~7 admin calls. Fetching backlog byte size adds broker-side locking to the stats call. All requests run through a bounded worker pool (`--concurrency`).
- Drain mode adds one cheap aggregate call per topic and a sleep of `--drain-window-secs`. For a fast, lighter run, use `--drain-window-secs 0`.
- If admin-API latency becomes a problem under load, reducing `--concurrency` or disabling drain are the first levers.

## Building

Targets a recent stable Rust toolchain (edition 2024; `rust-toolchain.toml` pins the exact version). From the repo root:

```bash
cargo build --release        # binary at target/release/pulsar-topic-health
cargo test                   # unit tests
cargo clippy                 # lint
```

## Files in this skill

- `config-example.toml` — a generic, ready-to-edit config with placeholder tenant/namespace/subscription names.

## Guardrails for the agent

- This tool is read-only; keep it that way. Never suggest wiring in acks, deletes, or cursor resets as part of "using" it.
- Never fabricate or hardcode real tenant, namespace, subscription, or broker names in configs or examples. Use placeholders.
- Never place the auth token in a file, in the config, or on the command line; it belongs in `TOKEN`/`PULSAR_TOKEN` env only, and must never be echoed back.
- When a `TRIMMED` state is present, surface it prominently and explain that waiting won't help — but leave the reset decision to the user.
