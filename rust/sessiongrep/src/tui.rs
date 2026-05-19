use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::models::{Provider, SearchFilters, SessionRecord};
use sessiongrep::util::{
    current_repo, highlight_matches, prompt_confirm, relative_age, render_command, resume_plan,
    truncate_for_display,
};

pub fn run(config: &Config, db: &Db) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let action = run_app(&mut terminal, config, db);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    match action? {
        AppAction::Quit => Ok(()),
        AppAction::Resume(session) => {
            let (command, cwd) = resume_plan(&session)?;
            println!("resume command: {}", render_command(&command));
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

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: &Config,
    db: &Db,
) -> Result<AppAction> {
    let mut app = AppState::new(config, db)?;

    loop {
        terminal.draw(|frame| app.render(frame))?;
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.search_mode {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    app.search_mode = false;
                }
                KeyCode::Backspace => {
                    app.query.pop();
                    app.refresh(db)?;
                }
                KeyCode::Char(ch) => {
                    app.query.push(ch);
                    app.refresh(db)?;
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(AppAction::Quit),
                KeyCode::Char('/') => {
                    app.search_mode = true;
                }
                KeyCode::Down | KeyCode::Char('j') => app.move_selection(1, db)?,
                KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1, db)?,
                KeyCode::PageDown => app.move_selection(10, db)?,
                KeyCode::PageUp => app.move_selection(-10, db)?,
                KeyCode::Char('g') => app.select_index(0, db)?,
                KeyCode::Char('G') => {
                    let last = app.results.len().saturating_sub(1);
                    app.select_index(last, db)?;
                }
                KeyCode::Char('l') | KeyCode::Right => app.scroll_preview(5),
                KeyCode::Char('h') | KeyCode::Left => app.scroll_preview(-5),
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.scroll_preview(15);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.scroll_preview(-15);
                }
                KeyCode::Enter | KeyCode::Char('r') => {
                    if let Some(selected) = app.selected_session() {
                        return Ok(AppAction::Resume(selected.clone()));
                    }
                }
                _ => {}
            }
        }
    }
}

enum AppAction {
    Quit,
    Resume(SessionRecord),
}

struct AppState<'a> {
    config: &'a Config,
    query: String,
    search_mode: bool,
    selected: usize,
    results: Vec<SessionRecord>,
    preview: String,
    preview_scroll: u16,
    preview_line_count: usize,
}

impl<'a> AppState<'a> {
    fn new(config: &'a Config, db: &Db) -> Result<Self> {
        let mut state = Self {
            config,
            query: String::new(),
            search_mode: false,
            selected: 0,
            results: Vec::new(),
            preview: String::new(),
            preview_scroll: 0,
            preview_line_count: 0,
        };
        state.refresh(db)?;
        Ok(state)
    }

    fn render(&mut self, frame: &mut ratatui::Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(1),
            ])
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
                    Provider::Codex => ("CODEX", Color::Cyan),
                    Provider::Cursor => ("CURSOR", Color::Magenta),
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
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(list_title),
            )
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
        frame.render_widget(bottom, chunks[2]);
    }

    fn refresh(&mut self, db: &Db) -> Result<()> {
        let filters = SearchFilters {
            provider: None,
            path_prefix: None,
            since: None,
            limit: self.config.search.default_limit.max(100),
            warnings_only: false,
        };
        self.results = if self.query.trim().is_empty() {
            db.list_recent(&filters)?
        } else {
            db.search(&self.query, &filters, current_repo(self.config).as_deref())?
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
        let new = (self.selected as isize + delta)
            .clamp(0, self.results.len() as isize - 1) as usize;
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
                parts.push(format!("⋯ {gap} more turn{} hidden ⋯", if gap == 1 { "" } else { "s" }));
            }
        }
        parts.push((*label).to_string());
        parts.push(truncate_body(turns[*idx].body, *max_lines));
        last_emitted_idx = Some(*idx);
    }

    if hidden > 0 && sections.len() < 2 {
        // Single section displayed but more turns exist after it (rare edge case).
        parts.push(format!("⋯ {hidden} more turn{} hidden ⋯", if hidden == 1 { "" } else { "s" }));
    }

    parts.push(format!("({total} turn{} total)", if total == 1 { "" } else { "s" }));
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
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)
        };
        return Line::from(Span::styled(line.to_string(), style));
    }

    if line.starts_with("⋯ ") || line.starts_with('(') && line.ends_with(" total)") {
        return Line::from(Span::styled(
            line.to_string(),
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
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
        let big_body: String = (0..30).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let raw = join_turns(&[("user", &big_body), ("assistant", "ok")]);
        let summary = build_transcript_summary(&raw);
        assert!(summary.contains("[…]"));
    }

    #[test]
    fn summary_for_empty_transcript() {
        assert_eq!(build_transcript_summary(""), "(no transcript content)");
    }
}
