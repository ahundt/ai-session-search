// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::num::NonZeroUsize;

use chrono::Utc;
use clap::ValueEnum;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
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
use crate::db::{
    ConversationBookends, Db, QueryCancellation, QueryCancelled, MIN_READABLE_SCHEMA_VERSION,
    SCHEMA_VERSION,
};
use crate::models::{Provider, Role, SearchFilters, SessionKind, SessionRecord};
use crate::runtime::ExecutionRuntime;
use crate::search_scope::EffectiveAccessScope;
use crate::service::CatalogService;
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

/// Error line height when a keystroke error is being shown. It takes its row from the body,
/// never from the help bar, so REQ047's recovery guidance keeps its line.
const ERROR_LINE_ROWS: u16 = 1;

/// Two border rows plus one content row: the scroll-clamp floor before the first render has
/// recorded a viewport height (D11).
const PREVIEW_VIEWPORT_SLACK: usize = 3;

/// Worst-case rendered width of a list row's " [age]" suffix, from relative_age's output
/// shapes: its longest form is the date branch, " [2026-01-16]".
const AGE_SUFFIX_ALLOWANCE: usize = 13;

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

/// Middle-elide `text` to `width` columns, keeping the tail intact: an anyhow chain ends
/// with its recovery guidance, and REQ047 forbids losing it to a clipped line. Char-count
/// approximation — the TUI's own strings are ASCII with one │ separator.
fn elide_middle(text: &str, width: usize) -> String {
    let total = text.chars().count();
    if total <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let head = width.saturating_sub(2) / 2;
    let tail = width - head - 1;
    let head_chars: String = text.chars().take(head).collect();
    let tail_chars: String = text.chars().skip(total - tail).collect();
    format!("{head_chars}…{tail_chars}")
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
pub fn run(config: &Config, db: &Db) -> Result<()> {
    // Open and validate the worker before entering raw/alternate-screen mode: a slow or failed
    // startup must leave the user's ordinary terminal visible.
    let (worker, _observed_scope) = spawn_search_worker(db_backed_executor(
        config.clone(),
        db.access_scope().clone(),
        db.execution_runtime(),
    ))?;
    let mut app = AppState::new(config, worker)?;

    // `app` is declared before the guard, so panic unwinding restores the terminal before
    // SearchWorker::drop can wait for cancellation. The explicit normal-path drops preserve the
    // same order and release the Db/runtime before any resume prompt or child process.
    let terminal_guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut events = CrosstermEventSource;
    let action = run_app(&mut terminal, &mut events, &mut app);
    drop(terminal_guard);
    drop(app);

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
    // Wait for input in slices no longer than the configured interval, absorbing worker
    // responses between slices. Two measured defects forced this shape (§12): a key's echo
    // must render immediately — not after the interval expires — and a worker response
    // arriving mid-wait must be applied the same way; the first post-fix measurement pinned
    // results p50 at the 150 ms idle interval because nothing drained until the next step.
    // Slices are capped at 10 ms so pickup latency stays far below the configured pacing,
    // and each handled key restarts the idle window, preserving burst draining.
    let mut idle_deadline = std::time::Instant::now()
        .checked_add(Duration::from_millis(app.config.ui.event_poll_interval_ms))
        .ok_or_else(|| anyhow::anyhow!("ui.event_poll_interval_ms exceeds the monotonic clock"))?;
    loop {
        let now = std::time::Instant::now();
        if now >= idle_deadline {
            return Ok(None);
        }
        let slice = idle_deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(10));
        if events.poll(slice)? {
            let Event::Key(key) = events.read()? else {
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
                .checked_add(Duration::from_millis(app.config.ui.event_poll_interval_ms))
                .ok_or_else(|| {
                    anyhow::anyhow!("ui.event_poll_interval_ms exceeds the monotonic clock")
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
/// the UI thread, and it keeps up to `D_max` bytes of transcript out of the search response
/// (C28). The originating query travels back so a superseded response is dropped without a
/// sequence counter; `previewed_id` says which session the preview is *for*, so a preview
/// overtaken by newer navigation is discarded rather than rendered beside the wrong row.
struct WorkerResponse {
    results: Option<Vec<SessionRecord>>,
    previewed_id: Option<String>,
    preview: Option<String>,
    preview_line_count: usize,
}

impl WorkerResponse {
    /// A search response: results only, no preview.
    fn results(_request: &WorkerRequest, rows: Vec<SessionRecord>) -> Self {
        Self {
            results: Some(rows),
            previewed_id: None,
            preview: None,
            preview_line_count: 0,
        }
    }

    /// A preview response. `None` text means "no selected session" and renders the standing
    /// empty-state message, matching the inline preview path.
    fn preview(request: &WorkerRequest, text: Option<String>) -> Self {
        let preview = text.unwrap_or_else(|| NO_SESSIONS_PREVIEW.to_string());
        let preview_line_count = preview.lines().count();
        Self {
            results: None,
            previewed_id: request.selected_id.clone(),
            preview: Some(preview),
            preview_line_count,
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
        match request.kind {
            RequestKind::Search => {
                // Hold the mailbox lock across cancel and replacement: the interrupt handle is
                // connection-scoped, so cancellation must not race publication of its successor.
                if let Some((_, cancellation)) = state.in_flight.take() {
                    cancellation.cancel();
                }
                state.search = Some(request);
            }
            RequestKind::PreviewOnly => {
                // Navigation B supersedes preview A's O(M) metadata scan, but it must never cancel
                // an in-flight Search (the result-list contract covered by C21).
                if state
                    .in_flight
                    .as_ref()
                    .is_some_and(|(kind, _)| *kind == RequestKind::PreviewOnly)
                {
                    if let Some((_, cancellation)) = state.in_flight.take() {
                        cancellation.cancel();
                    }
                }
                state.preview = Some(request);
            }
        }
        self.wake.notify_one();
        Ok(())
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
        if let Some((_, cancellation)) = state.in_flight.take() {
            cancellation.cancel();
        }
        state.search.take();
        state.preview.take();
        state.closed = true;
        self.wake.notify_all();
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
    handle: Option<thread::JoinHandle<()>>,
}

impl SearchWorker {
    fn send(&self, request: WorkerRequest) -> std::result::Result<(), String> {
        self.mailbox.send(request)
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
                worker_mailbox.finish(&cancellation, search_succeeded);
                if cancellation.is_cancelled() {
                    continue;
                }
                match result {
                    Err(error) if is_expected_interruption(&error) => {}
                    result => {
                        let _ = response_tx.send(WorkerOutcome {
                            kind: request.kind,
                            generation: request.generation,
                            result,
                        });
                    }
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
/// the CLI, MCP, and Python use. Preview bookends use normalized message rows directly because
/// they are presentation-local and must not materialize one session's joined transcript.
///
/// Complexity (REQ010): one connection (≤64 MiB page cache, 256 MiB virtual mmap window) sharing
/// the caller's `config.resolve_threads()` Rayon pool. Search/list delegate to their documented
/// bounds. Preview scans `O(M)` lightweight `(seq, role)` entries and retains at most four bodies
/// `O(D_4)`, instead of `O(D_session + M)` transcript-plus-turn-vector memory.
fn db_backed_executor(
    config: Config,
    access: EffectiveAccessScope,
    runtime: Arc<ExecutionRuntime>,
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
        let preview_budget = config.ui.preview_lines;
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
                            // Presentation-local bounded bookends: no full transcript allocation.
                            let text = match request.selected_id.as_deref() {
                                Some(id) => Some(build_bookend_summary(
                                    &db.conversation_bookends(id, cancellation)?,
                                    preview_budget,
                                )),
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

struct AppState<'a> {
    config: &'a Config,
    /// The session filters the browser runs with. The state IS a `SearchFilters` value: no
    /// parallel filter representation exists (R8/§5.5d).
    filters: SearchFilters,
    /// Search worker: owns every database query. The UI thread never blocks on it — it sends
    /// requests and drains responses (§2.3).
    worker: SearchWorker,
    query: String,
    search_mode: bool,
    current_search_generation: RequestGeneration,
    current_preview_generation: RequestGeneration,
    /// True from request submission until the matching search success/error is applied. Rendered
    /// in the list title so the real-terminal benchmark can observe final-generation completion.
    searching: bool,
    selected: usize,
    results: Vec<SessionRecord>,
    preview: String,
    preview_scroll: u16,
    preview_line_count: usize,
    /// The session whose preview is currently rendered: the skip guard for re-reading up to
    /// `D_max` bytes per keystroke, and the match check that discards an overtaken preview.
    previewed_id: Option<String>,
    /// The preview pane's interior height, recorded by `render`, so scroll clamping bounds by
    /// viewport, not just content length (D11). Zero until the first draw; the clamp's slack
    /// floor covers that case.
    preview_viewport_rows: u16,
    /// Last error from a keystroke-triggered operation, shown on its own line. A key press
    /// can never abort the TUI: errors land here instead of propagating through `?`.
    error: Option<String>,
    /// A disconnected response producer is terminal for this worker. Remember reporting it so
    /// every 10 ms idle drain does not redraw the same error forever.
    worker_disconnected_reported: bool,
}

impl<'a> AppState<'a> {
    fn new(config: &'a Config, worker: SearchWorker) -> Result<Self> {
        let mut state = Self::new_quiet(config, worker);
        // The initial empty-query search runs on the worker: the first frame draws before the
        // first query completes, and the startup response populates the list (C28).
        state.request_search();
        Ok(state)
    }

    /// Construct without the startup request — the test harness path, where the first
    /// request must wait until the fixture rows are seeded so the startup response and the
    /// seeded state agree.
    fn new_quiet(config: &'a Config, worker: SearchWorker) -> Self {
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
                limit: tui_result_limit(config.search.default_limit),
                warnings_only: false,
            },
            worker,
            query: String::new(),
            search_mode: false,
            current_search_generation: RequestGeneration::new(),
            current_preview_generation: RequestGeneration::new(),
            searching: false,
            selected: 0,
            results: Vec::new(),
            preview: String::new(),
            preview_scroll: 0,
            preview_line_count: 0,
            previewed_id: None,
            preview_viewport_rows: 0,
            error: None,
            worker_disconnected_reported: false,
        }
    }

    /// Handle one key press. Returns `Some(action)` when the loop should stop. Never returns
    /// `Err`: a database failure becomes `self.error` — a keystroke cannot end the TUI.
    fn handle_key(&mut self, key: KeyEvent) -> Option<AppAction> {
        if self.search_mode {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.search_mode = false;
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.request_search();
                }
                KeyCode::Char(ch) => {
                    self.query.push(ch);
                    self.request_search();
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Some(AppAction::Quit),
                KeyCode::Char('/') => {
                    self.search_mode = true;
                }
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::PageDown => {
                    let page = saturating_step(self.config.ui.list_page_step);
                    self.move_selection(page);
                }
                KeyCode::PageUp => {
                    let page = saturating_step(self.config.ui.list_page_step);
                    self.move_selection(-page);
                }
                KeyCode::Char('g') => self.select_index(0),
                KeyCode::Char('G') => {
                    let last = self.results.len().saturating_sub(1);
                    self.select_index(last);
                }
                KeyCode::Char('p') => self.cycle_provider(),
                KeyCode::Char('f') => self.cycle_session_kinds(),
                KeyCode::Char('s') => self.cycle_since_window(),
                KeyCode::Char('w') => self.toggle_warnings_only(),
                KeyCode::Char('l') | KeyCode::Right => {
                    let step = saturating_step(self.config.ui.preview_scroll_step);
                    self.scroll_preview(step);
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    let step = saturating_step(self.config.ui.preview_scroll_step);
                    self.scroll_preview(-step);
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let page = saturating_step(self.config.ui.preview_page_step);
                    self.scroll_preview(page);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    let page = saturating_step(self.config.ui.preview_page_step);
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

    /// Absorb finished worker responses. Runs at the top of every loop turn, so the UI never
    /// blocks on the worker; a response for a superseded query is dropped without touching
    /// state (P3).
    ///
    /// Complexity (REQ010): `O(responses × K)` for the id lookup; no I/O, no lock held.
    fn drain_responses(&mut self) -> bool {
        let mut applied = false;
        loop {
            match self.worker.responses.try_recv() {
                Ok(outcome) => applied |= self.apply_outcome(outcome),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if !self.worker_disconnected_reported {
                        self.worker_disconnected_reported = true;
                        self.searching = false;
                        self.error = Some(
                            "the search worker stopped; press q to quit and rerun aise tui"
                                .to_string(),
                        );
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
        }
        match outcome.result {
            Ok(response) => self.apply_response(response),
            Err(error) => self.error = Some(format!("{error:#}")),
        }
        true
    }

    /// Queue a search for the current query. `send` cancels any in-flight search first —
    /// this is what makes supersede real (D3). Never `?`: a dead worker sets the error line.
    fn request_search(&mut self) {
        let generation = RequestGeneration::new();
        self.current_search_generation = generation.clone();
        self.searching = true;
        if let Err(message) = self.worker.send(WorkerRequest {
            kind: RequestKind::Search,
            generation,
            query: self.query.clone(),
            filters: self.filters.clone(),
            selected_id: self.selected_session().map(|session| session.id.clone()),
        }) {
            self.searching = false;
            self.error = Some(message);
        }
    }

    /// Ask the worker for the selected row's preview, unless it is already on screen.
    ///
    /// The skip is load-bearing, not an optimisation: every search response calls this, and
    /// without it a preserved selection (D4) would re-read up to `D_max` bytes per keystroke.
    /// A `PreviewOnly` never cancels (C21), so a held `j` merely queues requests the worker
    /// drains to the latest.
    fn request_preview(&mut self) {
        let selected = self.selected_session().map(|session| session.id.clone());
        // Invalidate any outstanding preview success/error even when the already-rendered row is
        // selected again and no replacement I/O is needed (A→B→A fast path).
        let generation = RequestGeneration::new();
        self.current_preview_generation = generation.clone();
        if selected == self.previewed_id {
            return;
        }
        if let Err(message) = self.worker.send(WorkerRequest {
            kind: RequestKind::PreviewOnly,
            generation,
            query: self.query.clone(),
            filters: self.filters.clone(),
            selected_id: selected,
        }) {
            self.error = Some(message);
        }
    }

    /// Revalidate after a filter binding and re-run the search. A rejected combination is
    /// rendered on the error line, never sent — the request would be unsatisfiable.
    fn apply_filter_change(&mut self) {
        if let Err(error) = self.filters.validate() {
            self.error = Some(format!("{error:#}"));
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
        let hours = match self.filters.since {
            None => 24,
            Some(since) => {
                let age = (Utc::now() - since).num_hours();
                if age <= 24 * 2 {
                    24 * 7
                } else if age <= 24 * 8 {
                    24 * 30
                } else {
                    0
                }
            }
        };
        self.filters.since = if hours == 0 {
            None
        } else {
            Some(Utc::now() - chrono::Duration::seconds(i64::from(hours) * 3600))
        };
        self.apply_filter_change();
    }

    fn toggle_warnings_only(&mut self) {
        self.filters.warnings_only = !self.filters.warnings_only;
        self.apply_filter_change();
    }

    /// Apply one worker response. Search responses replace the list and preserve the user's
    /// place; preview responses never replace the list (C13) and are discarded when overtaken
    /// by newer navigation.
    fn apply_response(&mut self, response: WorkerResponse) {
        if let Some(results) = response.results {
            // Clear the error only here: a PreviewOnly response arriving right after a failed
            // search must not wipe the message before a frame carried it (C30).
            self.error = None;
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
            let same_session = response.previewed_id == self.previewed_id;
            self.preview = preview;
            self.preview_line_count = response.preview_line_count;
            self.previewed_id = response.previewed_id;
            if same_session {
                self.clamp_preview_scroll();
            } else {
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

        // Session list. The label column is the configured width clamped up to the longest
        // label, so a smaller value pads but can never truncate (D10 stays fixed).
        let longest_label = longest_provider_label();
        let list_interior_width = usize::from(middle[0].width.saturating_sub(2));
        let label_width = self
            .config
            .ui
            .provider_label_width
            .max(longest_label)
            // A config value cannot request an allocation wider than the actual pane. Keep the
            // structural label floor for terminals too narrow to display it in full.
            .min(list_interior_width.max(longest_label));
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
                    .saturating_sub(AGE_SUFFIX_ALLOWANCE);
                let title = session
                    .title
                    .as_deref()
                    .map(|value| truncate_for_display(value, title_budget))
                    .unwrap_or_else(|| session.preview_text.clone());
                let age = relative_age(session.updated_at);
                let (provider_name, provider_color) = provider_label(session.provider);
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
            " · stopped"
        } else if self.searching {
            " · searching"
        } else {
            " · ready"
        };
        let list_title = format!(
            " Sessions · {mode}{activity} ({}/{}) ",
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
        if !visible_range.is_empty() {
            list_state.select(Some(self.selected - visible_range.start));
        }
        frame.render_stateful_widget(list, middle[0], &mut list_state);

        // Preview with scroll
        let preview_lines = self
            .preview
            .lines()
            .map(|line| render_preview_line(line, &self.query))
            .collect::<Vec<_>>();
        let wrap_width = usize::from(middle[1].width.saturating_sub(2)).max(1);
        self.preview_line_count = preview_lines
            .iter()
            .map(|line| line.width().max(1).div_ceil(wrap_width))
            .sum();
        self.clamp_preview_scroll();
        let preview = Paragraph::new(preview_lines)
            .block(Block::default().borders(Borders::ALL).title(" Preview "))
            .wrap(Wrap { trim: false })
            .scroll((self.preview_scroll, 0));
        frame.render_widget(preview, middle[1]);

        // Error line (own row, taken from the body when present — never the help bar),
        // middle-elided to the frame so the final recovery clause always survives (D9/REQ047).
        let status_index = chunks.len() - 1;
        let frame_width = frame.area().width as usize;
        if let Some(error) = &self.error {
            let error_line = Paragraph::new(Span::styled(
                elide_middle(error.as_str(), frame_width),
                Style::default().fg(Color::Red),
            ));
            frame.render_widget(error_line, chunks[status_index - 1]);
        }

        // Status bar (single line, contextual) — also middle-elided to the frame, so the
        // navigation hints at the head and "q: quit" at the tail both survive a narrow frame.
        let help_text = if self.search_mode {
            "Type to search │ Enter/Esc: browse".to_string()
        } else {
            "j/k: move │ PgUp/PgDn: page │ g/G: top/bottom │ h/l: scroll │ p: provider │ f: class │ s: window │ w: warnings │ /: search │ Enter: resume │ q: quit".to_string()
        };
        let bottom = Paragraph::new(Span::styled(
            elide_middle(&help_text, frame_width),
            Style::default().fg(Color::DarkGray),
        ));
        frame.render_widget(bottom, chunks[status_index]);
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

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnRole {
    User,
    Assistant,
}

#[cfg(test)]
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

#[cfg(test)]
struct Turn<'a> {
    role: TurnRole,
    body: &'a str,
}

#[cfg(test)]
fn parse_turns(transcript: &str) -> Vec<Turn<'_>> {
    parse_turns_inner(transcript, None).expect("an uncancelled parse cannot fail")
}

#[cfg(test)]
fn parse_turns_inner<'a>(
    transcript: &'a str,
    cancellation: Option<&QueryCancellation>,
) -> Result<Vec<Turn<'a>>> {
    let mut turns: Vec<Turn<'a>> = Vec::new();
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
    if let Some(cancellation) = cancellation {
        cancellation.ensure_active()?;
    }
    Ok(turns)
}

fn truncate_body(body: &str, max_lines: usize) -> String {
    let trimmed = body.trim_end();
    if trimmed.is_empty() {
        return "(empty)".to_string();
    }
    let mut source = trimmed.lines();
    let lines: Vec<&str> = source.by_ref().take(max_lines).collect();
    if source.next().is_none() {
        return trimmed.to_string();
    }
    let mut out = lines.join("\n");
    out.push_str("\n  […]");
    out
}

/// Relative weights for the preview summary's sections (Decision 2a): first prompt, first
/// reply, final prompt, final reply. `[ui].preview_lines` is the total body budget; each
/// emitted section gets a proportional share (floor-rounded), floored at one line so a small
/// budget cannot erase a bookend. The historical fixed layout was these weights verbatim —
/// a budget of 34 reproduces it exactly.
const PREVIEW_WEIGHT_FIRST_PROMPT: usize = 8;
const PREVIEW_WEIGHT_FIRST_REPLY: usize = 4;
const PREVIEW_WEIGHT_FINAL_PROMPT: usize = 8;
const PREVIEW_WEIGHT_FINAL_REPLY: usize = 14;

#[cfg(test)]
fn build_transcript_summary(transcript: &str, budget: usize) -> String {
    build_transcript_summary_inner(transcript, budget, None)
        .expect("an uncancelled summary cannot fail")
}

#[cfg(test)]
fn build_transcript_summary_cancellable(
    transcript: &str,
    budget: usize,
    cancellation: &QueryCancellation,
) -> Result<String> {
    build_transcript_summary_inner(transcript, budget, Some(cancellation))
}

#[cfg(test)]
fn build_transcript_summary_inner(
    transcript: &str,
    budget: usize,
    cancellation: Option<&QueryCancellation>,
) -> Result<String> {
    let turns = parse_turns_inner(transcript, cancellation)?;
    if turns.is_empty() {
        return Ok("(no transcript content)".to_string());
    }

    let first_user = turns.iter().position(|t| t.role == TurnRole::User);
    let first_assistant = turns.iter().position(|t| t.role == TurnRole::Assistant);
    let last_user = turns.iter().rposition(|t| t.role == TurnRole::User);
    let last_assistant = turns.iter().rposition(|t| t.role == TurnRole::Assistant);

    // (turn_index, label, relative weight)
    let candidates = [
        (
            first_user,
            "── First prompt ──",
            PREVIEW_WEIGHT_FIRST_PROMPT,
        ),
        (
            first_assistant,
            "── First reply ──",
            PREVIEW_WEIGHT_FIRST_REPLY,
        ),
        (last_user, "── Final prompt ──", PREVIEW_WEIGHT_FINAL_PROMPT),
        (
            last_assistant,
            "── Final reply ──",
            PREVIEW_WEIGHT_FINAL_REPLY,
        ),
    ];

    let mut shown_indices: Vec<usize> = Vec::new();
    let mut sections: Vec<(usize, &'static str, usize, &str)> = Vec::new();
    for (idx, label, weight) in candidates {
        let Some(idx) = idx else { continue };
        if shown_indices.contains(&idx) {
            continue;
        }
        shown_indices.push(idx);
        sections.push((idx, label, weight, turns[idx].body));
    }
    sections.sort_by_key(|(idx, _, _, _)| *idx);
    if let Some(cancellation) = cancellation {
        cancellation.ensure_active()?;
    }
    Ok(render_summary_sections(&sections, turns.len(), budget))
}

fn build_bookend_summary(bookends: &ConversationBookends, budget: usize) -> String {
    if bookends.turns.is_empty() {
        return "(no transcript content)".to_string();
    }
    let first_user = bookends.turns.iter().find(|turn| turn.role == Role::User);
    let first_assistant = bookends
        .turns
        .iter()
        .find(|turn| turn.role == Role::Assistant);
    let last_user = bookends.turns.iter().rfind(|turn| turn.role == Role::User);
    let last_assistant = bookends
        .turns
        .iter()
        .rfind(|turn| turn.role == Role::Assistant);
    let candidates = [
        (
            first_user,
            "── First prompt ──",
            PREVIEW_WEIGHT_FIRST_PROMPT,
        ),
        (
            first_assistant,
            "── First reply ──",
            PREVIEW_WEIGHT_FIRST_REPLY,
        ),
        (last_user, "── Final prompt ──", PREVIEW_WEIGHT_FINAL_PROMPT),
        (
            last_assistant,
            "── Final reply ──",
            PREVIEW_WEIGHT_FINAL_REPLY,
        ),
    ];
    let mut shown = Vec::new();
    let mut sections = Vec::new();
    for (turn, label, weight) in candidates {
        let Some(turn) = turn else { continue };
        if shown.contains(&turn.ordinal) {
            continue;
        }
        shown.push(turn.ordinal);
        sections.push((turn.ordinal, label, weight, turn.content.as_str()));
    }
    sections.sort_by_key(|(ordinal, _, _, _)| *ordinal);
    render_summary_sections(&sections, bookends.total_turns, budget)
}

fn render_summary_sections(
    sections: &[(usize, &'static str, usize, &str)],
    total: usize,
    budget: usize,
) -> String {
    let total_weight: usize = sections.iter().map(|(_, _, weight, _)| *weight).sum();
    let hidden = total.saturating_sub(sections.len());
    let mut parts = Vec::new();
    let mut last_emitted_idx = None;
    for (idx, label, weight, body) in sections {
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
        let max_lines = (budget.saturating_mul(*weight) / total_weight).max(1);
        parts.push(truncate_body(body, max_lines));
        last_emitted_idx = Some(*idx);
    }
    if hidden > 0 && sections.len() < 2 {
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

    /// One scripted TUI: owns the terminal, the event script, the app state, and the fixture
    /// database. The config and Db are leaked so `AppState<'static>` can borrow them; the leak
    /// is bounded by the number of harness instances per test process.
    struct TuiHarness {
        _dir: tempfile::TempDir,
        db: &'static Db,
        terminal: Terminal<TestBackend>,
        events: ScriptedEventSource,
        app: AppState<'static>,
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
            if config.ui.event_poll_interval_ms == 150 {
                config.ui.event_poll_interval_ms = 1;
            }
            let config: &'static Config = Box::leak(Box::new(config));
            let (worker, observed) =
                spawn_search_worker(factory).expect("worker startup handshake");
            assert!(matches!(observed, EffectiveAccessScope::All));
            let dir = tempfile::tempdir().unwrap();
            let db: &'static Db =
                Box::leak(Box::new(Db::open(&dir.path().join("index.db")).unwrap()));
            let app = AppState::new_quiet(config, worker);
            Self {
                _dir: dir,
                db,
                terminal: Terminal::new(TestBackend::new(100, 24)).unwrap(),
                events: ScriptedEventSource::new(Vec::new()),
                app,
            }
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
            harness.search_box().contains("a█"),
            "typed character renders with the visual cursor"
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
        config.ui.event_poll_interval_ms = 7;
        config.ui.list_page_step = 2;
        config.ui.preview_scroll_step = 3;
        config.ui.preview_page_step = 4;
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
            "poll slices must stay within [ui].event_poll_interval_ms, got {:?}",
            harness.events.poll_timeouts
        );

        // PageDown moves by the configured list page step, not a hardcoded 10.
        assert_eq!(harness.app.selected, 2);

        // l scrolls by the configured preview scroll step; Ctrl-d adds the page step. Settle
        // the row-2 preview first: a late preview response would overwrite the manual line
        // count mid-scroll.
        harness.wait_until_previewed("claude:idle-three");
        harness.app.preview = (0..100)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        harness.script(vec![key(KeyCode::Char('l'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.preview_scroll, 3);
        harness.script(vec![ctrl_key(KeyCode::Char('d'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.preview_scroll, 7);
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
            &cancellation,
        )
        .unwrap_err();
        assert!(error.is::<QueryCancelled>());
        assert!(is_expected_interruption(&error));
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
            key(KeyCode::Char('l')),
            key(KeyCode::Char('l')),
            key(KeyCode::Char('l')),
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
        let factory = db_backed_executor(config, EffectiveAccessScope::All, runtime);
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
        let factory = db_backed_executor(config, EffectiveAccessScope::All, runtime);
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
    fn tui_and_catalog_service_return_identical_ordered_results() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        for id in ["claude:alpha", "claude:beta", "claude:gamma"] {
            db.upsert_session(&session(id), 0, 0).unwrap();
        }
        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        let factory = db_backed_executor(
            config.clone(),
            db.access_scope().clone(),
            db.execution_runtime(),
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

        // Matching and no-match queries through the worker and the service.
        for query in ["a", "zzz"] {
            harness.script(vec![key(KeyCode::Char('/'))]);
            harness.step_until_script_drained();
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

    fn recording_executor() -> (mpsc::Receiver<(RequestKind, SearchFilters)>, SearchExecutor) {
        let (tx, rx) = mpsc::channel::<(RequestKind, SearchFilters)>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                let _ = tx.send((request.kind, request.filters.clone()));
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
        rx: &mpsc::Receiver<(RequestKind, SearchFilters)>,
        kind: RequestKind,
    ) -> SearchFilters {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok((seen, filters)) if seen == kind => return filters,
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        panic!("no {kind:?} request arrived");
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
    fn tui_filters_match_build_filters_for_equivalent_selections() {
        let (filters_rx, executor) = recording_executor();
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        wait_for_filters(&filters_rx, RequestKind::Search);
        harness.wait_until_previewed("claude:keep");

        // Drive to provider=codex, both classes, warnings-only.
        let variants = Provider::value_variants().to_vec();
        let presses = variants
            .iter()
            .position(|provider| *provider == Provider::Codex)
            .expect("codex is a provider variant")
            + 1;
        for _ in 0..presses {
            harness.script(vec![key(KeyCode::Char('p'))]);
            harness.step_until_script_drained();
            wait_for_filters(&filters_rx, RequestKind::Search);
        }
        harness.script(vec![key(KeyCode::Char('f'))]);
        harness.step_until_script_drained();
        wait_for_filters(&filters_rx, RequestKind::Search);
        harness.script(vec![key(KeyCode::Char('w'))]);
        harness.step_until_script_drained();
        let tui = wait_for_filters(&filters_rx, RequestKind::Search);

        let args = crate::cli::SessionFilterArgs {
            provider: Some(Provider::Codex),
            path: None,
            exclude_paths: Vec::new(),
            exclude_sessions: Vec::new(),
            session_kind: None,
            session_kinds: vec![SessionKind::User, SessionKind::Subagent],
            parent_session: None,
            dates: crate::dates::DateRange {
                since: None,
                until: None,
                when: None,
            },
            warnings_only: true,
        };
        let expected = crate::cli::build_filters(&args, tui_result_limit(50)).unwrap();
        assert_eq!(tui, expected, "TUI selections must equal the CLI's filters");
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
        config.ui.list_page_step = usize::MAX;
        config.ui.preview_page_step = usize::MAX;
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
    // ---- step 6: [ui].preview_lines as the preview body budget (Decision 2a) ----

    fn harness_with_preview_budget(budget: usize, transcript: &str) -> (TuiHarness, Config) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        let mut parsed = session("claude:long");
        parsed.transcript_text = transcript.to_string();
        parsed.messages = parse_turns(transcript)
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
        db.upsert_session(&parsed, 0, 0).unwrap();
        let runtime = db.execution_runtime();
        drop(db);
        let mut config = Config::default();
        config.ui.preview_lines = budget;
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        // The harness owns the tempdir for its whole life; forget it deliberately — the
        // factory reads the same file for the worker's own connection.
        let factory = db_backed_executor(config.clone(), EffectiveAccessScope::All, runtime);
        let harness = TuiHarness::with_factory(config.clone(), factory);
        // The factory's worker opens its own connection to this file; the harness lives
        // for the whole test, so leak the directory like the harness leaks its own db —
        // bounded by the number of harnesses per test process.
        std::mem::forget(dir);
        (harness, config)
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
            "[ui].preview_lines must reach the rendered preview: budget 1 produced {} lines, budget 34 produced {} (D8)",
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
            build_transcript_summary(&transcript, 34),
            "bounded normalized-message bookends must preserve the established preview output"
        );
    }
}
