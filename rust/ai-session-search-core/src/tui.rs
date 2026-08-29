// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::num::NonZeroUsize;
use std::sync::{mpsc, Arc, Mutex};
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
use crate::db::{Db, QueryCancellation, SCHEMA_VERSION};
use crate::models::{Provider, SearchFilters, SessionRecord};
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
    let (worker, _observed_scope) = spawn_search_worker(db_backed_executor(
        config.clone(),
        db.access_scope().clone(),
    ))?;
    let mut app = AppState::new(config, db, worker)?;
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

/// Which kind of work a [`WorkerRequest`] asks for. The discriminant is load-bearing: a
/// preview-only response must never replace the result list (C13).
///
/// First CONSTRUCTED by step 4's request builders; present now so step 3's RED tests compile
/// against real types (E32). The allow is removed in step 4.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Search,
    PreviewOnly,
}

/// A unit of work for the search worker. First CONSTRUCTED by step 4's request builders;
/// the type exists now so step 3's RED tests compile against real types and fail on behavior,
/// not on `cannot find type` (E32) — hence the temporary allow, removed in step 4.
#[allow(dead_code)]
struct WorkerRequest {
    kind: RequestKind,
    query: String,
    filters: SearchFilters,
    selected_id: Option<String>,
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
///
/// Fields are first READ by step 4's `apply_response`; present now so step 3's RED tests
/// compile against real types (E32). The allow is removed in step 4.
#[allow(dead_code)]
struct WorkerResponse {
    query: String,
    results: Option<Vec<SessionRecord>>,
    previewed_id: Option<String>,
    preview: Option<String>,
    preview_line_count: usize,
}

impl WorkerResponse {
    /// A search response: results only, no preview.
    fn results(request: &WorkerRequest, rows: Vec<SessionRecord>) -> Self {
        Self {
            query: request.query.clone(),
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
            query: request.query.clone(),
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

/// RAII owner. A bare tuple has no destructor, so any `?` between spawn and join leaks a
/// thread holding an open SQLite connection (C3).
///
/// Cleanup: cancels the in-flight query, drops the request sender to end the loop, and joins.
/// Cancellation lands within at most one scoring batch (E19b), which bounds quit latency.
struct SearchWorker {
    requests: Option<mpsc::Sender<WorkerRequest>>,
    responses: mpsc::Receiver<Result<WorkerResponse>>,
    in_flight: Arc<Mutex<Option<Arc<QueryCancellation>>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SearchWorker {
    /// Stop whatever query is running.
    ///
    /// The guard is bound to a local rather than left as a temporary: the interrupt handle is
    /// connection-scoped (E21b), so `cancel()` must not overlap the worker publishing the next
    /// query's cancellation, or it would interrupt that one instead and the worker would
    /// silently drop its result as an expected supersede.
    fn cancel_in_flight(&self) {
        let mut slot = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cancellation) = slot.take() {
            cancellation.cancel();
        }
    }

    /// Queue a request. Never returns `Err` as control flow: a dead worker sets the error
    /// line, it does not end the TUI (C14).
    ///
    /// Only a new SEARCH supersedes a running one. A `PreviewOnly` request from j/k must not
    /// cancel an in-flight search: the worker would treat the interruption as an expected
    /// supersede, send nothing, and then answer with `results: None`, stranding a stale list
    /// with no error (C21). Navigation is already instant without this — the UI moves the
    /// selection locally. First called by step 4's request builders (E32 ordering).
    #[allow(dead_code)]
    fn send(&self, request: WorkerRequest) -> std::result::Result<(), String> {
        if request.kind == RequestKind::Search {
            self.cancel_in_flight();
        }
        match self.requests.as_ref() {
            Some(sender) => sender.send(request).map_err(|_| {
                "the search worker stopped; press q to quit and rerun aise tui".to_string()
            }),
            None => Err("the search worker is shut down".to_string()),
        }
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        self.cancel_in_flight();
        self.requests.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn the search worker. Concurrency and cleanup (REQ010): one thread, one SQLite
/// connection, one lazy Rayon pool of `config.resolve_threads()` workers (owned by the
/// production executor). The only shared mutable state is the in-flight cancellation slot,
/// whose lock is held for `O(1)` and never across a query. `Drop` cancels, closes the request
/// channel, and joins; join latency is bounded by one scoring batch (E19b), not the query.
fn spawn_search_worker(
    make_executor: ExecutorFactory,
) -> Result<(SearchWorker, EffectiveAccessScope)> {
    let (ready_tx, ready_rx) =
        mpsc::sync_channel::<std::result::Result<EffectiveAccessScope, String>>(1);
    let (request_tx, request_rx) = mpsc::channel::<WorkerRequest>();
    let (response_tx, response_rx) = mpsc::channel::<Result<WorkerResponse>>();
    // The ONE cancellation slot. Both the worker thread and the returned SearchWorker hold
    // clones of this same Arc — a second slot is what made supersede a silent no-op (C20).
    let in_flight: Arc<Mutex<Option<Arc<QueryCancellation>>>> = Arc::new(Mutex::new(None));
    let worker_in_flight = Arc::clone(&in_flight);
    let handle = thread::Builder::new()
        .name("aise-tui-search".to_string())
        .spawn(move || {
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
            while let Ok(first) = request_rx.recv() {
                // Drain to the latest queued request: superseded searches never execute (§2.3).
                let request = request_rx.try_iter().last().unwrap_or(first);
                // Publish under the same lock the UI cancels under, so a supersede either stops
                // THIS query or arrives before it starts — never lands on its successor (E21b).
                let cancellation = Arc::new(QueryCancellation::new());
                {
                    let mut slot = worker_in_flight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *slot = Some(Arc::clone(&cancellation));
                }
                let outcome = execute(&request, &cancellation);
                {
                    let mut slot = worker_in_flight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    slot.take();
                }
                match outcome {
                    Ok(response) => {
                        let _ = response_tx.send(Ok(response));
                    }
                    // Superseded mid-query: silent by design; the newer request owns the screen.
                    Err(error) if is_expected_interruption(&error) => {}
                    Err(error) => {
                        let _ = response_tx.send(Err(error));
                    }
                }
            }
        })?;
    // Startup handshake: block until the executor opened its connection, so a startup failure
    // surfaces with the terminal restored rather than a TUI that silently never populates.
    let observed = ready_rx
        .recv()
        .map_err(|_| anyhow::anyhow!("the search worker exited during startup"))?
        .map_err(|message| anyhow::anyhow!("the search worker failed to start: {message}"))?;
    Ok((
        SearchWorker {
            requests: Some(request_tx),
            responses: response_rx,
            in_flight,
            handle: Some(handle),
        },
        observed,
    ))
}

/// Two lines, not `message_search_batches`'s predicate: that one is a private bare `fn` and
/// also matches `MessageSearchCancelled` and `ReadSnapshotCleanupError`, neither of which this
/// worker can produce (E22).
fn is_expected_interruption(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
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
/// the CLI, MCP, and Python use. Preview resolution stays on `Db` because
/// `CatalogService::resolve_session` returns no transcript (§6.4).
///
/// Complexity (REQ010): one connection (≤64 MiB page cache, 256 MiB virtual mmap window), one
/// lazy Rayon pool bounded by `config.resolve_threads()`; per request the bounds are
/// `db.search`/`list_recent`'s documented ones, plus `O(D_max)` transient transcript bytes for
/// a preview.
fn db_backed_executor(config: Config, access: EffectiveAccessScope) -> ExecutorFactory {
    Box::new(move || {
        let worker_threads = NonZeroUsize::new(config.resolve_threads())
            .expect("Config::resolve_threads always returns at least one");
        let mut db = Db::open_existing_read_only_with_threads(
            &config.db_path(),
            config.index.busy_timeout_ms,
            worker_threads,
        )?;
        db.set_access_scope(access);
        anyhow::ensure!(
            db.schema_version()? == SCHEMA_VERSION,
            "the TUI search worker requires database schema {SCHEMA_VERSION}; \
             run `aise reindex --full`, then retry"
        );
        let observed = db.access_scope().clone();
        let repo = current_repo(&config);
        let scoring = config.search.scoring.clone();
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
                                .search_sessions(
                                    &request.query,
                                    &request.filters,
                                    repo.as_deref(),
                                    &scoring,
                                )?
                                .into_iter()
                                .map(|hit| hit.session)
                                .collect(),
                        )),
                        RequestKind::PreviewOnly => {
                            // The one place the TUI still touches `Db` directly:
                            // `CatalogService::resolve_session` returns no transcript (§6.4).
                            // O(D_max) transient.
                            let text = match request.selected_id.as_deref() {
                                Some(id) => {
                                    let resolved = db.resolve_session(id)?;
                                    Some(build_transcript_summary(&resolved.transcript_text))
                                }
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
    /// Temporary: the key dispatch moved onto `AppState` and still calls
    /// `refresh`/`move_selection`/`select_index`, which need the handle. Step 4 moves that
    /// work onto the worker and deletes this field.
    db: &'a Db,
    /// The session filters the browser runs with. The state IS a `SearchFilters` value: no
    /// parallel filter representation exists (R8/§5.5d).
    filters: SearchFilters,
    /// Search worker. Spawned from step 2; no key arm consults it until step 4 re-routes
    /// refresh/navigation onto requests.
    worker: SearchWorker,
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
    fn new(config: &'a Config, db: &'a Db, worker: SearchWorker) -> Result<Self> {
        let mut state = Self {
            config,
            db,
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

    /// Absorb finished worker responses. Step 4 applies them to state; until a key arm sends
    /// a request the channel is always empty, so this stays observationally a no-op.
    fn drain_responses(&mut self) {
        while let Ok(response) = self.worker.responses.try_recv() {
            // Step 4 replaces this with apply_response; nothing can send before then.
            drop(response);
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
        let results = if self.query.trim().is_empty() {
            db.list_recent(&self.filters)?
        } else {
            db.search(
                &self.query,
                &self.filters,
                current_repo(self.config).as_deref(),
                &self.config.search.scoring,
            )?
            .into_iter()
            .map(|hit| hit.session)
            .collect()
        };
        self.results = results;
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
            let config: &'static Config = Box::leak(Box::new(config));
            let (worker, observed) =
                spawn_search_worker(Box::new(move || Ok((executor, EffectiveAccessScope::All))))
                    .expect("worker startup handshake");
            assert!(matches!(observed, EffectiveAccessScope::All));
            let dir = tempfile::tempdir().unwrap();
            let db: &'static Db =
                Box::leak(Box::new(Db::open(&dir.path().join("index.db")).unwrap()));
            let app = AppState::new(config, db, worker).unwrap();
            Self {
                _dir: dir,
                db,
                terminal: Terminal::new(TestBackend::new(100, 24)).unwrap(),
                events: ScriptedEventSource::new(Vec::new()),
                app,
            }
        }

        /// Install the starting rows on `AppState::results` AND in the fixture database, so an
        /// inline refresh or a worker query sees the same corpus.
        fn seeded(mut self, sessions: &[&str]) -> Self {
            let mut records = Vec::new();
            for id in sessions {
                let parsed = session(id);
                self.db.upsert_session(&parsed, 0, 0).unwrap();
                records.push(parsed.session);
            }
            self.app.results = records;
            self
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

        // Esc returns to browse so the navigation keys below apply.
        harness.script(vec![key(KeyCode::Esc)]);
        harness.step_until_script_drained();
        assert!(!harness.app.search_mode);

        // j moves the selection and loads the preview of the new row.
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.selected, 1);

        // Ctrl-d scrolls the preview by the page step against the content-length bound.
        harness.app.preview_line_count = 100;
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

        // The idle poll paces with the configured interval, observed clock-free through the
        // seam's recorded timeouts (every poll — including the one that returns false).
        harness.script(vec![key(KeyCode::PageDown)]);
        assert!(harness.step_until_script_drained().is_none());
        assert!(
            harness
                .events
                .poll_timeouts
                .iter()
                .all(|timeout| *timeout == Duration::from_millis(7)),
            "poll timeout must follow [ui].event_poll_interval_ms, got {:?}",
            harness.events.poll_timeouts
        );

        // PageDown moves by the configured list page step, not a hardcoded 10.
        assert_eq!(harness.app.selected, 2);

        // l scrolls by the configured preview scroll step; Ctrl-d adds the page step.
        harness.app.preview_line_count = 100;
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
    fn rows(ids: &[&str]) -> Vec<SessionRecord> {
        ids.iter().map(|id| session(id).session).collect()
    }

    /// Wait for one executor completion, failing with the reason that carries the RED signal:
    /// the executor can only run if the UI actually sent a request (§5.3).
    fn wait_for_executed(executed: &mpsc::Receiver<String>) -> String {
        executed
            .recv_timeout(Duration::from_secs(5))
            .expect("executor ran (it can only if the UI sent a request)")
    }

    /// Wait for one observed request shape (kind, query, selected_id).
    fn wait_for_request(
        requests: &mpsc::Receiver<(RequestKind, String, Option<String>)>,
    ) -> (RequestKind, String, Option<String>) {
        requests
            .recv_timeout(Duration::from_secs(5))
            .expect("a request reached the executor")
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
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    gate.recv()
                        .map_err(|_| anyhow::anyhow!("harness dropped the gate"))?;
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
        wait_for_executed(&executed_rx);
        harness.step();
        assert!(harness.session_rows().contains("after"));
        finish_gated(&release);
    }

    #[test]
    fn backspace_echoes_while_the_executor_is_blocked() {
        let (release, gate) = mpsc::channel::<()>();
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    gate.recv()
                        .map_err(|_| anyhow::anyhow!("harness dropped the gate"))?;
                }
                let _ = executed_tx.send(request.query.clone());
                Ok(match request.kind {
                    RequestKind::Search => WorkerResponse::results(request, Vec::new()),
                    RequestKind::PreviewOnly => WorkerResponse::preview(request, None),
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![
            key(KeyCode::Char('/')),
            key(KeyCode::Char('a')),
            key(KeyCode::Backspace),
        ]);
        harness.step_until_script_drained();
        // Backspace rendered immediately (query lost 'a'); a whitespace-only query counts as
        // empty, matching refresh's trim() contract.
        assert!(!harness.search_box().contains("a█"));
        assert_eq!(harness.app.query, "");
        release.send(()).unwrap();
        wait_for_executed(&executed_rx);
        finish_gated(&release);
    }

    #[test]
    fn worker_serves_only_the_latest_of_a_drained_burst() {
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                let _ = executed_tx.send(request.query.clone());
                Ok(WorkerResponse::results(request, Vec::new()))
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![
            key(KeyCode::Char('/')),
            key(KeyCode::Char('a')),
            key(KeyCode::Char('b')),
            key(KeyCode::Char('c')),
            key(KeyCode::Char('d')),
            key(KeyCode::Char('e')),
        ]);
        harness.step_until_script_drained();
        // Collect until quiet: the worker executes in microseconds after the sends.
        let mut executed: Vec<String> = Vec::new();
        while let Ok(query) = executed_rx.recv_timeout(Duration::from_millis(200)) {
            executed.push(query);
        }
        // The startup empty query runs, then the burst must coalesce to its latest: the
        // five queued searches never execute as five (§2.3 drain-to-latest).
        assert_eq!(
            executed,
            vec![String::new(), "e".to_string()],
            "a drained burst must serve only the latest request"
        );
    }

    #[test]
    fn superseding_a_query_cancels_the_one_in_flight() {
        let (release, gate) = mpsc::channel::<()>();
        let (observed_tx, observed_rx) = mpsc::channel::<Arc<QueryCancellation>>();
        let executor = Box::new(
            move |request: &WorkerRequest, cancellation: &Arc<QueryCancellation>| {
                let _ = observed_tx.send(Arc::clone(cancellation));
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    gate.recv()
                        .map_err(|_| anyhow::anyhow!("harness dropped the gate"))?;
                }
                Ok(WorkerResponse::results(request, Vec::new()))
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        // The fake executor cloned the FIRST request's cancellation out; the test owns the
        // only other Arc, which is what makes is_cancelled() assertable at all (C29).
        let first = observed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("first search reached the executor");
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
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::Search && !request.query.trim().is_empty() {
                    gate.recv()
                        .map_err(|_| anyhow::anyhow!("harness dropped the gate"))?;
                }
                let id = format!("claude:{}", request.query);
                let _ = executed_tx.send(request.query.clone());
                Ok(WorkerResponse::results(request, rows(&[&id])))
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:before"]);
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        harness.script(vec![key(KeyCode::Char('b'))]);
        harness.step_until_script_drained();
        // Release both: the stale "a" response must be dropped by the originating-query
        // guard, never rendered; the fresh "b" response applies.
        release.send(()).unwrap();
        release.send(()).unwrap();
        wait_for_executed(&executed_rx);
        wait_for_executed(&executed_rx);
        harness.step();
        harness.step();
        assert!(
            !harness.session_rows().contains("claude:a"),
            "a response for a superseded query must not mutate state"
        );
        assert!(harness.session_rows().contains("claude:b"));
        finish_gated(&release);
    }

    #[test]
    fn preview_only_response_does_not_replace_the_result_list() {
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                let _ = executed_tx.send(request.query.clone());
                Ok(match request.kind {
                    RequestKind::Search => WorkerResponse::results(request, rows(&["claude:keep"])),
                    RequestKind::PreviewOnly => {
                        WorkerResponse::preview(request, Some("preview body".to_string()))
                    }
                })
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        harness.step_until_script_drained();
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        wait_for_executed(&executed_rx);
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
        let (kind, _, selected) = wait_for_request(&requests_rx);
        assert_eq!(
            (kind, selected),
            (RequestKind::PreviewOnly, Some("claude:one".into()))
        );
        harness.step();
        harness.step();
        assert!(harness.app.preview.contains("claude:one"));

        // Query change that drops the selected row: the new first row must get its preview.
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('z'))]);
        harness.step_until_script_drained();
        let (kind, _, selected) = wait_for_request(&requests_rx);
        assert_eq!(
            (kind, selected),
            (RequestKind::PreviewOnly, Some("claude:two".into())),
            "the preview must follow whatever row the UI actually selected (C28)"
        );
        harness.step();
        harness.step();
        assert!(harness.app.preview.contains("claude:two"));
    }

    #[test]
    fn a_preview_overtaken_by_newer_navigation_is_discarded() {
        let (release, gate) = mpsc::channel::<()>();
        let (executed_tx, executed_rx) = mpsc::channel::<String>();
        let executor = Box::new(
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::PreviewOnly {
                    gate.recv()
                        .map_err(|_| anyhow::anyhow!("harness dropped the gate"))?;
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
        // The startup preview (row 0) parks; j selects row 1 and queues its preview.
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.selected, 1);
        release.send(()).unwrap();
        wait_for_executed(&executed_rx);
        harness.step();
        assert!(
            !harness.app.preview.contains("claude:one"),
            "a preview overtaken by newer navigation must be discarded, not rendered beside the wrong row"
        );
        // The queued row-1 preview then executes (drained burst) and applies.
        release.send(()).unwrap();
        wait_for_executed(&executed_rx);
        harness.step();
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
        wait_for_executed(&executed_rx); // startup search
        wait_for_executed(&executed_rx); // startup preview
        harness.step();
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        wait_for_executed(&executed_rx); // row-1 preview
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
        wait_for_executed(&executed_rx); // the reordered search response
        harness.step();
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
            move |request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                if request.kind == RequestKind::PreviewOnly {
                    gate.recv()
                        .map_err(|_| anyhow::anyhow!("harness dropped the gate"))?;
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
        wait_for_executed(&executed_rx);
    }

    #[test]
    fn a_dead_worker_reports_an_error_and_leaves_the_ui_usable() {
        let executor = Box::new(
            move |_request: &WorkerRequest, _cancellation: &Arc<QueryCancellation>| {
                panic!("executor exploded")
            },
        );
        let mut harness = TuiHarness::with_executor(executor).seeded(&["claude:keep"]);
        harness.step_until_script_drained();
        harness.script(vec![key(KeyCode::Char('/')), key(KeyCode::Char('a'))]);
        harness.step_until_script_drained();
        assert!(
            harness.app.error.is_some(),
            "a dead worker must surface as the error line, not a hang or an exit (C14)"
        );
        harness.step();
        assert!(
            harness.error_line().contains("worker"),
            "the error line must render the failure"
        );
        // The list stays navigable and q still quits.
        harness.script(vec![key(KeyCode::Char('j'))]);
        harness.step_until_script_drained();
        assert_eq!(harness.app.selected, 1);
        harness.script(vec![key(KeyCode::Char('q'))]);
        assert!(matches!(
            harness.step_until_script_drained(),
            Some(AppAction::Quit)
        ));
    }
}
