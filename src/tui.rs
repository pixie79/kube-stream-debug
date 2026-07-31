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
//!   v             cycle view (topic → partition → kube → combined)
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
    partition_rows, partition_rows_for_topic, partition_status_legend, status_legend,
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
/// checks). Kept as a closure so the TUI doesn't depend on main's wiring.
pub type Refresh<'a> = dyn FnMut() -> Frame + 'a;

/// Run the interactive TUI until the user quits. `interval` is the auto-refresh
/// cadence.
pub fn run(mut refresh: Box<Refresh<'_>>, interval: Duration) -> std::io::Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, refresh.as_mut(), interval);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    refresh: &mut Refresh<'_>,
    interval: Duration,
) -> std::io::Result<()> {
    let mut state = ViewState::default();
    let mut frame = refresh();
    let mut last_refresh = Instant::now();

    loop {
        terminal.draw(|f| draw(f, &state, &frame))?;

        // Wait for input up to the remaining interval; refresh when it elapses.
        let elapsed = last_refresh.elapsed();
        let wait = interval.saturating_sub(elapsed);
        if event::poll(wait)? {
            if let Event::Key(key) = event::read()? {
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

                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('?') => state.toggle_help(),
                    KeyCode::Char('v') => state.cycle_view(),
                    KeyCode::Tab if state.view == View::Kube => state.toggle_kube_focus(),
                    KeyCode::Up | KeyCode::Char('k') => state.cursor_up(),
                    KeyCode::Down | KeyCode::Char('j') => state.cursor_down(cursor_len),
                    KeyCode::Enter => match state.view {
                        View::Kube => state.kube_drill_in(&pod_names, &node_names),
                        View::PodDetail | View::NodeDetail => {} // already in detail
                        _ => state.drill_in(&frame.topics),
                    },
                    KeyCode::Esc => state.drill_out(),
                    KeyCode::Char('r') => {
                        frame = refresh();
                        state.clamp_cursor(frame.topics.len());
                        last_refresh = Instant::now();
                    }
                    _ => {}
                }
            }
        }

        if last_refresh.elapsed() >= interval {
            frame = refresh();
            state.clamp_cursor(frame.topics.len());
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn draw(f: &mut ratatui::Frame, state: &ViewState, frame: &Frame) {
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
        View::Partition => draw_partitions(f, chunks[1], state, frame),
        View::Kube => draw_kube(f, chunks[1], state, frame),
        View::Combined => draw_combined(f, chunks[1], state, frame),
        View::PodDetail => draw_pod_detail(f, chunks[1], state, frame),
        View::NodeDetail => draw_node_detail(f, chunks[1], state, frame),
    }

    let help = match state.view {
        View::Kube => " ↑/↓=move  Tab=pods/nodes  Enter=open  Esc=back  v=view  ?=legend  q=quit",
        View::PodDetail | View::NodeDetail => " Esc=back  ?=legend  r=refresh  q=quit",
        _ => " ↑/↓=move  Enter=drill in  Esc=back  v=view  ?=legend  r=refresh  q=quit",
    };
    f.render_widget(Paragraph::new(help), chunks[2]);

    // Help overlay draws on top of everything else.
    if state.show_help {
        draw_help_overlay(f);
    }
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
    let header = Row::new(vec!["TOPIC", "STATUS", "BACKLOG", "SIZE", "CONS", "NET/s", "DETAIL"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = frame.topics.iter().enumerate().map(|(i, t)| {
        let row = Row::new(vec![
            Cell::from(short_topic(&t.topic)),
            status_cell(t.status.label()),
            Cell::from(opt_num(t.total_backlog)),
            Cell::from(opt_bytes(t.backlog_bytes)),
            Cell::from(opt_num(t.consumers)),
            Cell::from(net_str(t)),
            Cell::from(topic_detail(t)),
        ]);
        emphasise(row, i == state.cursor)
    });
    let widths = [
        Constraint::Percentage(28),
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(11),
        Constraint::Length(5),
        Constraint::Length(8),
        Constraint::Percentage(30),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("topics"));
    f.render_widget(table, area);
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

fn draw_partitions(f: &mut ratatui::Frame, area: Rect, state: &ViewState, frame: &Frame) {
    let all = partition_rows(&frame.topics);
    draw_partition_rows(f, area, &all, Some(state.cursor), "partitions");
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
        let state_txt = if p.oom_killed {
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

    // Bottom: raw logs, scrolled to the cursor.
    let log_lines: Vec<Line> = logs
        .map(|l| l.lines.iter().map(|s| Line::from(s.clone())).collect())
        .unwrap_or_else(|| vec![Line::from("no logs captured for this pod")]);
    f.render_widget(
        Paragraph::new(log_lines)
            .block(Block::default().borders(Borders::ALL).title("logs (↑/↓ scroll)"))
            .scroll((state.cursor as u16, 0)),
        halves[1],
    );
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
