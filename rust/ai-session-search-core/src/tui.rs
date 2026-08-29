// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};

use crate::config::Config;
use crate::db::Db;
use crate::models::{Provider, SearchFilters, SessionRecord};
use crate::util::{
    current_repo, highlight_matches, prompt_confirm, relative_age, render_posix_shell_command,
    resume_plan, truncate_for_display,
};

/// Minimum number of sessions loaded into the TUI's in-memory browser.
///
/// The TUI currently browses one materialized result set rather than exposing keyset pages. A
/// small CLI search default would otherwise make navigation appear to stop after only a few
/// sessions. This private floor affects only how many rows the browser can navigate; it is not a
/// public search limit or a hidden cap, because larger configured defaults remain unchanged.
const TUI_MIN_BROWSER_RESULTS: usize = 100;

fn tui_result_limit(configured_default: usize) -> usize {
    configured_default.max(TUI_MIN_BROWSER_RESULTS)
}

/// Search box height including its border rows; the query line is the middle row.
const SEARCH_BOX_ROWS: u16 = 3;

/// Minimum height of the list/preview body. Below 14 total rows the vertical layout
/// underflows (recorded as D11); the step-5 resize test pins that boundary.
const MIN_BODY_ROWS: u16 = 10;

/// Status/help bar height, shared by both modes.
const STATUS_BAR_ROWS: u16 = 1;

/// Error line height when a keystroke error is being shown. It takes its row from the body,
/// never from the help bar, so REQ047's recovery guidance keeps its line.
const ERROR_LINE_ROWS: u16 = 1;

/// The crossterm event API is a set of free functions over a process-global source, so it
/// cannot be substituted in a test. This is the seam: production wraps those functions, tests
/// replay a script. Mirrors `crossterm::event::{poll, read}` (crossterm 0.29).
trait EventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

struct CrosstermEventSource;

impl EventSource for CrosstermEventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

