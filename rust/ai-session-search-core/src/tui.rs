// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::num::NonZeroUsize;

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
    },
    Terminal,
};

use crate::config::Config;
use crate::db::{
    Db, QueryCancellation, QueryCancelled, MIN_READABLE_SCHEMA_VERSION, SCHEMA_VERSION,
};
use crate::keymap::{ActionMode, TuiAction};
#[cfg(test)]
use crate::models::Role;
use crate::models::{Provider, SearchFilters, SessionKind, SessionRecord};
use crate::runtime::ExecutionRuntime;
use crate::search_scope::EffectiveAccessScope;
use crate::service::CatalogService;
use crate::terminal_style::TerminalStyle;
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

/// Convert a configured unsigned step without `as` wrapping values above `isize::MAX` negative.
fn saturating_step(value: usize) -> isize {
    isize::try_from(value).unwrap_or(isize::MAX)
}

/// Absolute result range needed for one frame. Result membership stays complete in AppState;
/// formatting/allocation is proportional only to terminal-height V, never retained K.
fn visible_session_range(
    total: usize,
    selected: usize,
    viewport_rows: usize,
) -> std::ops::Range<usize> {
    let visible = total.min(viewport_rows);
    if visible == 0 {
        return 0..0;
    }
    let selected = selected.min(total - 1);
    let start = selected
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible));
    start..start + visible
}

/// Search box height including its border rows; the query line is the middle row.
const SEARCH_BOX_ROWS: u16 = 3;

/// Minimum height of the list/preview body. Below 14 total rows the vertical layout
/// underflows (recorded as D11); the step-5 resize test pins that boundary.
const MIN_BODY_ROWS: u16 = 10;

/// Status/help bar height, shared by both modes.
const STATUS_BAR_ROWS: u16 = 1;

/// Maximum delay before the UI observes current worker output. Idle turns use the configured
/// event interval directly; the 10 ms latency slice is paid only while search/preview work is live.
const ACTIVE_WORKER_POLL_SLICE: Duration = Duration::from_millis(10);

/// Error line height when a keystroke error is being shown. It takes its row from the body,
/// never from the help bar, so REQ047's recovery guidance keeps its line.
const ERROR_LINE_ROWS: u16 = 1;

/// Two border rows plus one content row: the scroll-clamp floor before the first render has
/// recorded a viewport height (D11).
const PREVIEW_VIEWPORT_SLACK: usize = 3;

/// Worst-case rendered width of a list row's " [age]" suffix, from relative_age's output
/// shapes: its longest form is the date branch, " [2026-01-16]".
const AGE_SUFFIX_ALLOWANCE: usize = 13;

/// The `s` binding's time windows in cycle order, as (status label, span in hours). One table,
/// so the binding that sets `filters.since` and the status bar that names the active window
/// cannot disagree about which one is on.
const SINCE_WINDOWS: [(&str, i64); 3] = [("1d", 24), ("7d", 24 * 7), ("30d", 24 * 30)];

/// Which entry of [`SINCE_WINDOWS`] a `since` timestamp names, recovered from the age it implies.
///
/// `SearchFilters::since` is an absolute instant, so the active window has to be read back rather
/// than stored twice. A window matches while its age has not yet reached the next window's span,
/// which leaves a day of slack: a TUI left open overnight keeps labelling its window correctly and
/// keeps advancing the cycle in order.
fn active_since_window(since: Option<DateTime<Utc>>) -> Option<usize> {
    let since = since?;
    let age_hours = (Utc::now() - since).num_hours();
    Some(
        SINCE_WINDOWS
            .iter()
            .position(|(_, span_hours)| age_hours <= span_hours + 24)
            .unwrap_or(SINCE_WINDOWS.len() - 1),
    )
}

/// The session list's provider labels (D6/D10): uppercase throughout — the old table's
/// "GEMINI" and "Gemini" were near-identical, and "AI Studio" overflowed a `{:<6}` field
/// that pads but never truncates.
fn provider_label(provider: Provider) -> (&'static str, Color) {
    match provider {
        Provider::Claude => ("CLAUDE", Color::Green),
        Provider::ClaudeDesktop => ("CL-DESK", Color::Green),
        Provider::Codex => ("CODEX", Color::Cyan),
        Provider::Cursor => ("CURSOR", Color::Magenta),
        Provider::Antigravity => ("ANTIGRAV", Color::Yellow),
        Provider::Pi => ("PI", Color::Green),
        Provider::PrimeAgent => ("PRIME", Color::LightGreen),
        Provider::AiStudio => ("AISTUDIO", Color::Cyan),
        Provider::GeminiCli => ("GEMINICLI", Color::Blue),
    }
}

/// The label-column floor, derived from the table so adding a provider can never produce a
/// truncating width.
fn longest_provider_label() -> usize {
    Provider::value_variants()
        .iter()
        .map(|provider| provider_label(*provider).0.len())
        .max()
        .unwrap_or(0)
}

/// Middle-elide `text` to terminal columns, keeping the tail intact: an anyhow chain ends with
/// recovery guidance, and REQ047 forbids losing it to clipping. Newlines are flattened because
/// the destination is exactly one row; grapheme display width handles CJK and emoji correctly.
fn elide_middle(text: &str, width: usize, ellipsis: &str) -> String {
    let sanitized = text
        .chars()
        .map(|character| {
            if matches!(character, '\r' | '\n') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if UnicodeWidthStr::width(sanitized.as_str()) <= width {
        return sanitized;
    }
    if width == 0 {
        return String::new();
    }
    let marker_width = UnicodeWidthStr::width(ellipsis);
    if width <= marker_width {
        // No room for both a marker and text: the marker alone at least says text was cut.
        return ellipsis.chars().take(width).collect();
    }
    let head_budget = width.saturating_sub(marker_width) / 2;
    let tail_budget = width - head_budget - marker_width;
    let mut head = String::new();
    let mut used = 0;
    for grapheme in sanitized.graphemes(true) {
        let columns = UnicodeWidthStr::width(grapheme);
        if used + columns > head_budget {
            break;
        }
        head.push_str(grapheme);
        used += columns;
    }
    let mut tail_graphemes = Vec::new();
    used = 0;
    for grapheme in sanitized.graphemes(true).rev() {
        let columns = UnicodeWidthStr::width(grapheme);
        if used + columns > tail_budget {
            break;
        }
        tail_graphemes.push(grapheme);
        used += columns;
    }
    let tail = tail_graphemes.into_iter().rev().collect::<String>();
    format!("{head}{ellipsis}{tail}")
}

/// Separator between status-bar hints.
/// Fit status-bar hints into `width` display columns by dropping whole hints instead of eliding
/// characters out of the middle of the joined line.
///
/// Middle-eliding a help bar is the wrong shape for it: at 80 columns it cut the line to
/// `p:any … │ j/k: move │ P…rs │ /: search`, which names no binding the reader can act on. A hint
/// is only worth its columns while it is readable, so the lowest-priority hints leave first and
/// the rest stay whole. `priority` is drop order, 0 last; ties drop the hint further right, so the
/// ones a reader scans first survive longest. The final hint is always kept, and the caller still
/// elides it for a frame too narrow even for that.
fn fit_status_hints(hints: &[(u8, String)], width: usize, separator: &str) -> String {
    let mut kept: Vec<usize> = (0..hints.len()).collect();
    loop {
        let text = kept
            .iter()
            .map(|index| hints[*index].1.as_str())
            .collect::<Vec<_>>()
            .join(separator);
        if kept.len() == 1 || UnicodeWidthStr::width(text.as_str()) <= width {
            return text;
        }
        let victim = kept
            .iter()
            .enumerate()
            .max_by_key(|(position, index)| (hints[**index].0, *position))
            .map(|(position, _)| position)
            .expect("kept is never empty");
        kept.remove(victim);
    }
}

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
/// Run only the terminal selection lifecycle so the CLI can drop its owning SessionSearch
/// (SQLite connections and Rayon runtime) before prompting or starting a resumed process.
pub(crate) fn select_session(config: &Config, db: &Db) -> Result<Option<SessionRecord>> {
    // Open and validate the worker before entering raw/alternate-screen mode: a slow or failed
    // startup must leave the user's ordinary terminal visible.
    // One resolution for the run: the worker builds preview text out of these symbols and the
    // renderer matches on them, so a second resolution could disagree with the first.
    let style = TerminalStyle::resolve(config.ui.unicode, config.ui.color);
    let (worker, _observed_scope) = spawn_search_worker(db_backed_executor(
        config.clone(),
        db.access_scope().clone(),
        db.execution_runtime(),
        style,
    ))?;
    let mut app = AppState::new(config.clone(), worker)?;

    // `app` is declared before the guard, so panic unwinding restores the terminal before
    // SearchWorker::drop can wait for cancellation. The explicit normal-path drop preserves that
    // order before returning a selection to the caller.
    let terminal_guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut events = CrosstermEventSource;
    let action = run_app(&mut terminal, &mut events, &mut app);
    drop(terminal_guard);
    drop(app);

    match action? {
        AppAction::Quit => Ok(None),
        AppAction::Resume(session) => Ok(Some(*session)),
    }
}

pub(crate) fn execute_resume(session: &SessionRecord) -> Result<()> {
    let (command, cwd) = resume_plan(session)?;
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

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    events: &mut dyn EventSource,
    app: &mut AppState,
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
    app: &mut AppState,
) -> Result<Option<AppAction>>
where
    B::Error: Send + Sync + 'static,
{
    app.drain_responses();
    terminal.draw(|frame| app.render(frame))?;
    // Wait for input in slices no longer than the configured interval, absorbing worker
    // responses between slices. Two measured defects forced this shape (§12): a key's echo
    // must render immediately — not after the interval expires — and a worker response
    // arriving mid-wait must be applied the same way; the first post-fix measurement pinned
    // results p50 at the 150 ms idle interval because nothing drained until the next step.
    // While current worker work is live, slices are capped at 10 ms so pickup latency stays far
    // below configured pacing. Fully idle turns perform one configured wait rather than waking
    // 100 times/second; each handled key restarts the idle window.
    let mut idle_deadline = std::time::Instant::now()
        .checked_add(Duration::from_millis(app.config.ui.idle_poll_interval_ms))
        .ok_or_else(|| anyhow::anyhow!("ui.idle_poll_interval_ms exceeds the monotonic clock"))?;
    loop {
        let now = std::time::Instant::now();
        // A query edited and then left alone becomes a search here, so the wait below is what
        // separates typing from searching. The slice is shortened to the remaining delay so the
        // search starts on time rather than at the next idle turn.
        let mut pending_search_delay = app.edited_query_delay(now);
        if pending_search_delay == Some(Duration::ZERO) {
            app.flush_edited_query();
            terminal.draw(|frame| app.render(frame))?;
            pending_search_delay = None;
        }
        if now >= idle_deadline {
            return Ok(None);
        }
        let remaining = idle_deadline.saturating_duration_since(now);
        let mut slice = if app.searching || app.preview_loading || app.worker.has_work() {
            remaining.min(ACTIVE_WORKER_POLL_SLICE)
        } else {
            remaining
        };
        if let Some(delay) = pending_search_delay {
            slice = slice.min(delay);
        }
        if events.poll(slice)? {
            let event = events.read()?;
            let Event::Key(key) = event else {
                if matches!(event, Event::Resize(_, _)) {
                    terminal.draw(|frame| app.render(frame))?;
                    idle_deadline = std::time::Instant::now()
                        .checked_add(Duration::from_millis(app.config.ui.idle_poll_interval_ms))
                        .ok_or_else(|| {
                            anyhow::anyhow!("ui.idle_poll_interval_ms exceeds the monotonic clock")
                        })?;
                }
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if let Some(action) = app.handle_key(key) {
                return Ok(Some(action));
            }
            // Redraw right after handling a key so its echo never waits out a poll slice.
            app.drain_responses();
            terminal.draw(|frame| app.render(frame))?;
            idle_deadline = std::time::Instant::now()
                .checked_add(Duration::from_millis(app.config.ui.idle_poll_interval_ms))
                .ok_or_else(|| {
                    anyhow::anyhow!("ui.idle_poll_interval_ms exceeds the monotonic clock")
                })?;
        } else if app.drain_responses() {
            // A worker response landed mid-wait: apply it and redraw now.
            terminal.draw(|frame| app.render(frame))?;
        }
    }
}

enum AppAction {
    Quit,
    // Boxed: SessionRecord is large; keep the enum small (clippy::large_enum_variant).
    Resume(Box<SessionRecord>),
}

/// Which kind of work a [`WorkerRequest`] asks for. The discriminant is load-bearing: a
/// preview-only response must never replace the result list (C13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Search,
    PreviewOnly,
}

/// Request identity is an allocation token rather than an integer, so it cannot wrap and make
/// an ancient response current again (the same generation-token pattern used by MCP refresh).
#[derive(Clone)]
struct RequestGeneration(Arc<()>);

impl RequestGeneration {
    fn new() -> Self {
        Self(Arc::new(()))
    }

    fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A unit of work for the search worker.
struct WorkerRequest {
    kind: RequestKind,
    generation: RequestGeneration,
    query: String,
    filters: SearchFilters,
    selected_id: Option<String>,
}

/// Typed response envelope: successes and errors carry the same operation identity, so a late
/// failure cannot overwrite the current screen any more than late rows can.
struct WorkerOutcome {
    kind: RequestKind,
    generation: RequestGeneration,
    result: Result<WorkerResponse>,
}

/// The standing empty-state preview text; `WorkerResponse::preview` supplies it when no row is
/// selected, exactly as the inline preview path always has.
const NO_SESSIONS_PREVIEW: &str = "No sessions matched the current query.";

/// Exactly one of `results` / `preview` is populated, and which one is decided by `RequestKind`.
///
/// A `Search` response carries **no** preview. It cannot: the selection-preservation rule runs
/// on the UI thread against the new result set, so the worker does not know which row will end
/// up selected and cannot pre-compute the right preview for it. The UI applies the results,
/// then issues a `PreviewOnly` for whatever it actually selected — one extra round trip, off
/// the UI thread, and it keeps preview scan/output work out of the search response
/// (C28). The originating query travels back so a superseded response is dropped without a
/// sequence counter; `previewed_id` says which session the preview is *for*, so a preview
/// overtaken by newer navigation is discarded rather than rendered beside the wrong row.
struct WorkerResponse {
    results: Option<Vec<SessionRecord>>,
    previewed_id: Option<String>,
    preview: Option<String>,
}

impl WorkerResponse {
    /// A search response: results only, no preview.
    fn results(_request: &WorkerRequest, rows: Vec<SessionRecord>) -> Self {
        Self {
            results: Some(rows),
            previewed_id: None,
            preview: None,
        }
    }

    /// A preview response. `None` text means "no selected session" and renders the standing
    /// empty-state message, matching the inline preview path.
    ///
    /// It deliberately carries no line count. The worker can only count logical lines, and the
    /// scroll bound is in terminal rows after word wrapping, which only the renderer knows; a
    /// second count here would clamp a reader out of the wrapped tail.
    fn preview(request: &WorkerRequest, text: Option<String>) -> Self {
        Self {
            results: None,
            previewed_id: request.selected_id.clone(),
            preview: Some(text.unwrap_or_else(|| NO_SESSIONS_PREVIEW.to_string())),
        }
    }
}

/// The cancellation is supplied by the worker, not created by the executor: one slot, one
/// owner (§2.3). A production executor installs it on its own connection.
///
/// It is passed as `&Arc<_>`, not `&QueryCancellation`, for one reason: `cancel_in_flight`
/// does `slot.take()`, so after a supersede the worker's `Arc` is the only one left and a
/// borrow cannot outlive the call. A fake executor clones the `Arc` out to the test thread,
/// which is the only way the supersede and quit tests can observe `is_cancelled()` at all.
type SearchExecutor =
    Box<dyn FnMut(&WorkerRequest, &Arc<QueryCancellation>) -> Result<WorkerResponse> + Send>;

/// Runs INSIDE the worker thread. The production executor owns a `Db` that must be opened
/// there, so it cannot be constructed on the UI thread and moved in; tests return a closure
/// directly. Reports the observed access scope on success so the caller can assert it (P5).
type ExecutorFactory = Box<dyn FnOnce() -> Result<(SearchExecutor, EffectiveAccessScope)> + Send>;

#[derive(Default)]
struct PendingWork {
    search: Option<WorkerRequest>,
    preview: Option<WorkerRequest>,
    in_flight: Option<(RequestKind, Arc<QueryCancellation>)>,
    closed: bool,
}

impl PendingWork {
    /// The one pending slot for `kind`. Constant capacity per kind is the mailbox's contract,
    /// so the slot is named here rather than matched at each call site.
    fn slot_mut(&mut self, kind: RequestKind) -> &mut Option<WorkerRequest> {
        match kind {
            RequestKind::Search => &mut self.search,
            RequestKind::PreviewOnly => &mut self.preview,
        }
    }

    /// Take the in-flight cancellation out of the slot before raising it. One owner, one order:
    /// the interrupt handle is connection-scoped, so clearing and cancelling must stay atomic
    /// under the mailbox lock or a cancel could race its successor's publication.
    fn cancel_in_flight(&mut self) {
        if let Some((_, cancellation)) = self.in_flight.take() {
            cancellation.cancel();
        }
    }

    /// Cancel the in-flight request only when it is `target`. Navigation supersedes an older
    /// preview scan this way without ever cancelling an in-flight search (C21).
    fn cancel_in_flight_if(&mut self, target: RequestKind) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|(kind, _)| *kind == target)
        {
            self.cancel_in_flight();
        }
    }
}

/// Constant-capacity latest-value mailbox. At most one Search and one PreviewOnly are retained,
/// so a Q-character paste keeps O(Q + F) bytes for the latest query/filters instead of every
/// cumulative prefix (Theta(Q^2) query bytes in the former unbounded FIFO).
struct WorkerMailbox {
    state: Mutex<PendingWork>,
    wake: Condvar,
}

impl WorkerMailbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(PendingWork::default()),
            wake: Condvar::new(),
        }
    }

    fn send(&self, request: WorkerRequest) -> std::result::Result<(), String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(
                "the search worker stopped; press q to quit and rerun aise tui".to_string(),
            );
        }
        let kind = request.kind;
        match kind {
            // A newer search supersedes whatever is running, search or preview: its result set
            // replaces the list either way.
            RequestKind::Search => state.cancel_in_flight(),
            // Navigation B supersedes preview A's transcript scan, but it must never cancel an
            // in-flight Search (the result-list contract covered by C21).
            RequestKind::PreviewOnly => state.cancel_in_flight_if(RequestKind::PreviewOnly),
        }
        *state.slot_mut(kind) = Some(request);
        self.wake.notify_one();
        Ok(())
    }

    /// Drop the pending request of one kind and stop its in-flight run. Used by the two callers
    /// that abandon work without replacing it: a rejected filter combination, and a preview whose
    /// row is already on screen.
    fn cancel_kind(&self, target: RequestKind) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.slot_mut(target).take();
        state.cancel_in_flight_if(target);
    }

    fn next(&self) -> Option<(WorkerRequest, Arc<QueryCancellation>)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.closed && state.search.is_none() && state.preview.is_none() {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.closed {
            return None;
        }
        let request = state.search.take().or_else(|| state.preview.take())?;
        let cancellation = Arc::new(QueryCancellation::new());
        state.in_flight = Some((request.kind, Arc::clone(&cancellation)));
        Some((request, cancellation))
    }

    fn finish(&self, cancellation: &Arc<QueryCancellation>, search_succeeded: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .in_flight
            .as_ref()
            .is_some_and(|(_, current)| Arc::ptr_eq(current, cancellation))
        {
            state.in_flight.take();
        }
        // A successful result replacement makes a queued old-list preview obsolete; AppState
        // requests the selected new row after applying results. On failure, preserve the latest
        // navigation preview so the old list and its selected row remain coherent.
        if search_succeeded && state.search.is_none() {
            state.preview.take();
        }
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancel_in_flight();
        state.search.take();
        state.preview.take();
        state.closed = true;
        self.wake.notify_all();
    }

    fn has_work(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.search.is_some() || state.preview.is_some() || state.in_flight.is_some()
    }

    #[cfg(test)]
    fn pending_counts(&self) -> (usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            usize::from(state.search.is_some()),
            usize::from(state.preview.is_some()),
        )
    }
}

struct CloseMailboxOnExit(Arc<WorkerMailbox>);

