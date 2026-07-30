//! Bounded-retention exhaustive traversal for canonical message search.
//!
//! [`SessionSearch::message_search_batches`](crate::service::SessionSearch::message_search_batches)
//! remains the public entry point. This module owns the dedicated read-only worker, snapshot,
//! cancellation, batch handoff, and terminal metadata lifecycle.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use anyhow::{anyhow, bail, Result};

use crate::config::Config;
use crate::db::{Db, ReadSnapshotCleanupError};
use crate::message_search::{
    MessageSearchIncludedData, MessageSearchOrigins, MessageSearchPageDocument,
    MessageSearchReceiptDocument, MessageSearchRequest, MessageSearchResultDocument,
    MessageSearchRuntimeDiagnostics, PageInfo, ResolvedMessageSearchRequest, SearchSurface,
};
use crate::models::{MessageHit, SearchExplain};
use crate::search_scope::EffectiveAccessScope;
use crate::service::{MessageSearchBatchControl, MessageService};

/// One owned, fully enriched result batch. The aligned context window at index `i` belongs to
/// result `i`; included session data is a mergeable delta for sessions first encountered in this
/// batch and is not repeated in later batches from the same request.
#[derive(Debug)]
pub struct MessageSearchBatch {
    pub(crate) results: Vec<crate::message_search::MessageSearchHit>,
    pub(crate) context_windows: Vec<Vec<MessageHit>>,
    pub(crate) included: MessageSearchIncludedData,
}

impl MessageSearchBatch {
    pub fn results(&self) -> &[crate::message_search::MessageSearchHit] {
        &self.results
    }

    pub fn context_windows(&self) -> &[Vec<MessageHit>] {
        &self.context_windows
    }

    pub const fn included(&self) -> &MessageSearchIncludedData {
        &self.included
    }

    /// Borrow one canonical semantic result without building an adapter-owned dictionary.
    pub fn result_document<'a>(
        &'a self,
        request: &'a ResolvedMessageSearchRequest,
        index: usize,
    ) -> Option<MessageSearchResultDocument<'a>> {
        self.results.get(index).map(|hit| {
            MessageSearchResultDocument::from_batch_parts(
                hit,
                request,
                self.context_windows.get(index).map(Vec::as_slice),
            )
        })
    }

    pub fn into_parts(
        self,
    ) -> (
        Vec<crate::message_search::MessageSearchHit>,
        Vec<Vec<MessageHit>>,
        MessageSearchIncludedData,
    ) {
        (self.results, self.context_windows, self.included)
    }
}

/// Terminal metadata produced only after a batch request naturally exhausts its result set.
#[derive(Debug)]
pub struct MessageSearchCompletion {
    page: PageInfo,
    planner: Option<SearchExplain>,
    origins: Option<MessageSearchOrigins>,
    ordered_digest: Option<String>,
}

impl MessageSearchCompletion {
    pub const fn page(&self) -> PageInfo {
        self.page
    }

    pub const fn search_explanation(&self) -> Option<&SearchExplain> {
        self.planner.as_ref()
    }

    pub const fn parameter_origins(&self) -> Option<&MessageSearchOrigins> {
        self.origins.as_ref()
    }

    pub fn ordered_digest(&self) -> Option<&str> {
        self.ordered_digest.as_deref()
    }

    pub const fn page_document(&self) -> MessageSearchPageDocument {
        MessageSearchPageDocument::from_page(self.page)
    }

    pub fn receipt_document<'a>(
        &'a self,
        request: &ResolvedMessageSearchRequest,
    ) -> Option<MessageSearchReceiptDocument<'a>> {
        MessageSearchReceiptDocument::from_parts(
            request.receipt_level(),
            self.search_explanation(),
            self.parameter_origins(),
            self.ordered_digest(),
        )
    }
}

enum MessageSearchBatchEvent {
    Batch(MessageSearchBatch),
    Complete(Box<MessageSearchCompletion>),
    Failed(String),
}

enum MessageSearchBatchState {
    Open,
    Exhausted(Box<MessageSearchCompletion>),
    Closed,
    Failed(String),
}

#[derive(Debug)]
struct MessageSearchCancelled;

impl std::fmt::Display for MessageSearchCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("message-search batches were cancelled")
    }
}

impl std::error::Error for MessageSearchCancelled {}

pub(crate) fn ensure_message_search_active(cancellation: Option<&AtomicBool>) -> Result<()> {
    if cancellation.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err(MessageSearchCancelled.into());
    }
    Ok(())
}

