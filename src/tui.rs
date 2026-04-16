//! ratatui-based interactive TUI.
//!
//! State machine:
//!
//!   Scanning ──► Review ──► Reclaiming ──► Done
//!                                   └──► (errors shown inline)

use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crossbeam_channel::Receiver;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::cursor::Show as ShowCursor;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, ExecutableCommand};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::{Frame, Terminal};

use crate::types::{FileGroup, ReclaimMessage, ScanMessage, ScanPhase};
use crate::{clone, scanner};

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

enum Screen {
    Scanning(ScanningState),
    Review(ReviewState),
    Reclaiming(ReclaimingState),
    Done(DoneState),
    FatalError(String),
}

struct ScanningState {
    paths: Vec<PathBuf>,
    phase: ScanPhase,
    phase_done: u64,
    phase_total: u64,
    files_found: u64,
    rx: Receiver<ScanMessage>,
}

struct ReviewState {
    groups: Vec<FileGroup>,
    list_state: ListState,
    #[allow(dead_code)]
    errors: Vec<String>,
    dry_run: bool,
}

impl ReviewState {
    fn selected_savings(&self) -> u64 {
        self.groups
            .iter()
            .filter(|g| g.selected)
            .map(|g| g.savings())
            .sum()
    }
    #[allow(dead_code)]
    fn total_savings(&self) -> u64 {
        self.groups.iter().map(|g| g.savings()).sum()
    }
    fn selected_idx(&self) -> Option<usize> {
        self.list_state.selected()
    }
    fn move_up(&mut self) {
        let i = match self.list_state.selected() {
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.list_state.select(Some(i));
    }
    fn move_down(&mut self) {
        let last = self.groups.len().saturating_sub(1);
        let i = match self.list_state.selected() {
            Some(i) => (i + 1).min(last),
            None => 0,
        };
        self.list_state.select(Some(i));
    }
    fn toggle_selected(&mut self) {
        if let Some(i) = self.list_state.selected() {
            if i < self.groups.len() {
                self.groups[i].selected = !self.groups[i].selected;
            }
        }
    }
    fn select_all(&mut self, value: bool) {
        for g in &mut self.groups {
            g.selected = value;
        }
    }
}

struct ReclaimingState {
    done: u64,
    total: u64,
    reclaimed: u64,
    errors: Vec<String>,
    rx: Receiver<ReclaimMessage>,
    dry_run: bool,
}

struct DoneState {
    total_reclaimed: u64,
    groups_done: u64,
    #[allow(dead_code)]
    files_done: u64,
    dry_run: bool,
    errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run(paths: Vec<PathBuf>, dry_run: bool, min_size: u64, scan_library: bool) -> anyhow::Result<()> {
    // Ensure the terminal is restored before any panic message is printed.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, ShowCursor);
        default_hook(info);
    }));

    let (scan_tx, scan_rx) = crossbeam_channel::unbounded::<ScanMessage>();

    {
        let paths = paths.clone();
        thread::spawn(move || scanner::scan(paths, min_size, scan_library, scan_tx));
    }

    let mut screen = Screen::Scanning(ScanningState {
        paths: paths.clone(),
        phase: ScanPhase::Walking,
        phase_done: 0,
        phase_total: 0,
        files_found: 0,
        rx: scan_rx,
    });

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut screen, dry_run);

    // Restore terminal — use let _ so cleanup always runs even if event_loop errored.
    let _ = terminal::disable_raw_mode();
    let _ = terminal.backend_mut().execute(LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    result
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

fn event_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
    screen: &mut Screen,
    dry_run: bool,
) -> anyhow::Result<()> {
    let mut frame_idx: u64 = 0;

    loop {
        terminal.draw(|f| render(f, screen, frame_idx))?;
        frame_idx = frame_idx.wrapping_add(1);

        // Drain all pending background messages (non-blocking)
        process_messages(screen, dry_run);

        // Poll for keyboard input with a short timeout so progress redraws smoothly
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                // Ctrl-C / q always quits
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return Ok(());
                }

                match screen {
                    Screen::Scanning(_) => {
                        if key.code == KeyCode::Char('q') {
                            return Ok(());
                        }
                    }
                    Screen::Review(state) => match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Up | KeyCode::Char('k') => state.move_up(),
                        KeyCode::Down | KeyCode::Char('j') => state.move_down(),
                        KeyCode::Char(' ') => state.toggle_selected(),
                        KeyCode::Char('a') => {
                            let all_on = state.groups.iter().all(|g| g.selected);
                            state.select_all(!all_on);
                        }
                        KeyCode::Char('r') | KeyCode::Enter => {
                            start_reclaim(screen, dry_run);
                        }
                        _ => {}
                    },
                    Screen::Reclaiming(_) => {
                        if key.code == KeyCode::Char('q') {
                            return Ok(());
                        }
                    }
                    Screen::Done(_) | Screen::FatalError(_) => {
                        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter) {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Background message processing
// ---------------------------------------------------------------------------

fn process_messages(screen: &mut Screen, dry_run: bool) {
    match screen {
        Screen::Scanning(state) => {
            // Drain all available messages
            loop {
                match state.rx.try_recv() {
                    Ok(msg) => match msg {
                        ScanMessage::Phase(p) => {
                            state.phase = p;
                            state.phase_done = 0;
                            state.phase_total = 0;
                        }
                        ScanMessage::Progress { done, total } => {
                            state.phase_done = done;
                            if total > 0 {
                                state.phase_total = total;
                            }
                            if matches!(state.phase, ScanPhase::Walking) {
                                state.files_found = done;
                            }
                        }
                        ScanMessage::Done(groups) => {
                            let mut ls = ListState::default();
                            if !groups.is_empty() {
                                ls.select(Some(0));
                            }
                            *screen = Screen::Review(ReviewState {
                                groups,
                                list_state: ls,
                                errors: vec![],
                                dry_run,
                            });
                            return;
                        }
                        ScanMessage::Error(e) => {
                            *screen = Screen::FatalError(e);
                            return;
                        }
                    },
                    Err(_) => break,
                }
            }
        }
        Screen::Reclaiming(state) => {
            loop {
                match state.rx.try_recv() {
                    Ok(msg) => match msg {
                        ReclaimMessage::Progress { done, total, reclaimed } => {
                            state.done = done;
                            state.total = total;
                            state.reclaimed = reclaimed;
                        }
                        ReclaimMessage::Done { total_reclaimed } => {
                            let errors = state.errors.clone();
                            let dry_run = state.dry_run;
                            let groups_done = state.done;
                            let files_done = state.done; // rough approximation
                            *screen = Screen::Done(DoneState {
                                total_reclaimed,
                                groups_done,
                                files_done,
                                dry_run,
                                errors,
                            });
                            return;
                        }
                        ReclaimMessage::Error(e) => {
                            state.errors.push(e);
                        }
                    },
                    Err(_) => break,
                }
            }
        }
        _ => {}
    }
}

fn start_reclaim(screen: &mut Screen, dry_run: bool) {
    let groups = match screen {
        Screen::Review(s) => s.groups.clone(),
        _ => return,
    };

    let total = groups.iter().filter(|g| g.selected).count() as u64;
    let (tx, rx) = crossbeam_channel::unbounded::<ReclaimMessage>();

    thread::spawn(move || clone::reclaim_groups(&groups, dry_run, tx));

    *screen = Screen::Reclaiming(ReclaimingState {
        done: 0,
        total,
        reclaimed: 0,
        errors: vec![],
        rx,
        dry_run,
    });
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(frame: &mut Frame, screen: &Screen, frame_idx: u64) {
    match screen {
        Screen::Scanning(s) => render_scanning(frame, s, frame_idx),
        Screen::Review(s) => render_review(frame, s),
        Screen::Reclaiming(s) => render_reclaiming(frame, s),
        Screen::Done(s) => render_done(frame, s),
        Screen::FatalError(e) => render_error(frame, e),
    }
}

// ── Scanning ────────────────────────────────────────────────────────────────

fn render_scanning(frame: &mut Frame, state: &ScanningState, frame_idx: u64) {
    let area = frame.area();
    let block = Block::default()
        .title(" MacDeDup — Scanning ")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // paths
            Constraint::Length(1), // blank
            Constraint::Length(1), // phase label
            Constraint::Length(3), // progress bar
            Constraint::Length(1), // blank
            Constraint::Min(0),    // stats
        ])
        .split(inner);

    // Paths
    let path_str = state
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    frame.render_widget(
        Paragraph::new(format!("Path: {path_str}"))
            .style(Style::default().fg(Color::White)),
        chunks[0],
    );

    // Phase label with spinner
    let spinners = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spin = spinners[(frame_idx as usize / 2) % spinners.len()];
    frame.render_widget(
        Paragraph::new(format!("{spin}  {}", state.phase.label()))
            .style(Style::default().fg(Color::Yellow)),
        chunks[2],
    );

    // Progress bar
    let pct = if state.phase_total > 0 {
        ((state.phase_done as f64 / state.phase_total as f64) * 100.0) as u16
    } else {
        0
    };
    let label = if state.phase_total > 0 {
        format!(
            "{} / {}",
            fmt_count(state.phase_done),
            fmt_count(state.phase_total)
        )
    } else {
        fmt_count(state.phase_done)
    };
    frame.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL))
            .gauge_style(Style::default().fg(Color::Green))
            .percent(pct.min(100))
            .label(label),
        chunks[3],
    );

    // Stats
    frame.render_widget(
        Paragraph::new(format!("Files found: {}", fmt_count(state.files_found)))
            .style(Style::default().fg(Color::Gray)),
        chunks[5],
    );
}