/// RAII guard for the TUI's raw-mode + alternate-screen terminal session.
/// [`TerminalGuard::enter`] switches the terminal into TUI mode; [`Drop`] switches it
/// back on EVERY exit path — a normal return, an early `?` (e.g. `Terminal::new` fails),
/// or a panic inside the event loop — so a failure mid-TUI never leaves the user's
/// terminal corrupted (raw mode still on, stuck on the alternate screen, cursor hidden,
/// requiring a blind `reset`).
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(err) = execute!(stdout, EnterAlternateScreen) {
            // Raw mode is on but entering the alternate screen failed — undo raw mode so we
            // never return leaving the terminal half-configured.
            let _ = disable_raw_mode();
            return Err(err.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort teardown — ignore errors: we may be unwinding from a panic, and there
        // is nothing useful to do if restoration itself fails.
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

/// Browse and fuzzy-search session-level title/summary/preview/transcript records.
///
/// This TUI does not expose message-field exact/regex/fuzzy modes. Use `aise messages search` or
/// MCP `search_messages` for content, canonical tool-name, and tool-argument search; keeping that
/// boundary explicit avoids a second interactive message-search contract.
pub fn run(config: &Config, db: &Db) -> Result<()> {
    let _terminal_guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut events = CrosstermEventSource;
    let mut app = AppState::new(config, db)?;
    let action = run_app(&mut terminal, &mut events, &mut app);

    // Restore the terminal before the resume output/prompt below runs on the normal screen.
    // (The guard also restores on drop at end of scope — including the error/panic paths
    // above; this just sequences restoration ahead of the interactive resume.)
    drop(_terminal_guard);

    match action? {
        AppAction::Quit => Ok(()),
        AppAction::Resume(session) => {
            let (command, cwd) = resume_plan(&session)?;
            println!(
                "POSIX shell resume command: {}",
                render_posix_shell_command(&command)?
            );
            println!("{}", crate::util::RESUME_COMMAND_POLICY_NOTE);
            if let Some(cwd) = &cwd {
                println!("cwd: {cwd}");
            }
            if !prompt_confirm("Execute resume command?")? {
                println!("resume cancelled");
                return Ok(());
            }
            let mut process = std::process::Command::new(&command[0]);
            process.args(&command[1..]);
            if let Some(cwd) = cwd {
                process.current_dir(cwd);
            }
            let status = process.status()?;
            if !status.success() {
                anyhow::bail!("resume command failed with status {status}");
            }
            Ok(())
        }
    }
}

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    events: &mut dyn EventSource,
    app: &mut AppState<'_>,
) -> Result<AppAction>
where
    B::Error: Send + Sync + 'static,
{
    loop {
        if let Some(action) = step(terminal, events, app)? {
            return Ok(action);
        }
    }
}

/// One loop turn: absorb finished work, draw, then drain every pending input event. Split out
/// of `run_app` so tests can advance the loop a bounded number of turns and inspect the rendered
/// buffer between them.
///
/// Complexity (REQ010): `O(1)` plus one frame render and the events already queued. It performs
/// no database work and takes no lock. Draining in a `while` (not an `if`) means a pasted burst
/// of input costs one poll, not one poll per event — itself a latency fix, measured in the plan's
/// step-0 baseline.
fn step<B: Backend>(
    terminal: &mut Terminal<B>,
    events: &mut dyn EventSource,
    app: &mut AppState<'_>,
) -> Result<Option<AppAction>>
where
    B::Error: Send + Sync + 'static,
{
    app.drain_responses();
    terminal.draw(|frame| app.render(frame))?;
    let poll_interval = Duration::from_millis(app.config.ui.event_poll_interval_ms);
    while events.poll(poll_interval)? {
        let Event::Key(key) = events.read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(action) = app.handle_key(key) {
            return Ok(Some(action));
        }
    }
    Ok(None)
}

enum AppAction {
    Quit,
    // Boxed: SessionRecord is large; keep the enum small (clippy::large_enum_variant).
    Resume(Box<SessionRecord>),
}

struct AppState<'a> {
    config: &'a Config,
    /// Temporary: the key dispatch moved onto `AppState` and still calls
    /// `refresh`/`move_selection`/`select_index`, which need the handle. Step 4 moves that
    /// work onto the worker and deletes this field.
    db: &'a Db,
    query: String,
    search_mode: bool,
    selected: usize,
    results: Vec<SessionRecord>,
    preview: String,
    preview_scroll: u16,
    preview_line_count: usize,
    /// Last error from a keystroke-triggered operation, shown on its own line. A key press
    /// can never abort the TUI: errors land here instead of propagating through `?`.
    error: Option<String>,
}

impl<'a> AppState<'a> {
    fn new(config: &'a Config, db: &'a Db) -> Result<Self> {
        let mut state = Self {
            config,
            db,
            query: String::new(),
            search_mode: false,
            selected: 0,
            results: Vec::new(),
            preview: String::new(),
            preview_scroll: 0,
            preview_line_count: 0,
            error: None,
        };
        state.refresh(db)?;
        Ok(state)
    }