fn is_expected_message_search_cancellation(error: &anyhow::Error) -> bool {
    !error.is::<ReadSnapshotCleanupError>()
        && (error.is::<MessageSearchCancelled>()
            || error.chain().any(|cause| {
                matches!(
                    cause.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::SqliteFailure(inner, _))
                        if inner.code == rusqlite::ErrorCode::OperationInterrupted
                )
            }))
}

/// Bounded-retention exhaustive message-search batches.
///
/// The ordinary [`MessageService::search`] API remains the easiest path for callers that want one
/// materialized list. This owner runs the one-snapshot visitor on a dedicated read-only connection
/// and transfers at most one enriched batch through a rendezvous channel. `close` interrupts an
/// active SQLite call, releases the snapshot without draining unread rows, and joins the producer.
/// Internally, the caller's current batch and the producer's next handoff can coexist; callers may
/// also retain any number of already returned owned batches, so their memory is caller-controlled.
///
/// # Complexity
///
/// Let `C` be SQLite candidate work, `H` returned hits, `B` `batch_rows`, `T_B` selected text and
/// included metadata bytes in one batch, `W_B` context-window rows for one batch, `U` distinct
/// sessions whose metadata has been emitted, and `I_U` their total ID bytes. SQLite owns filtering,
/// indexed candidate selection, ordering, offsets, context joins, and session-metadata joins. The
/// producer steps one result statement, so retrieval is `O(C + H)` rather than repeated offset
/// scans. With `P = ceil(H / B)`, context uses at most `P` set queries when requested; each
/// requested metadata form uses at most `min(P, U)` set queries, `O(H log U)` deduplication work,
/// and emits each session's metadata bytes once. Internal live memory is
/// `O(B + T_B + W_B + U + I_U)` plus one worker thread and one read-only SQLite connection;
/// memory for batches retained by the caller is additional. Closing does not scan unread hits,
/// though its latency includes the active SQLite progress-handler interval or one selected
/// field's current Rust enrichment step.
pub struct MessageSearchBatches {
    request: ResolvedMessageSearchRequest,
    runtime_diagnostics: Option<MessageSearchRuntimeDiagnostics>,
    start: Option<mpsc::SyncSender<()>>,
    events: Option<mpsc::Receiver<MessageSearchBatchEvent>>,
    cancellation: Arc<AtomicBool>,
    interrupt: Option<rusqlite::InterruptHandle>,
    worker: Option<std::thread::JoinHandle<std::result::Result<(), String>>>,
    state: MessageSearchBatchState,
}

impl MessageSearchBatches {
    /// The request after purpose, operation, surface, and typed defaults were resolved.
    pub const fn request(&self) -> &ResolvedMessageSearchRequest {
        &self.request
    }

    /// Request-wide runtime diagnostics, computed once when explicitly requested.
    ///
    /// Per-batch [`MessageSearchBatch::included`] contains only session-scoped metadata deltas, so
    /// adapters can emit these invariant diagnostics once in their initial metadata record.
    pub const fn runtime_diagnostics(&self) -> Option<&MessageSearchRuntimeDiagnostics> {
        self.runtime_diagnostics.as_ref()
    }

    pub fn next_batch(&mut self) -> Result<Option<MessageSearchBatch>> {
        match &self.state {
            MessageSearchBatchState::Exhausted(_) => return Ok(None),
            MessageSearchBatchState::Closed => {
                bail!(
                    "message-search batches are closed; start a new batch request to restart the search"
                )
            }
            MessageSearchBatchState::Failed(error) => {
                bail!("message-search batches previously failed: {error}")
            }
            MessageSearchBatchState::Open => {}
        }
        // Construction resolves and snapshots the request but does not start result SQL. The first
        // read supplies demand, so an immediate close releases the snapshot without racing an FTS
        // virtual-table constructor or doing work the caller never requested.
        if let Some(start) = self.start.take() {
            let _ = start.send(());
        }
        let event = match self
            .events
            .as_ref()
            .ok_or_else(|| anyhow!("message-search batches lost their producer channel"))?
            .recv()
        {
            Ok(event) => event,
            Err(_) => {
                self.events.take();
                let error = self.join_worker()?.unwrap_or_else(|| {
                    "message-search batch producer exited without a terminal state".to_string()
                });
                self.state = MessageSearchBatchState::Failed(error.clone());
                bail!("message-search batches failed: {error}");
            }
        };
        match event {
            MessageSearchBatchEvent::Batch(batch) => Ok(Some(batch)),
            MessageSearchBatchEvent::Complete(completion) => {
                self.state = MessageSearchBatchState::Exhausted(completion);
                self.events.take();
                if let Some(error) = self.join_worker()? {
                    self.state = MessageSearchBatchState::Failed(error.clone());
                    bail!("message-search batches failed after terminal metadata: {error}");
                }
                Ok(None)
            }
            MessageSearchBatchEvent::Failed(error) => {
                self.state = MessageSearchBatchState::Failed(error.clone());
                self.events.take();
                if let Some(worker_error) = self.join_worker()? {
                    debug_assert_eq!(worker_error, error);
                }
                bail!("message-search batches failed: {error}")
            }
        }
    }