impl Drop for CloseMailboxOnExit {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// RAII owner for one constant-capacity mailbox, response receiver, and worker thread.
struct SearchWorker {
    mailbox: Arc<WorkerMailbox>,
    responses: mpsc::Receiver<WorkerOutcome>,
    pending_outcomes: Arc<AtomicUsize>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SearchWorker {
    fn send(&self, request: WorkerRequest) -> std::result::Result<(), String> {
        self.mailbox.send(request)
    }

    fn cancel_preview(&self) {
        self.mailbox.cancel_kind(RequestKind::PreviewOnly);
    }

    fn cancel_search(&self) {
        self.mailbox.cancel_kind(RequestKind::Search);
    }

    fn has_work(&self) -> bool {
        self.pending_outcomes.load(Ordering::Acquire) > 0 || self.mailbox.has_work()
    }

    fn acknowledge_outcome(&self) {
        self.pending_outcomes.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.mailbox.close();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn one search worker. Pending request count is O(1); shared mailbox locks are held only for
/// replacement/cancellation bookkeeping and never while SQLite or scoring executes.
fn spawn_search_worker(
    make_executor: ExecutorFactory,
) -> Result<(SearchWorker, EffectiveAccessScope)> {
    let (ready_tx, ready_rx) =
        mpsc::sync_channel::<std::result::Result<EffectiveAccessScope, String>>(1);
    let (response_tx, response_rx) = mpsc::channel::<WorkerOutcome>();
    let pending_outcomes = Arc::new(AtomicUsize::new(0));
    let worker_pending_outcomes = Arc::clone(&pending_outcomes);
    let mailbox = Arc::new(WorkerMailbox::new());
    let worker_mailbox = Arc::clone(&mailbox);
    let handle = thread::Builder::new()
        .name("aise-tui-search".to_string())
        .spawn(move || {
            let _close_on_exit = CloseMailboxOnExit(Arc::clone(&worker_mailbox));
            let mut execute = match make_executor() {
                Ok((executor, scope)) => {
                    let _ = ready_tx.send(Ok(scope));
                    executor
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                    return;
                }
            };
            while let Some((request, cancellation)) = worker_mailbox.next() {
                let result = execute(&request, &cancellation);
                let search_succeeded = request.kind == RequestKind::Search && result.is_ok();
                let publishable = !cancellation.is_cancelled()
                    && !matches!(&result, Err(error) if is_expected_interruption(error));
                // Publish pending state before clearing mailbox in-flight state so the UI never
                // mistakes the handoff gap for a fully idle worker.
                if publishable {
                    worker_pending_outcomes.fetch_add(1, Ordering::Release);
                }
                worker_mailbox.finish(&cancellation, search_succeeded);
                if !publishable || cancellation.is_cancelled() {
                    if publishable {
                        worker_pending_outcomes.fetch_sub(1, Ordering::AcqRel);
                    }
                    continue;
                }
                if response_tx
                    .send(WorkerOutcome {
                        kind: request.kind,
                        generation: request.generation,
                        result,
                    })
                    .is_err()
                {
                    worker_pending_outcomes.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })?;
    let observed = ready_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("the search worker exited during startup"))?
        .map_err(|message| anyhow::anyhow!("the search worker failed to start: {message}"))?;
    Ok((
        SearchWorker {
            mailbox,
            responses: response_rx,
            pending_outcomes,
            handle: Some(handle),
        },
        observed,
    ))
}

/// Two lines, not `message_search_batches`'s predicate: that one is a private bare `fn` and
/// also matches `MessageSearchCancelled` and `ReadSnapshotCleanupError`, neither of which this
/// worker can produce (E22).
fn is_expected_interruption(error: &anyhow::Error) -> bool {
    error.is::<QueryCancelled>()
        || error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(inner, _))
                    if inner.code == rusqlite::ErrorCode::OperationInterrupted
            )
        })
}

/// The production executor factory: opens the worker's OWN read-only connection — the
/// message-search worker's recipe (read-only unconditionally, access scope set explicitly,
/// schema version ensured, progress reporter deliberately unset so nothing writes over the
/// alternate screen) — and routes search/list through `CatalogService` (D1), the same seam
/// the CLI, MCP, and Python use. Preview bookends use the canonical transcript projection shared
/// by CLI, MCP, Python/export, and session search; normalized message rows are not substituted
/// because they can contain harness notices or
/// generated mixed-content parts intentionally excluded from that projection.
///
/// Complexity (REQ010): one worker connection (≤64 MiB page-cache ceiling, 256 MiB virtual mmap
/// window) in addition to the caller's idle connection, sharing one `config.resolve_threads()`
/// Rayon pool process-wide. Search/list delegate to their documented
/// bounds. Preview scans canonical transcript bytes once and retains only four borrowed bookend
/// spans plus rendered output, instead of cloning the transcript and collecting every turn.
fn db_backed_executor(
    config: Config,
    access: EffectiveAccessScope,
    runtime: Arc<ExecutionRuntime>,
    style: TerminalStyle,
) -> ExecutorFactory {
    Box::new(move || {
        let worker_threads = NonZeroUsize::new(config.resolve_threads())
            .expect("Config::resolve_threads always returns at least one");
        debug_assert_eq!(runtime.worker_threads(), worker_threads.get());
        let mut db = Db::open_existing_read_only_with_runtime(
            &config.db_path(),
            config.index.busy_timeout_ms,
            runtime,
        )?;
        db.set_access_scope(access);
        let schema_version = db.schema_version()?;
        anyhow::ensure!(
            schema_version <= SCHEMA_VERSION,
            "the index uses schema generation {schema_version}, newer than this aise build \
             supports ({SCHEMA_VERSION}); upgrade aise before opening it"
        );
        anyhow::ensure!(
            schema_version >= MIN_READABLE_SCHEMA_VERSION,
            "the TUI search worker requires readable database schema generation \
             {MIN_READABLE_SCHEMA_VERSION} or newer, got {schema_version}; \
             run `aise reindex --full`, then retry"
        );
        let observed = db.access_scope().clone();
        let repo = current_repo(&config);
        let scoring = config.search.scoring.clone();
        let preview_budget = config.ui.preview_body_lines;
        Ok((
            Box::new(
                move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                    // The worker created and published this; the executor only installs it on
                    // its own connection.
                    db.install_query_cancellation(cancellation)?;
                    // Constructed per call: the closure owns `db`, so it cannot also store a
                    // CatalogService borrowing it. Copy newtype over a reference; free.
                    let catalog = CatalogService::new(&db);
                    match request.kind {
                        RequestKind::Search if request.query.trim().is_empty() => {
                            Ok(WorkerResponse::results(
                                request,
                                catalog.list_sessions(&request.filters)?,
                            ))
                        }
                        RequestKind::Search => Ok(WorkerResponse::results(
                            request,
                            catalog
                                .search_sessions_cancellable(
                                    &request.query,
                                    &request.filters,
                                    repo.as_deref(),
                                    &scoring,
                                    cancellation,
                                )?
                                .into_iter()
                                .map(|hit| hit.session)
                                .collect(),
                        )),
                        RequestKind::PreviewOnly => {
                            let text = match request.selected_id.as_deref() {
                                Some(id) => Some(db.inspect_session_transcript(
                                    id,
                                    cancellation,
                                    |session, transcript| {
                                        let summary = build_transcript_summary_cancellable(
                                            transcript,
                                            preview_budget,
                                            style,
                                            cancellation,
                                        )?;
                                        Ok(format!(
                                            "Session: {}\nCWD: {}\n\n{}",
                                            session.id,
                                            session.cwd.as_deref().unwrap_or("-"),
                                            summary
                                        ))
                                    },
                                )?),
                                None => None,
                            };
                            Ok(WorkerResponse::preview(request, text))
                        }
                    }
                },
            ) as SearchExecutor,
            observed,
        ))
    })
}

struct AppState {
    config: Config,
    /// The session filters the browser runs with. The state IS a `SearchFilters` value: no
    /// parallel filter representation exists (R8/§5.5d).
    filters: SearchFilters,
    /// Search worker: owns every database query. The UI thread never blocks on it — it sends
    /// requests and drains responses (§2.3).
    worker: SearchWorker,
    query: String,
    /// Where the next typed character goes, as a byte index into `query` on a character
    /// boundary. The box was append-only, so a typo in the middle meant deleting back to it.
    query_cursor: usize,
    search_mode: bool,
    current_search_generation: RequestGeneration,
    current_preview_generation: RequestGeneration,
    /// When the query was last edited with no search issued for it yet. `step` turns it into one
    /// request once typing has been quiet for `[ui].search_debounce_ms`, so a typed burst costs
    /// one search instead of one cancelled scan per character. `None` means nothing is pending.
    query_edited_at: Option<std::time::Instant>,
    /// True from request submission until the matching search success/error is applied. Rendered
    /// in the list title so the real-terminal benchmark can observe final-generation completion.
    searching: bool,
    preview_loading: bool,
    selected: usize,
    results: Vec<SessionRecord>,
    preview: String,
    preview_scroll: u16,
    /// The preview's height in terminal rows after word wrapping, recorded by `render`. The
    /// worker cannot supply it: wrapping depends on the pane width, so this is the only count
    /// the scroll bound may use.
    preview_line_count: usize,
    /// The session whose preview is currently rendered: the skip guard for rescanning the
    /// canonical transcript per keystroke, and the match check that discards overtaken output.
    previewed_id: Option<String>,
    /// The preview pane's interior height, recorded by `render`, so scroll clamping bounds by
    /// viewport, not just content length (D11). Zero until the first draw; the clamp's slack
    /// floor covers that case.
    preview_viewport_rows: u16,
    /// Last error from a keystroke-triggered operation, shown on its own line. A key press
    /// can never abort the TUI: errors land here instead of propagating through `?`.
    error: Option<String>,
    /// Operation that owns `error`; worker/lifecycle failures use `None`. Search and preview
    /// successes clear only their own failure, so one operation cannot erase another's evidence.
    error_owner: Option<RequestKind>,
    /// A disconnected response producer is terminal for this worker. Remember reporting it so
    /// later active/idle drains do not redraw the same error forever.
    worker_disconnected_reported: bool,
    /// True after one interrupt, cleared by any other key. The next one quits. It is a field
    /// rather than a timer so the state is exactly what the status bar shows.
    interrupt_armed: bool,
    /// True while the key list covers the screen. It scrolls with the preview scroll keys, and
    /// any other key closes it, so nothing else has to be bound to leave.
    showing_help: bool,
    help_scroll: u16,
    /// The key list's interior height, recorded by `render` so its scroll clamps by viewport.
    help_viewport_rows: u16,
    /// What this terminal can draw, resolved once at startup from `[ui].unicode`, `[ui].color`,
    /// and the environment. Every symbol and colour the browser emits comes from here.
    style: TerminalStyle,
}

impl AppState {
    fn new(config: Config, worker: SearchWorker) -> Result<Self> {
        let mut state = Self::new_quiet(config, worker);
        // The initial empty-query search runs on the worker: the first frame draws before the
        // first query completes, and the startup response populates the list (C28).
        state.request_search();
        Ok(state)
    }

    /// Construct without the startup request — the test harness path, where the first
    /// request must wait until the fixture rows are seeded so the startup response and the
    /// seeded state agree.
    fn new_quiet(config: Config, worker: SearchWorker) -> Self {
        let result_limit = tui_result_limit(config.search.default_limit);
        let style = TerminalStyle::resolve(config.ui.unicode, config.ui.color);
        Self {
            config,
            filters: SearchFilters {
                provider: None,
                path_prefix: None,
                exclude_path_prefixes: Vec::new(),
                exclude_session_ids: Vec::new(),
                // Every class, matching the CLI default: the browser shows what is indexed.
                session_kinds: None,
                parent_session_id: None,
                since: None,
                until: None,
                limit: result_limit,
                warnings_only: false,
            },
            worker,
            query: String::new(),
            query_cursor: 0,
            search_mode: false,
            current_search_generation: RequestGeneration::new(),
            current_preview_generation: RequestGeneration::new(),
            query_edited_at: None,
            searching: false,
            preview_loading: false,
            selected: 0,
            results: Vec::new(),
            preview: String::new(),
            preview_scroll: 0,
            preview_line_count: 0,
            previewed_id: None,
            preview_viewport_rows: 0,
            error: None,
            error_owner: None,
            worker_disconnected_reported: false,
            interrupt_armed: false,
            showing_help: false,
            help_scroll: 0,
            help_viewport_rows: 0,
            style,
        }
    }

    /// Handle one key press. Returns `Some(action)` when the loop should stop. Never returns
    /// `Err`: a database failure becomes `self.error` — a keystroke cannot end the TUI.
    ///
    /// What each key means comes from `[ui.keys]`, so the modifiers are part of the match. They
    /// were not: every arm matched a bare character, and Ctrl+Q quit, Ctrl+S moved the time
    /// window, Ctrl+P changed the provider, and Ctrl+H — ASCII backspace on many terminals —
    /// scrolled the preview.
    fn handle_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        let mode = if self.search_mode {
            ActionMode::Search
        } else {
            ActionMode::Browse
        };
        let action = self.config.ui.keys.action_for(&key, mode);

        // The interrupt is answered before anything else and in both modes. Raw mode turns off
        // the terminal's own interrupt character, so Ctrl+C arrives as an ordinary key event and
        // no SIGINT is ever raised: before this, browse mode ignored it and the search box typed
        // a literal `c` into the query, leaving `q` and Esc as the only ways out of a
        // full-screen application. One press arms and says so in the status bar; the next one
        // quits, and any other key disarms, so a stray Ctrl+C cannot end a session by itself.
        if action == Some(TuiAction::Interrupt) {
            if self.interrupt_armed {
                return Some(AppAction::Quit);
            }
            self.interrupt_armed = true;
            return None;
        }
        self.interrupt_armed = false;

        // While the key list is up it owns the keyboard, so a reader can read past its end and
        // then leave with whatever key they reach for. Only scrolling keeps it open.
        if self.showing_help {
            match action {
                Some(TuiAction::PreviewScrollDown) => {
                    let step = saturating_step(self.config.ui.preview_scroll_rows);
                    self.scroll_help(step);
                }
                Some(TuiAction::PreviewScrollUp) => {
                    let step = saturating_step(self.config.ui.preview_scroll_rows);
                    self.scroll_help(-step);
                }
                Some(TuiAction::PreviewPageDown) => {
                    let page = saturating_step(self.config.ui.preview_page_rows);
                    self.scroll_help(page);
                }
                Some(TuiAction::PreviewPageUp) => {
                    let page = saturating_step(self.config.ui.preview_page_rows);
                    self.scroll_help(-page);
                }
                _ => {
                    self.showing_help = false;
                    self.help_scroll = 0;
                }
            }
            return None;
        }

