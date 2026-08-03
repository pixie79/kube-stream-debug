//! Interactive terminal UI (ratatui + crossterm), compiled into every build.
//!
//! The pure view-state and selection logic it drives (in `crate::view`) is
//! unit-tested; this file is the terminal shell around it.
//!
//! Keybindings:
//!   ↑/↓ (or k/j)  move the cursor
//!   Tab           (kube view) switch the cursor between the pods and nodes
//!                 sections
//!   Enter         drill in: a topic → combined; a pod → pod-detail (logs +
//!                 stats); a node → node-detail (capacity + its pods)
//!   Esc           back out of any drill-in
//!   v             cycle view (topic → kube → combined)
//!   ?             toggle the status-legend help overlay
//!   r             refresh now
//!   q / Ctrl-C    quit

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use ratatui::DefaultTerminal;

use crate::health::TopicHealth;
use crate::kube::KubeReport;
use crate::view::{
    partition_rows_for_topic, partition_status_legend, status_legend,
    status_severity, trend_legend,
    PartitionRow, Severity, View, ViewState,
};

/// Everything the TUI needs to render one frame. The caller refreshes this on
/// the interval / on demand by re-running the checks.
pub struct Frame {
    pub run_at: String,
    pub topics: Vec<TopicHealth>,
    pub kube: Option<KubeReport>,
    pub subscription: String,
}

/// A callback that fetches a fresh `Frame` (runs the Pulsar + optional kube
/// checks). Kept as a closure so the TUI doesn't depend on main's wiring. Must
/// be `Send`: the refresh runs on a background worker thread so it never blocks
/// the UI. Its captures (the admin client, topic list, kube config) are all
/// `Send`, so callers satisfy this naturally.
pub type Refresh<'a> = dyn FnMut() -> Frame + Send + 'a;

/// Run the interactive TUI until the user quits. `interval` is the auto-refresh
/// cadence.
pub fn run(mut refresh: Box<Refresh<'_>>, interval: Duration) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, refresh.as_mut(), interval);
    ratatui::restore();
    result
}

/// Message from the UI thread to the refresh worker.
enum RefreshCmd {
    Now,
    Stop,
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    refresh: &mut Refresh<'_>,
    interval: Duration,
) -> std::io::Result<()> {
    use std::sync::mpsc;

    let mut state = ViewState::default();
    let mut frame = refresh();

    // Refresh runs on a background thread so the slow fetch (Pulsar admin +
    // Kubernetes API) never blocks input or view switching. A scoped thread lets
    // the worker borrow the same non-'static refresh closure the UI uses.
    let (frame_tx, frame_rx) = mpsc::channel::<Frame>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<RefreshCmd>();

    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut last = Instant::now();
            loop {
                let wait = interval.saturating_sub(last.elapsed());
                match cmd_rx.recv_timeout(wait) {
                    Ok(RefreshCmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Ok(RefreshCmd::Now) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                let f = refresh();
                last = Instant::now();
                if frame_tx.send(f).is_err() {
                    break;
                }
            }
        });
        let ui_result = ui_loop(terminal, &mut state, &mut frame, &frame_rx, &cmd_tx);
        let _ = cmd_tx.send(RefreshCmd::Stop);
        ui_result
    })
}

