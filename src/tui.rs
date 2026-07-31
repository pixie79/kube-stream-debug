//! Interactive terminal UI (ratatui + crossterm), compiled into every build.
//!
//! The pure view-state and selection logic it drives (in `crate::view`) is
//! unit-tested; this file is the terminal shell around it.
//!
//! Keybindings:
//!   ↑/↓ (or k/j)  move the cursor between topics
//!   Enter         drill into the selected topic (combined view, its partitions
//!                 in the lower pane)
//!   Esc           back out of the drill-in to the topic view
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
    partition_rows, partition_rows_for_topic, status_legend, status_severity, trend_legend,
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
                let topic_count = frame.topics.len();
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('?') => state.toggle_help(),
                    KeyCode::Char('v') => state.cycle_view(),
                    KeyCode::Up | KeyCode::Char('k') => state.cursor_up(),
                    KeyCode::Down | KeyCode::Char('j') => state.cursor_down(topic_count),
                    KeyCode::Enter => state.drill_in(&frame.topics),
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
        View::Kube => draw_kube(f, chunks[1], frame),
        View::Combined => draw_combined(f, chunks[1], state, frame),
    }

    let help = " ↑/↓=move  Enter=drill in  Esc=back  v=view  ?=legend  r=refresh  q=quit";
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
            Cell::from(t.total_backlog.to_string()),
            Cell::from(crate::view::fmt_bytes(t.backlog_bytes)),
            Cell::from(t.consumers.to_string()),
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

fn draw_kube(f: &mut ratatui::Frame, area: Rect, frame: &Frame) {
    let text = match &frame.kube {
        None => "kube view: run with --kube (and a binary built with --features kube)".to_string(),
        Some(report) => crate::output::render_kube_section(report),
    };
    f.render_widget(
        Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("kubernetes")),
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