    /// Handle one key press. Returns `Some(action)` when the loop should stop. Never returns
    /// `Err`: a database failure becomes `self.error` — a keystroke cannot end the TUI.
    fn handle_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        let db = self.db;
        if self.search_mode {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.search_mode = false;
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    let outcome = self.refresh(db);
                    self.record_error(outcome);
                }
                KeyCode::Char(ch) => {
                    self.query.push(ch);
                    let outcome = self.refresh(db);
                    self.record_error(outcome);
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Some(AppAction::Quit),
                KeyCode::Char('/') => {
                    self.search_mode = true;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let outcome = self.move_selection(1, db);
                    self.record_error(outcome);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let outcome = self.move_selection(-1, db);
                    self.record_error(outcome);
                }
                KeyCode::PageDown => {
                    let page = self.config.ui.list_page_step as isize;
                    let outcome = self.move_selection(page, db);
                    self.record_error(outcome);
                }
                KeyCode::PageUp => {
                    let page = self.config.ui.list_page_step as isize;
                    let outcome = self.move_selection(-page, db);
                    self.record_error(outcome);
                }
                KeyCode::Char('g') => {
                    let outcome = self.select_index(0, db);
                    self.record_error(outcome);
                }
                KeyCode::Char('G') => {
                    let last = self.results.len().saturating_sub(1);
                    let outcome = self.select_index(last, db);
                    self.record_error(outcome);
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    let step = self.config.ui.preview_scroll_step as isize;
                    self.scroll_preview(step);
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    let step = self.config.ui.preview_scroll_step as isize;
                    self.scroll_preview(-step);
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let page = self.config.ui.preview_page_step as isize;
                    self.scroll_preview(page);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let page = self.config.ui.preview_page_step as isize;
                    self.scroll_preview(-page);
                }
                KeyCode::Enter | KeyCode::Char('r') => {
                    if let Some(selected) = self.selected_session() {
                        return Some(AppAction::Resume(Box::new(selected.clone())));
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn record_error(&mut self, result: Result<()>) {
        if let Err(error) = result {
            self.error = Some(format!("{error:#}"));
        }
    }

    /// Absorb finished worker responses. The call site is here from step 1 so the loop's
    /// drain-then-draw ordering is fixed; step 4 fills in the body.
    fn drain_responses(&mut self) {}

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let mut constraints = Vec::with_capacity(4);
        constraints.push(Constraint::Length(SEARCH_BOX_ROWS));
        constraints.push(Constraint::Min(MIN_BODY_ROWS));
        if self.error.is_some() {
            constraints.push(Constraint::Length(ERROR_LINE_ROWS));
        }
        constraints.push(Constraint::Length(STATUS_BAR_ROWS));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(frame.area());

        // Search box with visual cursor
        let search_display = if self.search_mode {
            format!("{}█", self.query)
        } else {
            self.query.clone()
        };
        let search_title = if self.search_mode {
            " Search (Enter/Esc to browse) "
        } else {
            " Search (press /) "
        };
        let search_border_style = if self.search_mode {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let top = Paragraph::new(search_display).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(search_border_style)
                .title(search_title),
        );
        frame.render_widget(top, chunks[0]);

        let middle = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(chunks[1]);

        // Session list
        let items = self
            .results
            .iter()
            .map(|session| {
                let title = session
                    .title
                    .as_deref()
                    .map(|value| truncate_for_display(value, 74))
                    .unwrap_or_else(|| session.preview_text.clone());
                let age = relative_age(session.updated_at);
                let (provider_label, provider_color) = match session.provider {
                    Provider::Claude => ("CLAUDE", Color::Green),
                    Provider::ClaudeDesktop => ("CL-DESK", Color::Green),
                    Provider::Codex => ("CODEX", Color::Cyan),
                    Provider::Cursor => ("CURSOR", Color::Magenta),
                    Provider::Antigravity => ("GEMINI", Color::Yellow),
                    Provider::Pi => ("PI", Color::Green),
                    Provider::PrimeAgent => ("PRIME", Color::LightGreen),
                    Provider::AiStudio => ("AI Studio", Color::Cyan),
                    Provider::GeminiCli => ("Gemini", Color::Blue),
                };
                let mut spans = vec![Span::styled(
                    format!("[{provider_label:<6}] "),
                    Style::default()
                        .fg(provider_color)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.extend(marked_spans_with_style(
                    &highlight_matches(&title, &self.query),
                    Style::default(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" [{age}]"),
                    Style::default().fg(Color::DarkGray),
                ));
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();

        let list_title = format!(
            " Sessions ({}/{}) ",
            if self.results.is_empty() {
                0
            } else {
                self.selected + 1
            },
            self.results.len()
        );
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(list_title))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        let mut list_state = ListState::default();
        if !self.results.is_empty() {
            list_state.select(Some(self.selected));
        }
        frame.render_stateful_widget(list, middle[0], &mut list_state);

        // Preview with scroll
        let preview_lines = self
            .preview
            .lines()
            .map(|line| render_preview_line(line, &self.query))
            .collect::<Vec<_>>();
        let preview = Paragraph::new(preview_lines)
            .block(Block::default().borders(Borders::ALL).title(" Preview "))
            .wrap(Wrap { trim: false })
            .scroll((self.preview_scroll, 0));
        frame.render_widget(preview, middle[1]);

        // Error line (own row, taken from the body when present — never the help bar).
        let status_index = chunks.len() - 1;
        if let Some(error) = &self.error {
            let error_line = Paragraph::new(Span::styled(
                error.as_str(),
                Style::default().fg(Color::Red),
            ));
            frame.render_widget(error_line, chunks[status_index - 1]);
        }

        // Status bar (single line, contextual)
        let help_text = if self.search_mode {
            "Type to search │ Enter/Esc: browse"
        } else {
            "j/k: move │ PgUp/PgDn: page │ g/G: top/bottom │ h/l: scroll preview │ /: search │ Enter: resume │ q: quit"
        };
        let bottom = Paragraph::new(Span::styled(
            help_text,
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(bottom, chunks[status_index]);
    }

    fn refresh(&mut self, db: &Db) -> Result<()> {
        let filters = SearchFilters {
            provider: None,
            path_prefix: None,
            exclude_path_prefixes: Vec::new(),
            exclude_session_ids: Vec::new(),
            // Every class, matching the CLI default: the browser shows what is indexed.
            session_kinds: None,
            parent_session_id: None,
            since: None,
            until: None,
            limit: tui_result_limit(self.config.search.default_limit),
            warnings_only: false,
        };
        self.results = if self.query.trim().is_empty() {
            db.list_recent(&filters)?
        } else {
            db.search(
                &self.query,
                &filters,
                current_repo(self.config).as_deref(),
                &self.config.search.scoring,
            )?
            .into_iter()
            .map(|hit| hit.session)
            .collect()
        };
        self.selected = 0;
        self.load_preview(db)?;
        Ok(())
    }

    fn move_selection(&mut self, delta: isize, db: &Db) -> Result<()> {
        if self.results.is_empty() {
            return Ok(());
        }
        let new =
            (self.selected as isize + delta).clamp(0, self.results.len() as isize - 1) as usize;
        if new != self.selected {
            self.selected = new;
            self.preview_scroll = 0;
            self.load_preview(db)?;
        }
        Ok(())
    }

    fn select_index(&mut self, index: usize, db: &Db) -> Result<()> {
        if self.results.is_empty() {
            return Ok(());
        }
        let new = index.min(self.results.len() - 1);
        if new != self.selected {
            self.selected = new;
            self.preview_scroll = 0;
            self.load_preview(db)?;
        }
        Ok(())
    }

    fn scroll_preview(&mut self, delta: isize) {
        let max = self.preview_line_count.saturating_sub(3) as u16;
        self.preview_scroll = (self.preview_scroll as isize + delta).clamp(0, max as isize) as u16;
    }

    fn selected_session(&self) -> Option<&SessionRecord> {
        self.results.get(self.selected)
    }

    fn load_preview(&mut self, db: &Db) -> Result<()> {
        let Some(selected) = self.selected_session() else {
            self.preview = "No sessions matched the current query.".to_string();
            self.preview_line_count = 1;
            return Ok(());
        };
        let full = db.resolve_session(&selected.id)?;
        let summary = build_transcript_summary(&full.transcript_text);
        self.preview = format!(
            "Session: {}\nCWD: {}\n\n{}",
            selected.id,
            selected.cwd.as_deref().unwrap_or("-"),
            summary
        );
        self.preview_line_count = self.preview.lines().count();
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnRole {
    User,
    Assistant,
}

impl TurnRole {
    fn parse(line: &str) -> Option<Self> {
        let close = line.strip_prefix('[')?.find(']')?;
        // close is the index of ']' within the post-'[' slice; role text is after the ']'.
        let after = &line[close + 2..];
        match after.trim() {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

struct Turn<'a> {
    role: TurnRole,
    body: &'a str,
}

fn parse_turns(transcript: &str) -> Vec<Turn<'_>> {
    let mut turns: Vec<Turn<'_>> = Vec::new();
    let mut current_role: Option<TurnRole> = None;
    let mut body_start: usize = 0;

    let mut cursor = 0usize;
    while cursor < transcript.len() {
        let line_end = transcript[cursor..]
            .find('\n')
            .map(|i| cursor + i)
            .unwrap_or(transcript.len());
        let line = &transcript[cursor..line_end];
        if let Some(role) = TurnRole::parse(line) {
            if let Some(prev) = current_role.take() {
                let body = transcript[body_start..cursor].trim_matches('\n');
                turns.push(Turn { role: prev, body });
            }
            current_role = Some(role);
            body_start = (line_end + 1).min(transcript.len());
        }
        cursor = if line_end == transcript.len() {
            transcript.len()
        } else {
            line_end + 1
        };
    }
    if let Some(prev) = current_role {
        let body = transcript[body_start..].trim_matches('\n');
        turns.push(Turn { role: prev, body });
    }
    turns
}

fn truncate_body(body: &str, max_lines: usize) -> String {
    let trimmed = body.trim_end();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() <= max_lines {
        return trimmed.to_string();
    }
    let mut out = lines[..max_lines].join("\n");
    out.push_str("\n  […]");
    out
}

fn build_transcript_summary(transcript: &str) -> String {
    let turns = parse_turns(transcript);
    if turns.is_empty() {
        return "(no transcript content)".to_string();
    }

    let first_user = turns.iter().position(|t| t.role == TurnRole::User);
    let first_assistant = turns.iter().position(|t| t.role == TurnRole::Assistant);
    let last_user = turns.iter().rposition(|t| t.role == TurnRole::User);
    let last_assistant = turns.iter().rposition(|t| t.role == TurnRole::Assistant);

    // (turn_index, label, max_lines)
    let candidates = [
        (first_user, "── First prompt ──", 8usize),
        (first_assistant, "── First reply ──", 4),
        (last_user, "── Final prompt ──", 8),
        (last_assistant, "── Final reply ──", 14),
    ];

    let mut shown_indices: Vec<usize> = Vec::new();
    let mut sections: Vec<(usize, &'static str, usize)> = Vec::new();
    for (idx, label, max_lines) in candidates {
        let Some(idx) = idx else { continue };
        if shown_indices.contains(&idx) {
            continue;
        }
        shown_indices.push(idx);
        sections.push((idx, label, max_lines));
    }
    sections.sort_by_key(|(idx, _, _)| *idx);

    let total = turns.len();
    let hidden = total.saturating_sub(shown_indices.len());

    let mut parts: Vec<String> = Vec::new();
    let mut last_emitted_idx: Option<usize> = None;
    for (idx, label, max_lines) in &sections {
        if let Some(prev) = last_emitted_idx {
            if *idx > prev + 1 {
                let gap = *idx - prev - 1;
                parts.push(format!(
                    "⋯ {gap} more turn{} hidden ⋯",
                    if gap == 1 { "" } else { "s" }
                ));
            }
        }
        parts.push((*label).to_string());
        parts.push(truncate_body(turns[*idx].body, *max_lines));
        last_emitted_idx = Some(*idx);
    }

    if hidden > 0 && sections.len() < 2 {
        // Single section displayed but more turns exist after it (rare edge case).
        parts.push(format!(
            "⋯ {hidden} more turn{} hidden ⋯",
            if hidden == 1 { "" } else { "s" }
        ));
    }

    parts.push(format!(
        "({total} turn{} total)",
        if total == 1 { "" } else { "s" }
    ));
    parts.join("\n\n")
}

fn render_preview_line(line: &str, query: &str) -> Line<'static> {
    if let Some(session_id) = line.strip_prefix("Session: ") {
        let mut spans = vec![Span::styled(
            "Session: ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )];
        spans.extend(marked_spans_with_style(
            &highlight_matches(session_id, query),
            Style::default().fg(Color::Gray),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        return Line::from(spans);
    }

    if let Some(cwd) = line.strip_prefix("CWD: ") {
        let mut spans = vec![Span::styled(
            "CWD: ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )];
        spans.extend(marked_spans_with_style(
            &highlight_matches(cwd, query),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        return Line::from(spans);
    }

    if line.starts_with("── ") {
        let style = if line.contains("prompt") {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD)
        };
        return Line::from(Span::styled(line.to_string(), style));
    }

    if line.starts_with("⋯ ") || line.starts_with('(') && line.ends_with(" total)") {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ));
    }

    Line::from(marked_spans_with_style(
        &highlight_matches(line, query),
        Style::default(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))
}

fn marked_spans_with_style(
    input: &str,
    base_style: Style,
    highlight_style: Style,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = input;

    while let Some(start) = rest.find("[[") {
        if start > 0 {
            spans.push(Span::styled(rest[..start].to_string(), base_style));
        }
        let after_start = &rest[start + 2..];
        if let Some(end) = after_start.find("]]") {
            spans.push(Span::styled(
                after_start[..end].to_string(),
                highlight_style,
            ));
            rest = &after_start[end + 2..];
        } else {
            spans.push(Span::styled(rest[start..].to_string(), base_style));
            rest = "";
            break;
        }
    }

    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base_style));
    }

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::collections::VecDeque;

    /// Replays a fixed event script and records every poll timeout, so tests can assert the
    /// loop's pacing without a clock.
    struct ScriptedEventSource {
        events: VecDeque<Event>,
        poll_timeouts: Vec<Duration>,
    }

    impl ScriptedEventSource {
        fn new(events: Vec<Event>) -> Self {
            Self {
                events: events.into(),
                poll_timeouts: Vec::new(),
            }
        }
    }

    impl EventSource for ScriptedEventSource {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            self.poll_timeouts.push(timeout);
            Ok(!self.events.is_empty())
        }

        fn read(&mut self) -> io::Result<Event> {
            self.events
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "script exhausted"))
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl_key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
    }

    /// One seeded row: per-id path, id, provider_session_id, AND title. Every override is
    /// load-bearing — `minimal_record` leaves `title: None` and derives the session id from the
    /// file stem, and `upsert_session` keys on that id, so a shared path collapses rows.
    fn session(id: &str) -> crate::models::ParsedSession {
        let path = std::path::Path::new("/fixture").join(format!("{id}.jsonl"));
        let mut parsed = crate::util::minimal_record(Provider::Claude, &path, String::new());
        parsed.session.id = id.to_string();
        parsed.session.provider_session_id = id.to_string();
        parsed.session.title = Some(id.to_string());
        parsed
    }

    fn fixture_db(sessions: &[&str]) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        for id in sessions {
            db.upsert_session(&session(id), 0, 0).unwrap();
        }
        (dir, db)
    }

    fn screen_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut lines = Vec::new();
        for y in area.top()..area.bottom() {
            let mut line = String::new();
            for x in area.left()..area.right() {
                line.push_str(buffer[(x, y)].symbol());
            }
            lines.push(line);
        }
        lines.join("\n")
    }

    #[test]
    fn event_seam_reproduces_current_keybindings() {
        let (_dir, db) = fixture_db(&["claude:alpha", "claude:beta", "claude:gamma"]);
        let config = Config::default();
        let mut app = AppState::new(&config, &db).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

        // '/' enters search mode and 'a' types; one step drains both, and because step is
        // draw-then-drain, a second step renders the post-key state.
        let mut events =
            ScriptedEventSource::new(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert!(app.search_mode);
        assert_eq!(app.query, "a");
        step(&mut terminal, &mut events, &mut app).unwrap();
        let screen = screen_text(&terminal);
        assert!(
            screen.contains("a█"),
            "typed character renders with the visual cursor"
        );
        assert!(screen.contains("Sessions"));

        // Esc in search mode returns to browse without quitting.
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Esc)]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert!(!app.search_mode);

        // Backspace to an empty query re-browses without panicking (search mode first:
        // browse mode has no Backspace arm — that IS the current contract).
        let mut events =
            ScriptedEventSource::new(vec![key(KeyCode::Char('/')), key(KeyCode::Backspace)]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert_eq!(app.query, "");

        // Esc returns to browse so the navigation keys below apply (the Backspace section
        // re-entered search mode).
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Esc)]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert!(!app.search_mode);

        // j/k move the selection and load the preview of the new row.
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Char('j'))]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert_eq!(app.selected, 1);

        // Ctrl-d scrolls the preview by the page step against the content-length bound.
        app.preview_line_count = 100;
        let mut events = ScriptedEventSource::new(vec![ctrl_key(KeyCode::Char('d'))]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert_eq!(app.preview_scroll, 15);

        // Enter on a selected session yields the resume action.
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Enter)]);
        assert!(matches!(
            step(&mut terminal, &mut events, &mut app).unwrap(),
            Some(AppAction::Resume(_))
        ));

        // q quits from browse mode.
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Char('q'))]);
        assert!(matches!(
            step(&mut terminal, &mut events, &mut app).unwrap(),
            Some(AppAction::Quit)
        ));

        // g/G/PageDown/PageUp on an EMPTY result set must not panic.
        let (_dir2, db2) = fixture_db(&[]);
        let mut empty_app = AppState::new(&config, &db2).unwrap();
        let mut terminal2 = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let mut events = ScriptedEventSource::new(vec![
            key(KeyCode::Char('g')),
            key(KeyCode::Char('G')),
            key(KeyCode::PageDown),
            key(KeyCode::PageUp),
        ]);
        assert!(step(&mut terminal2, &mut events, &mut empty_app)
            .unwrap()
            .is_none());
    }