/// The interactive UI loop: instant view switches, non-blocking frame updates.
fn ui_loop(
    terminal: &mut DefaultTerminal,
    state: &mut ViewState,
    frame: &mut Frame,
    frame_rx: &std::sync::mpsc::Receiver<Frame>,
    cmd_tx: &std::sync::mpsc::Sender<RefreshCmd>,
) -> std::io::Result<()> {
    loop {
        while let Ok(f) = frame_rx.try_recv() {
            *frame = f;
            state.clamp_cursor(frame.topics.len());
        }

        terminal.draw(|f| draw(f, state, frame))?;

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()? {
                // Names in render order, for cursor bounds and drill resolution.
                let pod_names: Vec<String> =
                    frame.kube.as_ref().map(|k| k.pods.iter().map(|p| p.name.clone()).collect()).unwrap_or_default();
                let node_names: Vec<String> =
                    frame.kube.as_ref().map(|k| k.nodes.iter().map(|n| n.name.clone()).collect()).unwrap_or_default();

                // How many rows the cursor can move over in the current view.
                let cursor_len = match state.view {
                    View::Kube => match state.kube_focus {
                        crate::view::KubeFocus::Pods => pod_names.len(),
                        crate::view::KubeFocus::Nodes => node_names.len(),
                    },
                    _ => frame.topics.len(),
                };

                // In pod-detail, the log line count bounds the log cursor.
                let log_len = if state.view == View::PodDetail {
                    state
                        .selected_pod
                        .as_ref()
                        .and_then(|name| {
                            frame.kube.as_ref().and_then(|k| {
                                k.pod_logs.iter().find(|l| &l.pod == name).map(|l| l.lines.len())
                            })
                        })
                        .unwrap_or(0)
                } else {
                    0
                };

                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('?') => state.toggle_help(),
                    KeyCode::Char('v') => state.cycle_view(),
                    KeyCode::Char('w') if state.view == View::PodDetail => state.toggle_log_wrap(),
                    KeyCode::Char('m') if state.view == View::PodDetail => {
                        state.pod_detail_metrics = !state.pod_detail_metrics;
                    }
                    KeyCode::Tab if state.view == View::Kube => state.toggle_kube_focus(),
                    KeyCode::Up | KeyCode::Char('k') => {
                        if state.view == View::PodDetail {
                            state.log_cursor_up();
                        } else if state.view == View::Metrics {
                            state.metrics_scroll_up(1);
                        } else {
                            state.cursor_up();
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if state.view == View::PodDetail {
                            state.log_cursor_down(log_len);
                        } else if state.view == View::Metrics {
                            state.metrics_scroll_down(1);
                        } else {
                            state.cursor_down(cursor_len);
                        }
                    }
                    KeyCode::PageUp if state.view == View::Metrics => state.metrics_scroll_up(10),
                    KeyCode::PageDown if state.view == View::Metrics => state.metrics_scroll_down(10),
                    KeyCode::Enter => match state.view {
                        View::Kube => state.kube_drill_in(&pod_names, &node_names),
                        View::PodDetail => state.log_expand(),
                        View::NodeDetail | View::Metrics | View::Stability => {}
                        _ => state.drill_in(&frame.topics),
                    },
                    KeyCode::Esc => {
                        // In pod-detail, Esc collapses an expanded line first;
                        // a second Esc leaves the view.
                        if state.view == View::PodDetail && state.log_collapse() {
                            // consumed by collapse
                        } else {
                            state.drill_out();
                        }
                    }
                    KeyCode::Char('r') => {
                        // Ask the worker to refresh; the frame arrives via the
                        // channel and folds in at the top of the loop.
                        let _ = cmd_tx.send(RefreshCmd::Now);
                    }
                    _ => {}
                }
            }
    }
    Ok(())
}

fn draw(f: &mut ratatui::Frame, state: &ViewState, frame: &Frame) {
    // Clear the entire frame first. Some views (notably the sparse metrics
    // summary and the header) don't paint every cell, so without a full clear,
    // characters from a previous, longer frame bleed through the gaps.
    f.render_widget(Clear, f.area());
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(3),    // body
            Constraint::Length(1), // footer / help
        ])
        .split(f.area());

    let focus = state
        .drilled_topic
        .as_deref()
        .map(|t| format!(" · drilled: {}", short_topic(t)))
        .unwrap_or_default();
    let header = format!(
        " pulsar-topic-health · sub: {} · view: {}{} · as of {}",
        short_topic(&frame.subscription),
        state.view.label(),
        focus,
        frame.run_at,
    );
    f.render_widget(Paragraph::new(header), chunks[0]);

    match state.view {
        View::Topic => draw_topics(f, chunks[1], state, frame),
        View::Kube => draw_kube(f, chunks[1], state, frame),
        View::Metrics => draw_metrics(f, chunks[1], frame, state.metrics_scroll),
        View::Stability => draw_stability(f, chunks[1], frame),
        View::Combined => draw_combined(f, chunks[1], state, frame),
        View::PodDetail => draw_pod_detail(f, chunks[1], state, frame),
        View::NodeDetail => draw_node_detail(f, chunks[1], state, frame),
    }

    let help = match state.view {
        View::Kube => " ↑/↓=move  Tab=pods/nodes  Enter=open  Esc=back  v=view  ?=legend  q=quit",
        View::PodDetail => " ↑/↓=select  Enter=expand  Esc=collapse/back  m=logs/metrics  w=wrap  q=quit",
        View::NodeDetail => " Esc=back  ?=legend  r=refresh  q=quit",
        View::Metrics => " ↑/↓ PgUp/PgDn=scroll  v=view  r=refresh  q=quit",
        View::Stability => " v=view  r=refresh  q=quit  (connection stability — flapping pods flagged)",
        _ => " ↑/↓=move  Enter=drill in  Esc=back  v=view  ?=legend  r=refresh  q=quit",
    };
    f.render_widget(Paragraph::new(help), chunks[2]);

    // Help overlay draws on top of everything else.
    if state.show_help {
        draw_help_overlay(f);
    }
}

/// Render one metric as coloured spans. Value shown with a "/s" suffix for
/// rates; a trend arrow only when the value actually moved. Colour: red breach,
/// yellow worsening or stalled (higher-better at 0), green improving, dim gray
/// when flat and healthy (so the eye skips the noise and lands on what matters).
fn metric_line_spans(line: &crate::kube::MetricLine) -> Vec<Span<'static>> {
    // Configured but not returned by the scrape → show it, dimmed, so the
    // operator sees what's missing rather than it silently vanishing.
    if !line.present {
        return vec![Span::styled(
            format!("{}=(no data)", line.label),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )];
    }
    let style = if line.breached {
        Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD)
    } else if line.worsening || line.stalled {
        Style::default().fg(Color::Yellow)
    } else if line.improving {
        Style::default().fg(Color::Green)
    } else {
        // Flat and healthy — dim so it recedes.
        Style::default().fg(Color::DarkGray)
    };
    // Arrow only when the value moved; rates get a "/s" suffix.
    let suffix = if line.is_rate { "/s" } else { "" };
    let arrow = if line.changed && !line.arrow.is_empty() {
        line.arrow.clone()
    } else {
        String::new()
    };
    vec![Span::styled(
        format!("{}={:.0}{suffix}{arrow}", line.label, line.value),
        style,
    )]
}