    /// Return terminal metadata without reading unread batches.
    pub fn completion(&self) -> Result<&MessageSearchCompletion> {
        match &self.state {
            MessageSearchBatchState::Exhausted(completion) => Ok(completion),
            MessageSearchBatchState::Open => bail!(
                "message-search batches have unread results; call next_batch() until it returns None, or call close() to stop without terminal metadata"
            ),
            MessageSearchBatchState::Closed => bail!(
                "message-search batches were closed before natural exhaustion; no terminal metadata exists"
            ),
            MessageSearchBatchState::Failed(error) => {
                bail!("message-search batches failed before completion: {error}")
            }
        }
    }

    /// Stop without draining unread results. Repeated calls are safe.
    pub fn close(&mut self) -> Result<()> {
        if matches!(
            self.state,
            MessageSearchBatchState::Closed | MessageSearchBatchState::Exhausted(_)
        ) {
            return match self.join_worker()? {
                Some(error) => {
                    bail!("message-search batch producer failed during cleanup: {error}")
                }
                None => Ok(()),
            };
        }
        self.cancellation.store(true, Ordering::Release);
        self.start.take();
        if let Some(interrupt) = self.interrupt.take() {
            interrupt.interrupt();
        }
        self.events.take();
        let worker_error = self.join_worker()?;
        self.state = MessageSearchBatchState::Closed;
        match worker_error {
            Some(error) => {
                bail!("message-search batch producer failed while closing: {error}")
            }
            None => Ok(()),
        }
    }

    fn join_worker(&mut self) -> Result<Option<String>> {
        self.interrupt.take();
        if let Some(worker) = self.worker.take() {
            return match worker.join() {
                Ok(Ok(())) => Ok(None),
                Ok(Err(error)) => Ok(Some(error)),
                Err(_) => Err(anyhow!(
                    "message-search batch producer panicked during cleanup"
                )),
            };
        }
        Ok(None)
    }

    pub(crate) fn spawn(
        config: Config,
        access: EffectiveAccessScope,
        surface: SearchSurface,
        request: MessageSearchRequest,
        batch_rows: NonZeroUsize,
    ) -> Result<Self> {
        Self::spawn_with_before_traversal(config, access, surface, request, batch_rows, || {})
    }