    #[test]
    fn ui_interaction_fields_reach_the_loop() {
        let (_dir, db) = fixture_db(&["claude:one", "claude:two", "claude:three", "claude:four"]);
        let mut config = Config::default();
        config.ui.event_poll_interval_ms = 7;
        config.ui.list_page_step = 2;
        config.ui.preview_scroll_step = 3;
        config.ui.preview_page_step = 4;
        let mut app = AppState::new(&config, &db).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

        // The idle poll paces with the configured interval, observed clock-free through the
        // seam's recorded timeouts (every poll — including the one that returns false).
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::PageDown)]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert!(
            events
                .poll_timeouts
                .iter()
                .all(|timeout| *timeout == Duration::from_millis(7)),
            "poll timeout must follow [ui].event_poll_interval_ms, got {:?}",
            events.poll_timeouts
        );

        // PageDown moves by the configured list page step, not a hardcoded 10.
        assert_eq!(app.selected, 2);

        // l scrolls by the configured preview scroll step; Ctrl-d adds the page step.
        app.preview_line_count = 100;
        let mut events = ScriptedEventSource::new(vec![key(KeyCode::Char('l'))]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert_eq!(app.preview_scroll, 3);
        let mut events = ScriptedEventSource::new(vec![ctrl_key(KeyCode::Char('d'))]);
        assert!(step(&mut terminal, &mut events, &mut app)
            .unwrap()
            .is_none());
        assert_eq!(app.preview_scroll, 7);
    }

    #[test]
    fn result_limit_preserves_large_config_and_fills_the_browser_for_small_config() {
        assert_eq!(tui_result_limit(25), 100);
        assert_eq!(tui_result_limit(100), 100);
        assert_eq!(tui_result_limit(250), 250);
    }

    fn make_turn(role: &str, body: &str) -> String {
        format!("[2026-05-08 12:00:00 UTC] {role}\n{body}")
    }

    fn join_turns(turns: &[(&str, &str)]) -> String {
        turns
            .iter()
            .map(|(role, body)| make_turn(role, body))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn parses_turns_in_canonical_format() {
        let raw = join_turns(&[
            ("user", "hello"),
            ("assistant", "hi there\nmulti-line"),
            ("user", "bye"),
        ]);
        let turns = parse_turns(&raw);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].role, TurnRole::User);
        assert_eq!(turns[0].body, "hello");
        assert_eq!(turns[1].role, TurnRole::Assistant);
        assert_eq!(turns[1].body, "hi there\nmulti-line");
        assert_eq!(turns[2].body, "bye");
    }

    #[test]
    fn summary_shows_bookends_with_elision() {
        let raw = join_turns(&[
            ("user", "first prompt"),
            ("assistant", "first reply"),
            ("user", "middle 1"),
            ("assistant", "middle 2"),
            ("user", "middle 3"),
            ("assistant", "middle 4"),
            ("user", "final prompt"),
            ("assistant", "final reply"),
        ]);
        let summary = build_transcript_summary(&raw);
        assert!(summary.contains("First prompt"));
        assert!(summary.contains("first prompt"));
        assert!(summary.contains("First reply"));
        assert!(summary.contains("first reply"));
        assert!(summary.contains("Final prompt"));
        assert!(summary.contains("final prompt"));
        assert!(summary.contains("Final reply"));
        assert!(summary.contains("final reply"));
        assert!(summary.contains("4 more turns hidden"));
        assert!(summary.contains("8 turns total"));
    }

    #[test]
    fn summary_handles_short_session() {
        let raw = join_turns(&[("user", "hey"), ("assistant", "yo")]);
        let summary = build_transcript_summary(&raw);
        // first==last for both roles, so we should see exactly 2 sections, no elision.
        assert!(summary.contains("First prompt"));
        assert!(summary.contains("First reply"));
        assert!(!summary.contains("more turn"));
        assert!(summary.contains("2 turns total"));
    }

    #[test]
    fn summary_truncates_long_body() {
        let big_body: String = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let raw = join_turns(&[("user", &big_body), ("assistant", "ok")]);
        let summary = build_transcript_summary(&raw);
        assert!(summary.contains("[…]"));
    }

    #[test]
    fn summary_for_empty_transcript() {
        assert_eq!(build_transcript_summary(""), "(no transcript content)");
    }

    #[test]
    fn marked_spans_with_style_splits_on_double_bracket_markers() {
        let base = Style::default();
        let hi = Style::default().fg(Color::Yellow);

        // Text before/inside/after a marker yields base, highlight, base spans.
        let spans = marked_spans_with_style("abc[[def]]ghi", base, hi);
        let got: Vec<(&str, Style)> = spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(got, vec![("abc", base), ("def", hi), ("ghi", base)]);

        // A whole-string match is a single highlighted span.
        let spans = marked_spans_with_style("[[all]]", base, hi);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "all");
        assert_eq!(spans[0].style, hi);

        // No marker is one base span; empty input is one empty base span.
        assert_eq!(marked_spans_with_style("plain", base, hi)[0].style, base);
        let spans = marked_spans_with_style("", base, hi);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "");

        // An unclosed marker keeps the trailing "[[" as base-styled literal text.
        let spans = marked_spans_with_style("ab[[cd", base, hi);
        let got: Vec<(&str, Style)> = spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(got, vec![("ab", base), ("[[cd", base)]);
    }

    #[test]
    fn render_preview_line_styles_known_prefixes_and_highlights_queries() {
        // A Session: line keeps the label span then the id, text preserved.
        let line = render_preview_line("Session: claude:s1", "");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Session: claude:s1");
        assert!(line.spans.len() >= 2);

        // A section header renders as a single styled span.
        let line = render_preview_line("── user prompt ──", "");
        assert_eq!(line.spans.len(), 1);

        // A plain line with a query splits the matched term into its own span
        // while preserving the full text.
        let line = render_preview_line("find the needle here", "needle");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "find the needle here");
        assert!(line.spans.len() >= 2);
    }
}