/// Fleet-wide metrics summary: every pod's curated metrics, grouped by category
/// (consumer / throughput / bottleneck / health). Pods with a breach are listed
/// first within each group. This is the "where's the problem across the fleet"
/// view.
fn draw_metrics(f: &mut ratatui::Frame, area: Rect, frame: &Frame, scroll: u16) {
    let block = Block::default().borders(Borders::ALL).title("fleet metrics");
    let Some(report) = &frame.kube else {
        f.render_widget(
            Paragraph::new("metrics scraping is off (set [metrics] enabled = true)").block(block),
            area,
        );
        return;
    };
    if report.pod_metric_summaries.is_empty() {
        f.render_widget(
            Paragraph::new("no pod metrics scraped yet").block(block),
            area,
        );
        return;
    }

    let categories = ["consumer", "throughput", "bottleneck", "health"];
    let mut lines: Vec<Line> = Vec::new();

    // A pod that returned NO present metric this cycle wasn't scraped (the
    // port-forward is best-effort and can miss a pod per cycle). Pull those out
    // into one compact line instead of repeating "(no data)" per metric per pod.
    let (scraped, not_scraped): (Vec<_>, Vec<_>) = report
        .pod_metric_summaries
        .iter()
        .partition(|s| s.lines.iter().any(|l| l.present));
    if !not_scraped.is_empty() {
        let names: Vec<&str> = not_scraped.iter().map(|s| short_pod(&s.pod)).collect();
        lines.push(Line::from(Span::styled(
            format!("not scraped this cycle ({}): {}", names.len(), names.join(", ")),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM),
        )));
        lines.push(Line::from(""));
    }

    // Pipeline flow, per pod: consumed/s → written/s → sink sent/s, so the shape
    // of the pipeline (and where it narrows) is obvious at a glance.
    lines.push(Line::from(Span::styled(
        "── flow (per pod) ──",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for summary in &scraped {
        let find = |needle: &str| {
            summary
                .lines
                .iter()
                .find(|l| l.label.contains(needle))
                .map(|l| l.value)
        };
        // Prefer explicit stage rates; fall back to throughput rps.
        let consumed = find("consumed");
        let written = find("written");
        let sink = find("sink");
        if consumed.is_none() && written.is_none() && sink.is_none() {
            continue;
        }
        let fmt = |v: Option<f64>| v.map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into());
        let mut spans = vec![Span::raw(format!("  {:<14} ", short_pod(&summary.pod)))];
        // Colour the flow yellow if any stage is zero while an upstream one isn't
        // (a stall/narrowing), else dim.
        let stalled_flow = matches!((consumed, written), (Some(c), Some(w)) if c > 0.0 && w == 0.0)
            || matches!((written, sink), (Some(w), Some(s)) if w > 0.0 && s == 0.0);
        let style = if stalled_flow {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(
            format!("consumed {}/s → written {}/s → sink {}/s", fmt(consumed), fmt(written), fmt(sink)),
            style,
        ));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));

    for cat in categories {
        // Collect each pod's lines for this category.
        let mut rows: Vec<(&str, Vec<&crate::kube::MetricLine>, bool)> = Vec::new();
        for summary in &scraped {
            let cat_lines: Vec<&crate::kube::MetricLine> =
                summary.lines.iter().filter(|l| l.category == cat).collect();
            if cat_lines.is_empty() {
                continue;
            }
            let has_breach = cat_lines.iter().any(|l| l.breached);
            rows.push((short_pod(&summary.pod), cat_lines, has_breach));
        }
        if rows.is_empty() {
            continue;
        }

        lines.push(Line::from(Span::styled(
            format!("── {cat} ──"),
            Style::default().add_modifier(Modifier::BOLD),
        )));

        // Collapse metrics that are identical across every pod into one line, so
        // the screen doesn't repeat "throughput rps=0" six times. A metric label
        // qualifies if every pod reports the same value, none breached/worsening.
        let all_labels: Vec<String> = rows
            .first()
            .map(|(_, ls, _)| ls.iter().map(|l| l.label.clone()).collect())
            .unwrap_or_default();
        let mut collapsed: Vec<&str> = Vec::new();
        for label in &all_labels {
            let vals: Vec<&crate::kube::MetricLine> = rows
                .iter()
                .filter_map(|(_, ls, _)| ls.iter().find(|l| &l.label == label).copied())
                .collect();
            let uniform = vals.len() == rows.len()
                && vals.iter().all(|l| {
                    (l.value - vals[0].value).abs() < f64::EPSILON
                        && l.present == vals[0].present
                        && !l.breached
                        && !l.worsening
                        && !l.stalled
                });
            if uniform && !vals.is_empty() {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(metric_line_spans(vals[0]));
                spans.push(Span::styled(
                    format!("  (all {} pods)", rows.len()),
                    Style::default().fg(Color::DarkGray),
                ));
                lines.push(Line::from(spans));
                collapsed.push(label);
            }
        }

        // Per-pod rows for the metrics that aren't uniform. Breached pods first.
        let mut per_pod: Vec<&(&str, Vec<&crate::kube::MetricLine>, bool)> =
            rows.iter().filter(|(_, ls, _)| ls.iter().any(|l| !collapsed.contains(&l.label.as_str()))).collect();
        per_pod.sort_by_key(|(_, _, breach)| if *breach { 0 } else { 1 });
        for (pod, cat_lines, _) in per_pod {
            let shown: Vec<&&crate::kube::MetricLine> =
                cat_lines.iter().filter(|l| !collapsed.contains(&l.label.as_str())).collect();
            if shown.is_empty() {
                continue;
            }
            let mut spans: Vec<Span> = vec![Span::raw(format!("  {pod:<14} "))];
            for (i, ml) in shown.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.extend(metric_line_spans(ml));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
    }

    // Clamp scroll so you can't page past the end. The inner height is the pane
    // minus the top/bottom border rows.
    let inner_h = area.height.saturating_sub(2);
    let total = lines.len() as u16;
    let max_scroll = total.saturating_sub(inner_h);
    let offset = scroll.min(max_scroll);
    // Title shows a scroll position hint when the content overflows.
    let title = if max_scroll > 0 {
        format!("fleet metrics  [{}–{}/{}  ↑/↓ PgUp/PgDn]", offset + 1, (offset + inner_h).min(total), total)
    } else {
        "fleet metrics".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    f.render_widget(Paragraph::new(lines).block(block).scroll((offset, 0)), area);
}

/// Shorten a pod name to its trailing hash for compact display (the deployment
/// prefix is the same across all pods, so the tail is what distinguishes them).
fn short_pod(pod: &str) -> &str {
    pod.rsplit('-').next().unwrap_or(pod)
}

/// Connection-stability view: a per-pod table of reconnect/throttle-transition
/// rates and active-partition churn, flagging pods caught in an idle→cull→
/// rebalance flapping loop. Unstable pods sort to the top.
fn draw_stability(f: &mut ratatui::Frame, area: Rect, frame: &Frame) {
    let block = Block::default().borders(Borders::ALL).title("connection stability");
    let Some(report) = &frame.kube else {
        f.render_widget(
            Paragraph::new("metrics scraping is off (set [metrics] enabled = true)").block(block),
            area,
        );
        return;
    };
    if report.pod_stability.is_empty() {
        f.render_widget(
            Paragraph::new("no stability data yet (needs a few scrapes to compute churn)").block(block),
            area,
        );
        return;
    }

    let header = Row::new(vec![
        Cell::from("POD"),
        Cell::from("RECONNECT/s"),
        Cell::from("THROTTLE-TRANS/s"),
        Cell::from("ACTIVE-PARTS"),
        Cell::from("CHURN"),
        Cell::from("VERDICT"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let mut rows: Vec<&crate::kube::PodStabilityLine> = report.pod_stability.iter().collect();
    // Unstable pods first.
    rows.sort_by_key(|s| if s.flapping_rate || s.flapping_rebalance { 0 } else { 1 });

    let table_rows: Vec<Row> = rows
        .iter()
        .map(|s| {
            let unstable = s.flapping_rate || s.flapping_rebalance;
            // Build the verdict text from the two independent signals.
            let verdict = match (s.flapping_rate, s.flapping_rebalance) {
                (true, true) => "FLAPPING (reconnect + rebalance)",
                (true, false) => "FLAPPING (reconnect churn)",
                (false, true) => "FLAPPING (rebalance churn)",
                (false, false) => "stable",
            };
            let row_style = if unstable {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Row::new(vec![
                Cell::from(short_pod(&s.pod).to_string()),
                Cell::from(format!("{:.1}", s.reconnect_rate)),
                Cell::from(format!("{:.1}", s.throttle_transition_rate)),
                Cell::from(format!("{:.0}", s.active_parts)),
                Cell::from(format!("{:.0}", s.active_parts_churn)),
                Cell::from(verdict),
            ])
            .style(row_style)
        })
        .collect();

    let widths = [
        Constraint::Length(16),
        Constraint::Length(13),
        Constraint::Length(18),
        Constraint::Length(13),
        Constraint::Length(8),
        Constraint::Min(20),
    ];
    let table = Table::new(table_rows, widths).header(header).block(block);
    f.render_widget(table, area);
}

/// Map a status severity to a ratatui colour.
fn severity_color(sev: Severity) -> Color {
    match sev {
        Severity::Ok => Color::Green,
        Severity::Warn => Color::Yellow,
        Severity::Bad => Color::Red,
    }
}

/// A status cell coloured by its severity.
fn status_cell(label: &str) -> Cell<'static> {
    Cell::from(label.to_string()).style(Style::default().fg(severity_color(status_severity(label))))
}

/// Centered legend overlay explaining statuses and trends. Toggled with `?`.
fn draw_help_overlay(f: &mut ratatui::Frame) {
    let area = centered_rect(70, 70, f.area());
    f.render_widget(Clear, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Status legend",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for (label, meaning) in status_legend() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:<14}"),
                Style::default().fg(severity_color(status_severity(label))),
            ),
            Span::raw(*meaning),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Trend legend",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for (label, meaning) in trend_legend() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:<14}"),
                Style::default().fg(severity_color(status_severity(label))),
            ),
            Span::raw(*meaning),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Partition status (partition view)",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for (label, meaning) in partition_status_legend() {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label:<22}"),
                Style::default().fg(severity_color(status_severity(label))),
            ),
            Span::raw(*meaning),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from("CPU/MEM columns show used/limit, coloured by % of limit."));
    lines.push(Line::from(Span::styled(
        "press ? to close",
        Style::default().add_modifier(Modifier::ITALIC),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title("help")
        .style(Style::default());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Compute a centered rect `pct_x`×`pct_y` percent of `r`.
fn centered_rect(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(vert[1])[1]
}

fn draw_topics(f: &mut ratatui::Frame, area: Rect, state: &ViewState, frame: &Frame) {
    // Show TREND/ETA only when at least one topic has a drain sample, matching
    // the plain-table renderer (watch mode / --drain-window populate these).
    let show_drain = frame.topics.iter().any(|t| t.drain.is_some());

    let mut header_cells = vec!["TOPIC", "STATUS", "BACKLOG", "SIZE", "CONS", "NET/s"];
    if show_drain {
        header_cells.push("TREND");
        header_cells.push("ETA");
    }
    header_cells.push("DETAIL");
    let header = Row::new(header_cells).style(Style::default().add_modifier(Modifier::BOLD));

    let rows = frame.topics.iter().enumerate().map(|(i, t)| {
        let mut cells = vec![
            Cell::from(short_topic(&t.topic)),
            status_cell(t.status.label()),
            Cell::from(opt_num(t.total_backlog)),
            Cell::from(opt_bytes(t.backlog_bytes)),
            Cell::from(opt_num(t.consumers)),
            Cell::from(net_str(t)),
        ];
        if show_drain {
            cells.push(trend_cell(t));
            cells.push(Cell::from(eta_str(t)));
        }
        cells.push(Cell::from(topic_detail(t)));
        emphasise(Row::new(cells), i == state.cursor)
    });

    let mut widths = vec![
        Constraint::Percentage(24),
        Constraint::Length(13),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(5),
        Constraint::Length(8),
    ];
    if show_drain {
        widths.push(Constraint::Length(9)); // TREND
        widths.push(Constraint::Length(8)); // ETA
    }
    widths.push(Constraint::Percentage(26)); // DETAIL

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("topics"));
    f.render_widget(table, area);
}

/// TREND cell for a topic, coloured by direction (matches the plain table).
fn trend_cell(t: &TopicHealth) -> Cell<'static> {
    use crate::drain::Trend;
    let Some(drain) = &t.drain else {
        return Cell::from("—");
    };
    let color = match drain.trend {
        Trend::Draining => Color::Green,
        Trend::Growing => Color::Red,
        Trend::Stable => Color::Yellow,
        Trend::Empty => Color::DarkGray,
    };
    let modifier = if matches!(drain.trend, Trend::Growing) {
        Modifier::BOLD
    } else {
        Modifier::empty()
    };
    Cell::from(drain.trend.label().to_string())
        .style(Style::default().fg(color).add_modifier(modifier))
}

/// ETA-to-empty string for a topic, or "—" when not draining / unknown.
fn eta_str(t: &TopicHealth) -> String {
    match t.drain.as_ref().and_then(|d| d.eta_secs) {
        Some(secs) => crate::drain::format_eta(secs),
        None => "—".to_string(),
    }
}

/// Draw a partition table from a given row set. `cursor` highlights a row when
/// `Some` (used in the standalone partition view); `None` for the drill-in pane.
fn draw_partition_rows(
    f: &mut ratatui::Frame,
    area: Rect,
    rows_data: &[PartitionRow],
    cursor: Option<usize>,
    title: &str,
) {
    let header = Row::new(vec!["TOPIC", "PART", "STATUS", "BACKLOG", "SIZE", "CONS", "UNACKED"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = rows_data.iter().enumerate().map(|(i, p)| {
        let row = Row::new(vec![
            Cell::from(short_topic(&p.topic)),
            Cell::from(format!("p{}", p.index)),
            status_cell(p.status),
            Cell::from(p.backlog.to_string()),
            Cell::from(crate::view::fmt_bytes(p.backlog_bytes)),
            Cell::from(p.consumers.to_string()),
            Cell::from(p.unacked_messages.to_string()),
        ]);
        emphasise(row, cursor == Some(i))
    });
    let widths = [
        Constraint::Percentage(30),
        Constraint::Length(6),
        Constraint::Length(20),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(5),
        Constraint::Length(9),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title.to_string()));
    f.render_widget(table, area);
}

fn draw_kube(f: &mut ratatui::Frame, area: Rect, state: &ViewState, frame: &Frame) {
    use crate::view::KubeFocus;

    let Some(report) = &frame.kube else {
        f.render_widget(
            Paragraph::new("kube view: run with --kube (and a binary built with --features kube)")
                .block(Block::default().borders(Borders::ALL).title("kubernetes")),
            area,
        );
        return;
    };

    // No pods matched: show the discovery help (namespaces or labels) in place,
    // rather than a dead panel. The session keeps running.
    if report.pods.is_empty() {
        let text = match &report.discovery {
            Some(d) => format!("namespace {}\n{}", report.namespace, crate::kube::format_discovery(d)),
            None => format!("no pods matched in namespace {}", report.namespace),
        };
        f.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("kubernetes — no match")),
            area,
        );
        return;
    }

    // Three stacked panels: pods (cursorable), nodes (cursorable), log stats.
    let panels = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(45), // pods
            Constraint::Length((report.nodes.len() as u16) + 3), // nodes + borders/header
            Constraint::Min(4),         // log stats
        ])
        .split(area);

    // --- Pods panel ---
    let pods_focused = state.kube_focus == KubeFocus::Pods;
    let pod_header = Row::new(vec!["POD", "READY", "REST", "AGE", "CPU", "MEM", "STATE"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let pod_rows = report.pods.iter().enumerate().map(|(i, p)| {
        let ready = format!("{}/{}", p.ready, p.total_containers);
        let state_txt = if p.transform_error {
            "DLQ-ERROR".to_string()
        } else if p.memory_pressure {
            "MEM-CRITICAL".to_string()
        } else if p.oom_killed {
            "OOMKilled".to_string()
        } else {
            p.reason.clone().unwrap_or_else(|| "Running".to_string())
        };
        let row = Row::new(vec![
            Cell::from(p.name.clone()),
            Cell::from(ready),
            Cell::from(p.restarts.to_string()),
            Cell::from(p.age_secs.map(fmt_age).unwrap_or_else(|| "—".to_string())),
            Cell::from(pod_cpu(p)),
            Cell::from(pod_mem(p)),
            status_cell(&state_txt),
        ]);
        emphasise(row, pods_focused && i == state.cursor)
    });
    let pod_widths = [
        Constraint::Percentage(34),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Length(19),
        Constraint::Percentage(16),
    ];
    let pods_title = if pods_focused { "pods ◀" } else { "pods" };
    f.render_widget(
        Table::new(pod_rows, pod_widths)
            .header(pod_header)
            .block(Block::default().borders(Borders::ALL).title(pods_title)),
        panels[0],
    );

    // --- Nodes panel ---
    let nodes_focused = state.kube_focus == KubeFocus::Nodes;
    let node_rows = report.nodes.iter().enumerate().map(|(i, n)| {
        let cpu = n.alloc_cpu_milli.map(crate::kube::format_cpu).unwrap_or_else(|| "?".into());
        let mem = n.alloc_mem_bytes.map(crate::view::fmt_bytes).unwrap_or_else(|| "?".into());
        let inst = n.instance_type.as_deref().unwrap_or("—");
        let row = Row::new(vec![
            Cell::from(n.name.clone()),
            Cell::from(inst.to_string()),
            Cell::from(format!("{cpu} CPU")),
            Cell::from(format!("{mem} alloc")),
        ]);
        emphasise(row, nodes_focused && i == state.cursor)
    });
    let node_widths = [
        Constraint::Percentage(46),
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Percentage(24),
    ];
    let nodes_title = if nodes_focused { "nodes ◀" } else { "nodes" };
    f.render_widget(
        Table::new(node_rows, node_widths)
            .block(Block::default().borders(Borders::ALL).title(nodes_title)),
        panels[1],
    );

    // --- Log stats panel ---
    f.render_widget(
        Paragraph::new(log_stats_lines(report))
            .block(Block::default().borders(Borders::ALL).title("log summary")),
        panels[2],
    );
}

/// Compact age like the plain renderer: 45s / 12m / 3h / 2d.
fn fmt_age(secs: i64) -> String {
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn pod_cpu(p: &crate::kube::PodSummary) -> String {
    let used = p.cpu_used_milli.map(crate::kube::format_cpu).unwrap_or_else(|| "·".into());
    let limit = p.cpu_limit_milli.map(crate::kube::format_cpu).unwrap_or_else(|| "·".into());
    format!("{used}/{limit}")
}

fn pod_mem(p: &crate::kube::PodSummary) -> String {
    let used = p.mem_used_bytes.map(crate::view::fmt_bytes).unwrap_or_else(|| "·".into());
    let limit = p.mem_limit_bytes.map(crate::view::fmt_bytes).unwrap_or_else(|| "·".into());
    format!("{used}/{limit}")
}

/// Lines for the log-stats panel, from the aggregated stats.
fn log_stats_lines(report: &crate::kube::KubeReport) -> Vec<Line<'static>> {
    let Some(stats) = &report.log_stats else {
        return vec![Line::from("no logs scanned (set --kube-log-tail > 0)")];
    };
    let mut lines: Vec<Line> = Vec::new();
    let alert = |text: String| {
        Line::from(Span::styled(
            text,
            Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    };
    if stats.transform_errors > 0 {
        lines.push(alert(format!(
            "⚠ TRANSFORM/DLQ ERRORS: {} — rows dropped to DLQ (silent data loss)",
            stats.transform_errors
        )));
    }
    if stats.oom_warnings > 0 {
        lines.push(alert(format!(
            "⚠ MEMORY PRESSURE: {} pre-OOM warning(s) — OOM kill imminent",
            stats.oom_warnings
        )));
    }
    if stats.throughput_collapsed {
        lines.push(alert("⚠ THROUGHPUT COLLAPSE: a pod was processing, now at 0 rps".to_string()));
    }
    if crate::kube::is_reconnect_storm(stats.reconnects) {
        lines.push(alert(format!("⚠ RECONNECT STORM: {} reconnects", stats.reconnects)));
    }
    if stats.backpressure > 0 {
        lines.push(alert(format!(
            "⚠ BACKPRESSURE: {} throttle/channel-full signal(s)",
            stats.backpressure
        )));
    }
    if !stats.by_level.is_empty() {
        let levels: Vec<String> = stats.by_level.iter().map(|(l, c)| format!("{l} {c}")).collect();
        lines.push(Line::from(format!("levels: {}", levels.join("  "))));
    }
    if let (Some(first), Some(last)) = (stats.rss_first_mb, stats.rss_last_mb) {
        let arrow = if last > first { "↑" } else if last < first { "↓" } else { "→" };
        let style = if last > first {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!("rss: {first} MB {arrow} {last} MB"), style)));
    }
    if let Some(rps) = stats.last_throughput_rps {
        lines.push(Line::from(format!("throughput: {rps} rps")));
    }
    for (label, count) in &stats.operational {
        lines.push(Line::from(format!("{label}: {count}")));
    }
    for (msg, count) in stats.top_messages.iter().take(4) {
        lines.push(Line::from(format!("{count}× {msg}")));
    }

    lines
}

/// Pod-detail view: resource breakdown, this pod's log stats, and its raw logs.
fn draw_pod_detail(f: &mut ratatui::Frame, area: Rect, state: &ViewState, frame: &Frame) {
    let Some(report) = &frame.kube else { return };
    let Some(name) = &state.selected_pod else { return };
    let pod = report.pods.iter().find(|p| &p.name == name);
    let logs = report.pod_logs.iter().find(|l| &l.pod == name);

    let halves = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(4)])
        .split(area);

    // Top: pod facts.
    let mut info: Vec<Line> = Vec::new();
    if let Some(p) = pod {
        info.push(Line::from(format!("pod:   {}", p.name)));
        info.push(Line::from(format!("node:  {}", p.node.as_deref().unwrap_or("—"))));
        info.push(Line::from(format!("ready: {}/{}   restarts: {}", p.ready, p.total_containers, p.restarts)));
        info.push(Line::from(format!("cpu:   {}", pod_cpu(p))));
        info.push(Line::from(format!("mem:   {}", pod_mem(p))));
        if p.oom_killed {
            info.push(Line::from(Span::styled("OOMKilled", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))));
        }
    }
    f.render_widget(
        Paragraph::new(info).block(Block::default().borders(Borders::ALL).title(format!("pod · {}", short_topic(name)))),
        halves[0],
    );

    // Bottom: metrics (if toggled with `m`), else logs.
    if state.pod_detail_metrics {
        let summary = report.pod_metric_summaries.iter().find(|s| &s.pod == name);
        let mut lines: Vec<Line> = Vec::new();
        if let Some(s) = summary {
            if let Some(h) = &s.health {
                lines.push(Line::from(format!("health: {h}")));
                lines.push(Line::from(""));
            }
            for cat in ["consumer", "throughput", "bottleneck", "health"] {
                let cat_lines: Vec<&crate::kube::MetricLine> =
                    s.lines.iter().filter(|l| l.category == cat).collect();
                if cat_lines.is_empty() {
                    continue;
                }
                lines.push(Line::from(Span::styled(
                    format!("── {cat} ──"),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                for ml in cat_lines {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(metric_line_spans(ml));
                    lines.push(Line::from(spans));
                }
            }
        } else {
            lines.push(Line::from("no metrics for this pod (scraping off or not yet scraped)"));
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("metrics (m logs · grouped)"),
            ),
            halves[1],
        );
    } else if state.log_expanded {
        // Pretty-print the selected line, full and (optionally) wrapped.
        let selected = logs
            .and_then(|l| l.lines.get(state.log_cursor))
            .map(|s| s.as_str())
            .unwrap_or("");
        let pretty = crate::kube::pretty_log_line(selected);
        let detail: Vec<Line> = pretty.into_iter().map(Line::from).collect();
        let mut para = Paragraph::new(detail).block(
            Block::default()
                .borders(Borders::ALL)
                .title("log entry (Esc collapse · w wrap)"),
        );
        if state.log_wrap {
            para = para.wrap(ratatui::widgets::Wrap { trim: false });
        }
        f.render_widget(para, halves[1]);
    } else {
        // Compact one-line-per-entry list with the cursor highlighted. Width
        // caps the summary so it never overflows the pane.
        let width = halves[1].width.saturating_sub(2) as usize;
        let rows: Vec<Line> = logs
            .map(|l| {
                l.lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| {
                        let text = crate::kube::log_line_summary(line, width);
                        if i == state.log_cursor {
                            Line::from(Span::styled(
                                text,
                                Style::default().add_modifier(Modifier::REVERSED),
                            ))
                        } else {
                            Line::from(text)
                        }
                    })
                    .collect()
            })
            .unwrap_or_else(|| vec![Line::from("no logs captured for this pod")]);
        // Scroll so the cursor stays visible: keep it near the middle.
        let visible = halves[1].height.saturating_sub(2) as usize;
        let scroll = state.log_cursor.saturating_sub(visible / 2) as u16;
        f.render_widget(
            Paragraph::new(rows)
                .block(Block::default().borders(Borders::ALL).title("logs (↑/↓ select · Enter expand)"))
                .scroll((scroll, 0)),
            halves[1],
        );
    }
}