        match action {
            Some(TuiAction::Interrupt) => unreachable!("answered above"),
            Some(TuiAction::Quit) => return Some(AppAction::Quit),
            Some(TuiAction::EnterSearch) => {
                self.search_mode = true;
                // Resuming an existing query puts the caret where a reader would expect to
                // continue typing it.
                self.query_cursor = self.query.len();
            }
            Some(TuiAction::LeaveSearch) => {
                self.search_mode = false;
                // Leaving the box searches whatever is in it now. Waiting out the delay after
                // an explicit Enter is the one case where the reader has already said they are
                // done typing.
                self.flush_edited_query();
            }
            Some(TuiAction::MoveDown) => self.move_selection(1),
            Some(TuiAction::MoveUp) => self.move_selection(-1),
            Some(TuiAction::PageDown) => {
                let page = saturating_step(self.config.ui.list_page_rows);
                self.move_selection(page);
            }
            Some(TuiAction::PageUp) => {
                let page = saturating_step(self.config.ui.list_page_rows);
                self.move_selection(-page);
            }
            Some(TuiAction::Top) => self.select_index(0),
            Some(TuiAction::Bottom) => {
                let last = self.results.len().saturating_sub(1);
                self.select_index(last);
            }
            Some(TuiAction::CycleProvider) => self.cycle_provider(),
            Some(TuiAction::CycleSessionKind) => self.cycle_session_kinds(),
            Some(TuiAction::CycleTimeWindow) => self.cycle_since_window(),
            Some(TuiAction::ToggleWarningsOnly) => self.toggle_warnings_only(),
            Some(TuiAction::PreviewScrollDown) => {
                let step = saturating_step(self.config.ui.preview_scroll_rows);
                self.scroll_preview(step);
            }
            Some(TuiAction::PreviewScrollUp) => {
                let step = saturating_step(self.config.ui.preview_scroll_rows);
                self.scroll_preview(-step);
            }
            Some(TuiAction::PreviewPageDown) => {
                let page = saturating_step(self.config.ui.preview_page_rows);
                self.scroll_preview(page);
            }
            Some(TuiAction::PreviewPageUp) => {
                let page = saturating_step(self.config.ui.preview_page_rows);
                self.scroll_preview(-page);
            }
            Some(TuiAction::Help) => {
                self.showing_help = true;
                self.help_scroll = 0;
            }
            Some(TuiAction::Resume) => {
                if let Some(selected) = self.selected_session() {
                    return Some(AppAction::Resume(Box::new(selected.clone())));
                }
            }
            Some(TuiAction::ClearQuery) => self.clear_query(),
            Some(TuiAction::DeleteBackward) => self.delete_backward(),
            Some(TuiAction::DeleteForward) => self.delete_forward(),
            Some(TuiAction::DeleteWordBackward) => self.delete_word_backward(),
            Some(TuiAction::CursorLeft) => {
                let to = Self::boundary_before(&self.query, self.query_cursor);
                self.move_cursor(to);
            }
            Some(TuiAction::CursorRight) => {
                let to = Self::boundary_after(&self.query, self.query_cursor);
                self.move_cursor(to);
            }
            Some(TuiAction::CursorStart) => self.move_cursor(0),
            Some(TuiAction::CursorEnd) => {
                let to = self.query.len();
                self.move_cursor(to);
            }
            // Not a binding in this mode. In the search box an unmodified character is text;
            // anywhere else, and for a chord that happens to be unbound, it is nothing.
            None => {
                if self.search_mode {
                    if let KeyCode::Char(character) = key.code {
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                        ) {
                            self.insert_character(character);
                        }
                    }
                }
            }
        }
        None
    }

    /// Absorb finished worker responses. Runs at the top of every loop turn, so the UI never
    /// blocks on the worker; a response for a superseded query is dropped without touching
    /// state (P3).
    ///
    /// Complexity (REQ010): `O(responses × K)` for the id lookup; no I/O, no lock held.
    fn drain_responses(&mut self) -> bool {
        let mut applied = false;
        loop {
            match self.worker.responses.try_recv() {
                Ok(outcome) => {
                    self.worker.acknowledge_outcome();
                    applied |= self.apply_outcome(outcome);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.worker_disconnected_reported {
                        self.worker_disconnected_reported = true;
                        self.searching = false;
                        self.preview_loading = false;
                        self.error = Some(
                            "the search worker stopped; press q to quit and rerun aise tui"
                                .to_string(),
                        );
                        self.error_owner = None;
                        applied = true;
                    }
                    break;
                }
            }
        }
        applied
    }

    /// Apply only the current operation generation. Query equality remains useful presentation
    /// context, but it is not request identity: filter-only changes keep the same query (R9-F1).
    fn apply_outcome(&mut self, outcome: WorkerOutcome) -> bool {
        let current = match outcome.kind {
            RequestKind::Search => outcome.generation.same_as(&self.current_search_generation),
            RequestKind::PreviewOnly => {
                outcome.generation.same_as(&self.current_preview_generation)
            }
        };
        if !current {
            return false;
        }
        if outcome.kind == RequestKind::Search {
            self.searching = false;
        } else {
            self.preview_loading = false;
        }
        match outcome.result {
            Ok(response) => self.apply_response(response),
            Err(error) => {
                if outcome.kind == RequestKind::PreviewOnly {
                    let selected = self.selected_session().map(|session| session.id.clone());
                    self.preview = match selected.as_deref() {
                        Some(id) => format!("Preview unavailable for {id}."),
                        None => NO_SESSIONS_PREVIEW.to_string(),
                    };
                    self.previewed_id = selected;
                    self.preview_scroll = 0;
                }
                self.error = Some(format!("{error:#}"));
                self.error_owner = Some(outcome.kind);
            }
        }
        true
    }

    /// The character boundary before `at`, or `at` when it is already the start.
    fn boundary_before(text: &str, at: usize) -> usize {
        text[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    /// The character boundary after `at`, or the end of `text`.
    fn boundary_after(text: &str, at: usize) -> usize {
        text[at..]
            .char_indices()
            .nth(1)
            .map_or(text.len(), |(offset, _)| at + offset)
    }

    /// The start of the run of non-space characters that ends at `at`, skipping any spaces
    /// immediately before it, so a second press deletes the word rather than the gap.
    fn word_start_before(text: &str, at: usize) -> usize {
        let mut cursor = at;
        while cursor > 0 {
            let previous = Self::boundary_before(text, cursor);
            if !text[previous..cursor].chars().all(char::is_whitespace) {
                break;
            }
            cursor = previous;
        }
        while cursor > 0 {
            let previous = Self::boundary_before(text, cursor);
            if text[previous..cursor].chars().all(char::is_whitespace) {
                break;
            }
            cursor = previous;
        }
        cursor
    }

    fn insert_character(&mut self, character: char) {
        self.query.insert(self.query_cursor, character);
        self.query_cursor += character.len_utf8();
        self.note_query_edit();
    }

    fn delete_backward(&mut self) {
        if self.query_cursor == 0 {
            return;
        }
        let start = Self::boundary_before(&self.query, self.query_cursor);
        self.query.replace_range(start..self.query_cursor, "");
        self.query_cursor = start;
        self.note_query_edit();
    }

    fn delete_forward(&mut self) {
        if self.query_cursor >= self.query.len() {
            return;
        }
        let end = Self::boundary_after(&self.query, self.query_cursor);
        self.query.replace_range(self.query_cursor..end, "");
        self.note_query_edit();
    }

    fn delete_word_backward(&mut self) {
        let start = Self::word_start_before(&self.query, self.query_cursor);
        if start == self.query_cursor {
            return;
        }
        self.query.replace_range(start..self.query_cursor, "");
        self.query_cursor = start;
        self.note_query_edit();
    }

    fn clear_query(&mut self) {
        if self.query.is_empty() {
            return;
        }
        self.query.clear();
        self.query_cursor = 0;
        self.note_query_edit();
    }

    /// Moving the cursor is not an edit: it changes nothing to search for, so it must not start
    /// a search or the delay would restart on every arrow key.
    fn move_cursor(&mut self, to: usize) {
        self.query_cursor = to.min(self.query.len());
    }

    /// Record that the query changed. The search itself waits for `[ui].search_debounce_ms` of
    /// quiet, so a typed word costs one search rather than one per character; the keystroke
    /// still echoes on this turn either way.
    ///
    /// `searching` goes true here rather than at submission, because from the reader's side the
    /// search has begun: the list title says so, and the loop starts taking its short wait
    /// slices so the result is picked up as soon as it lands.
    fn note_query_edit(&mut self) {
        self.query_edited_at = Some(std::time::Instant::now());
        self.searching = true;
        if self.config.ui.search_debounce_ms == 0 {
            self.flush_edited_query();
        }
    }

    /// Issue the pending query's search now, if one is pending.
    fn flush_edited_query(&mut self) {
        if self.query_edited_at.take().is_some() {
            self.request_search();
        }
    }

    /// How long until the pending query is due to be searched for, if one is pending. `None`
    /// means nothing is waiting; `Some(ZERO)` means it is due now.
    fn edited_query_delay(&self, now: std::time::Instant) -> Option<Duration> {
        let edited = self.query_edited_at?;
        Some(
            Duration::from_millis(self.config.ui.search_debounce_ms)
                .saturating_sub(now.saturating_duration_since(edited)),
        )
    }

    /// Queue a search for the current query. `send` cancels any in-flight search first —
    /// this is what makes supersede real (D3). Never `?`: a dead worker sets the error line.
    fn request_search(&mut self) {
        let generation = RequestGeneration::new();
        self.current_search_generation = generation.clone();
        self.searching = true;
        self.preview_loading = false;
        if let Err(message) = self.worker.send(WorkerRequest {
            kind: RequestKind::Search,
            generation,
            query: self.query.clone(),
            filters: self.filters.clone(),
            selected_id: self.selected_session().map(|session| session.id.clone()),
        }) {
            self.searching = false;
            self.error = Some(message);
            self.error_owner = Some(RequestKind::Search);
        }
    }

    /// Ask the worker for the selected row's preview, unless it is already on screen.
    ///
    /// The skip is load-bearing, not an optimisation: every search response calls this, and
    /// without it a preserved selection (D4) would rescan the transcript per keystroke.
    /// A replacement preview cancels only an older preview, never a Search (C21), so held
    /// navigation remains latest-value bounded without invalidating the result list.
    fn request_preview(&mut self) {
        let selected = self.selected_session().map(|session| session.id.clone());
        // Invalidate any outstanding preview success/error even when the already-rendered row is
        // selected again and no replacement I/O is needed (A→B→A fast path).
        let generation = RequestGeneration::new();
        self.current_preview_generation = generation.clone();
        if selected == self.previewed_id && self.error_owner != Some(RequestKind::PreviewOnly) {
            self.worker.cancel_preview();
            self.preview_loading = false;
            return;
        }
        self.preview_loading = true;
        if let Err(message) = self.worker.send(WorkerRequest {
            kind: RequestKind::PreviewOnly,
            generation,
            query: self.query.clone(),
            filters: self.filters.clone(),
            selected_id: selected,
        }) {
            self.preview_loading = false;
            self.error = Some(message);
            self.error_owner = Some(RequestKind::PreviewOnly);
        }
    }

    /// Revalidate after a filter binding and re-run the search. A rejected combination is
    /// rendered on the error line, never sent — the request would be unsatisfiable.
    fn apply_filter_change(&mut self) {
        if let Err(error) = self.filters.validate() {
            self.current_search_generation = RequestGeneration::new();
            self.worker.cancel_search();
            self.searching = false;
            self.error = Some(format!("{error:#}"));
            self.error_owner = Some(RequestKind::Search);
            return;
        }
        self.request_search();
    }

    /// Cycle the provider filter through `value_variants()` and back to unbounded.
    fn cycle_provider(&mut self) {
        let variants = Provider::value_variants();
        self.filters.provider = match self.filters.provider {
            None => Some(variants[0]),
            Some(current) => variants
                .iter()
                .position(|provider| *provider == current)
                .and_then(|index| variants.get(index + 1).copied()),
        };
        self.apply_filter_change();
    }

    /// Cycle the session-class filter: unbounded → both classes (the default search set) →
    /// user → subagent → unbounded. A kind set, never per-class booleans (the field's doc
    /// contract in models.rs).
    fn cycle_session_kinds(&mut self) {
        let both = SessionKind::default_search_set();
        self.filters.session_kinds = match &self.filters.session_kinds {
            None => Some(both),
            Some(kinds) if *kinds == both => Some(vec![SessionKind::User]),
            Some(kinds) if kinds.as_slice() == [SessionKind::User] => {
                Some(vec![SessionKind::Subagent])
            }
            _ => None,
        };
        self.apply_filter_change();
    }

    /// Cycle the time window: unbounded → 1 day → 7 days → 30 days → unbounded. Only `since`
    /// is set; `until` stays open, so the newest sessions always qualify.
    fn cycle_since_window(&mut self) {
        let next = match active_since_window(self.filters.since) {
            None => Some(0),
            Some(index) => Some(index + 1).filter(|next| *next < SINCE_WINDOWS.len()),
        };
        self.filters.since =
            next.map(|index| Utc::now() - chrono::Duration::hours(SINCE_WINDOWS[index].1));
        self.apply_filter_change();
    }

    fn toggle_warnings_only(&mut self) {
        self.filters.warnings_only = !self.filters.warnings_only;
        self.apply_filter_change();
    }

    /// True while nothing has been asked for: no query and every filter at its opening value.
    /// An empty list then means an empty index, which no key in here can fix.
    fn nothing_has_been_narrowed(&self) -> bool {
        self.query.is_empty()
            && self.filters.provider.is_none()
            && self.filters.session_kinds.is_none()
            && self.filters.since.is_none()
            && !self.filters.warnings_only
    }

    /// What to press when the list is empty.
    ///
    /// The keys are read from the bindings for the same reason the status bar reads them: a
    /// message that names `/` after a reader has rebound it sends them to press something that
    /// does nothing, which is worse than saying nothing. An empty index is a separate case and
    /// gets the command that fills it, because no key in the browser will.
    fn empty_state_guidance(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if self.nothing_has_been_narrowed() {
            lines.push("No sessions are indexed yet.".to_string());
            lines.push(String::new());
            lines.push("Nothing here was filtered out — the index is empty. Leave the".to_string());
            lines.push("browser and run `aise reindex`, then start it again.".to_string());
        } else {
            lines.push("No sessions matched.".to_string());
            lines.push(String::new());
            lines.push("The query and filters in use are shown in the status bar.".to_string());
            lines.push(String::new());
            let narrowing: [(&str, &[TuiAction]); 2] = [
                ("edit the query", &[TuiAction::EnterSearch]),
                (
                    "change filters",
                    &[
                        TuiAction::CycleProvider,
                        TuiAction::CycleSessionKind,
                        TuiAction::CycleTimeWindow,
                        TuiAction::ToggleWarningsOnly,
                    ],
                ),
            ];
            for (label, actions) in narrowing {
                if let Some(hint) = self.key_hint(label, actions) {
                    lines.push(format!("  {hint}"));
                }
            }
        }
        if let Some(hint) = self.key_hint("every key", &[TuiAction::Help]) {
            lines.push(String::new());
            lines.push(format!("  {hint}"));
        }
        lines.join("\n")
    }

    fn filter_status(&self) -> String {
        let provider = self
            .filters
            .provider
            .map_or("any", |provider| provider.as_str());
        let class = match self.filters.session_kinds.as_deref() {
            None => "any",
            Some([SessionKind::User]) => "user",
            Some([SessionKind::Subagent]) => "subagent",
            Some(kinds) if *kinds == SessionKind::default_search_set() => "user+subagent",
            Some(_) => "custom",
        };
        let window =
            active_since_window(self.filters.since).map_or("any", |index| SINCE_WINDOWS[index].0);
        format!(
            "p:{provider} f:{class} s:{window} w:{}",
            if self.filters.warnings_only {
                "on"
            } else {
                "off"
            }
        )
    }

    /// One line per bound command: its keys, then what it does.
    ///
    /// Built from the same table the loop dispatches through, so a rebound key changes the help
    /// and an unbound command does not claim to exist. Without this the only place a command was
    /// named was the status bar, which sheds most of them on an eighty-column frame — paging,
    /// scrolling, top and bottom, and resume were unreachable for anyone who had not read the
    /// configuration file.
    fn help_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (heading, mode) in [
            ("Browsing", ActionMode::Browse),
            ("Search box", ActionMode::Search),
        ] {
            let mut section: Vec<String> = Vec::new();
            for action in TuiAction::ALL {
                if action.mode() != mode {
                    continue;
                }
                let keys = self
                    .config
                    .ui
                    .keys
                    .chords(action)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>();
                if keys.is_empty() {
                    continue;
                }
                section.push(format!("  {:<18} {}", keys.join(", "), action.name()));
            }
            if section.is_empty() {
                continue;
            }
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(format!("{heading}:"));
            lines.extend(section);
        }
        // Both modes answer the interrupt, so it is listed once at the end rather than twice.
        let interrupt = self.config.ui.keys.chords(TuiAction::Interrupt);
        if !interrupt.is_empty() {
            let keys = interrupt
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(String::new());
            lines.push(format!("  {keys:<18} interrupt (twice to quit)"));
        }
        lines
    }

    /// Scroll the key list, bounded the same way the preview is.
    fn scroll_help(&mut self, delta: isize) {
        let lines = self.help_lines().len();
        let visible = usize::from(self.help_viewport_rows).max(PREVIEW_VIEWPORT_SLACK);
        let max = u16::try_from(lines.saturating_sub(visible)).unwrap_or(u16::MAX);
        self.help_scroll = u16::try_from((self.help_scroll as isize).saturating_add(delta).max(0))
            .unwrap_or(u16::MAX)
            .min(max);
    }

    /// The key list's title, which is the only chrome it has.
    ///
    /// The overlay takes the whole frame, so it covers the status bar that would otherwise name
    /// the scroll keys — and the list is longer than a twenty-four-row terminal, so a reader met
    /// a list cut off at `cursor_left` with nothing saying more existed or how to reach it. The
    /// row count and the keys appear only when the list actually overflows, and the keys are read
    /// from the bindings like everywhere else.
    fn help_title(&self, lines: usize) -> String {
        let viewport = usize::from(self.help_viewport_rows);
        if viewport == 0 || lines <= viewport {
            return " Keys (any other key closes) ".to_string();
        }
        let first = usize::from(self.help_scroll).saturating_add(1);
        let last = first.saturating_add(viewport).saturating_sub(1).min(lines);
        let separator = self.style.title_separator();
        let scroll = self
            .key_hint(
                "scroll",
                &[TuiAction::PreviewScrollUp, TuiAction::PreviewScrollDown],
            )
            .map(|hint| format!("{hint} {separator} "))
            .unwrap_or_default();
        format!(
            " Keys {separator} {first}-{last}/{lines} {separator} {scroll}any other key closes "
        )
    }

    /// The preview pane's title, naming the visible rows when there are more than fit.
    ///
    /// Without it the pane gives no sign that the transcript continues below the fold: a reader
    /// who does not already know the scroll keys has no reason to look for them, and one who
    /// does cannot tell whether scrolling would do anything.
    fn preview_title(&self) -> String {
        let viewport = usize::from(self.preview_viewport_rows);
        if viewport == 0 || self.preview_line_count <= viewport {
            return " Preview ".to_string();
        }
        let first = usize::from(self.preview_scroll).saturating_add(1);
        let last = first
            .saturating_add(viewport)
            .saturating_sub(1)
            .min(self.preview_line_count);
        format!(
            " Preview {} {first}-{last}/{} ",
            self.style.title_separator(),
            self.preview_line_count
        )
    }

    /// `"j/k: move"`, built from the keys bound to `actions` rather than written out, so the
    /// bar teaches whatever `[ui.keys]` says. Only each action's first chord is named: the bar
    /// is one row, and `j` teaches the binding as well as `j/down` does. An action nobody bound
    /// is not advertised.
    fn key_hint(&self, label: &str, actions: &[TuiAction]) -> Option<String> {
        let keys = actions
            .iter()
            .filter_map(|action| self.config.ui.keys.chords(*action).first())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        (!keys.is_empty()).then(|| format!("{}: {label}", keys.join("/")))
    }

    /// Apply one worker response. Search responses replace the list and preserve the user's
    /// place; preview responses never replace the list (C13) and are discarded when overtaken
    /// by newer navigation.
    fn apply_response(&mut self, response: WorkerResponse) {
        if let Some(results) = response.results {
            // Search and preview failures are independent. A success clears only its own error,
            // so a preview completion cannot hide a failed search (or vice versa).
            if self.error_owner == Some(RequestKind::Search) {
                self.error = None;
                self.error_owner = None;
            }
            let keep = self.selected_session().map(|session| session.id.clone());
            self.results = results;
            // D4: keep the user's place when the same session survives into the new set.
            self.selected = keep
                .and_then(|id| self.results.iter().position(|session| session.id == id))
                .unwrap_or(0);
            // The worker could not know which row preservation would land on, so ask for its
            // preview now — unless D4 retained the already-previewed row, which
            // request_preview's skip guard turns into a no-op (C28).
            self.request_preview();
        }
        if let Some(preview) = response.preview {
            if response.previewed_id != self.selected_session().map(|s| s.id.clone()) {
                return;
            }
            if self.error_owner == Some(RequestKind::PreviewOnly) {
                self.error = None;
                self.error_owner = None;
            }
            let same_session = response.previewed_id == self.previewed_id;
            self.preview = preview;
            self.previewed_id = response.previewed_id;
            // A new row starts at the top. The same row keeps its place: the next draw
            // recomputes the wrapped row count and clamps against it, and every path that
            // applies a response draws before the next key is handled.
            if !same_session {
                self.preview_scroll = 0;
            }
        }
    }

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

        // The search box draws the terminal's own cursor rather than a block character. Ratatui
        // shows it wherever a frame asks and hides it otherwise, so this needs no glyph, works
        // on a terminal whose encoding has none, and blinks and takes its shape from the
        // reader's own settings.
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
        // A query wider than the box scrolls under it so the caret stays in view. Without this
        // the text a reader is typing disappears past the right border, cursor and all.
        let search_interior = chunks[0].width.saturating_sub(2);
        let before_cursor = u16::try_from(UnicodeWidthStr::width(&self.query[..self.query_cursor]))
            .unwrap_or(u16::MAX);
        let horizontal_scroll =
            before_cursor.saturating_sub(search_interior.saturating_sub(1).max(1));
        let top = Paragraph::new(self.query.clone())
            .scroll((0, horizontal_scroll))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(self.style.border_set())
                    .border_style(search_border_style)
                    .title(search_title),
            );
        frame.render_widget(top, chunks[0]);
        if self.search_mode && search_interior > 0 && !self.showing_help {
            frame.set_cursor_position((
                chunks[0].x + 1 + before_cursor.saturating_sub(horizontal_scroll),
                chunks[0].y + 1,
            ));
        }

        // The list pane's share comes from [ui].list_pane_percent; the structural clamp keeps
        // both panes alive for out-of-range values instead of panicking on 100 - percent.
        let list_percent = self.config.ui.list_pane_percent.clamp(10, 90);
        let middle = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(list_percent),
                Constraint::Percentage(100 - list_percent),
            ])
            .split(chunks[1]);
        // The preview pane's interior height, minus its two border rows — recorded every draw
        // so the scroll clamp bounds by the actual viewport (D11).
        self.preview_viewport_rows = middle[1].height.saturating_sub(2);

        // Session list. Normal panes clamp the configured width up to the longest label; an
        // exceptionally narrow pane clamps to its actual interior because geometry must win when
        // the two requirements cannot both fit.
        let longest_label = longest_provider_label();
        let list_interior_width = usize::from(middle[0].width.saturating_sub(2));
        let label_width = self
            .config
            .ui
            .provider_label_width
            .max(longest_label)
            // A config value cannot request an allocation wider than the actual pane.
            .min(list_interior_width);
        // The marker is drawn on every row's worth of width whether or not that row is the
        // selected one, so that moving the selection does not shift the text sideways.
        let selection_symbol = self.style.selection_symbol();
        let selection_width = UnicodeWidthStr::width(selection_symbol);
        let visible_range = visible_session_range(
            self.results.len(),
            self.selected,
            usize::from(middle[0].height.saturating_sub(2)),
        );
        let items = self.results[visible_range.clone()]
            .iter()
            .map(|session| {
                // Title budget derived from the frame at render time (§6.3): the pane's
                // interior minus the label field and the age suffix — no fixed 74.
                let title_budget = (middle[0].width.saturating_sub(2) as usize)
                    .saturating_sub(label_width + 3)
                    .saturating_sub(AGE_SUFFIX_ALLOWANCE)
                    // The marker column is reserved on every row, selected or not, so the row
                    // has that much less width for its title.
                    .saturating_sub(selection_width);
                let title = session
                    .title
                    .as_deref()
                    .map(|value| truncate_for_display(value, title_budget))
                    .unwrap_or_else(|| session.preview_text.clone());
                let age = relative_age(session.updated_at);
                let (provider_name, provider_color) = provider_label(session.provider);
                let provider_name = truncate_for_display(provider_name, label_width);
                let mut spans = vec![Span::styled(
                    format!("[{provider_name:<label_width$}] "),
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

        // D7: name the active ordering — the empty query lists `updated_at DESC`, a typed
        // query lists score order, and the screen says which.
        let mode = if self.query.trim().is_empty() {
            "recent"
        } else {
            "ranked"
        };
        let activity = if self.worker_disconnected_reported {
            "stopped"
        } else if self.searching {
            "searching"
        } else {
            "ready"
        };
        let dot = self.style.title_separator();
        let list_title = format!(
            " Sessions {dot} {mode} {dot} {activity} ({}/{}) ",
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
                    .border_set(self.style.border_set())
                    .title(list_title),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            // A marker as well as the colour: a monochrome terminal, a colour-blind reader, and
            // a captured log all lose the highlight, and the selected row is the one Enter
            // resumes. `Always` reserves the column so the rows do not shift as it moves.
            .highlight_symbol(selection_symbol)
            .highlight_spacing(HighlightSpacing::Always);
        let mut list_state = ListState::default();
        if !visible_range.is_empty() {
            list_state.select(Some(self.selected - visible_range.start));
        }
        frame.render_stateful_widget(list, middle[0], &mut list_state);

        // Preview with scroll. With nothing in the list there is no session to preview, so the
        // pane carries the way out of the empty state instead of a sentence about it.
        let empty_state;
        let preview_body: &str = if self.results.is_empty() {
            empty_state = self.empty_state_guidance();
            &empty_state
        } else {
            &self.preview
        };
        let preview_body_lines = preview_body
            .lines()
            .map(|line| render_preview_line(line, &self.query, self.style))
            .collect::<Vec<_>>();
        let preview = Paragraph::new(preview_body_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(self.style.border_set()),
            )
            .wrap(Wrap { trim: false });
        // Use the renderer's own WordWrapper rather than width arithmetic: wrapping at word
        // boundaries can produce more rows than ceil(display_width / pane_width). The count has
        // to exist before the title that reports it, and re-attaching a block does not re-wrap:
        // both blocks have the same borders, and a title does not change the interior width.
        //
        // `line_count` is behind ratatui's `unstable-rendered-line-info`, outside its semver
        // guarantee; Cargo.toml records why that is taken knowingly. If a future ratatui drops
        // it, this line is where the build stops, and the replacement has to measure what the
        // renderer produced rather than what the text implies.
        self.preview_line_count = preview.line_count(middle[1].width);
        self.clamp_preview_scroll();
        let preview = preview
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(self.style.border_set())
                    .title(self.preview_title()),
            )
            .scroll((self.preview_scroll, 0));
        frame.render_widget(preview, middle[1]);

        // Error line (own row, taken from the body when present — never the help bar),
        // middle-elided to the frame so the final recovery clause always survives (D9/REQ047).
        let status_index = chunks.len() - 1;
        let frame_width = frame.area().width as usize;
        if let Some(error) = &self.error {
            let error_line = Paragraph::new(Span::styled(
                elide_middle(error.as_str(), frame_width, self.style.ellipsis()),
                Style::default().fg(Color::Red),
            ));
            frame.render_widget(error_line, chunks[status_index - 1]);
        }

        // Status bar (single line, contextual) — also middle-elided to the frame, so the
        // navigation hints at the head and "q: quit" at the tail both survive a narrow frame.
        // Each hint names the keys actually bound to it. Written out, the bar kept teaching the
        // defaults after `[ui.keys]` changed them, which is worse than no bar: a reader presses
        // what it says and nothing happens.
        let mut hints: Vec<(u8, String)> = vec![(1, self.filter_status())];
        let labelled: &[(u8, &str, &[TuiAction])] = if self.search_mode {
            // The box binds eight editing commands and named none of them, so the three a
            // reader is least likely to guess were reachable only by leaving and pressing `?`.
            // Backspace, Delete, and the arrows are left out: they are what a text field does
            // everywhere, and the row is one line. The way out stays priority 0, because a
            // reader who cannot leave the box cannot reach anything else — including `?`,
            // which is a literal `?` in a query and so belongs to browse mode.
            &[
                (1, "type to search", &[]),
                (0, "browse", &[TuiAction::LeaveSearch]),
                (2, "clear", &[TuiAction::ClearQuery]),
                (3, "delete word", &[TuiAction::DeleteWordBackward]),
                (
                    4,
                    "line start/end",
                    &[TuiAction::CursorStart, TuiAction::CursorEnd],
                ),
            ]
        } else {
            &[
                (1, "move", &[TuiAction::MoveDown, TuiAction::MoveUp]),
                (4, "page", &[TuiAction::PageDown, TuiAction::PageUp]),
                (4, "top/bottom", &[TuiAction::Top, TuiAction::Bottom]),
                (
                    3,
                    "scroll",
                    &[TuiAction::PreviewScrollUp, TuiAction::PreviewScrollDown],
                ),
                (
                    2,
                    "filters",
                    &[
                        TuiAction::CycleProvider,
                        TuiAction::CycleSessionKind,
                        TuiAction::CycleTimeWindow,
                        TuiAction::ToggleWarningsOnly,
                    ],
                ),
                (1, "search", &[TuiAction::EnterSearch]),
                (2, "resume", &[TuiAction::Resume]),
                // Above every hint but quit: it is the one that names the rest, so on a frame
                // too narrow for them it is what the reader needs.
                (1, "keys", &[TuiAction::Help]),
                (0, "quit", &[TuiAction::Quit]),
            ]
        };
        for (priority, label, actions) in labelled {
            if actions.is_empty() {
                hints.push((*priority, (*label).to_string()));
            } else if let Some(text) = self.key_hint(label, actions) {
                hints.push((*priority, text));
            }
        }
        // Priority 0 and first, so the one hint that says how to leave cannot be shed to fit.
        if self.interrupt_armed {
            hints.insert(0, (0, "press the interrupt again to quit".to_string()));
        }
        let help_text = fit_status_hints(&hints, frame_width, self.style.hint_separator());
        let bottom = Paragraph::new(Span::styled(
            elide_middle(&help_text, frame_width, self.style.ellipsis()),
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(bottom, chunks[status_index]);

        // Last, over everything, and `Clear` first so the panes underneath do not show through
        // where the list is shorter than the block. It takes the whole frame rather than a
        // centred box: the list is the tallest thing the browser draws, and a terminal with ten
        // rows would otherwise show two of it.
        if self.showing_help {
            let area = frame.area();
            self.help_viewport_rows = area.height.saturating_sub(2);
            let lines = self.help_lines();
            let body = lines.join("\n");
            self.scroll_help(0);
            // The overlay is not wrapped, so ratatui truncates a long row rather than folding it:
            // one entry is one rendered row and the title can count them directly.
            let title = self.help_title(lines.len());
            let overlay = Paragraph::new(body)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_set(self.style.border_set())
                        .title(title),
                )
                .scroll((self.help_scroll, 0));
            frame.render_widget(Clear, area);
            frame.render_widget(overlay, area);
        }

        // One choke point rather than a colour decision at every span. Twenty call sites each
        // remembering to ask would be twenty chances to forget, and the next widget somebody
        // adds would forget by default; clearing the finished frame cannot be bypassed. The
        // attributes stay, so bold and italic still carry the emphasis the colour did.
        if !self.style.color() {
            for cell in frame.buffer_mut().content.iter_mut() {
                cell.set_fg(Color::Reset);
                cell.set_bg(Color::Reset);
            }
        }
    }

    /// Move the selection locally — the UI never waits for the worker (G2) — and ask for the
    /// new row's preview.
    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let new = (self.selected as isize)
            .saturating_add(delta)
            .clamp(0, self.results.len() as isize - 1) as usize;
        if new != self.selected {
            self.selected = new;
            self.preview_scroll = 0;
            self.request_preview();
        }
    }

    fn select_index(&mut self, index: usize) {
        if self.results.is_empty() {
            return;
        }
        let new = index.min(self.results.len() - 1);
        if new != self.selected {
            self.selected = new;
            self.preview_scroll = 0;
            self.request_preview();
        }
    }

    /// The one place the scroll bound is expressed; `apply_response`'s same-session branch
    /// reuses it so the two cannot drift (D11). Bounding by viewport stops the scroll when
    /// the last line reaches the pane bottom — the old content-minus-3 bound let the pane
    /// empty out as you scrolled.
    fn clamp_preview_scroll(&mut self) {
        let visible = usize::from(self.preview_viewport_rows).max(PREVIEW_VIEWPORT_SLACK);
        let max =
            u16::try_from(self.preview_line_count.saturating_sub(visible)).unwrap_or(u16::MAX);
        self.preview_scroll = self.preview_scroll.min(max);
    }

    fn scroll_preview(&mut self, delta: isize) {
        self.preview_scroll =
            u16::try_from((self.preview_scroll as isize).saturating_add(delta).max(0))
                .unwrap_or(u16::MAX);
        self.clamp_preview_scroll();
    }

    fn selected_session(&self) -> Option<&SessionRecord> {
        self.results.get(self.selected)
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

#[derive(Clone, Copy)]
struct Turn<'a> {
    ordinal: usize,
    body: &'a str,
}

#[cfg(test)]
struct ParsedTurn<'a> {
    role: TurnRole,
    body: &'a str,
}

#[cfg(test)]
fn parse_turns(transcript: &str) -> Vec<ParsedTurn<'_>> {
    parse_turns_inner(transcript, None).expect("an uncancelled parse cannot fail")
}

#[cfg(test)]
fn parse_turns_inner<'a>(
    transcript: &'a str,
    cancellation: Option<&QueryCancellation>,
) -> Result<Vec<ParsedTurn<'a>>> {
    let mut turns: Vec<ParsedTurn<'a>> = Vec::new();
    let mut current_role: Option<TurnRole> = None;
    let mut body_start: usize = 0;

    let mut cursor = 0usize;
    while cursor < transcript.len() {
        if let Some(cancellation) = cancellation {
            cancellation.ensure_active()?;
        }
        let line_end = transcript[cursor..]
            .find('\n')
            .map(|i| cursor + i)
            .unwrap_or(transcript.len());
        let line = &transcript[cursor..line_end];
        if let Some(role) = TurnRole::parse(line) {
            if let Some(prev) = current_role.take() {
                let body = transcript[body_start..cursor].trim_matches('\n');
                turns.push(ParsedTurn { role: prev, body });
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
        turns.push(ParsedTurn { role: prev, body });
    }
    if let Some(cancellation) = cancellation {
        cancellation.ensure_active()?;
    }
    Ok(turns)
}

#[cfg(test)]
fn truncate_body(body: &str, max_lines: usize) -> String {
    truncate_body_inner(body, max_lines, None).expect("an uncancelled copy cannot fail")
}

fn push_cancellable(
    output: &mut String,
    text: &str,
    cancellation: Option<&QueryCancellation>,
) -> Result<()> {
    let mut start = 0;
    while start < text.len() {
        if let Some(cancellation) = cancellation {
            cancellation.ensure_active()?;
        }
        let mut end = start
            .saturating_add(TRANSCRIPT_CANCELLATION_CHUNK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    Ok(())
}

fn trim_end_cancellable<'a>(
    body: &'a str,
    cancellation: Option<&QueryCancellation>,
) -> Result<&'a str> {
    let mut end = body.len();
    let mut checked_at = end;
    for (index, character) in body.char_indices().rev() {
        if checked_at.saturating_sub(index) >= TRANSCRIPT_CANCELLATION_CHUNK_BYTES {
            if let Some(cancellation) = cancellation {
                cancellation.ensure_active()?;
            }
            checked_at = index;
        }
        if character.is_whitespace() {
            end = index;
        } else {
            break;
        }
    }
    Ok(&body[..end])
}

fn truncate_body_inner(
    body: &str,
    max_lines: usize,
    cancellation: Option<&QueryCancellation>,
) -> Result<String> {
    let trimmed = trim_end_cancellable(body, cancellation)?;
    if trimmed.is_empty() {
        return Ok("(empty)".to_string());
    }
    let mut lines = Vec::with_capacity(max_lines.min(64));
    let mut cursor = 0;
    while cursor < trimmed.len() && lines.len() < max_lines {
        let line_end = next_transcript_line_end(trimmed, cursor, cancellation)?;
        lines.push(&trimmed[cursor..line_end]);
        cursor = if line_end == trimmed.len() {
            trimmed.len()
        } else {
            line_end + 1
        };
    }
    if cursor == trimmed.len() {
        let mut output = String::with_capacity(trimmed.len());
        push_cancellable(&mut output, trimmed, cancellation)?;
        return Ok(output);
    }
    let selected_bytes = lines.iter().fold(0_usize, |bytes, line| {
        bytes.saturating_add(line.len()).saturating_add(1)
    });
    let mut output = String::with_capacity(selected_bytes);
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        push_cancellable(&mut output, line, cancellation)?;
    }
    output.push_str("\n  […]");
    Ok(output)
}

