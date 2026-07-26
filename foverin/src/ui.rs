//! Matrix-style Ratatui dashboard for Foverin CLI clients.

use std::{
    io,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use foverin_common::StateSnapshot;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
};

const FRAME_BUDGET: Duration = Duration::from_millis(33); // ~30 FPS
const GREEN: Color = Color::Green;
const DIM_GREEN: Color = Color::Rgb(0, 120, 0);
const BRIGHT: Color = Color::Rgb(80, 255, 80);
const FATAL_RED: Color = Color::Rgb(220, 40, 40);

/// Run the TUI until `q` / `Esc` / `Ctrl+C`. Restores the terminal on exit.
pub fn run(state: Arc<Mutex<StateSnapshot>>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = ui_loop(&mut terminal, state);
    ratatui::restore();
    result
}

/// Full-screen fatal banner when the daemon UDS is unreachable.
pub fn run_fatal_unreachable() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let result = fatal_loop(&mut terminal);
    ratatui::restore();
    result
}

fn fatal_loop(terminal: &mut DefaultTerminal) -> io::Result<()> {
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(FATAL_RED))
                .style(Style::default().bg(Color::Black));
            let inner = block.inner(area);
            frame.render_widget(block, area);

            let msg = Paragraph::new(Line::from(Span::styled(
                "[ FATAL ]: FOVERIN DAEMON NOT REACHABLE",
                Style::default().fg(FATAL_RED).add_modifier(Modifier::BOLD),
            )))
            .centered();
            frame.render_widget(msg, inner);
        })?;

        if event::poll(FRAME_BUDGET)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        break;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    Ok(())
}

fn ui_loop(terminal: &mut DefaultTerminal, state: Arc<Mutex<StateSnapshot>>) -> io::Result<()> {
    loop {
        let frame_start = Instant::now();

        terminal.draw(|frame| {
            let snap = state.lock().unwrap_or_else(|e| e.into_inner());
            draw(frame, &snap);
        })?;

        // Drain input within the remaining frame budget.
        let deadline = FRAME_BUDGET.saturating_sub(frame_start.elapsed());
        if event::poll(deadline)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                        break;
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        // Pace to ~30 FPS if the poll returned early.
        let spent = frame_start.elapsed();
        if spent < FRAME_BUDGET {
            std::thread::sleep(FRAME_BUDGET - spent);
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame<'_>, state: &StateSnapshot) {
    let root = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .title(Line::from(vec![
            Span::styled("[ FOVERIN ]", Style::default().fg(BRIGHT).bold()),
            Span::styled("// MATRIX UPLINK ", Style::default().fg(DIM_GREEN)),
            Span::styled("[q] quit", Style::default().fg(DIM_GREEN)),
        ]))
        .style(Style::default().bg(Color::Black).fg(GREEN));
    let inner = root.inner(frame.area());
    frame.render_widget(root, frame.area());

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(inner);

    draw_stream(frame, cols[0], state);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(cols[1]);

    draw_inference(frame, right[0], state);
    draw_actuator(frame, right[1], state);
}

fn draw_stream(frame: &mut Frame<'_>, area: Rect, state: &StateSnapshot) {
    let items: Vec<ListItem> = state
        .process_stream
        .iter()
        .rev()
        .map(|line| {
            ListItem::new(Line::from(Span::styled(
                format!(" ▸ {line}"),
                Style::default().fg(GREEN),
            )))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(GREEN))
            .title(Span::styled(
                " eBPF SENSOR STREAM ",
                Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(Color::Black)),
    );
    frame.render_widget(list, area);
}

fn draw_inference(frame: &mut Frame<'_>, area: Rect, state: &StateSnapshot) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .title(Span::styled(
            " MEMORY POLICY ",
            Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    let workload = state.workload.as_deref().unwrap_or("—");
    let confidence = state.confidence;
    let latency = state.latency_us;

    let workload_line = Paragraph::new(Line::from(vec![
        Span::styled("WORKLOAD  ", Style::default().fg(DIM_GREEN)),
        Span::styled(
            workload,
            Style::default()
                .fg(BRIGHT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
    ]));
    frame.render_widget(workload_line, chunks[0]);

    let latency_line = Paragraph::new(Line::from(vec![
        Span::styled("LATENCY   ", Style::default().fg(DIM_GREEN)),
        Span::styled(format!("{latency} µs"), Style::default().fg(GREEN)),
    ]));
    frame.render_widget(latency_line, chunks[1]);

    let ratio = (confidence / 100.0).clamp(0.0, 1.0) as f64;
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::NONE)
                .title(Span::styled("CONFIDENCE", Style::default().fg(DIM_GREEN))),
        )
        .gauge_style(Style::default().fg(GREEN).bg(Color::Rgb(0, 40, 0)))
        .ratio(ratio)
        .label(Span::styled(
            format!("{confidence:5.1}%"),
            Style::default().fg(BRIGHT).bold(),
        ));
    frame.render_widget(gauge, chunks[2]);

    // Textual bar matching the brief: [████████░░] 85%
    let filled = ((confidence / 10.0).round() as usize).min(10);
    let bar: String = std::iter::repeat_n('█', filled)
        .chain(std::iter::repeat_n('░', 10 - filled))
        .collect();
    let bar_line = Paragraph::new(Line::from(Span::styled(
        format!("[{bar}] {confidence:5.1}%"),
        Style::default().fg(GREEN),
    )));
    frame.render_widget(bar_line, chunks[3]);
}

fn draw_actuator(frame: &mut Frame<'_>, area: Rect, state: &StateSnapshot) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(GREEN))
        .title(Span::styled(
            " SYSFS ACTUATOR ",
            Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(Color::Black));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let gov = state.active_governor.to_ascii_uppercase();
    let is_perf = gov == "PERFORMANCE";
    let label = format!("[ {gov} ]");
    let style = if is_perf {
        Style::default().fg(BRIGHT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM_GREEN)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "CPU SCALING GOVERNOR",
            Style::default().fg(DIM_GREEN),
        ))),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(label, style))).centered(),
        chunks[1],
    );

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            &state.status,
            Style::default().fg(DIM_GREEN),
        ))),
        chunks[2],
    );
}