/// Node-detail view: node capacity + which pods run on it (node-scoped only).
fn draw_node_detail(f: &mut ratatui::Frame, area: Rect, state: &ViewState, frame: &Frame) {
    let Some(report) = &frame.kube else { return };
    let Some(name) = &state.selected_node else { return };
    let node = report.nodes.iter().find(|n| &n.name == name);

    let mut lines: Vec<Line> = Vec::new();
    if let Some(n) = node {
        lines.push(Line::from(format!("node:  {}", n.name)));
        if let Some(inst) = &n.instance_type {
            lines.push(Line::from(format!("type:  {inst}")));
        }
        let cpu = n.alloc_cpu_milli.map(crate::kube::format_cpu).unwrap_or_else(|| "?".into());
        let mem = n.alloc_mem_bytes.map(crate::view::fmt_bytes).unwrap_or_else(|| "?".into());
        lines.push(Line::from(format!("alloc: {cpu} CPU / {mem}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("pods on this node:", Style::default().add_modifier(Modifier::BOLD))));
    for p in report.pods.iter().filter(|p| p.node.as_deref() == Some(name.as_str())) {
        lines.push(Line::from(format!("  {}   {}   mem {}", p.name, pod_cpu(p), pod_mem(p))));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(format!("node · {}", name))),
        area,
    );
}

fn draw_combined(f: &mut ratatui::Frame, area: Rect, state: &ViewState, frame: &Frame) {
    let halves = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_topics(f, halves[0], state, frame);

    // Lower pane: partitions of the drilled-into topic, or a hint if none.
    match &state.drilled_topic {
        Some(topic) => {
            let rows = partition_rows_for_topic(&frame.topics, topic);
            let title = format!("partitions · {}", short_topic(topic));
            draw_partition_rows(f, halves[1], &rows, None, &title);
        }
        None => {
            f.render_widget(
                Paragraph::new(" press Enter on a topic above to drill into its partitions")
                    .block(Block::default().borders(Borders::ALL).title("partitions")),
                halves[1],
            );
        }
    }
}

fn emphasise(row: Row, on: bool) -> Row {
    if on {
        row.style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
    } else {
        row
    }
}

fn short_topic(full: &str) -> String {
    full.rsplit('/').next().unwrap_or(full).to_string()
}

/// Render an optional count: the number, or `—` when unmeasurable.
fn opt_num(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "—".to_string())
}

/// Render optional bytes: formatted size, or `—` when unmeasurable.
fn opt_bytes(v: Option<i64>) -> String {
    v.map(crate::view::fmt_bytes).unwrap_or_else(|| "—".to_string())
}

fn net_str(t: &TopicHealth) -> String {
    t.drain
        .as_ref()
        .map(|d| format!("{:+.1}", d.net_per_sec))
        .unwrap_or_else(|| "—".to_string())
}

fn topic_detail(t: &TopicHealth) -> String {
    let mut parts = Vec::new();
    if !t.hot_partitions.is_empty() {
        parts.push(format!("{} hot", t.hot_partitions.len()));
    }
    if !t.partition_gaps.is_empty() {
        parts.push(format!("{} gap", t.partition_gaps.len()));
    }
    if let Some(h) = &t.kube_hint {
        parts.push(h.clone());
    }
    parts.join(", ")
}