/// Relative weights for the preview summary's sections (Decision 2a): first prompt, first
/// reply, final prompt, final reply. `[ui].preview_body_lines` is the total body budget; each
/// emitted section gets a proportional share (floor-rounded), floored at one line so a small
/// budget cannot erase a bookend. The historical fixed layout was these weights verbatim —
/// a budget of 34 reproduces it exactly.
/// The preview's section headings, without the rule that surrounds them: the rule is a symbol
/// the terminal may not carry, so it is supplied at build time and matched on at render time.
const PREVIEW_SECTION_LABELS: [&str; 4] =
    ["First prompt", "First reply", "Final prompt", "Final reply"];

const PREVIEW_WEIGHT_FIRST_PROMPT: usize = 8;
const PREVIEW_WEIGHT_FIRST_REPLY: usize = 4;
const PREVIEW_WEIGHT_FINAL_PROMPT: usize = 8;
const PREVIEW_WEIGHT_FINAL_REPLY: usize = 14;
const HISTORICAL_PREVIEW_BODY_LINES: usize = PREVIEW_WEIGHT_FIRST_PROMPT
    + PREVIEW_WEIGHT_FIRST_REPLY
    + PREVIEW_WEIGHT_FINAL_PROMPT
    + PREVIEW_WEIGHT_FINAL_REPLY;
/// Maximum transcript bytes scanned between cooperative cancellation checks. Shared with the
/// caseless matcher and whitespace compaction so all three scans answer a cancel on the same
/// bound; see [`crate::util::CANCELLATION_CHECK_BYTES`].
const TRANSCRIPT_CANCELLATION_CHUNK_BYTES: usize = crate::util::CANCELLATION_CHECK_BYTES;

#[cfg(test)]
fn build_transcript_summary(transcript: &str, budget: usize) -> String {
    build_transcript_summary_inner(transcript, budget, TerminalStyle::default(), None)
        .expect("an uncancelled summary cannot fail")
}

fn build_transcript_summary_cancellable(
    transcript: &str,
    budget: usize,
    style: TerminalStyle,
    cancellation: &QueryCancellation,
) -> Result<String> {
    build_transcript_summary_inner(transcript, budget, style, Some(cancellation))
}

#[derive(Default)]
struct TranscriptBookends<'a> {
    total: usize,
    first_user: Option<Turn<'a>>,
    first_assistant: Option<Turn<'a>>,
    last_user: Option<Turn<'a>>,
    last_assistant: Option<Turn<'a>>,
}

fn remember_transcript_turn<'a>(
    bookends: &mut TranscriptBookends<'a>,
    role: TurnRole,
    body: &'a str,
) {
    let turn = Turn {
        ordinal: bookends.total,
        body,
    };
    match role {
        TurnRole::User => {
            bookends.first_user.get_or_insert(turn);
            bookends.last_user = Some(turn);
        }
        TurnRole::Assistant => {
            bookends.first_assistant.get_or_insert(turn);
            bookends.last_assistant = Some(turn);
        }
    }
    bookends.total = bookends.total.saturating_add(1);
}

fn next_transcript_line_end(
    transcript: &str,
    cursor: usize,
    cancellation: Option<&QueryCancellation>,
) -> Result<usize> {
    let bytes = transcript.as_bytes();
    let mut chunk_start = cursor;
    while chunk_start < bytes.len() {
        if let Some(cancellation) = cancellation {
            cancellation.ensure_active()?;
        }
        let mut chunk_end = chunk_start
            .saturating_add(TRANSCRIPT_CANCELLATION_CHUNK_BYTES)
            .min(bytes.len());
        while chunk_end < bytes.len() && !transcript.is_char_boundary(chunk_end) {
            chunk_end -= 1;
        }
        if let Some(relative) = transcript[chunk_start..chunk_end].find('\n') {
            return Ok(chunk_start + relative);
        }
        chunk_start = chunk_end;
    }
    Ok(bytes.len())
}

fn trim_newlines_cancellable<'a>(
    text: &'a str,
    cancellation: Option<&QueryCancellation>,
) -> Result<&'a str> {
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut next_check = 0;
    while start < bytes.len() && bytes[start] == b'\n' {
        if start == next_check {
            if let Some(cancellation) = cancellation {
                cancellation.ensure_active()?;
            }
            next_check = next_check.saturating_add(TRANSCRIPT_CANCELLATION_CHUNK_BYTES);
        }
        start += 1;
    }
    let mut end = bytes.len();
    let mut scanned = 0;
    next_check = 0;
    while end > start && bytes[end - 1] == b'\n' {
        if scanned == next_check {
            if let Some(cancellation) = cancellation {
                cancellation.ensure_active()?;
            }
            next_check = next_check.saturating_add(TRANSCRIPT_CANCELLATION_CHUNK_BYTES);
        }
        end -= 1;
        scanned += 1;
    }
    Ok(&text[start..end])
}