    pub(crate) fn spawn_with_before_traversal(
        config: Config,
        access: EffectiveAccessScope,
        surface: SearchSurface,
        request: MessageSearchRequest,
        batch_rows: NonZeroUsize,
        before_traversal: impl FnOnce() + Send + 'static,
    ) -> Result<Self> {
        // Request-only eligibility is checked before allocating channels or starting a thread, so
        // callers receive the typed MessageSearchError and invalid input owns no resources. The
        // worker repeats the rule on the resolved plan as a drift assertion after config policy.
        MessageService::validate_batch_request(surface, &request)?;
        let (ready_sender, ready_receiver) = mpsc::sync_channel::<
            std::result::Result<
                (
                    ResolvedMessageSearchRequest,
                    Option<MessageSearchRuntimeDiagnostics>,
                    rusqlite::InterruptHandle,
                ),
                String,
            >,
        >(1);
        let (start_sender, start_receiver) = mpsc::sync_channel(0);
        // A rendezvous channel provides backpressure without retaining a queued result batch.
        // The producer may hold one batch while the consumer owns and may retain prior batches.
        // This bounds internal queued work, not memory deliberately retained by the caller.
        let (event_sender, event_receiver) = mpsc::sync_channel(0);
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);
        let worker = std::thread::Builder::new()
            .name("aise-message-search-batches".into())
            .spawn(move || {
                let mut ready_sent = false;
                let result = (|| -> Result<()> {
                    let worker_threads = NonZeroUsize::new(config.resolve_threads())
                        .expect("Config::resolve_threads always returns at least one");
                    let mut db = Db::open_existing_read_only_with_threads(
                        &config.db_path(),
                        config.index.busy_timeout_ms,
                        worker_threads,
                    )?;
                    db.set_access_scope(access);
                    anyhow::ensure!(
                        db.schema_version()? == crate::db::SCHEMA_VERSION,
                        "exhaustive batched message search requires database schema {}; run `aise reindex --full`, then retry",
                        crate::db::SCHEMA_VERSION
                    );
                    db.interrupt_while(Arc::clone(&worker_cancellation));
                    db.with_read_snapshot(|| {
                        let service = MessageService::new(&config, &db, surface);
                        let plan = service.plan(request)?;
                        MessageService::validate_batch_plan(&plan)?;
                        let resolved_request = ResolvedMessageSearchRequest::from_plan(&plan)?;
                        let runtime_diagnostics =
                            service.runtime_diagnostics_for_plan(&plan)?;
                        let interrupt = db.interrupt_handle();
                        ready_sender
                            .send(Ok((resolved_request, runtime_diagnostics, interrupt)))
                            .map_err(|_| {
                                anyhow!("message-search batches were dropped during startup")
                            })?;
                        ready_sent = true;
                        before_traversal();
                        start_receiver
                            .recv()
                            .map_err(|_| anyhow::Error::new(MessageSearchCancelled))?;
                        ensure_message_search_active(Some(&worker_cancellation))?;
                        let outcome = service.visit_search_plan_batches(
                            plan,
                            batch_rows,
                            Some(&worker_cancellation),
                            |batch| {
                                ensure_message_search_active(Some(&worker_cancellation))?;
                                match event_sender.send(MessageSearchBatchEvent::Batch(batch)) {
                                    Ok(()) => Ok(MessageSearchBatchControl::Continue),
                                    Err(_) => Ok(MessageSearchBatchControl::Stop),
                                }
                            },
                        )?;
                        if outcome.exhausted && !worker_cancellation.load(Ordering::Acquire) {
                            let completion = MessageSearchCompletion {
                                page: outcome
                                    .page
                                    .expect("naturally exhausted traversal has page metadata"),
                                planner: outcome.planner,
                                origins: outcome.origins,
                                ordered_digest: outcome.ordered_digest,
                            };
                            let _ = event_sender
                                .send(MessageSearchBatchEvent::Complete(Box::new(completion)));
                        }
                        Ok(())
                    })
                })();
                match result {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        let rendered = format!("{error:#}");
                        if !ready_sent {
                            let _ = ready_sender.send(Err(rendered));
                            Ok(())
                        } else if worker_cancellation.load(Ordering::Acquire)
                            && is_expected_message_search_cancellation(&error)
                        {
                            Ok(())
                        } else {
                            let _ = event_sender
                                .send(MessageSearchBatchEvent::Failed(rendered.clone()));
                            Err(rendered)
                        }
                    }
                }
            })
            .map_err(|error| {
                anyhow!("failed to start message-search batch producer: {error}")
            })?;
        let (request, runtime_diagnostics, interrupt) = match ready_receiver.recv() {
            Ok(Ok(ready)) => ready,
            Ok(Err(error)) => {
                match worker.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(worker_error)) => {
                        bail!(
                            "failed to open message-search batches: {error}; producer cleanup also failed: {worker_error}"
                        )
                    }
                    Err(_) => {
                        bail!(
                            "failed to open message-search batches: {error}; producer panicked during startup cleanup"
                        )
                    }
                }
                bail!("failed to open message-search batches: {error}");
            }
            Err(_) => match worker.join() {
                Ok(Ok(())) => {
                    bail!("message-search batch producer exited during startup")
                }
                Ok(Err(error)) => {
                    bail!("message-search batch producer failed during startup: {error}")
                }
                Err(_) => {
                    bail!("message-search batch producer panicked during startup")
                }
            },
        };
        Ok(Self {
            request,
            runtime_diagnostics,
            start: Some(start_sender),
            events: Some(event_receiver),
            cancellation,
            interrupt: Some(interrupt),
            worker: Some(worker),
            state: MessageSearchBatchState::Open,
        })
    }
}

impl Drop for MessageSearchBatches {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_cleanup_failure_is_never_treated_as_successful_cancellation() {
        let error = anyhow::Error::new(ReadSnapshotCleanupError::new(
            Some(anyhow::Error::new(MessageSearchCancelled)),
            anyhow!("injected rollback failure"),
        ));

        assert!(!is_expected_message_search_cancellation(&error));
        assert!(error
            .to_string()
            .contains("read snapshot also failed to release"));
    }
}