// ── Review ──────────────────────────────────────────────────────────────────

fn render_review(frame: &mut Frame, state: &ReviewState) {
    let area = frame.area();

    let total_label = if state.dry_run { " [DRY RUN]" } else { "" };
    let block = Block::default()
        .title(format!(
            " MacDeDup — Review Duplicates{}  ({} groups, save {})",
            total_label,
            state.groups.len(),
            bytesize::ByteSize(state.selected_savings())
        ))
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split: left list | right detail
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    // Footer hint at bottom of left pane
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(h[0]);

    let items: Vec<ListItem> = state
        .groups
        .iter()
        .map(|g| {
            let check = if g.selected { "✓" } else { " " };
            let saves = bytesize::ByteSize(g.savings());
            let name = g.files[0]
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            ListItem::new(format!(
                "[{}] {:40} ×{}  {}",
                check,
                truncate(&name, 40),
                g.files.len(),
                saves,
            ))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::RIGHT))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    frame.render_stateful_widget(list, left[0], &mut state.list_state.clone());

    frame.render_widget(
        Paragraph::new(
            "↑↓/j/k: navigate  Space: toggle  A: all  R/Enter: reclaim  Q: quit",
        )
        .style(Style::default().fg(Color::DarkGray)),
        left[1],
    );

    // Detail pane (right side)
    render_group_detail(frame, state, h[1]);
}

