//! Interactive terminal UI (ratatui + crossterm). Compiled only with the `tui`
//! feature.
//!
//! ## Compilation note
//!
//! This module was written against ratatui 0.29 / crossterm 0.28 but could not
//! be compiled in the environment where it was authored. The pure view-state
//! and selection logic it drives (in `crate::view`) is fully unit-tested; this
//! file is the terminal shell around it. If the first `--features tui` build
//! fails, likely spots: the `ratatui::init()` / `restore()` helpers (0.28+),
//! `Table`/`Row`/`Cell` construction, and the crossterm `event::read` enum
//! shapes. These are mechanical to fix and don't affect the tested logic.
//!
//! Keybindings:
//!   v            cycle view (topic → partition → kube → combined)
//!   /            edit the filter/select query (Enter to apply, Esc to cancel)
//!   f            toggle Filter vs Highlight mode
//!   c            clear the query
//!   r            refresh now
//!   q / Ctrl-C   quit

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::DefaultTerminal;

use crate::health::TopicHealth;
use crate::kube::KubeReport;
use crate::view::{
    is_highlighted, partition_rows, visible_partition_indices, visible_topic_indices, SelectMode,
    View, ViewState,
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
            match event::read()? {
                Event::Key(key) => {
                    if state.editing_query {
                        match key.code {
                            KeyCode::Enter | KeyCode::Esc => state.editing_query = false,
                            KeyCode::Backspace => {
                                state.query.pop();
                            }
                            KeyCode::Char(c) => state.query.push(c),
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Char('c')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                break
                            }
                            KeyCode::Char('v') => state.cycle_view(),
                            KeyCode::Char('/') => state.editing_query = true,
                            KeyCode::Char('f') => state.toggle_mode(),
                            KeyCode::Char('c') => state.clear_query(),
                            KeyCode::Char('r') => {
                                frame = refresh();
                                last_refresh = Instant::now();
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        if last_refresh.elapsed() >= interval {
            frame = refresh();
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

    let header = format!(
        " pulsar-topic-health · sub: {} · view: {} · mode: {} · query: {} · as of {}",
        short_topic(&frame.subscription),
        state.view.label(),
        match state.mode {
            SelectMode::Filter => "filter",
            SelectMode::Highlight => "highlight",
        },
        if state.query.is_empty() { "(none)" } else { &state.query },
        frame.run_at,
    );
    f.render_widget(Paragraph::new(header), chunks[0]);

    match state.view {
        View::Topic => draw_topics(f, chunks[1], state, frame),
        View::Partition => draw_partitions(f, chunks[1], state, frame),
        View::Kube => draw_kube(f, chunks[1], frame),
        View::Combined => draw_combined(f, chunks[1], state, frame),
    }

    let help = if state.editing_query {
        " typing filter… Enter=apply Esc=cancel"
    } else {
        " v=view /=filter f=mode c=clear r=refresh q=quit"
    };
    f.render_widget(Paragraph::new(help), chunks[2]);
}

fn draw_topics(f: &mut ratatui::Frame, area: Rect, state: &ViewState, frame: &Frame) {
    let visible = visible_topic_indices(state, &frame.topics);
    let header = Row::new(vec!["TOPIC", "STATUS", "BACKLOG", "SIZE", "CONS", "NET/s", "DETAIL"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = visible.iter().map(|&i| {
        let t = &frame.topics[i];
        let row = Row::new(vec![
            Cell::from(short_topic(&t.topic)),
            Cell::from(t.status.label()),
            Cell::from(t.total_backlog.to_string()),
            Cell::from(crate::view::fmt_bytes(t.backlog_bytes)),
            Cell::from(t.consumers.to_string()),
            Cell::from(net_str(t)),
            Cell::from(topic_detail(t)),
        ]);
        emphasise(row, is_highlighted(state, &t.topic))
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

fn draw_partitions(f: &mut ratatui::Frame, area: Rect, state: &ViewState, frame: &Frame) {
    let all = partition_rows(&frame.topics);
    let visible = visible_partition_indices(state, &all);
    let header = Row::new(vec!["TOPIC", "PART", "STATUS", "BACKLOG", "SIZE", "CONS", "UNACKED"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = visible.iter().map(|&i| {
        let p = &all[i];
        let row = Row::new(vec![
            Cell::from(short_topic(&p.topic)),
            Cell::from(format!("p{}", p.index)),
            Cell::from(p.status),
            Cell::from(p.backlog.to_string()),
            Cell::from(crate::view::fmt_bytes(p.backlog_bytes)),
            Cell::from(p.consumers.to_string()),
            Cell::from(p.unacked_messages.to_string()),
        ]);
        emphasise(row, is_highlighted(state, &p.partition))
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
        .block(Block::default().borders(Borders::ALL).title("partitions"));
    f.render_widget(table, area);
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
    draw_partitions(f, halves[1], state, frame);
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