fn transcript_bookends<'a>(
    transcript: &'a str,
    cancellation: Option<&QueryCancellation>,
) -> Result<TranscriptBookends<'a>> {
    let mut bookends = TranscriptBookends::default();
    let mut current_role = None;
    let mut body_start = 0;
    let mut cursor = 0;
    while cursor < transcript.len() {
        let line_end = next_transcript_line_end(transcript, cursor, cancellation)?;
        let line = &transcript[cursor..line_end];
        if let Some(role) = TurnRole::parse(line) {
            if let Some(previous) = current_role.take() {
                let body =
                    trim_newlines_cancellable(&transcript[body_start..cursor], cancellation)?;
                remember_transcript_turn(&mut bookends, previous, body);
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
    if let Some(role) = current_role {
        let body = trim_newlines_cancellable(&transcript[body_start..], cancellation)?;
        remember_transcript_turn(&mut bookends, role, body);
    }
    if let Some(cancellation) = cancellation {
        cancellation.ensure_active()?;
    }
    Ok(bookends)
}

fn build_transcript_summary_inner(
    transcript: &str,
    budget: usize,
    style: TerminalStyle,
    cancellation: Option<&QueryCancellation>,
) -> Result<String> {
    let bookends = transcript_bookends(transcript, cancellation)?;
    if bookends.total == 0 {
        return Ok("(no transcript content)".to_string());
    }

    let rule = style.section_rule();
    let heading = |index: usize| format!("{rule} {} {rule}", PREVIEW_SECTION_LABELS[index]);
    let candidates = [
        (bookends.first_user, heading(0), PREVIEW_WEIGHT_FIRST_PROMPT),
        (
            bookends.first_assistant,
            heading(1),
            PREVIEW_WEIGHT_FIRST_REPLY,
        ),
        (bookends.last_user, heading(2), PREVIEW_WEIGHT_FINAL_PROMPT),
        (
            bookends.last_assistant,
            heading(3),
            PREVIEW_WEIGHT_FINAL_REPLY,
        ),
    ];
    let mut shown_ordinals = Vec::with_capacity(4);
    let mut sections = Vec::with_capacity(4);
    for (turn, label, weight) in candidates {
        let Some(turn) = turn else { continue };
        if shown_ordinals.contains(&turn.ordinal) {
            continue;
        }
        shown_ordinals.push(turn.ordinal);
        sections.push((turn.ordinal, label, weight, turn.body));
    }
    sections.sort_by_key(|(index, _, _, _)| *index);
    render_summary_sections(&sections, bookends.total, budget, style, cancellation)
}

fn render_summary_sections(
    sections: &[(usize, String, usize, &str)],
    total: usize,
    budget: usize,
    style: TerminalStyle,
    cancellation: Option<&QueryCancellation>,
) -> Result<String> {
    let hidden = total.saturating_sub(sections.len());
    let mut parts = Vec::new();
    let mut last_emitted_idx = None;
    for (idx, label, weight, body) in sections {
        if let Some(prev) = last_emitted_idx {
            if *idx > prev + 1 {
                let gap = *idx - prev - 1;
                let marker = style.elision_marker();
                parts.push(format!(
                    "{marker} {gap} more turn{} hidden {marker}",
                    if gap == 1 { "" } else { "s" }
                ));
            }
        }
        parts.push(label.clone());
        let max_lines = (budget.saturating_mul(*weight) / HISTORICAL_PREVIEW_BODY_LINES).max(1);
        parts.push(truncate_body_inner(body, max_lines, cancellation)?);
        last_emitted_idx = Some(*idx);
    }
    if hidden > 0 && sections.len() < 2 {
        let marker = style.elision_marker();
        parts.push(format!(
            "{marker} {hidden} more turn{} hidden {marker}",
            if hidden == 1 { "" } else { "s" }
        ));
    }
    parts.push(format!(
        "({total} turn{} total)",
        if total == 1 { "" } else { "s" }
    ));
    Ok(parts.join("\n\n"))
}

fn render_preview_line(line: &str, query: &str, style: TerminalStyle) -> Line<'static> {
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

    if line.starts_with(&format!("{} ", style.section_rule())) {
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

    if line.starts_with(&format!("{} ", style.elision_marker()))
        || line.starts_with('(') && line.ends_with(" total)")
    {
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
    use crate::terminal_style::CapabilityMode;
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
            if !self.events.is_empty() {
                return Ok(true);
            }
            // Mirror the real source: reporting no input waits out the requested timeout.
            // The sliced wait in step() would otherwise busy-spin through the idle window.
            std::thread::sleep(timeout);
            Ok(false)
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

    /// Maximum loop turns a scripted scenario may take before the harness gives up, panicking
    /// with the rendered screen (P9).
    const MAX_TEST_STEPS: usize = 64;

    /// An executor for scenarios that never send requests (pre-step-4 key handling still runs
    /// inline). It answers honestly if invoked, so an accidental send surfaces as a visible
    /// state change instead of a hang.
    fn idle_executor() -> SearchExecutor {
        Box::new(
            |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                Ok(match request.kind {
                    // The empty-query startup must answer with navigable rows: an empty
                    // answer would wipe the seeded fixture on the first drain.
                    RequestKind::Search if request.query.trim().is_empty() => {
                        WorkerResponse::results(
                            request,
                            rows(&["claude:idle-one", "claude:idle-two", "claude:idle-three"]),
                        )
                    }
                    RequestKind::Search => WorkerResponse::results(request, Vec::new()),
                    RequestKind::PreviewOnly => WorkerResponse::preview(request, None),
                })
            },
        )
    }

    /// One scripted TUI: owns the terminal, event script, app state, and fixture databases.
    /// No `'static` test leaks: production-executor fixtures attach their TempDir here.
    struct TuiHarness {
        // Field order is drop order: stop/join the worker, close the fixture Db, then remove
        // directories. This matters on Windows, where an open SQLite file cannot be unlinked.
        app: AppState,
        db: Db,
        terminal: Terminal<TestBackend>,
        events: ScriptedEventSource,
        external_fixture: Option<tempfile::TempDir>,
        _dir: tempfile::TempDir,
    }

    impl TuiHarness {
        fn with_executor(executor: SearchExecutor) -> Self {
            Self::with_config(Config::default(), executor)
        }

        fn with_config(config: Config, executor: SearchExecutor) -> Self {
            Self::with_factory(
                config,
                Box::new(move || Ok((executor, EffectiveAccessScope::All))),
            )
        }

        /// Like `with_config`, but the caller owns the whole factory (the digest guard passes
        /// the production `db_backed_executor` against its own fixture).
        fn with_factory(mut config: Config, factory: ExecutorFactory) -> Self {
            // The real 150 ms idle pacing makes every step wait it out against the
            // sleeping scripted source; shrink the default so steps complete fast. A test
            // that sets the field explicitly keeps its value.
            // Compared against the shipped default rather than a literal: a literal that stops
            // matching would silently hand every scripted test the real timings and hang them.
            let shipped = Config::default().ui;
            if config.ui.idle_poll_interval_ms == shipped.idle_poll_interval_ms {
                config.ui.idle_poll_interval_ms = 1;
            }
            // Same reason for the typing delay: a scripted test presses a key and then asserts
            // on what the search did, with no wall clock advancing in between. Zero is the
            // documented "search on every keystroke" setting, so those tests read as they did
            // before the delay existed. The three tests that are about the delay set their own.
            if config.ui.search_debounce_ms == shipped.search_debounce_ms {
                config.ui.search_debounce_ms = 0;
            }
            let (worker, observed) =
                spawn_search_worker(factory).expect("worker startup handshake");
            assert!(matches!(observed, EffectiveAccessScope::All));
            let dir = tempfile::tempdir().unwrap();
            let db = Db::open(&dir.path().join("index.db")).unwrap();
            let app = AppState::new_quiet(config, worker);
            Self {
                _dir: dir,
                external_fixture: None,
                db,
                terminal: Terminal::new(TestBackend::new(100, 24)).unwrap(),
                events: ScriptedEventSource::new(Vec::new()),
                app,
            }
        }

        fn own_external_fixture(mut self, fixture: tempfile::TempDir) -> Self {
            self.external_fixture = Some(fixture);
            self
        }

        /// Install the starting rows on `AppState::results` AND in the fixture database, then
        /// send the startup request — construction is quiet, so the startup response and the
        /// seeded rows always agree.
        fn seeded(mut self, sessions: &[&str]) -> Self {
            let mut records = Vec::new();
            for id in sessions {
                let parsed = session(id);
                self.db.upsert_session(&parsed, 0, 0).unwrap();
                records.push(parsed.session);
            }
            self.app.results = records;
            self.app.request_search();
            self
        }

        /// Send the startup request for a harness that seeded nothing (the digest guard's
        /// real-executor path, whose fixture lives in the factory's own database).
        #[allow(dead_code)]
        fn start(&mut self) {
            self.app.request_search();
        }

        /// Step until the given session's preview is applied — the C28 settled point —
        /// bounded by MAX_TEST_STEPS, panicking with the rendered screen on expiry.
        fn wait_until_previewed(&mut self, id: &str) {
            for _ in 0..MAX_TEST_STEPS {
                if self.app.previewed_id.as_deref() == Some(id) {
                    return;
                }
                self.step();
            }
            panic!(
                "preview for {id} never applied; previewed={:?}, screen:\n{}",
                self.app.previewed_id,
                self.screen()
            );
        }

        fn script(&mut self, events: Vec<Event>) {
            self.events = ScriptedEventSource::new(events);
        }

        fn step(&mut self) -> Option<AppAction> {
            step(&mut self.terminal, &mut self.events, &mut self.app).unwrap()
        }

        /// Step until the script has drained, then one more turn: `step` is draw-then-drain,
        /// so the turn that consumes the last key renders the pre-key state — without the
        /// extra step, post-script assertions read a stale frame (C32).
        fn step_until_script_drained(&mut self) -> Option<AppAction> {
            let mut turns = 0;
            let mut action = None;
            while !self.events.events.is_empty() && action.is_none() {
                action = self.step();
                turns += 1;
                assert!(
                    turns < MAX_TEST_STEPS,
                    "script did not drain after {MAX_TEST_STEPS} steps; screen:\n{}",
                    self.screen()
                );
            }
            action.or_else(|| self.step())
        }

        fn screen(&self) -> String {
            screen_text(&self.terminal)
        }

        fn region_text(&self, rows: std::ops::Range<u16>) -> String {
            let buffer = self.terminal.backend().buffer();
            let width = buffer.area.width;
            let mut lines = Vec::new();
            for y in rows {
                let mut line = String::new();
                for x in 0..width {
                    line.push_str(buffer[(x, y)].symbol());
                }
                lines.push(line.trim_end().to_string());
            }
            lines.join("\n")
        }

        fn search_box(&self) -> String {
            self.region_text(0..SEARCH_BOX_ROWS)
        }

        /// The session-list region: below the search box, above the status bar and the
        /// possible error line, at full width (containment assertions do not need the pane
        /// split).
        fn session_rows(&self) -> String {
            let area = self.terminal.backend().buffer().area;
            let bottom = area.height - STATUS_BAR_ROWS - ERROR_LINE_ROWS;
            self.region_text(SEARCH_BOX_ROWS..bottom)
        }

        /// The row where the error line renders when an error is present. Without an error the
        /// row belongs to the body's bottom border, so only read it while `app.error` is set.
        fn error_line(&self) -> String {
            let area = self.terminal.backend().buffer().area;
            self.region_text(
                area.height - STATUS_BAR_ROWS - ERROR_LINE_ROWS..area.height - STATUS_BAR_ROWS,
            )
        }

        fn status_line(&self) -> String {
            let area = self.terminal.backend().buffer().area;
            self.region_text(area.height - STATUS_BAR_ROWS..area.height)
        }
    }

    #[test]
    fn event_seam_reproduces_current_keybindings() {
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&[
            "claude:alpha",
            "claude:beta",
            "claude:gamma",
        ]);
        harness.wait_until_previewed("claude:idle-one");
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        assert!(harness.step_until_script_drained().is_none());
        assert!(harness.app.search_mode);
        assert_eq!(harness.app.query, "a");
        assert!(
            harness.search_box().contains('a'),
            "typed character renders in the box"
        );
        // The caret is the terminal's own, placed by the frame, so it is not a character in the
        // buffer: after `a` it sits one column past it.
        assert_eq!(
            harness.terminal.get_cursor_position().unwrap().x,
            harness.terminal.get_frame().area().x + 2
        );
        assert!(harness.session_rows().contains("Sessions"));

        // Esc in search mode returns to browse without quitting.
        harness.script(vec![key(KeyCode::Esc)]);
        assert!(harness.step_until_script_drained().is_none());
        assert!(!harness.app.search_mode);

        // Backspace to an empty query re-browses without panicking (search mode first:
        // browse mode has no Backspace arm — that IS the current contract).
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Backspace)]);
        assert!(harness.step_until_script_drained().is_none());
        assert_eq!(harness.app.query, "");
        // Settle the re-browse response before navigating: the empty-query response must
        // re-apply the idle rows, or j runs against the 'a' section's empty result set.
        harness.wait_until_previewed("claude:idle-one");

        // Esc returns to browse so the navigation keys below apply.
        harness.script(vec![key(KeyCode::Esc)]);
        harness.step_until_script_drained();
        assert!(!harness.app.search_mode);

        // j moves the selection and loads the preview of the new row.
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.selected, 1);
        // Settle the row-1 preview BEFORE the manual line count: a late preview response
        // would overwrite it and clamp the scroll back to zero mid-test.
        harness.wait_until_previewed("claude:idle-two");

        // Ctrl-d scrolls the preview by the page step against the content-length bound.
        harness.app.preview = (0..100)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        harness.script(vec![ctrl_key(KeyCode::Char('d'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.preview_scroll, 15);

        // Enter on a selected session yields the resume action; q quits from browse mode.
        harness.script(vec![key(KeyCode::Enter)]);
        assert!(matches!(
            harness.step_until_script_drained(),
            Some(AppAction::Resume(_))
        ));
        harness.script(vec![key(KeyCode::Char('q'))]);
        assert!(matches!(
            harness.step_until_script_drained(),
            Some(AppAction::Quit)
        ));

        // g/G/PageDown/PageUp on an EMPTY result set must not panic.
        let mut empty = TuiHarness::with_executor(idle_executor());
        empty.script(vec![
            key(KeyCode::Char('g')),
            key(KeyCode::Char('G')),
            key(KeyCode::PageDown),
            key(KeyCode::PageUp),
        ]);
        assert!(empty.step_until_script_drained().is_none());
    }

    #[test]
    fn ui_interaction_fields_reach_the_loop() {
        let mut config = Config::default();
        config.ui.idle_poll_interval_ms = 7;
        config.ui.list_page_rows = 2;
        config.ui.preview_scroll_rows = 3;
        config.ui.preview_page_rows = 4;
        let mut harness = TuiHarness::with_config(config, idle_executor()).seeded(&[
            "claude:one",
            "claude:two",
            "claude:three",
            "claude:four",
        ]);
        harness.wait_until_previewed("claude:idle-one");

        // The idle poll paces with the configured interval, observed clock-free through the
        // seam's recorded timeouts (every poll — including the one that returns false).
        harness.script(vec![key(KeyCode::PageDown)]);
        assert!(harness.step_until_script_drained().is_none());
        // The idle wait is sliced (worker responses are picked up between slices), so each
        // recorded poll is the deadline-derived remainder: bounded by the configured
        // interval, never exceeding it, and never a stale default.
        assert!(
            harness.events.poll_timeouts.iter().all(|timeout| {
                *timeout <= Duration::from_millis(7) && *timeout >= Duration::from_millis(5)
            }),
            "poll slices must stay within [ui].idle_poll_interval_ms, got {:?}",
            harness.events.poll_timeouts
        );

        // PageDown moves by the configured list page step, not a hardcoded 10.
        assert_eq!(harness.app.selected, 2);

        // J scrolls by the configured preview scroll step; Ctrl-d adds the page step. Settle
        // the row-2 preview first: a late preview response would overwrite the manual line
        // count mid-scroll.
        harness.wait_until_previewed("claude:idle-three");
        harness.app.preview = (0..100)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        harness.script(vec![key(KeyCode::Char('J'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.preview_scroll, 3);
        harness.script(vec![ctrl_key(KeyCode::Char('d'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.preview_scroll, 7);
    }

    #[test]
    fn settled_idle_turn_uses_one_configured_poll_instead_of_ten_ms_wakeups() {
        let mut config = Config::default();
        config.ui.idle_poll_interval_ms = 25;
        let mut harness = TuiHarness::with_config(config, idle_executor());
        harness.step();
        assert_eq!(harness.events.poll_timeouts.len(), 1);
        assert!(
            harness.events.poll_timeouts[0] >= Duration::from_millis(23),
            "idle poll should use the configured interval: {:?}",
            harness.events.poll_timeouts
        );
    }

    #[test]
    fn mailbox_work_keeps_short_polling_even_when_visible_flags_are_clear() {
        let (release, gate) = mpsc::channel::<()>();
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                let _ = entered_tx.send(());
                park_until_released_or_cancelled(&gate, cancellation);
                Ok(WorkerResponse::preview(request, None))
            },
        );
        let mut config = Config::default();
        config.ui.idle_poll_interval_ms = 25;
        let mut harness = TuiHarness::with_config(config, executor);
        harness
            .app
            .worker
            .send(WorkerRequest {
                kind: RequestKind::PreviewOnly,
                generation: harness.app.current_preview_generation.clone(),
                query: String::new(),
                filters: harness.app.filters.clone(),
                selected_id: None,
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        harness.app.searching = false;
        harness.app.preview_loading = false;
        harness.step();
        assert!(
            harness.events.poll_timeouts[0] <= ACTIVE_WORKER_POLL_SLICE,
            "mailbox in-flight work must not use the settled idle interval"
        );
        release.send(()).unwrap();
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

    fn historical_summary_reference(transcript: &str) -> String {
        let turns = parse_turns(transcript);
        if turns.is_empty() {
            return "(no transcript content)".to_string();
        }
        let candidates = [
            (
                turns.iter().position(|turn| turn.role == TurnRole::User),
                "── First prompt ──",
                8,
            ),
            (
                turns
                    .iter()
                    .position(|turn| turn.role == TurnRole::Assistant),
                "── First reply ──",
                4,
            ),
            (
                turns.iter().rposition(|turn| turn.role == TurnRole::User),
                "── Final prompt ──",
                8,
            ),
            (
                turns
                    .iter()
                    .rposition(|turn| turn.role == TurnRole::Assistant),
                "── Final reply ──",
                14,
            ),
        ];
        let mut shown = Vec::new();
        let mut sections = Vec::new();
        for (ordinal, label, max_lines) in candidates {
            let Some(ordinal) = ordinal else { continue };
            if shown.contains(&ordinal) {
                continue;
            }
            shown.push(ordinal);
            sections.push((ordinal, label, max_lines, turns[ordinal].body));
        }
        sections.sort_by_key(|(ordinal, _, _, _)| *ordinal);
        let hidden = turns.len().saturating_sub(sections.len());
        let mut parts = Vec::new();
        let mut previous = None;
        for (ordinal, label, max_lines, body) in sections {
            if previous.is_some_and(|prior| ordinal > prior + 1) {
                let gap = ordinal - previous.unwrap() - 1;
                parts.push(format!(
                    "⋯ {gap} more turn{} hidden ⋯",
                    if gap == 1 { "" } else { "s" }
                ));
            }
            parts.push(label.to_string());
            parts.push(truncate_body(body, max_lines));
            previous = Some(ordinal);
        }
        if hidden > 0 && shown.len() < 2 {
            parts.push(format!(
                "⋯ {hidden} more turn{} hidden ⋯",
                if hidden == 1 { "" } else { "s" }
            ));
        }
        parts.push(format!(
            "({} turn{} total)",
            turns.len(),
            if turns.len() == 1 { "" } else { "s" }
        ));
        parts.join("\n\n")
    }

    #[test]
    fn borrowed_transcript_scan_matches_main_preview_semantics() {
        let long = (0..40)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cases = [
            String::new(),
            join_turns(&[("user", &long)]),
            join_turns(&[("assistant", &long)]),
            join_turns(&[("user", &long), ("assistant", &long)]),
            join_turns(&[
                ("user", "first"),
                ("assistant", "reply"),
                ("user", "middle"),
                ("assistant", "last"),
            ]),
        ];
        for transcript in cases {
            assert_eq!(
                build_transcript_summary(&transcript, HISTORICAL_PREVIEW_BODY_LINES),
                historical_summary_reference(&transcript)
            );
        }
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
        let summary = build_transcript_summary(&raw, 34);
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
        let summary = build_transcript_summary(&raw, 34);
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
        let summary = build_transcript_summary(&raw, 34);
        assert!(summary.contains("[…]"));
    }

    #[test]
    fn summary_for_empty_transcript() {
        assert_eq!(build_transcript_summary("", 30), "(no transcript content)");
    }

    #[test]
    fn transcript_summary_observes_typed_cancellation() {
        let cancellation = QueryCancellation::new();
        cancellation.cancel();
        let error = build_transcript_summary_cancellable(
            "[2026-01-01T00:00:00Z] user\nbody\n",
            30,
            TerminalStyle::default(),
            &cancellation,
        )
        .unwrap_err();
        assert!(error.is::<QueryCancelled>());
        assert!(is_expected_interruption(&error));
    }

    #[test]
    fn error_elision_bounds_terminal_columns_and_flattens_newlines() {
        let rendered = elide_middle("漢字🙂 prefix\npress q and rerun", 16, "…");
        assert!(UnicodeWidthStr::width(rendered.as_str()) <= 16);
        assert!(!rendered.contains(['\r', '\n']));
        assert!(rendered.ends_with("rerun"));
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
        let line = render_preview_line("Session: claude:s1", "", TerminalStyle::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Session: claude:s1");
        assert!(line.spans.len() >= 2);

        // A section header renders as a single styled span.
        let line = render_preview_line("── user prompt ──", "", TerminalStyle::default());
        assert_eq!(line.spans.len(), 1);

        // A plain line with a query splits the matched term into its own span
        // while preserving the full text.
        let line = render_preview_line("find the needle here", "needle", TerminalStyle::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "find the needle here");
        assert!(line.spans.len() >= 2);
    }
    fn rows(ids: &[&str]) -> Vec<SessionRecord> {
        ids.iter().map(|id| session(id).session).collect()
    }

    /// Wait for one executor completion, failing with the reason that carries the RED signal:
    /// the executor can only run if the UI actually sent a request (§5.3).
    /// Wait for the executor to finish a request with this exact query, discarding stale
    /// signals (the startup empty query always fires first). FIFO order makes consecutive
    /// same-query waits consume distinct completions.
    fn wait_for_executed(executed: &mpsc::Receiver<String>, expected: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match executed.recv_timeout(Duration::from_millis(100)) {
                Ok(query) if query == expected => return query,
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("executor never finished query {expected:?} (it can only run if the UI sent it)");
    }

    /// Wait for one observed request shape (kind, query, selected_id).
    fn wait_for_request(
        requests: &mpsc::Receiver<(RequestKind, String, Option<String>)>,
    ) -> (RequestKind, String, Option<String>) {
        requests
            .recv_timeout(Duration::from_secs(5))
            .expect("a request reached the executor")
    }

    /// Step until the executor finishes another request with this exact query — for
    /// follow-ups that only issue from apply_response, where a bare wait would deadlock
    /// (no steps run to apply the prior response).
    fn step_until_executed(
        harness: &mut TuiHarness,
        executed: &mpsc::Receiver<String>,
        expected: &str,
    ) -> String {
        for _ in 0..MAX_TEST_STEPS {
            harness.step();
            if let Ok(query) = executed.try_recv() {
                if query == expected {
                    return query;
                }
            }
        }
        panic!("executor never finished another {expected:?} within {MAX_TEST_STEPS} steps");
    }

    /// Step until `condition` holds, bounded by MAX_TEST_STEPS. Needed wherever a test
    /// waits on an executor completion signal: the signal fires inside the executor, BEFORE
    /// the worker sends the response, so a single drain step after the wait can miss it.
    fn step_until<F: Fn(&TuiHarness) -> bool>(harness: &mut TuiHarness, condition: F) {
        for _ in 0..MAX_TEST_STEPS {
            if condition(harness) {
                return;
            }
            harness.step();
        }
        panic!("condition never held within {MAX_TEST_STEPS} steps");
    }

    /// Step until the NEXT request reaches the executor and return it. A bare step may run
    /// before the previous response arrives, and a follow-up request only issues from
    /// apply_response — stepping until it appears removes that race.
    fn step_until_next_request(
        harness: &mut TuiHarness,
        requests: &mpsc::Receiver<(RequestKind, String, Option<String>)>,
    ) -> (RequestKind, String, Option<String>) {
        for _ in 0..MAX_TEST_STEPS {
            harness.step();
            if let Ok(request) = requests.try_recv() {
                return request;
            }
        }
        panic!("no follow-up request arrived within {MAX_TEST_STEPS} steps");
    }

    /// Release parked gates enough times for every queued request to finish, so a harness
    /// holding a parked executor can drop without hanging on the worker join.
    fn finish_gated(release: &mpsc::Sender<()>) {
        for _ in 0..8 {
            let _ = release.send(());
        }
    }

    #[test]
    fn echo_renders_while_the_search_executor_is_still_blocked() {
        let (release, gate) = mpsc::channel::<()>();
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                let response = match request.kind {
                    // The ungated startup request answers with the seeded rows; answering
                    // "after" here would race the !contains assertion via the stale guard
                    // instead of excluding it (§5.3).
                    RequestKind::Search if request.query.trim().is_empty() => {
                        WorkerResponse::results(request, rows(&["claude:before"]))
                    }
                    RequestKind::Search => {
                        WorkerResponse::results(request, rows(&["claude:after"]))
                    }
                    RequestKind::PreviewOnly => {
                        WorkerResponse::preview(request, Some(String::new()))
                    }
                };
                let _ = executed_tx.send(request.query.clone());
                Ok(response)
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();

        // G1: the character is rendered and the previous result set is intact while the
        // search for "a" is provably still parked inside the executor.
        assert!(
            harness.search_box().contains('a'),
            "typed character must render before the search completes"
        );
        assert!(
            harness.session_rows().contains("before"),
            "previous results must survive while the search is in flight"
        );
        assert!(!harness.session_rows().contains("after"));

        release.send(()).unwrap();
        wait_for_executed(&executed_rx, "a");
        step_until(&mut harness, |harness| {
            harness.session_rows().contains("after")
        });
        assert!(harness.session_rows().contains("after"));
        finish_gated(&release);
    }

    #[test]
    fn backspace_echoes_while_the_executor_is_blocked() {
        let (release, gate) = mpsc::channel::<()>();
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let (parked_tx, parked_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    let _ = parked_tx.send(request.query.clone());
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                let _ = executed_tx.send(request.query.clone());
                Ok(match request.kind {
                    RequestKind::Search => WorkerResponse::results(request, Vec::new()),
                    RequestKind::PreviewOnly => WorkerResponse::preview(request, None),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        // '/','a' first, and wait until "a" is provably parked: with all three keys in one
        // script the worker's drain-to-latest coalesces the queued pair and "a" never
        // executes at all — correct worker behavior, wrong setup for this contract.
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        loop {
            let query = parked_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the 'a' search parked");
            if query == "a" {
                break;
            }
        }
        harness.script(vec![key(KeyCode::Backspace)]);
        harness.step_until_script_drained();
        // Backspace rendered immediately (query lost 'a'); a whitespace-only query counts as
        // empty, matching refresh's trim() contract. The Backspace's supersede cancels the
        // parked "a" — the cancel-aware fake completes it, as a real interrupted query would.
        assert!(!harness.search_box().contains("a█"));
        assert_eq!(harness.app.query, "");
        wait_for_executed(&executed_rx, "a");
        finish_gated(&release);
    }

    #[test]
    fn worker_serves_only_the_latest_of_a_drained_burst() {
        let (release, gate) = mpsc::channel::<()>();
        let (parked_tx, parked_rx) = mpsc::channel::<()>();
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search && request.query.is_empty() {
                    // Park the STARTUP request so the burst provably queues behind it: all
                    // five searches sit in the channel before the worker takes any. Without
                    // this the worker races the UI's sends and the coalescing is untestable.
                    let _ = parked_tx.send(());
                    // Ignore cancellation until explicit release so every pasted prefix is
                    // submitted while the worker cannot consume the mailbox.
                    let _ = gate.recv();
                }
                let _ = executed_tx.send(request.query.clone());
                Ok(WorkerResponse::results(request, Vec::new()))
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        parked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("startup request parked");
        harness.script(vec![
            key(KeyCode::Char('/')),
            key(KeyCode::Char('a')),
            key(KeyCode::Char('b')),
            key(KeyCode::Char('c')),
            key(KeyCode::Char('d')),
            key(KeyCode::Char('e')),
        ]);
        harness.step_until_script_drained();
        assert_eq!(
            harness.app.worker.mailbox.pending_counts(),
            (1, 0),
            "a pasted burst retains one latest Search, never every cumulative prefix"
        );
        release.send(()).unwrap();
        // Collect until quiet: the worker drains the queued burst to its latest.
        let mut executed: Vec<String> = Vec::new();
        while let Ok(query) = executed_rx.recv_timeout(Duration::from_millis(200)) {
            executed.push(query);
        }
        // The queries are cumulative keystrokes: the burst's latest request carries the full
        // "abcde" — one execution for the whole queued burst is the drain-to-latest win.
        assert_eq!(
            executed,
            vec![String::new(), "abcde".to_string()],
            "a queued burst must serve only the latest request (§2.3 drain-to-latest)"
        );
    }

    #[test]
    fn navigation_preview_survives_queued_search_failure() {
        let (release, gate) = mpsc::channel::<()>();
        let (startup_parked_tx, startup_parked_rx) = mpsc::channel::<()>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| match request
                .kind
            {
                RequestKind::Search if request.query.is_empty() => {
                    let _ = startup_parked_tx.send(());
                    // Hold the worker despite supersede so both pending slots are observable.
                    let _ = gate.recv();
                    Ok(WorkerResponse::results(
                        request,
                        rows(&["claude:one", "claude:two"]),
                    ))
                }
                RequestKind::Search => Err(anyhow::anyhow!("search failed")),
                RequestKind::PreviewOnly => Ok(WorkerResponse::preview(
                    request,
                    request
                        .selected_id
                        .as_deref()
                        .map(|id| format!("preview of {id}")),
                )),
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:one", "claude:two"]);
        startup_parked_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("startup search parked");
        harness.script(vec![
            key(KeyCode::Char('/')),
            key(KeyCode::Char('a')),
            key(KeyCode::Esc),
            key(KeyCode::Char('j')),
        ]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.selected, 1);
        assert_eq!(harness.app.worker.mailbox.pending_counts(), (1, 1));
        release.send(()).unwrap();
        step_until(&mut harness, |harness| {
            harness.app.preview.contains("claude:two")
        });
        assert!(harness.app.preview.contains("claude:two"));
    }

    #[test]
    fn superseding_a_query_cancels_the_one_in_flight() {
        let (release, gate) = mpsc::channel::<()>();
        let (observed_tx, observed_rx) = mpsc::channel::<(String, Arc<QueryCancellation>)>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                let _ = observed_tx.send((request.query.clone(), Arc::clone(cancellation)));
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                Ok(WorkerResponse::results(request, Vec::new()))
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        // Wait until the "a" search is provably parked (its cancellation published), so the
        // supersede lands on it deterministically. The fake cloned the request's Arc out;
        // the test owns the only other one, which is what makes is_cancelled() assertable
        // at all (C29).
        let first = loop {
            let (query, cancellation) = observed_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("a search reached the executor");
            if query == "a" {
                break cancellation;
            }
        };
        harness.script(vec![key(KeyCode::Char('b'))]);
        harness.step_until_script_drained();
        assert!(
            first.is_cancelled(),
            "typing a newer query must cancel the superseded in-flight search"
        );
        finish_gated(&release);
    }

    #[test]
    fn response_for_a_superseded_query_is_dropped() {
        let (release, gate) = mpsc::channel::<()>();
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let (parked_tx, parked_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    let _ = parked_tx.send(request.query.clone());
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                // Suffix the id so superseded and fresh results are not substrings of each
                // other ("claude:a-result" vs "claude:ab-result").
                let id = format!("claude:{}-result", request.query);
                let _ = executed_tx.send(request.query.clone());
                Ok(WorkerResponse::results(request, rows(&[&id])))
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        loop {
            let query = parked_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("a search parked");
            if query == "a" {
                break;
            }
        }
        harness.script(vec![key(KeyCode::Char('b'))]);
        harness.step_until_script_drained();
        // Typing 'b' makes the cumulative query "ab": that request cancels the in-flight
        // "a" — the cancel-aware fake completes it, exactly as a real interrupted query
        // would — then "ab" parks, and one release serves it. The stale "a" response must
        // be dropped by the originating-query guard, never rendered; the fresh "ab"
        // response applies.
        release.send(()).unwrap();
        wait_for_executed(&executed_rx, "a");
        wait_for_executed(&executed_rx, "ab");
        step_until(&mut harness, |harness| {
            harness.session_rows().contains("claude:ab-result")
        });
        assert!(
            !harness.session_rows().contains("claude:a-result"),
            "a response for a superseded query must not mutate state"
        );
        assert!(harness.session_rows().contains("claude:ab-result"));
        finish_gated(&release);
    }

    #[test]
    fn same_query_response_from_superseded_filters_is_dropped() {
        let (release, gate) = mpsc::channel::<()>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search && request.filters.provider.is_some() {
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                Ok(match request.kind {
                    RequestKind::Search => {
                        WorkerResponse::results(request, rows(&["claude:current-filter-result"]))
                    }
                    RequestKind::PreviewOnly => WorkerResponse::preview(request, None),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        let old_request = WorkerRequest {
            kind: RequestKind::Search,
            generation: harness.app.current_search_generation.clone(),
            query: harness.app.query.clone(),
            filters: harness.app.filters.clone(),
            selected_id: harness
                .app
                .selected_session()
                .map(|session| session.id.clone()),
        };

        // Provider changes the request semantics without changing the query. Its successor is
        // parked, so an already-queued old response cannot be rescued by a later correct one.
        harness.app.cycle_provider();
        assert_eq!(harness.app.query, old_request.query);
        assert!(!harness.app.apply_outcome(WorkerOutcome {
            kind: RequestKind::Search,
            generation: old_request.generation.clone(),
            result: Ok(WorkerResponse::results(
                &old_request,
                rows(&["claude:stale-unfiltered"]),
            )),
        }));
        assert!(
            !harness
                .app
                .results
                .iter()
                .any(|row| row.id == "claude:stale-unfiltered"),
            "response identity must include the filter generation, not only equal query text"
        );
        finish_gated(&release);
    }

    #[test]
    fn error_from_superseded_search_is_dropped() {
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:keep"]);
        let stale = harness.app.current_search_generation.clone();
        harness.app.request_search();
        assert!(!harness.app.apply_outcome(WorkerOutcome {
            kind: RequestKind::Search,
            generation: stale,
            result: Err(anyhow::anyhow!("stale failure")),
        }));
        assert!(
            harness.app.error.is_none(),
            "a stale error must not replace the current operation state"
        );
    }

    #[test]
    fn list_title_exposes_current_search_completion() {
        let (release, gate) = mpsc::channel::<()>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search {
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                Ok(WorkerResponse::results(request, rows(&["claude:done"])))
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.step();
        assert!(screen_text(&harness.terminal).contains("searching"));
        release.send(()).unwrap();
        step_until(&mut harness, |harness| !harness.app.searching);
        harness.step();
        assert!(!screen_text(&harness.terminal).contains("searching"));
        finish_gated(&release);
    }

    #[test]
    fn returning_to_rendered_preview_invalidates_outstanding_preview_error() {
        let mut harness =
            TuiHarness::with_executor(idle_executor()).seeded(&["claude:a", "claude:b"]);
        harness.app.previewed_id = Some("claude:a".to_string());
        harness.app.selected = 1;
        harness.app.request_preview();
        let stale_b = harness.app.current_preview_generation.clone();
        harness.app.selected = 0;
        harness.app.request_preview(); // fast path: A is already rendered, but B must go stale.
        assert!(!harness.app.apply_outcome(WorkerOutcome {
            kind: RequestKind::PreviewOnly,
            generation: stale_b,
            result: Err(anyhow::anyhow!("preview B failed")),
        }));
        assert!(harness.app.error.is_none());
    }

    #[test]
    fn current_preview_error_replaces_stale_content_and_success_recovers() {
        let mut harness =
            TuiHarness::with_executor(idle_executor()).seeded(&["claude:a", "claude:b"]);
        harness.app.preview = "preview of claude:a".to_string();
        harness.app.previewed_id = Some("claude:a".to_string());
        harness.app.selected = 1;
        harness.app.request_preview();
        let generation = harness.app.current_preview_generation.clone();

        assert!(harness.app.apply_outcome(WorkerOutcome {
            kind: RequestKind::PreviewOnly,
            generation: generation.clone(),
            result: Err(anyhow::anyhow!("selected row disappeared")),
        }));
        assert!(!harness.app.preview.contains("claude:a"));
        assert!(harness.app.preview.contains("claude:b"));
        assert!(harness.app.preview.contains("unavailable"));

        let request = WorkerRequest {
            kind: RequestKind::PreviewOnly,
            generation: generation.clone(),
            query: harness.app.query.clone(),
            filters: harness.app.filters.clone(),
            selected_id: Some("claude:b".to_string()),
        };
        assert!(harness.app.apply_outcome(WorkerOutcome {
            kind: RequestKind::PreviewOnly,
            generation,
            result: Ok(WorkerResponse::preview(
                &request,
                Some("preview of claude:b".to_string()),
            )),
        }));
        assert_eq!(harness.app.preview, "preview of claude:b");
        assert!(harness.app.error.is_none());
    }

    #[test]
    fn preview_only_response_does_not_replace_the_result_list() {
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                let _ = executed_tx.send(request.query.clone());
                Ok(match request.kind {
                    RequestKind::Search => {
                        WorkerResponse::results(request, rows(&["claude:keep", "claude:second"]))
                    }
                    RequestKind::PreviewOnly => {
                        WorkerResponse::preview(request, Some("preview body".to_string()))
                    }
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        harness.step_until_script_drained();
        wait_for_executed(&executed_rx, ""); // startup search (FIFO first)
        harness.wait_until_previewed("claude:keep");
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        wait_for_executed(&executed_rx, ""); // the j preview, after the startup signal
        harness.step();
        assert_eq!(harness.app.selected, 1);
        assert!(
            harness.session_rows().contains("keep"),
            "a preview-only response must never replace the result list (C13)"
        );
    }

    #[test]
    fn startup_and_every_query_change_end_with_a_preview_for_the_selected_row() {
        let (requests_tx, requests_rx) = mpsc::channel::<(RequestKind, String, Option<String>)>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                let _ = requests_tx.send((
                    request.kind,
                    request.query.clone(),
                    request.selected_id.clone(),
                ));
                Ok(match request.kind {
                    RequestKind::Search if request.query.trim().is_empty() => {
                        WorkerResponse::results(request, rows(&["claude:one", "claude:two"]))
                    }
                    RequestKind::Search => WorkerResponse::results(request, rows(&["claude:two"])),
                    RequestKind::PreviewOnly => WorkerResponse::preview(
                        request,
                        request
                            .selected_id
                            .as_deref()
                            .map(|id| format!("preview of {id}")),
                    ),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:one", "claude:two"]);
        // Startup: empty-query Search, then a preview for the row the UI actually selected.
        let (kind, query, _) = wait_for_request(&requests_rx);
        assert_eq!((kind, query.as_str()), (RequestKind::Search, ""));
        // Apply the startup response: the preview request only issues from apply_response,
        // which runs during a step — the signal alone does not apply it.
        let (kind, _, selected) = step_until_next_request(&mut harness, &requests_rx);
        assert_eq!(
            (kind, selected),
            (RequestKind::PreviewOnly, Some("claude:one".into()))
        );
        step_until(&mut harness, |harness| {
            harness.app.preview.contains("claude:one")
        });
        assert!(harness.app.preview.contains("claude:one"));

        // Query change that drops the selected row: the new first row must get its preview.
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('z'))]);
        harness.step_until_script_drained();
        let (kind, query, _) = wait_for_request(&requests_rx);
        assert_eq!((kind, query.as_str()), (RequestKind::Search, "z"));
        let (kind, _, selected) = step_until_next_request(&mut harness, &requests_rx);
        assert_eq!(
            (kind, selected),
            (RequestKind::PreviewOnly, Some("claude:two".into())),
            "the preview must follow whatever row the UI actually selected (C28)"
        );
        step_until(&mut harness, |harness| {
            harness.app.preview.contains("claude:two")
        });
        assert!(harness.app.preview.contains("claude:two"));
    }

    #[test]
    fn a_preview_overtaken_by_newer_navigation_is_discarded() {
        let (release, gate) = mpsc::channel::<()>();
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let (parking_tx, parking_rx) = mpsc::channel::<(Option<String>, Arc<QueryCancellation>)>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::PreviewOnly {
                    // Signal WHICH preview is parking so the test can wait for the startup
                    // preview to provably start — otherwise j's request can queue alongside
                    // it and the worker's drain-to-latest legitimately drops the first.
                    let _ =
                        parking_tx.send((request.selected_id.clone(), Arc::clone(cancellation)));
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                let _ = executed_tx.send(request.query.clone());
                Ok(match request.kind {
                    RequestKind::Search => {
                        WorkerResponse::results(request, rows(&["claude:one", "claude:two"]))
                    }
                    RequestKind::PreviewOnly => WorkerResponse::preview(
                        request,
                        request
                            .selected_id
                            .as_deref()
                            .map(|id| format!("preview of {id}")),
                    ),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:one", "claude:two"]);
        harness.step_until_script_drained();
        // Wait until the startup preview (row 0) is provably parked, stepping to drive
        // apply_response — the preview request only issues once the search response lands,
        // and a bare wait would deadlock if that single step ran too early. THEN navigate:
        // the parked request must not still be queued, or drain-to-latest may drop it
        // instead of the overtaken-render discard this test pins.
        let mut turns = 0;
        let first_preview = loop {
            match parking_rx.recv_timeout(Duration::from_millis(50)) {
                Ok((selected, cancellation)) if selected.as_deref() == Some("claude:one") => {
                    break cancellation;
                }
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    turns += 1;
                    assert!(turns < MAX_TEST_STEPS, "startup preview never parked");
                    harness.step();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("worker died before the startup preview parked");
                }
            }
        };
        // j selects row 1 and queues its preview.
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.selected, 1);
        assert!(
            first_preview.is_cancelled(),
            "new navigation must cancel the obsolete O(M) preview scan"
        );
        release.send(()).unwrap();
        wait_for_executed(&executed_rx, ""); // startup search (FIFO first)
        wait_for_executed(&executed_rx, ""); // the overtaken row-0 preview
        harness.step();
        assert!(
            !harness.app.preview.contains("claude:one"),
            "a preview overtaken by newer navigation must be discarded, not rendered beside the wrong row"
        );
        // The queued row-1 preview then executes (drained burst) and applies.
        release.send(()).unwrap();
        wait_for_executed(&executed_rx, "");
        step_until(&mut harness, |harness| {
            harness.app.preview.contains("claude:two")
        });
        assert!(harness.app.preview.contains("claude:two"));
        finish_gated(&release);
    }

    #[test]
    fn selection_and_preview_scroll_survive_a_response_that_still_contains_it() {
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                let _ = executed_tx.send(request.query.clone());
                Ok(match request.kind {
                    RequestKind::Search if request.query.trim().is_empty() => {
                        WorkerResponse::results(
                            request,
                            rows(&["claude:one", "claude:two", "claude:three"]),
                        )
                    }
                    // Same set, reordered: the selected id survives, at a new position.
                    RequestKind::Search => WorkerResponse::results(
                        request,
                        rows(&["claude:three", "claude:two", "claude:one"]),
                    ),
                    RequestKind::PreviewOnly => WorkerResponse::preview(
                        request,
                        Some(
                            (0..50)
                                .map(|line| format!("line {line}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                    ),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&[
            "claude:one",
            "claude:two",
            "claude:three",
        ]);
        harness.step_until_script_drained();
        wait_for_executed(&executed_rx, ""); // startup search executed
                                             // The startup preview only issues once the search response applies — stepping
                                             // drives apply_response, a bare wait would deadlock.
        step_until_executed(&mut harness, &executed_rx, "");
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        wait_for_executed(&executed_rx, ""); // row-1 preview
        harness.step();
        harness.script(vec![
            key(KeyCode::Char('J')),
            key(KeyCode::Char('J')),
            key(KeyCode::Char('J')),
        ]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.preview_scroll, 15);
        assert_eq!(harness.app.selected, 1);

        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('z'))]);
        harness.step_until_script_drained();
        wait_for_executed(&executed_rx, "z"); // the reordered search response
                                              // Wait for the response to APPLY (its first row becomes claude:three), then assert
                                              // preservation — asserting on the pre-response state would prove nothing.
        step_until(&mut harness, |harness| {
            harness.app.results.first().map(|s| s.id.as_str()) == Some("claude:three")
        });
        assert_eq!(
            harness.app.selected, 1,
            "a response still containing the selected session must preserve the selection (D4)"
        );
        assert_eq!(harness.app.preview_scroll, 15);
    }

    #[test]
    fn navigation_moves_selection_while_a_preview_is_outstanding() {
        let (release, gate) = mpsc::channel::<()>();
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::PreviewOnly {
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                let _ = executed_tx.send(request.query.clone());
                Ok(match request.kind {
                    RequestKind::Search => WorkerResponse::results(
                        request,
                        rows(&["claude:one", "claude:two", "claude:three"]),
                    ),
                    RequestKind::PreviewOnly => WorkerResponse::preview(
                        request,
                        request
                            .selected_id
                            .as_deref()
                            .map(|id| format!("preview of {id}")),
                    ),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&[
            "claude:one",
            "claude:two",
            "claude:three",
        ]);
        harness.step_until_script_drained();
        // The startup preview parks (outstanding). j and k must move the selection NOW,
        // without waiting for it (G2).
        harness.script(vec![
            key(KeyCode::Char('j')),
            key(KeyCode::Char('j')),
            key(KeyCode::Char('k')),
        ]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.selected, 1);
        finish_gated(&release);
        wait_for_executed(&executed_rx, "");
    }

    #[test]
    fn worker_disconnect_is_reported_once_without_another_key() {
        let executor = Box::new(
            move |_request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                panic!("startup executor exploded")
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        for _ in 0..MAX_TEST_STEPS {
            harness.step();
            if harness.app.error.is_some() {
                break;
            }
        }
        assert!(
            harness
                .app
                .error
                .as_deref()
                .is_some_and(|error| error.contains("worker stopped")),
            "response-channel disconnect must surface without requiring another key"
        );
        assert!(
            !harness.app.drain_responses(),
            "disconnect is reported once"
        );
    }

    #[test]
    fn a_dead_worker_reports_an_error_and_leaves_the_ui_usable() {
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let executor = Box::new(
            move |_request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                // Signal entry so the test knows the panic is in flight — the thread still
                // needs a moment to unwind and close the request channel.
                let _ = entered_tx.send(());
                panic!("executor exploded")
            },
        );
        let mut harness =
            TuiHarness::with_executor(executor).seeded(&["claude:keep", "claude:also"]);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the executor entered (and is now panicking)");
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        // The worker is mid-unwind when the signal fires; keep typing until a send fails —
        // each keystroke retries, and the closed channel is what sets the error line.
        for _ in 0..MAX_TEST_STEPS {
            if harness.app.error.is_some() {
                break;
            }
            harness.script(vec![key(KeyCode::Char('x'))]);
            harness.step_until_script_drained();
        }
        assert!(
            harness.app.error.is_some(),
            "a dead worker must surface as the error line, not a hang or an exit (C14)"
        );
        harness.step();
        assert!(
            harness.error_line().contains("worker"),
            "the error line must render the failure"
        );
        // The list stays navigable and q still quits. Esc first: 'a' left search mode,
        // where j would type into the query instead of moving the selection.
        harness.script(vec![key(KeyCode::Esc)]);
        harness.step_until_script_drained();
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.selected, 1);
        harness.script(vec![key(KeyCode::Char('q'))]);
        assert!(matches!(
            harness.step_until_script_drained(),
            Some(AppAction::Quit)
        ));
    }
    /// Park like a real query would: until released, or until cancelled (a real SQLite query
    /// unblocks on interrupt). A gate-only fake would hang the worker join in Drop.
    fn park_until_released_or_cancelled(
        gate: &mpsc::Receiver<()>,
        cancellation: &QueryCancellation,
    ) {
        loop {
            if cancellation.is_cancelled() {
                return;
            }
            match gate.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(1)),
            }
        }
    }

    #[test]
    fn worker_startup_failure_is_reported_not_swallowed() {
        let factory: ExecutorFactory = Box::new(|| Err(anyhow::anyhow!("cannot open database")));
        let error = spawn_search_worker(factory)
            .err()
            .expect("startup failure must surface through the handshake");
        assert!(
            format!("{error:#}").contains("cannot open database"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn worker_handle_carries_the_main_handles_access_scope() {
        // The factory reports the scope its connection observed; a UI-thread test cannot call
        // access_scope() on a Db living in another thread (P5).
        let factory: ExecutorFactory = Box::new(|| {
            Ok((
                idle_executor(),
                EffectiveAccessScope::AllowedRoots { roots: Vec::new() },
            ))
        });
        let (_worker, observed) = spawn_search_worker(factory).unwrap();
        assert!(
            matches!(observed, EffectiveAccessScope::AllowedRoots { .. }),
            "the observed access scope must travel back to the handle"
        );
    }

    #[test]
    fn worker_refuses_a_mismatched_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        db.upsert_session(&session("claude:alpha"), 0, 0).unwrap();
        let runtime = db.execution_runtime();
        drop(db);
        // Bump the stamp through a raw connection: every in-crate writer writes the current
        // value only (the service.rs precedent for forging drift).
        let raw = rusqlite::Connection::open(&db_path).unwrap();
        raw.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(raw);
        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        let factory = db_backed_executor(
            config,
            EffectiveAccessScope::All,
            runtime,
            TerminalStyle::default(),
        );
        let error = factory().err().expect("schema drift must be refused");
        assert!(
            format!("{error:#}").contains("upgrade aise"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn worker_accepts_every_shared_readable_schema_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        db.upsert_session(&session("claude:alpha"), 0, 0).unwrap();
        let runtime = db.execution_runtime();
        drop(db);
        let raw = rusqlite::Connection::open(&db_path).unwrap();
        raw.pragma_update(None, "user_version", SCHEMA_VERSION - 1)
            .unwrap();
        drop(raw);
        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        let factory = db_backed_executor(
            config,
            EffectiveAccessScope::All,
            runtime,
            TerminalStyle::default(),
        );
        let (_executor, observed) = factory().expect("shared-readable schema must open");
        assert!(matches!(observed, EffectiveAccessScope::All));
    }

    #[test]
    fn an_interrupted_query_is_not_reported_as_an_error() {
        // libsqlite3-sys's Error is a plain pub-fields struct; no constructor suits this case.
        let interrupted = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::OperationInterrupted,
                extended_code: rusqlite::ffi::SQLITE_INTERRUPT,
            },
            None,
        );
        assert!(is_expected_interruption(&anyhow::Error::new(interrupted)));
        let other = anyhow::Error::new(rusqlite::Error::QueryReturnedNoRows);
        assert!(!is_expected_interruption(&other));
    }

    #[test]
    fn navigating_during_a_search_does_not_cancel_it() {
        let (release, gate) = mpsc::channel::<()>();
        let (observed_tx, observed_rx) = mpsc::channel::<(String, Arc<QueryCancellation>)>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                let _ = observed_tx.send((request.query.clone(), Arc::clone(cancellation)));
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                Ok(match request.kind {
                    RequestKind::Search => {
                        WorkerResponse::results(request, rows(&["claude:delivered"]))
                    }
                    RequestKind::PreviewOnly => WorkerResponse::preview(request, None),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        let search = loop {
            let (query, cancellation) = observed_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the search reached the executor");
            if query == "a" {
                break cancellation;
            }
        };
        harness.script(vec![key(KeyCode::Esc), key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        assert!(
            !search.is_cancelled(),
            "navigation must not cancel an in-flight search (C21/F2)"
        );
        release.send(()).unwrap();
        for _ in 0..MAX_TEST_STEPS {
            harness.step();
            if harness.session_rows().contains("delivered") {
                break;
            }
        }
        assert!(
            harness.session_rows().contains("delivered"),
            "the search must still deliver after navigation"
        );
    }

    #[test]
    fn quit_joins_the_worker_without_hanging() {
        let (release, gate) = mpsc::channel::<()>();
        let (observed_tx, observed_rx) = mpsc::channel::<(String, Arc<QueryCancellation>)>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                let _ = observed_tx.send((request.query.clone(), Arc::clone(cancellation)));
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                Ok(WorkerResponse::results(request, Vec::new()))
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        // Wait for the "a" request's own cancellation (the startup "" fires first).
        let search = loop {
            let (query, cancellation) = observed_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("a search reached the executor");
            if query == "a" {
                break cancellation;
            }
        };
        // Drop cancels the in-flight query, closes the request channel, and joins — the
        // cancellation-aware park is what lets the join return.
        drop(harness);
        assert!(
            search.is_cancelled(),
            "Drop must cancel the in-flight query before joining"
        );
        let _ = release;
    }

    fn ordered_ids(results: &[SessionRecord]) -> Vec<String> {
        results.iter().map(|session| session.id.clone()).collect()
    }

    fn id_digest(ids: &[String]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        ids.hash(&mut hasher);
        hasher.finish()
    }

    /// Step until the worker's response has been applied (ids match) or steps run out.
    fn wait_for_results(harness: &mut TuiHarness, expected: &[String]) {
        for _ in 0..MAX_TEST_STEPS {
            if ordered_ids(&harness.app.results) == expected {
                return;
            }
            harness.step();
        }
    }

    #[test]
    fn catalog_session_search_refuses_pre_cancelled_work() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        db.upsert_session(&session("claude:alpha"), 0, 0).unwrap();
        let cancellation = QueryCancellation::new();
        cancellation.cancel();
        let error = CatalogService::new(&db)
            .search_sessions_cancellable(
                "alpha",
                &SearchFilters {
                    limit: 10,
                    ..Default::default()
                },
                None,
                &Config::default().search.scoring,
                &cancellation,
            )
            .unwrap_err();
        assert!(error.is::<QueryCancelled>());
    }

    #[test]
    fn oversized_real_transcript_scoring_observes_mid_scan_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let mut parsed = session("claude:oversized");
        parsed.transcript_text = "x".repeat(16 * 1024 * 1024);
        db.upsert_session(&parsed, 0, 0).unwrap();
        let cancellation = Arc::new(QueryCancellation::new());
        db.install_query_cancellation(&cancellation).unwrap();
        let cancel_from_thread = Arc::clone(&cancellation);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(5));
            cancel_from_thread.cancel();
        });
        let started = std::time::Instant::now();
        let error = CatalogService::new(&db)
            .search_sessions_cancellable(
                "not-present-anywhere",
                &SearchFilters {
                    limit: 10,
                    ..Default::default()
                },
                None,
                &Config::default().search.scoring,
                &cancellation,
            )
            .unwrap_err();
        canceller.join().unwrap();
        assert!(
            is_expected_interruption(&error),
            "unexpected error: {error:#}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "oversized-record cancellation exceeded the bounded scan deadline"
        );
    }

    #[test]
    fn tui_and_catalog_service_return_identical_ordered_results() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        for index in 0..40 {
            let id = format!("claude:alpha-{index:02}");
            db.upsert_session(&session(&id), 0, 0).unwrap();
        }
        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        let factory = db_backed_executor(
            config.clone(),
            db.access_scope().clone(),
            db.execution_runtime(),
            TerminalStyle::default(),
        );
        let mut harness = TuiHarness::with_factory(config.clone(), factory);
        harness.start();
        let catalog = CatalogService::new(&db);

        // Startup empty query: the worker's list path equals CatalogService's list path.
        harness.step_until_script_drained();
        let expected = catalog
            .list_sessions(&harness.app.filters)
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect::<Vec<_>>();
        wait_for_results(&mut harness, &expected);
        assert_eq!(ordered_ids(&harness.app.results), expected);
        assert!(
            expected.len() > 22,
            "parity must cover results beyond the terminal viewport"
        );

        // Matching and no-match queries through the worker and the service.
        for (iteration, query) in ["alpha", "zzz"].into_iter().enumerate() {
            if iteration == 0 {
                harness.script(vec![key(KeyCode::Char('/'))]);
                harness.step_until_script_drained();
            } else {
                for _ in 0.."alpha".len() {
                    harness.script(vec![key(KeyCode::Backspace)]);
                    harness.step_until_script_drained();
                }
            }
            for ch in query.chars() {
                harness.script(vec![key(KeyCode::Char(ch))]);
                harness.step_until_script_drained();
            }
            let expected = catalog
                .search_sessions(
                    query,
                    &harness.app.filters,
                    current_repo(&config).as_deref(),
                    &config.search.scoring,
                )
                .unwrap()
                .into_iter()
                .map(|hit| hit.session.id)
                .collect::<Vec<_>>();
            wait_for_results(&mut harness, &expected);
            let got = ordered_ids(&harness.app.results);
            assert_eq!(
                got, expected,
                "TUI vs CatalogService mismatch for {query:?}"
            );
            assert_eq!(id_digest(&got), id_digest(&expected));
        }
    }
    // ---- step 5: filter bindings, layout config, parity, presentation ----

    fn recording_executor() -> (
        mpsc::Receiver<(RequestKind, SearchFilters, String)>,
        SearchExecutor,
    ) {
        let (tx, rx) = mpsc::channel::<(RequestKind, SearchFilters, String)>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                let _ = tx.send((request.kind, request.filters.clone(), request.query.clone()));
                Ok(match request.kind {
                    RequestKind::Search => {
                        WorkerResponse::results(request, rows(&["claude:keep", "claude:also"]))
                    }
                    RequestKind::PreviewOnly => WorkerResponse::preview(request, None),
                })
            },
        );
        (rx, executor)
    }

    fn wait_for_filters(
        rx: &mpsc::Receiver<(RequestKind, SearchFilters, String)>,
        kind: RequestKind,
    ) -> SearchFilters {
        wait_for_recorded_request(rx, kind).0
    }

    fn wait_for_recorded_request(
        rx: &mpsc::Receiver<(RequestKind, SearchFilters, String)>,
        kind: RequestKind,
    ) -> (SearchFilters, String) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((seen, filters, query)) if seen == kind => return (filters, query),
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("no {kind:?} request arrived");
    }

    #[test]
    fn status_bar_names_every_active_filter_value() {
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        assert_eq!(harness.app.filter_status(), "p:any f:any s:any w:off");
        harness.app.filters.provider = Some(Provider::Codex);
        harness.app.filters.session_kinds = Some(vec![SessionKind::User]);
        harness.app.filters.since = Some(Utc::now() - chrono::Duration::days(7));
        harness.app.filters.warnings_only = true;
        harness.step();
        assert_eq!(harness.app.filter_status(), "p:codex f:user s:7d w:on");
        assert!(harness.status_line().contains("p:codex"));
        assert!(harness.status_line().contains("w:on"));
    }

    #[test]
    fn provider_cycle_key_issues_the_expected_filters() {
        let (filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        let startup = wait_for_filters(&filters_rx, RequestKind::Search);
        assert_eq!(startup.provider, None);
        harness.wait_until_previewed("claude:keep");

        // Every press issues one Search with the next provider, cycling through
        // value_variants() and back to None.
        let variants = Provider::value_variants().to_vec();
        let mut expected: Vec<Option<Provider>> = variants.iter().copied().map(Some).collect();
        expected.push(None);
        for want in expected {
            harness.script(vec![key(KeyCode::Char('p'))]);
            harness.step_until_script_drained();
            let filters = wait_for_filters(&filters_rx, RequestKind::Search);
            assert_eq!(
                filters.provider, want,
                "provider cycle must follow value_variants"
            );
        }
    }

    #[test]
    fn session_kind_key_issues_the_expected_filters() {
        let (filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        let startup = wait_for_filters(&filters_rx, RequestKind::Search);
        assert_eq!(startup.session_kinds, None);
        harness.wait_until_previewed("claude:keep");

        // None -> both classes (the default search set) -> user only -> subagent only -> None.
        let cycle: [Option<Vec<SessionKind>>; 4] = [
            Some(SessionKind::default_search_set()),
            Some(vec![SessionKind::User]),
            Some(vec![SessionKind::Subagent]),
            None,
        ];
        for want in cycle {
            harness.script(vec![key(KeyCode::Char('f'))]);
            harness.step_until_script_drained();
            let filters = wait_for_filters(&filters_rx, RequestKind::Search);
            assert_eq!(
                filters.session_kinds, want,
                "class cycle must visit both, each, none"
            );
        }
    }

    #[test]
    fn since_window_key_issues_the_expected_filters() {
        let (filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        wait_for_filters(&filters_rx, RequestKind::Search);
        harness.wait_until_previewed("claude:keep");

        // 1 day -> 7 days -> 30 days -> off; until stays unset and the bound ages correctly.
        let windows = [24, 24 * 7, 24 * 30];
        for hours in windows {
            harness.script(vec![key(KeyCode::Char('s'))]);
            harness.step_until_script_drained();
            let filters = wait_for_filters(&filters_rx, RequestKind::Search);
            let since = filters.since.expect("window must set since");
            assert_eq!(filters.until, None, "the TUI window cycle sets only since");
            let age_hours = (chrono::Utc::now() - since).num_hours();
            assert!(
                (age_hours - hours).abs() <= 1,
                "since must be ~{hours}h old, got {age_hours}h"
            );
        }
        harness.script(vec![key(KeyCode::Char('s'))]);
        harness.step_until_script_drained();
        let filters = wait_for_filters(&filters_rx, RequestKind::Search);
        assert_eq!(filters.since, None, "the cycle must return to unbounded");
    }

    #[test]
    fn warnings_only_key_issues_the_expected_filters() {
        let (filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        let startup = wait_for_filters(&filters_rx, RequestKind::Search);
        assert!(!startup.warnings_only);
        harness.wait_until_previewed("claude:keep");

        harness.script(vec![key(KeyCode::Char('w'))]);
        harness.step_until_script_drained();
        assert!(wait_for_filters(&filters_rx, RequestKind::Search).warnings_only);
        harness.script(vec![key(KeyCode::Char('w'))]);
        harness.step_until_script_drained();
        assert!(!wait_for_filters(&filters_rx, RequestKind::Search).warnings_only);
    }

    #[test]
    fn filter_validation_failure_renders_error_and_sends_nothing() {
        let (filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        wait_for_filters(&filters_rx, RequestKind::Search);
        harness.wait_until_previewed("claude:keep");

        // The one validation rule: parent + user-only kinds. The TUI has no parent binding,
        // so drive the state directly — the binding path must still refuse to send.
        harness.app.filters.parent_session_id = Some("claude:parent".to_string());
        harness.app.filters.session_kinds = Some(vec![SessionKind::User]);
        // Drain records already delivered (the startup preview) so the no-send assertion
        // below observes only requests issued after the invalid combination exists.
        while filters_rx.try_recv().is_ok() {}
        harness.script(vec![key(KeyCode::Char('w'))]);
        harness.step_until_script_drained();
        assert!(
            harness.app.error.is_some(),
            "a rejected filter combination must render the error line"
        );
        harness.step();
        assert!(harness.error_line().contains("session_kinds"));
        assert!(
            filters_rx.try_recv().is_err(),
            "no request may be sent for an invalid combination"
        );
    }

    #[test]
    fn provider_labels_are_case_folded_distinct_and_fit_the_width() {
        let labels: Vec<&str> = Provider::value_variants()
            .iter()
            .map(|provider| provider_label(*provider).0)
            .collect();
        assert_eq!(labels.len(), 9, "all nine providers must carry labels");
        let folded: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();
        let mut sorted = folded.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            folded.len(),
            sorted.len(),
            "labels must be case-folded distinct"
        );
        for label in &labels {
            assert!(
                label.len() <= longest_provider_label(),
                "{label} must fit the label column floor"
            );
        }
        // A configured width below the floor clamps up, never truncating.
        assert!(longest_provider_label() >= "GEMINICLI".len());
    }

    #[test]
    fn exceptionally_narrow_terminal_clamps_provider_column_without_panicking() {
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:keep"]);
        harness.terminal.backend_mut().resize(12, 14);
        harness.step();
        assert_eq!(harness.terminal.backend().buffer().area.width, 12);
    }

    #[test]
    fn status_bar_and_error_line_fit_an_eighty_column_frame() {
        let (_filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        harness.wait_until_previewed("claude:keep");

        for width in [80u16, 100] {
            let mut narrow = Terminal::new(TestBackend::new(width, 24)).unwrap();
            step(
                &mut narrow,
                &mut ScriptedEventSource::new(Vec::new()),
                &mut harness.app,
            )
            .unwrap();
            let status = screen_rows_text(&narrow, 23..24, width);
            assert!(
                status.chars().count() <= width as usize,
                "status bar must fit {width} columns: {status:?}"
            );

            harness.app.error = Some(
                "search failed: database busy: run `aise reindex --full`, then retry aise tui"
                    .to_string(),
            );
            step(
                &mut narrow,
                &mut ScriptedEventSource::new(Vec::new()),
                &mut harness.app,
            )
            .unwrap();
            let error_row = screen_rows_text(&narrow, 22..23, width);
            assert!(
                error_row.chars().count() <= width as usize,
                "error line must fit {width} columns: {error_row:?}"
            );
            assert!(
                error_row.contains("reindex"),
                "the final recovery clause must survive elision (REQ047)"
            );
            harness.app.error = None;
        }
    }

    fn screen_rows_text(
        terminal: &Terminal<TestBackend>,
        rows: std::ops::Range<u16>,
        _width: u16,
    ) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let mut lines = Vec::new();
        for y in rows {
            if y >= area.bottom() {
                continue;
            }
            let mut line = String::new();
            for x in area.left()..area.right() {
                line.push_str(buffer[(x, y)].symbol());
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    #[test]
    fn wrapped_single_line_preview_can_scroll_to_its_tail() {
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.terminal.backend_mut().resize(30, 10);
        harness.app.preview = "x".repeat(200);
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        assert!(
            harness.app.preview_line_count > 1,
            "wrapped terminal rows, not logical newline count, own the scroll bound"
        );
        harness.app.scroll_preview(isize::MAX);
        assert!(
            harness.app.preview_scroll > 0,
            "wrapped rows={}, viewport={}",
            harness.app.preview_line_count,
            harness.app.preview_viewport_rows
        );
    }

    #[test]
    fn multiword_preview_uses_actual_word_wrapped_row_count() {
        // Seeded, because the pane shows the empty-state guidance when the list is empty and
        // this measures the wrapping of a real preview.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.app.preview = std::iter::repeat_n("abcdefghij", 20)
            .collect::<Vec<_>>()
            .join(" ");
        harness.terminal.backend_mut().resize(30, 10);
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        let pane_interior_width = 15_usize;
        let arithmetic_count = harness
            .app
            .preview
            .chars()
            .count()
            .div_ceil(pane_interior_width);
        assert!(
            harness.app.preview_line_count > arithmetic_count,
            "word-boundary wrapping must use Ratatui's actual line composer"
        );
        harness.app.scroll_preview(isize::MAX);
        assert!(harness.app.preview_scroll > 0);
    }

    /// Every non-empty `Search` query the executor was asked for, in order, drained after the
    /// script settles. The startup request carries an empty query and is excluded.
    fn typed_search_queries(
        rx: &mpsc::Receiver<(RequestKind, SearchFilters, String)>,
    ) -> Vec<String> {
        let mut queries = Vec::new();
        while let Ok((kind, _, query)) = rx.recv_timeout(Duration::from_millis(200)) {
            if kind == RequestKind::Search && !query.is_empty() {
                queries.push(query);
            }
        }
        queries
    }

    #[test]
    fn one_interrupt_arms_and_says_so_and_the_next_one_quits() {
        // Raw mode turns off the terminal's interrupt character, so Ctrl+C is an ordinary key
        // event and nothing raises SIGINT. Browse mode ignored it outright, which left `q` and
        // Esc as the only exits from a full-screen application.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.script(vec![ctrl_key(KeyCode::Char('c'))]);
        assert!(
            harness.step_until_script_drained().is_none(),
            "one interrupt must not quit on its own"
        );
        assert!(
            harness.status_line().contains("interrupt again"),
            "the armed state must be visible, got {:?}",
            harness.status_line()
        );

        harness.script(vec![ctrl_key(KeyCode::Char('c'))]);
        assert!(matches!(
            harness.step_until_script_drained(),
            Some(AppAction::Quit)
        ));
    }

    #[test]
    fn any_other_key_disarms_the_interrupt() {
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.script(vec![ctrl_key(KeyCode::Char('c')), key(KeyCode::Char('j'))]);
        assert!(harness.step_until_script_drained().is_none());
        assert!(
            !harness.status_line().contains("interrupt again"),
            "a key press between the two must clear the armed state"
        );
        harness.script(vec![ctrl_key(KeyCode::Char('c'))]);
        assert!(
            harness.step_until_script_drained().is_none(),
            "the count restarts, so this is the first interrupt again"
        );
    }

    #[test]
    fn the_interrupt_in_the_search_box_does_not_type_a_c() {
        // The search box appended whatever character arrived, modifiers and all, so the one key
        // a reader reaches for to escape put a `c` in their query instead.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.script(vec![
            key(KeyCode::Char('/')),
            key(KeyCode::Char('a')),
            ctrl_key(KeyCode::Char('c')),
        ]);
        assert!(harness.step_until_script_drained().is_none());
        assert_eq!(harness.app.query, "a", "the interrupt is not text");

        harness.script(vec![ctrl_key(KeyCode::Char('c'))]);
        assert!(matches!(
            harness.step_until_script_drained(),
            Some(AppAction::Quit)
        ));
    }

    #[test]
    fn a_chorded_letter_does_not_fire_its_browse_binding() {
        // Each arm matched a bare character, so Ctrl+Q quit, Ctrl+S moved the time window, and
        // Ctrl+H — ASCII backspace on many terminals — scrolled the preview.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.script(vec![
            ctrl_key(KeyCode::Char('q')),
            ctrl_key(KeyCode::Char('s')),
            ctrl_key(KeyCode::Char('p')),
        ]);
        assert!(
            harness.step_until_script_drained().is_none(),
            "Ctrl+Q is not the quit binding"
        );
        assert_eq!(
            harness.app.filter_status(),
            "p:any f:any s:any w:off",
            "Ctrl+S and Ctrl+P are not filter bindings"
        );
    }

    #[test]
    fn a_rebound_key_drives_the_tui_and_the_default_it_replaced_does_not() {
        // The point of the table: a key named in `[ui.keys]` reaches the loop, and the key it
        // replaced stops meaning what it did.
        let mut config = Config::default();
        config.ui.keys = toml::from_str("quit = [\"x\"]").expect("a partial table parses");
        config.validate().expect("rebinding quit alone is valid");
        let mut harness = TuiHarness::with_config(config, idle_executor()).seeded(&["claude:one"]);

        harness.script(vec![key(KeyCode::Char('q'))]);
        assert!(
            harness.step_until_script_drained().is_none(),
            "q was rebound away from quit"
        );
        harness.script(vec![key(KeyCode::Char('x'))]);
        assert!(matches!(
            harness.step_until_script_drained(),
            Some(AppAction::Quit)
        ));
    }

    #[test]
    fn a_typed_burst_becomes_one_search_after_the_configured_quiet_period() {
        // Every keystroke used to start a search that the next keystroke cancelled, so typing a
        // five-letter word began five corpus scans to answer one question. On a 36.5 GB index a
        // session search costs 2.3 to 3.6 s, which is what the reader waits for either way, but
        // the four abandoned scans are work nobody asked for.
        let mut config = Config::default();
        config.ui.search_debounce_ms = 40;
        config.ui.idle_poll_interval_ms = 1;
        let (requests, executor) = recording_executor();
        let mut harness = TuiHarness::with_config(config, executor).seeded(&["claude:keep"]);
        wait_for_recorded_request(&requests, RequestKind::Search);

        harness.script(vec![
            key(KeyCode::Char('/')),
            key(KeyCode::Char('a')),
            key(KeyCode::Char('b')),
            key(KeyCode::Char('c')),
        ]);
        harness.step_until_script_drained();
        step_until(&mut harness, |harness| !harness.app.searching);

        assert_eq!(
            typed_search_queries(&requests),
            vec!["abc".to_string()],
            "a burst inside the quiet period must ask one question, and it must be the whole one"
        );
    }

    #[test]
    fn the_quiet_period_is_honored_however_it_compares_to_the_idle_interval() {
        // Two independent millisecond knobs, and every other test pins the interval at 1, so only
        // "quiet period longer than the interval" was ever exercised. The shorter case is the one
        // `slice.min(delay)` in `step` exists for: without it the wait runs to the end of the idle
        // interval and a 5 ms quiet period costs the reader 500 ms. The search still has to carry
        // the whole typed word in every case, so a delay that fires early is a failure too.
        // The wait is sliced, so `interval` shorter than `debounce` takes several turns to reach
        // it and the pairs are chosen to stay well inside MAX_TEST_STEPS.
        for (debounce_ms, interval_ms) in [(5, 500), (40, 40), (20, 5), (0, 500)] {
            let mut config = Config::default();
            config.ui.search_debounce_ms = debounce_ms;
            config.ui.idle_poll_interval_ms = interval_ms;
            let (requests, executor) = recording_executor();
            let mut harness = TuiHarness::with_config(config, executor).seeded(&["claude:keep"]);
            wait_for_recorded_request(&requests, RequestKind::Search);

            let typed = vec![
                key(KeyCode::Char('/')),
                key(KeyCode::Char('a')),
                key(KeyCode::Char('b')),
                key(KeyCode::Char('c')),
            ];
            // One poll per event read, so the poll after the last key is the first that sleeps.
            let first_sleeping_wait = harness.events.poll_timeouts.len() + typed.len();
            harness.script(typed);
            harness.step_until_script_drained();
            // `flush_edited_query` clears the edit stamp, so this asks "has the search been
            // issued yet" without touching the recorded requests. Waiting on `typed_search_queries`
            // instead would consume the very request the assertion is about.
            step_until(&mut harness, |harness| {
                harness.app.query_edited_at.is_none()
            });

            // Correctness alone does not pin the timing down: drop the shortening and the search
            // still happens, one whole idle interval late — half a second of silence for a five
            // millisecond quiet period. A zero quiet period is exempt because the keystroke has
            // already searched, leaving no pending delay for the wait to be shortened to.
            if debounce_ms > 0 {
                let wait = harness.events.poll_timeouts[first_sleeping_wait];
                assert!(
                    wait <= Duration::from_millis(debounce_ms),
                    "a {debounce_ms} ms quiet period with a {interval_ms} ms idle interval waited \
                     {wait:?} before looking at the edited query again"
                );
            }

            let queries = typed_search_queries(&requests);
            assert_eq!(
                queries.last().map(String::as_str),
                Some("abc"),
                "a {debounce_ms} ms quiet period with a {interval_ms} ms idle interval searched \
                 {queries:?}"
            );
        }
    }

    #[test]
    fn enter_searches_the_typed_query_without_waiting_out_the_quiet_period() {
        // Leaving the search box is the reader saying they are done typing, so the delay has
        // nothing left to wait for. Without this, a long configured delay would look exactly
        // like a TUI that ignores Enter.
        let mut config = Config::default();
        config.ui.search_debounce_ms = 600_000;
        config.ui.idle_poll_interval_ms = 1;
        let (requests, executor) = recording_executor();
        let mut harness = TuiHarness::with_config(config, executor).seeded(&["claude:keep"]);
        wait_for_recorded_request(&requests, RequestKind::Search);

        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        assert!(
            typed_search_queries(&requests).is_empty(),
            "the ten-minute delay must still be holding the search"
        );

        harness.script(vec![key(KeyCode::Enter)]);
        harness.step_until_script_drained();
        let (_, query) = wait_for_recorded_request(&requests, RequestKind::Search);
        assert_eq!(query, "a", "Enter searches what is in the box");
        assert!(!harness.app.search_mode, "Enter also leaves the search box");
    }

    #[test]
    fn a_zero_quiet_period_searches_on_every_keystroke() {
        // 0 names the behavior this had before the setting existed, so it has to keep meaning
        // that rather than becoming an accidental synonym for the default.
        //
        // The assertion is on the state a keystroke leaves behind, not on the sequence the
        // executor observes. The mailbox keeps one pending search, so whether it sees "a" before
        // "ab" replaces it depends on which thread runs next — an earlier version of this test
        // asserted `["a", "ab"]` and passed only while the worker happened to win that race.
        // What distinguishes zero from a delay is whether the request was issued on the key or
        // held, and `query_edited_at` says so deterministically.
        let mut config = Config::default();
        config.ui.search_debounce_ms = 0;
        config.ui.idle_poll_interval_ms = 1;
        let (requests, executor) = recording_executor();
        let mut harness = TuiHarness::with_config(config, executor).seeded(&["claude:keep"]);
        wait_for_recorded_request(&requests, RequestKind::Search);

        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        assert!(
            harness.app.query_edited_at.is_none(),
            "zero issues the search on the keystroke rather than holding it"
        );
        // `searching` is not asserted here: a fake executor can answer within the same drained
        // steps, so whether the flag is still up is another race between the two threads.

        harness.script(vec![key(KeyCode::Char('b'))]);
        harness.step_until_script_drained();
        assert!(harness.app.query_edited_at.is_none());
        step_until(&mut harness, |harness| !harness.app.searching);

        // Whatever the executor coalesced, the last question asked is the whole query.
        let observed = typed_search_queries(&requests);
        assert_eq!(
            observed.last().map(String::as_str),
            Some("ab"),
            "observed {observed:?}"
        );
    }

    #[test]
    fn a_configured_quiet_period_holds_the_keystroke_rather_than_issuing_it() {
        // The mirror of the test above, and the reason `query_edited_at` is the honest signal:
        // with a delay the key leaves a pending edit rather than a request.
        let mut config = Config::default();
        config.ui.search_debounce_ms = 600_000;
        config.ui.idle_poll_interval_ms = 1;
        let (requests, executor) = recording_executor();
        let mut harness = TuiHarness::with_config(config, executor).seeded(&["claude:keep"]);
        wait_for_recorded_request(&requests, RequestKind::Search);

        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        assert!(
            harness.app.query_edited_at.is_some(),
            "a delay holds the edit until typing goes quiet"
        );
        assert!(typed_search_queries(&requests).is_empty());
    }

    #[test]
    fn since_window_cycle_and_status_label_read_the_same_table() {
        // The binding stores an absolute `since`; the status bar reads the window back out of
        // it. Separate thresholds in the two places would let the cycle advance to the 7-day
        // span while the bar still said 1d, so each press must both move to the next span and
        // rename the window.
        let (filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        wait_for_filters(&filters_rx, RequestKind::Search);
        harness.wait_until_previewed("claude:keep");
        assert!(harness.app.filter_status().contains("s:any"));

        for (label, span_hours) in SINCE_WINDOWS {
            harness.script(vec![key(KeyCode::Char('s'))]);
            harness.step_until_script_drained();
            let filters = wait_for_filters(&filters_rx, RequestKind::Search);
            let since = filters.since.expect("the s binding sets a lower bound");
            assert_eq!(
                (Utc::now() - since).num_hours(),
                span_hours,
                "pressing s must move to the {label} span"
            );
            assert!(
                harness.app.filter_status().contains(&format!("s:{label}")),
                "the status bar must name the window the binding just set, got {}",
                harness.app.filter_status()
            );
        }

        harness.script(vec![key(KeyCode::Char('s'))]);
        harness.step_until_script_drained();
        let filters = wait_for_filters(&filters_rx, RequestKind::Search);
        assert_eq!(
            filters.since, None,
            "the last window cycles back to unbounded"
        );
        assert!(harness.app.filter_status().contains("s:any"));
    }

    #[test]
    fn the_status_bar_teaches_the_rebound_key_rather_than_the_default() {
        // A help bar that names keys the configuration replaced is worse than none: the reader
        // presses what it says and nothing happens.
        let mut config = Config::default();
        config.ui.keys = toml::from_str("quit = [\"x\"]").unwrap();
        config.validate().unwrap();
        let mut harness = TuiHarness::with_config(config, idle_executor()).seeded(&["claude:one"]);
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        let status = harness.status_line();
        assert!(status.contains("x: quit"), "{status:?}");
        assert!(!status.contains("q: quit"), "{status:?}");
    }

    /// A transcript with four bookends and a gap, so the preview carries section rules and an
    /// elision marker as well as body text.
    fn bookended_transcript() -> String {
        join_turns(&[
            ("user", "first question"),
            ("assistant", "first answer"),
            ("user", "middle question"),
            ("assistant", "middle answer"),
            ("user", "final question"),
            ("assistant", "final answer"),
        ])
    }

    #[test]
    fn the_preview_title_reports_the_visible_rows_only_when_some_are_hidden() {
        // A pane that shows two thirds of a transcript looked exactly like one showing all of
        // it, so a reader had no reason to reach for the scroll keys and no way to tell whether
        // pressing them had done anything.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.terminal.backend_mut().resize(60, 14);
        harness.app.preview = (0..60)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        let title = harness.app.preview_title();
        assert!(title.contains("1-"), "{title:?}");
        assert!(
            title.contains(&format!("/{}", harness.app.preview_line_count)),
            "{title:?}"
        );

        harness.app.scroll_preview(5);
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        assert!(
            harness.app.preview_title().contains("6-"),
            "scrolling must move the reported window, got {:?}",
            harness.app.preview_title()
        );

        // Content that fits gets no counter: a number that never changes is noise.
        harness.app.preview = "one short line".to_string();
        harness.app.preview_scroll = 0;
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        assert_eq!(harness.app.preview_title(), " Preview ");
    }

    /// Type `text` into the search box, entering it first.
    fn type_query(harness: &mut TuiHarness, text: &str) {
        let mut script = vec![key(KeyCode::Char('/'))];
        script.extend(text.chars().map(|character| key(KeyCode::Char(character))));
        harness.script(script);
        harness.step_until_script_drained();
    }

    #[test]
    fn the_key_list_names_every_bound_command_including_the_ones_the_status_bar_sheds() {
        // The status bar is one row and drops most hints on an eighty-column frame, so paging,
        // scrolling, top and bottom, and resume had nowhere else to be named.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.terminal.backend_mut().resize(80, 24);
        harness.script(vec![key(KeyCode::Char('?'))]);
        assert!(harness.step_until_script_drained().is_none());
        assert!(harness.app.showing_help);

        let listed = harness.app.help_lines().join("\n");
        for action in TuiAction::ALL {
            assert!(
                listed.contains(action.name()),
                "{} is bound but not listed:\n{listed}",
                action.name()
            );
        }
        let screen = harness.screen();
        assert!(screen.contains("Keys"), "{screen}");
    }

    #[test]
    fn the_key_list_follows_a_rebinding_and_omits_what_is_unbound() {
        let mut config = Config::default();
        config.ui.keys =
            toml::from_str("quit = [\"x\"]\ncycle_provider = []").expect("a partial table parses");
        config.validate().unwrap();
        let harness = TuiHarness::with_config(config, idle_executor()).seeded(&["claude:one"]);
        let listed = harness.app.help_lines().join("\n");
        assert!(listed.contains("x") && listed.contains("quit"), "{listed}");
        assert!(
            !listed.contains("cycle_provider"),
            "an unbound command must not claim to exist:\n{listed}"
        );
    }

    #[test]
    fn every_key_bound_to_one_command_reaches_it_through_the_loop() {
        // `action_for` resolving all three is not the same claim as the browser answering all
        // three: the loop is what a reader presses. Aliases are the reason the list is a list —
        // a reader adding their own key keeps the shipped one.
        let mut config = Config::default();
        config.ui.keys =
            toml::from_str("quit = [\"x\", \"ctrl+q\", \"f5\"]").expect("aliases parse");
        config
            .validate()
            .expect("several keys for one command is not a conflict");

        for press in [
            key(KeyCode::Char('x')),
            ctrl_key(KeyCode::Char('q')),
            key(KeyCode::F(5)),
        ] {
            let mut harness =
                TuiHarness::with_config(config.clone(), idle_executor()).seeded(&["claude:one"]);
            harness.script(vec![press.clone()]);
            assert!(
                matches!(harness.step_until_script_drained(), Some(AppAction::Quit)),
                "{press:?} is bound to quit but did not quit"
            );
        }

        let harness = TuiHarness::with_config(config, idle_executor()).seeded(&["claude:one"]);
        let listed = harness.app.help_lines().join("\n");
        assert!(
            listed.contains("x, ctrl+q, f5"),
            "the key list has to name every alias, not just the first:\n{listed}"
        );
    }

    #[test]
    fn any_other_key_closes_the_key_list_and_scrolling_keeps_it_open() {
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.terminal.backend_mut().resize(80, 14);
        harness.script(vec![key(KeyCode::Char('?'))]);
        harness.step_until_script_drained();

        // The list is taller than a fourteen-row frame, so it has to scroll rather than clip.
        harness.script(vec![ctrl_key(KeyCode::Char('d'))]);
        harness.step_until_script_drained();
        assert!(harness.app.showing_help, "scrolling must not close it");
        assert!(harness.app.help_scroll > 0, "and must actually move");

        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        assert!(!harness.app.showing_help, "any other key closes it");
        assert_eq!(
            harness.app.selected, 0,
            "the key that closed it is spent on closing, not on the browser underneath"
        );
    }

    #[test]
    fn the_caret_moves_and_typing_inserts_where_it_sits() {
        // The box only ever appended, so fixing a typo in the middle of a query meant deleting
        // back to it and retyping the rest, and Left, Right, Home, End, and Delete did nothing
        // at all — keys a reader presses in any other text field.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        type_query(&mut harness, "rust wrker");

        // Left four times puts the caret between `w` and `r`.
        harness.script(vec![key(KeyCode::Left); 4]);
        harness.step_until_script_drained();
        harness.script(vec![key(KeyCode::Char('o'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "rust worker");

        harness.script(vec![key(KeyCode::Home), key(KeyCode::Char('!'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "!rust worker");

        harness.script(vec![key(KeyCode::End), key(KeyCode::Char('?'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "!rust worker?");

        harness.script(vec![key(KeyCode::Home), key(KeyCode::Delete)]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "rust worker?");
    }

    #[test]
    fn backspace_deletes_before_the_caret_rather_than_at_the_end() {
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        type_query(&mut harness, "abcd");
        harness.script(vec![
            key(KeyCode::Left),
            key(KeyCode::Left),
            key(KeyCode::Backspace),
        ]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "acd", "backspace follows the caret");
    }

    #[test]
    fn clearing_and_deleting_a_word_are_bound_and_leave_the_caret_where_they_cut() {
        // Backspace was the only way to shorten a query, so clearing a long one meant holding
        // it down.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        type_query(&mut harness, "rust worker thread");
        harness.script(vec![ctrl_key(KeyCode::Char('w'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "rust worker ");

        // A second press takes the word and the space that preceded it, not just the space.
        harness.script(vec![ctrl_key(KeyCode::Char('w'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "rust ");

        harness.script(vec![ctrl_key(KeyCode::Char('u'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "");
        assert_eq!(harness.app.query_cursor, 0);
    }

    #[test]
    fn the_caret_stays_on_a_character_boundary_in_a_multi_byte_query() {
        // A byte index into a `String` that lands mid-character panics on the next slice, and a
        // query is whatever the reader typed.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        type_query(&mut harness, "café漢字");
        for _ in 0..3 {
            harness.script(vec![key(KeyCode::Left)]);
            harness.step_until_script_drained();
            assert!(
                harness.app.query.is_char_boundary(harness.app.query_cursor),
                "cursor {} is inside a character of {:?}",
                harness.app.query_cursor,
                harness.app.query
            );
        }
        // Three Lefts from the end put the caret between `f` and `é`, so backspace takes the
        // `f`. Each of those characters is a different width in bytes, which is the point.
        harness.script(vec![key(KeyCode::Backspace)]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query, "caé漢字");

        harness.script(vec![key(KeyCode::End)]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.query_cursor, harness.app.query.len());
    }

    #[test]
    fn moving_the_caret_does_not_start_a_search() {
        // An arrow key changes nothing to search for, so restarting the delay on one would keep
        // a reader who is repositioning from ever seeing a result.
        let mut config = Config::default();
        config.ui.search_debounce_ms = 600_000;
        config.ui.idle_poll_interval_ms = 1;
        let (requests, executor) = recording_executor();
        let mut harness = TuiHarness::with_config(config, executor).seeded(&["claude:keep"]);
        wait_for_recorded_request(&requests, RequestKind::Search);
        type_query(&mut harness, "ab");
        harness.app.flush_edited_query();
        step_until(&mut harness, |harness| !harness.app.searching);
        assert_eq!(
            typed_search_queries(&requests).last().map(String::as_str),
            Some("ab")
        );

        harness.script(vec![
            key(KeyCode::Left),
            key(KeyCode::Right),
            key(KeyCode::Home),
        ]);
        harness.step_until_script_drained();
        assert!(
            harness.app.query_edited_at.is_none(),
            "a caret move is not an edit"
        );
        assert!(typed_search_queries(&requests).is_empty());
    }

    #[test]
    fn a_query_wider_than_the_box_scrolls_under_the_caret() {
        // Without this the text a reader is typing runs past the right border and the caret
        // goes with it, so the box looks like it stopped accepting input.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.terminal.backend_mut().resize(24, 14);
        type_query(&mut harness, "0123456789abcdefghijklmnop");
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        let area = harness.terminal.get_frame().area();
        let cursor = harness.terminal.get_cursor_position().unwrap();
        assert!(
            cursor.x > area.x && cursor.x < area.x + area.width - 1,
            "caret at {} is outside the box {}..{}",
            cursor.x,
            area.x,
            area.x + area.width
        );
        assert!(
            harness.search_box().contains('p'),
            "the tail being typed must be the part on screen: {:?}",
            harness.search_box()
        );
    }

    #[test]
    fn the_selected_row_is_marked_and_the_marker_does_not_move_the_others() {
        // Colour and bold were the only things saying which row Enter would resume, and both are
        // lost on a monochrome terminal, to a colour-blind reader, and in a captured log.
        let mut harness =
            TuiHarness::with_executor(idle_executor()).seeded(&["claude:one", "claude:two"]);
        harness.app.selected = 1;
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        let rows: Vec<String> = harness
            .session_rows()
            .lines()
            .filter(|line| line.contains('['))
            .map(str::to_string)
            .collect();
        assert_eq!(rows.len(), 2, "{rows:?}");
        // The rendered rows still carry the pane border, so this looks for the marker inside
        // the row rather than at its start.
        let marker = TerminalStyle::default().selection_symbol().trim_end();
        assert!(rows[1].contains(marker), "{rows:?}");
        assert!(!rows[0].contains(marker), "{rows:?}");
        // The unselected row keeps the marker's column, so moving the selection does not slide
        // every other row sideways.
        // Display columns, not byte offsets: the marker is multi-byte, so `find` would report
        // two aligned rows as misaligned.
        let column = |row: &str| {
            let at = row.find('[').expect("every row shows a provider label");
            UnicodeWidthStr::width(&row[..at])
        };
        assert_eq!(column(&rows[0]), column(&rows[1]), "{rows:?}");
    }

    #[test]
    fn an_ascii_terminal_renders_nothing_outside_ascii() {
        // `LANG=C` over ssh is a real terminal, and every box border, ellipsis, status separator,
        // section rule, and elision marker the browser drew was outside ASCII. One assertion over
        // the finished frame is the only check that cannot be passed by fixing four of five.
        let mut config = Config::default();
        config.ui.unicode = CapabilityMode::Off;
        let ascii = TerminalStyle::resolve(CapabilityMode::Off, CapabilityMode::Auto);
        let mut harness = TuiHarness::with_config(config, idle_executor()).seeded(&["claude:one"]);
        harness.app.preview =
            build_transcript_summary_inner(&bookended_transcript(), 34, ascii, None).unwrap();
        harness.app.error = Some(
            "a failure long enough that the error line has to cut something out of its middle"
                .to_string(),
        );
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();

        let buffer = harness.terminal.backend().buffer();
        let area = buffer.area;
        let mut offenders = Vec::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let symbol = buffer[(x, y)].symbol().to_string();
                if !symbol.is_ascii() {
                    offenders.push(symbol);
                }
            }
        }
        offenders.sort();
        offenders.dedup();
        assert!(
            offenders.is_empty(),
            "these are not ASCII: {offenders:?}\n{}",
            harness.screen()
        );
        // And the frame still drew a border and a cut marker, so the check is not passing on an
        // empty screen.
        let screen = harness.screen();
        assert!(screen.contains('+') && screen.contains('|'), "{screen}");
        assert!(screen.contains("..."), "{screen}");
    }

    #[test]
    fn a_monochrome_terminal_drops_the_colour_and_keeps_the_emphasis() {
        // Under NO_COLOR or TERM=dumb the selected row was distinguished by colour alone.
        let mut config = Config::default();
        config.ui.color = CapabilityMode::Off;
        let mut harness = TuiHarness::with_config(config, idle_executor()).seeded(&["claude:one"]);
        harness.app.error = Some("a failure".to_string());
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();

        let buffer = harness.terminal.backend().buffer();
        let area = buffer.area;
        let mut bold = false;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &buffer[(x, y)];
                assert_eq!(cell.fg, Color::Reset, "coloured cell at {x},{y}");
                assert_eq!(cell.bg, Color::Reset, "coloured cell at {x},{y}");
                bold |= cell.modifier.contains(Modifier::BOLD);
            }
        }
        assert!(bold, "emphasis must survive when the colour does not");
    }

    #[test]
    fn the_key_list_says_when_it_continues_below_the_fold_and_how_to_get_there() {
        // The overlay covers the whole frame, status bar included, so its title is the only
        // chrome it has. Twenty-eight commands plus headings do not fit a twenty-four-row
        // terminal: the list stopped mid-way with nothing saying more existed, and the keys
        // that scroll it were behind the bar it had just covered.
        let mut short = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        short.terminal.backend_mut().resize(100, 24);
        short.script(vec![key(KeyCode::Char('?'))]);
        short.step_until_script_drained();
        short
            .terminal
            .draw(|frame| short.app.render(frame))
            .unwrap();
        assert!(short.app.showing_help, "the script did not open the list");
        let title = short.region_text(0..1);
        assert!(
            title.contains("/32"),
            "the key list hid its length: {title:?}"
        );
        assert!(
            title.contains("K/J: scroll"),
            "the key list named no way to reach the rest: {title:?}"
        );

        // A frame tall enough for every command says none of that, because none of it applies.
        let mut tall = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        tall.terminal.backend_mut().resize(100, 60);
        tall.script(vec![key(KeyCode::Char('?'))]);
        tall.step_until_script_drained();
        tall.terminal.draw(|frame| tall.app.render(frame)).unwrap();
        let title = tall.region_text(0..1);
        assert!(
            title.contains("Keys (any other key closes)"),
            "a list that fits gained a scroll hint: {title:?}"
        );
    }

    #[test]
    fn an_empty_result_set_names_the_keys_that_change_it() {
        // "No sessions matched the current query." stated the outcome and offered no way out of
        // it, and it said "query" even when there was none. An empty state that names the next
        // action is the difference between a dead end and a step; the keys come from the
        // bindings, because a message naming `/` after a reader has rebound it is worse than
        // no message.
        //
        // Nothing indexed and nothing asked for is a different problem from a query that
        // excluded everything, and only one of them is fixed with a key.
        let mut empty = TuiHarness::with_executor(idle_executor());
        empty.terminal.backend_mut().resize(120, 24);
        for _ in 0..8 {
            empty.step();
        }
        empty
            .terminal
            .draw(|frame| empty.app.render(frame))
            .unwrap();
        let screen = empty.region_text(0..24);
        assert!(
            screen.contains("No sessions are indexed yet"),
            "an empty index blamed the query: {screen}"
        );
        assert!(
            screen.contains("aise reindex"),
            "an empty index named no way to fill it: {screen}"
        );

        // A query that matched nothing: the keys that change the query and the filters.
        let mut filtered = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        filtered.terminal.backend_mut().resize(120, 24);
        filtered.app.query = "matches-nothing".to_string();
        filtered.app.results.clear();
        filtered
            .terminal
            .draw(|frame| filtered.app.render(frame))
            .unwrap();
        let screen = filtered.region_text(0..24);
        for required in ["No sessions matched", "/: edit the query", "?: every key"] {
            assert!(
                screen.contains(required),
                "{required:?} missing from the empty state: {screen}"
            );
        }
        assert!(
            screen.contains("p/f/s/w: change filters"),
            "the empty state named no filter keys: {screen}"
        );
    }

    #[test]
    fn the_search_box_names_its_own_commands_instead_of_leaving_them_to_be_guessed() {
        // The search box binds eight editing commands and the status bar named none of them:
        // it read `type to search │ enter: browse`, so Ctrl+U, Ctrl+W, Home and End were
        // reachable only by pressing Esc first and then `?`. A focused input naming its own
        // keys is what lazygit's prompt footer and fzf's header do, and the roster here is
        // built from the bindings, so rebinding one changes what the bar teaches.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.terminal.backend_mut().resize(160, 24);
        harness.script(vec![key(KeyCode::Char('/'))]);
        harness.step_until_script_drained();
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        let status = harness.status_line();
        assert!(harness.app.search_mode, "the script did not enter the box");
        for required in [
            "type to search",
            "enter: browse",
            "ctrl+u: clear",
            "ctrl+w: delete word",
            "home/end: line start/end",
        ] {
            assert!(
                status.contains(required),
                "{required:?} missing from the search box status: {status:?}"
            );
        }

        // Narrow frames shed, but the way out of the box is priority 0 and cannot be shed --
        // a reader who cannot leave the search box cannot reach anything else.
        for width in [120_u16, 100, 80, 60] {
            let mut narrow = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
            narrow.terminal.backend_mut().resize(width, 24);
            narrow.script(vec![key(KeyCode::Char('/'))]);
            narrow.step_until_script_drained();
            narrow
                .terminal
                .draw(|frame| narrow.app.render(frame))
                .unwrap();
            let status = narrow.status_line();
            assert!(
                status.contains("enter: browse"),
                "width {width} dropped the way out of the box: {status:?}"
            );
            assert!(
                !status.contains('…'),
                "width {width} cut a hint mid-word: {status:?}"
            );
            assert!(
                UnicodeWidthStr::width(status.as_str()) <= usize::from(width),
                "width {width} overflowed the frame: {status:?}"
            );
        }
    }

    #[test]
    fn status_bar_drops_whole_hints_instead_of_eliding_them() {
        // Middle-eliding the joined help line rendered `p:any … │ j/k: move │ P…rs │ /: search`
        // at 80 columns: no binding the reader can act on. Whatever fits must fit whole.
        //
        // The roster that survives each width is not asserted, because it changes whenever a
        // hint is added and asserting it would only record today's arithmetic. What must hold
        // is that nothing is cut mid-word, that the line fits, and that the two hints which
        // lead everywhere else survive every width a terminal is likely to have: `q: quit`,
        // and `?: keys`, which names every command the bar had to shed.
        for width in [120_u16, 100, 80, 60] {
            let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
            harness.terminal.backend_mut().resize(width, 24);
            harness
                .terminal
                .draw(|frame| harness.app.render(frame))
                .unwrap();
            let status = harness.status_line();
            assert!(
                !status.contains('…'),
                "width {width} cut a hint mid-word: {status:?}"
            );
            assert!(
                status.contains("q: quit"),
                "width {width} dropped the way out: {status:?}"
            );
            // `?: keys` survives to eighty columns. Below that the filter status outranks it,
            // because a reader who cannot see that a filter is on can misread the result set,
            // while one who cannot see `?` has only lost a shortcut to the key list.
            if width >= 80 {
                assert!(
                    status.contains("?: keys"),
                    "width {width} dropped {:?}: {status:?}",
                    "?: keys"
                );
            }
            assert!(
                UnicodeWidthStr::width(status.as_str()) <= usize::from(width),
                "width {width} overflowed the frame: {status:?}"
            );
        }

        // A wide frame still shows the whole roster, so the shedding above is scarcity rather
        // than a hint that stopped being rendered at all.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.terminal.backend_mut().resize(160, 24);
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        let status = harness.status_line();
        for required in [
            "j/k: move",
            "K/J: scroll",
            "p/f/s/w: filters",
            "/: search",
            "enter: resume",
            "?: keys",
            "q: quit",
        ] {
            assert!(
                status.contains(required),
                "{required:?} missing: {status:?}"
            );
        }
    }

    #[test]
    fn reapplying_the_same_preview_keeps_the_wrapped_scroll_position() {
        // Two authorities counted preview length: the worker's logical `lines().count()` and the
        // renderer's word-wrapped row count. Clamping on re-apply against the smaller logical
        // count dragged a reader who had scrolled into the wrapped tail back to the top. A
        // preview re-arrives for the row already on screen whenever an earlier preview failure
        // is retried, so the rendered count has to own the bound.
        let mut harness = TuiHarness::with_executor(idle_executor()).seeded(&["claude:one"]);
        harness.terminal.backend_mut().resize(30, 10);
        harness.app.previewed_id = Some("claude:one".to_string());
        harness.app.preview = std::iter::repeat_n("abcdefghij", 20)
            .collect::<Vec<_>>()
            .join(" ");
        harness
            .terminal
            .draw(|frame| harness.app.render(frame))
            .unwrap();
        harness.app.scroll_preview(isize::MAX);
        let scrolled = harness.app.preview_scroll;
        assert!(
            scrolled > 0,
            "the fixture must wrap past its viewport for this to mean anything"
        );

        let request = WorkerRequest {
            kind: RequestKind::PreviewOnly,
            generation: harness.app.current_preview_generation.clone(),
            query: String::new(),
            filters: harness.app.filters.clone(),
            selected_id: Some("claude:one".to_string()),
        };
        let same_text = harness.app.preview.clone();
        harness
            .app
            .apply_response(WorkerResponse::preview(&request, Some(same_text)));

        assert_eq!(
            harness.app.preview_scroll, scrolled,
            "re-applying the identical preview must not move the reader's position"
        );
    }

    #[test]
    fn resize_to_a_short_terminal_keeps_the_last_preview_line_visible() {
        let (release, gate) = mpsc::channel::<()>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::PreviewOnly {
                    park_until_released_or_cancelled(&gate, cancellation);
                }
                Ok(match request.kind {
                    RequestKind::Search => WorkerResponse::results(request, rows(&["claude:one"])),
                    RequestKind::PreviewOnly => WorkerResponse::preview(
                        request,
                        Some(
                            (0..50)
                                .map(|line| format!("line {line}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                    ),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:one"]);
        // Release the parked startup preview and settle it.
        let _ = release.send(());
        for _ in 0..MAX_TEST_STEPS {
            harness.step();
            if harness.app.previewed_id.is_some() {
                break;
            }
        }
        // Shrink to 14 rows, then scroll to the NEW maximum: the final content line must be
        // visible there (D11's strong form — an in-range bound is weaker than the property;
        // the viewport bound makes the last line land at the pane bottom).
        harness.terminal.backend_mut().resize(100, 14);
        harness.step(); // render records the new viewport height
        for _ in 0..64 {
            harness.script(vec![ctrl_key(KeyCode::Char('d'))]);
            harness.step_until_script_drained();
        }
        let text = screen_rows_text(&harness.terminal, 4..12, 100);
        assert!(
            text.contains("line 49"),
            "the last content line must be visible at maximum scroll after shrinking: {text:?}"
        );
        let _ = release;
    }

    #[test]
    fn list_title_names_the_active_mode() {
        let (_filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        harness.wait_until_previewed("claude:keep");
        let rows = harness.session_rows();
        assert!(
            rows.contains("Sessions"),
            "the registered benchmark title survives"
        );
        assert!(
            rows.contains("recent"),
            "the empty query names its ordering"
        );

        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('k'))]);
        harness.step_until_script_drained();
        assert!(
            harness.session_rows().contains("ranked"),
            "a typed query names its ordering"
        );
    }

    #[test]
    fn extreme_configured_steps_preserve_navigation_direction() {
        let mut config = Config::default();
        config.ui.list_page_rows = usize::MAX;
        config.ui.preview_page_rows = usize::MAX;
        let mut harness = TuiHarness::with_config(config, idle_executor()).seeded(&[
            "claude:one",
            "claude:two",
            "claude:three",
        ]);
        harness.app.selected = 1;
        harness.app.preview_line_count = usize::from(u16::MAX) + 100;
        harness.app.preview_viewport_rows = 10;
        harness.app.preview_scroll = 1;

        harness
            .app
            .handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(
            harness.app.selected, 2,
            "PageDown must never wrap into an upward move"
        );
        harness
            .app
            .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(
            harness.app.preview_scroll,
            u16::MAX,
            "a huge forward preview step must saturate, never wrap backward"
        );
    }

    #[test]
    fn visible_session_rows_are_bounded_by_the_terminal_viewport() {
        assert_eq!(visible_session_range(10_000, 0, 18), 0..18);
        assert_eq!(visible_session_range(10_000, 5_000, 18), 4_991..5_009);
        assert_eq!(visible_session_range(10_000, 9_999, 18), 9_982..10_000);
        assert_eq!(visible_session_range(10_000, 4, 0), 0..0);
        assert!(visible_session_range(10_000, 5_000, 18).len() <= 18);
    }

    #[test]
    fn provider_label_width_is_capped_to_list_interior() {
        let mut config = Config::default();
        config.ui.provider_label_width = usize::MAX;
        let mut harness = TuiHarness::with_config(config, idle_executor()).seeded(&["claude:one"]);
        harness.step();
        assert!(
            screen_text(&harness.terminal)
                .lines()
                .all(|line| line.chars().count() <= 100),
            "configured formatting width must remain bounded by the rendered pane"
        );
    }

    #[test]
    fn ui_layout_fields_reach_the_render() {
        // A configured list-pane share of 70% moves the divider right of the default 45%,
        // and a provider-label width below the floor still renders every label whole.
        let mut config = Config::default();
        config.ui.list_pane_percent = 70;
        config.ui.provider_label_width = 2;
        let (_filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_config(config, executor).seeded(&["claude:keep"]);
        harness.wait_until_previewed("claude:keep");
        let rows = harness.session_rows();
        assert!(
            rows.contains("CLAUDE"),
            "the label renders whole despite the configured width of 2 (upward clamp)"
        );

        let divider = divider_column(&harness);
        let default_harness_divider = {
            let (_rx2, executor2) = recording_executor();
            let mut h2 = TuiHarness::with_executor(executor2).seeded(&["claude:keep"]);
            h2.wait_until_previewed("claude:keep");
            divider_column(&h2)
        };
        assert!(
            divider > default_harness_divider,
            "list_pane_percent=70 must widen the list pane (divider {divider} vs default {default_harness_divider})"
        );
    }

    fn divider_column(harness: &TuiHarness) -> u16 {
        let buffer = harness.terminal.backend().buffer();
        let area = buffer.area;
        (1..area.width - 1)
            .filter(|&x| {
                (4..area.height - 2)
                    .filter(|y| *y < area.height)
                    .filter(|y| *y < area.height)
                    .all(|y| buffer[(x, y)].symbol() == "│")
            })
            .max()
            .expect("the pane divider must exist")
    }
    // ---- step 6: [ui].preview_body_lines as the preview body budget (Decision 2a) ----

    fn harness_with_preview_source(
        budget: usize,
        transcript: &str,
        normalized_message_source: Option<&str>,
    ) -> (TuiHarness, Config) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        let mut parsed = session("claude:long");
        parsed.session.cwd = Some("/fixture/project".to_string());
        parsed.transcript_text = transcript.to_string();
        if let Some(message_source) = normalized_message_source {
            parsed.messages = parse_turns(message_source)
                .into_iter()
                .enumerate()
                .map(|(seq, turn)| crate::models::Message {
                    seq: seq as i64,
                    role: match turn.role {
                        TurnRole::User => Role::User,
                        TurnRole::Assistant => Role::Assistant,
                    },
                    ts: None,
                    tool_name: None,
                    kind: crate::models::MessageKind::Conversation,
                    tool_call_id: None,
                    is_compaction: false,
                    content: turn.body.to_string(),
                    provenance: crate::models::MessageProvenance::default(),
                })
                .collect();
        }
        db.upsert_session(&parsed, 0, 0).unwrap();
        let runtime = db.execution_runtime();
        drop(db);
        let mut config = Config::default();
        config.ui.preview_body_lines = budget;
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        let factory = db_backed_executor(
            config.clone(),
            EffectiveAccessScope::All,
            runtime,
            TerminalStyle::default(),
        );
        let harness = TuiHarness::with_factory(config.clone(), factory).own_external_fixture(dir);
        (harness, config)
    }

    fn harness_with_preview_budget(budget: usize, transcript: &str) -> (TuiHarness, Config) {
        harness_with_preview_source(budget, transcript, Some(transcript))
    }

    #[test]
    fn harness_drop_closes_databases_before_removing_owned_directories() {
        let internal;
        let external;
        {
            let (harness, _config) = harness_with_preview_budget(
                HISTORICAL_PREVIEW_BODY_LINES,
                &join_turns(&[("user", "hello")]),
            );
            internal = harness._dir.path().to_path_buf();
            external = harness
                .external_fixture
                .as_ref()
                .expect("production fixture is owned")
                .path()
                .to_path_buf();
        }
        assert!(!internal.exists());
        assert!(!external.exists());
    }

    #[test]
    fn default_preview_preserves_historical_single_section_limit() {
        let body = (0..40)
            .map(|line| format!("body line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let transcript = join_turns(&[("user", &body)]);
        let summary =
            build_transcript_summary(&transcript, Config::default().ui.preview_body_lines);

        assert!(summary.contains("body line 7"));
        assert!(
            !summary.contains("body line 8"),
            "the default preview must preserve main's eight-line first-prompt limit: {summary}"
        );
    }

    #[test]
    fn transcript_preview_falls_back_when_normalized_messages_are_absent() {
        let transcript = join_turns(&[
            ("user", "first prompt"),
            ("assistant", "first reply"),
            ("user", "final prompt"),
            ("assistant", "final reply"),
        ]);
        let budget = Config::default().ui.preview_body_lines;
        let (mut harness, _config) = harness_with_preview_source(budget, &transcript, None);
        harness.start();
        for _ in 0..MAX_TEST_STEPS {
            harness.step();
            if harness.app.previewed_id.is_some() {
                break;
            }
        }

        assert_eq!(
            harness.app.preview,
            format!(
                "Session: claude:long\nCWD: /fixture/project\n\n{}",
                build_transcript_summary(&transcript, budget)
            ),
            "readable indexes with transcript rows but no normalized messages must retain the established preview"
        );
    }

    #[test]
    fn transcript_preview_excludes_noncanonical_normalized_message_content() {
        let transcript = join_turns(&[("user", "direct prompt"), ("assistant", "direct reply")]);
        let normalized = join_turns(&[
            (
                "user",
                "generated tool output that is absent from transcript",
            ),
            ("user", "direct prompt"),
            ("assistant", "direct reply"),
            (
                "user",
                "injected harness notice that is absent from transcript",
            ),
        ]);
        let budget = Config::default().ui.preview_body_lines;
        let (mut harness, _config) =
            harness_with_preview_source(budget, &transcript, Some(&normalized));
        harness.start();
        for _ in 0..MAX_TEST_STEPS {
            harness.step();
            if harness.app.previewed_id.is_some() {
                break;
            }
        }

        assert!(harness.app.preview.contains("Session: claude:long"));
        assert!(harness.app.preview.contains("CWD: /fixture/project"));
        assert!(harness.app.preview.contains("direct prompt"));
        assert!(harness.app.preview.contains("direct reply"));
        assert!(!harness.app.preview.contains("generated tool output"));
        assert!(!harness.app.preview.contains("injected harness notice"));
    }

    #[test]
    fn ui_config_field_reaches_the_rendered_preview() {
        // Eight turns with 30-line bodies: every section truncates under any plausible
        // budget, so the budget's effect on line counts is observable.
        let long_body: String = (0..30)
            .map(|line| format!("body line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let transcript = join_turns(&[
            ("user", &long_body),
            ("assistant", &long_body),
            ("user", &long_body),
            ("assistant", &long_body),
            ("user", &long_body),
            ("assistant", &long_body),
            ("user", &long_body),
            ("assistant", &long_body),
        ]);

        let (mut tight, _config) = harness_with_preview_budget(1, &transcript);
        tight.start();
        for _ in 0..MAX_TEST_STEPS {
            tight.step();
            if tight.app.previewed_id.is_some() {
                break;
            }
        }
        let (mut roomy, _config) = harness_with_preview_budget(34, &transcript);
        roomy.start();
        for _ in 0..MAX_TEST_STEPS {
            roomy.step();
            if roomy.app.previewed_id.is_some() {
                break;
            }
        }

        assert!(
            tight.app.preview_line_count < roomy.app.preview_line_count,
            "[ui].preview_body_lines must reach the rendered preview: budget 1 produced {} lines, budget 34 produced {} (D8)",
            tight.app.preview_line_count,
            roomy.app.preview_line_count
        );
        assert!(
            tight.app.preview.contains("[…]"),
            "a tight budget must truncate sections, not erase bookends"
        );
        // Budget 34 reproduces the historical fixed layout (8/4/8/14 weights sum to 34).
        assert!(roomy.app.preview.contains("First prompt"));
        assert!(roomy.app.preview.contains("Final reply"));
        assert_eq!(
            roomy.app.preview,
            format!(
                "Session: claude:long\nCWD: /fixture/project\n\n{}",
                build_transcript_summary(&transcript, 34)
            ),
            "the borrowed canonical-transcript scan must preserve the established complete preview output"
        );
    }
}