fn render_group_detail(frame: &mut Frame, state: &ReviewState, area: Rect) {
    let Some(idx) = state.selected_idx() else {
        return;
    };
    let Some(group) = state.groups.get(idx) else {
        return;
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("Size: ", Style::default().fg(Color::Gray)),
        Span::styled(
            bytesize::ByteSize(group.size).to_string(),
            Style::default().fg(Color::White),
        ),
        Span::raw("   "),
        Span::styled("Saves: ", Style::default().fg(Color::Gray)),
        Span::styled(
            bytesize::ByteSize(group.savings()).to_string(),
            Style::default().fg(Color::Green),
        ),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Source (kept):",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("  {}", group.files[0].path.display()),
        Style::default().fg(Color::White),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!("Targets ({}):", group.files.len() - 1),
        Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD),
    )));
    for f in &group.files[1..] {
        lines.push(Line::from(Span::styled(
            format!("  {}", f.path.display()),
            Style::default().fg(Color::Gray),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Group detail ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

// ── Reclaiming ──────────────────────────────────────────────────────────────

fn render_reclaiming(frame: &mut Frame, state: &ReclaimingState) {
    let area = frame.area();
    let label = if state.dry_run { " [DRY RUN]" } else { "" };
    let block = Block::default()
        .title(format!(" MacDeDup — Reclaiming Space{label} "))
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3), // gauge
            Constraint::Length(1), // stats
            Constraint::Min(0),    // errors
        ])
        .split(inner);

    let pct = if state.total > 0 {
        ((state.done as f64 / state.total as f64) * 100.0) as u16
    } else {
        0
    };
    frame.render_widget(
        Gauge::default()
            .block(Block::default().borders(Borders::ALL))
            .gauge_style(Style::default().fg(Color::Green))
            .percent(pct.min(100))
            .label(format!("{}/{} groups", state.done, state.total)),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(format!(
            "Space reclaimed: {}",
            bytesize::ByteSize(state.reclaimed)
        ))
        .style(Style::default().fg(Color::Green)),
        chunks[1],
    );

    if !state.errors.is_empty() {
        let err_lines: Vec<Line> = state
            .errors
            .iter()
            .map(|e| Line::from(Span::styled(e.as_str(), Style::default().fg(Color::Red))))
            .collect();
        frame.render_widget(
            Paragraph::new(err_lines)
                .block(Block::default().title("Errors").borders(Borders::ALL))
                .wrap(Wrap { trim: true }),
            chunks[2],
        );
    }
}

// ── Done ────────────────────────────────────────────────────────────────────

fn render_done(frame: &mut Frame, state: &DoneState) {
    let area = frame.area();
    let label = if state.dry_run { " [DRY RUN]" } else { "" };
    let block = Block::default()
        .title(format!(" MacDeDup — Complete!{label} "))
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Green));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let verb = if state.dry_run { "Would reclaim" } else { "Reclaimed" };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            format!(
                "  ✓ {verb}: {}",
                bytesize::ByteSize(state.total_reclaimed)
            ),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("    {} groups processed", state.groups_done),
            Style::default().fg(Color::White),
        )),
        Line::raw(""),
    ];

    let mut all_lines = lines;
    if !state.errors.is_empty() {
        all_lines.push(Line::from(Span::styled(
            format!("  {} errors occurred (see above)", state.errors.len()),
            Style::default().fg(Color::Yellow),
        )));
    }
    all_lines.push(Line::raw(""));
    all_lines.push(Line::from(Span::styled(
        "  Press Q or Enter to exit",
        Style::default().fg(Color::DarkGray),
    )));

    frame.render_widget(Paragraph::new(all_lines), inner);
}

// ── Error ────────────────────────────────────────────────────────────────────

fn render_error(frame: &mut Frame, msg: &str) {
    let area = frame.area();
    let block = Block::default()
        .title(" MacDeDup — Error ")
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Red));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(format!("{msg}\n\nPress Q to exit."))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.char_indices();
    match chars.nth(max.saturating_sub(1)) {
        None => s.to_string(), // fewer than max chars — fits as-is
        Some((_, _)) => match chars.next() {
            None => s.to_string(), // exactly max chars — fits as-is
            Some((byte_pos, _)) => format!("{}…", &s[..byte_pos]),
        },
    }
}
